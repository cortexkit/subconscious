use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use subc_os::{
    create_private_directory_exclusive, open_directory_nofollow_all, retained_file_index,
    windows_acl::{
        create_private_dir, create_private_file, verify_current_owner, verify_owner_only,
        verify_private_dir,
    },
    OpenRegularError,
};

use super::{absent, is_link, refused, AUTH};

const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const DELETE_ON_CLOSE: u32 = 0x0400_0000;
const SHARE_READ_WRITE: u32 = 0x0000_0003;

pub(in super::super) struct Directory {
    path: PathBuf,
    _handle: File,
}

impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        let handle = open_directory_nofollow_all(path).map_err(|error| match error {
            OpenRegularError::Io(error) => error,
            other => refused(&other.to_string()),
        })?;
        Ok(Self {
            path: path.into(),
            _handle: handle,
        })
    }

    pub(super) fn open_data_home(path: &Path) -> io::Result<Self> {
        // Resolve the user's junctions once. Managed paths use the resolved
        // directory so they never have to traverse the original junctions again.
        Self::open(&fs::canonicalize(path)?)
    }

    pub(super) fn open_install_root(&self, name: &str, create: bool) -> io::Result<Self> {
        let folder = self.open_child(name, create)?;
        verify_current_owner(folder.path(), true)?;
        Ok(folder)
    }

    fn open_child(&self, name: &str, create: bool) -> io::Result<Self> {
        let path = self.path.join(name);
        match Self::open(&path) {
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                match create_private_directory_exclusive(&path) {
                    Ok(()) => (),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                    Err(error) => return Err(error),
                }
                Self::open(&path)
            }
            result => result,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn managed(&self, name: &str, create: bool) -> io::Result<Self> {
        let folder = self.open_child(name, create)?;
        let path = folder.path();
        // Do not propagate a new access-control list to children of an already private folder.
        if create && verify_private_dir(path).is_err() {
            create_private_dir(path)?;
        }
        verify_private_dir(path)?;
        Ok(folder)
    }

    pub fn verify_child_owner(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        verify_current_owner(&self.path.join(name), true)
    }

    pub fn exclusive(&self, name: &str) -> io::Result<Self> {
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        Self::open(&path)
            .and_then(|folder| {
                verify_private_dir(&path)?;
                Ok(folder)
            })
            .inspect_err(|_| {
                let _ = fs::remove_dir(&path);
            })
    }

    pub fn new_file(&self, name: &str) -> io::Result<File> {
        create_private_file(&self.path.join(name))
    }

    pub fn open_lock(&self, name: &str) -> io::Result<File> {
        let path = self.path.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(SHARE_READ_WRITE)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(&path)?;
        regular_handle(&file)?;
        verify_current_owner(&path, false)?;
        Ok(file)
    }

    pub fn check_auth(&self) -> io::Result<()> {
        let path = self.path.join(AUTH);
        let Some(file) = absent(open_auth(&path, 0))? else {
            return Ok(());
        };
        regular_handle(&file)?;
        verify_current_owner(&path, false)?;
        verify_owner_only(&file).map_err(|error| {
            refused(&format!(
                "{error}; run ck doctor --agent --forget to remove the credential"
            ))
        })
    }

    pub fn forget(&self) -> io::Result<bool> {
        self.forget_after_inspection(|| Ok(()))
    }

    fn forget_after_inspection(
        &self,
        before_delete: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<bool> {
        let path = self.path.join(AUTH);
        let Some(file) = absent(open_auth(&path, 0))? else {
            return Ok(false);
        };
        regular_handle(&file)?;
        verify_current_owner(&path, false)?;
        let owned = retained_file_index(&file)?;
        // Only the owner of the private profile can swap this entry after the
        // first handle closes. The second open never follows a replacement
        // reparse point, and its identity must match the verified object.
        drop(file);
        before_delete()?;
        let deleting = open_auth(&path, DELETE_ON_CLOSE)?;
        regular_handle(&deleting)?;
        if retained_file_index(&deleting)? != owned {
            return Err(refused("credential changed during deletion"));
        }
        drop(deleting);
        Ok(true)
    }
}

fn open_auth(path: &Path, flags: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(OPEN_REPARSE_POINT | flags);
    if flags == DELETE_ON_CLOSE {
        const GENERIC_READ_AND_DELETE: u32 = 0x8001_0000;
        options.access_mode(GENERIC_READ_AND_DELETE);
    }
    options.open(path)
}

fn regular_handle(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file() || is_link(&meta) {
        return Err(refused("not a regular file (reparse points are refused)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_does_not_follow_a_reparse_point_swapped_in_at_the_leaf() {
        use cortexkit_test_support::ScratchDir;

        let scratch = ScratchDir::new("rescue-forget-junction");
        let root = Directory::root(scratch.path(), "cortexkit", true)
            .unwrap()
            .unwrap();
        let profile = root.managed("profile", true).unwrap();
        profile.new_file(AUTH).unwrap();
        let target = scratch.join("outside");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"unchanged").unwrap();
        let leaf = profile.path().join(AUTH);
        let result = profile.forget_after_inspection(|| {
            fs::remove_file(&leaf)?;
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&leaf)
                .arg(&target)
                .status()?;
            assert!(
                status.success(),
                "could not create the replacement junction"
            );
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"unchanged");
    }
}
