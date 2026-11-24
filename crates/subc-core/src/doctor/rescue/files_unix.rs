use std::{
    fs::File,
    io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
};

use rustix::{
    fs::{fstat, mkdirat, open, openat, statat, unlinkat, AtFlags, FileType, Mode, OFlags, Stat},
    process::geteuid,
};

use super::{absent, refused, AUTH};
use crate::setup::file_modes::set_descriptor_mode;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const LEAF_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
const DIR_MODE: Mode = Mode::from_bits_truncate(0o700);
const FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

pub(in super::super) struct Directory {
    path: PathBuf,
    fd: OwnedFd,
}

impl Directory {
    pub(super) fn open_data_home(path: &Path) -> io::Result<Self> {
        let fd = open(path, OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Self {
            path: path.into(),
            fd,
        })
    }

    pub(super) fn open_install_root(&self, name: &str, create: bool) -> io::Result<Self> {
        if create {
            self.create_child(name)?;
        }
        self.open_owned_child(name).map(|(directory, _)| directory)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_owned_child(&self, name: &str) -> io::Result<(Self, Stat)> {
        let path = self.path.join(name);
        let fd = openat(&self.fd, name, DIRECTORY_FLAGS, Mode::empty())?;
        let stat = fstat(&fd)?;
        check_owner(&stat, geteuid().as_raw())?;
        Ok((Self { path, fd }, stat))
    }

    fn open_child(&self, name: &str, tighten: bool) -> io::Result<Self> {
        let (directory, stat) = self.open_owned_child(name)?;
        if stat.st_mode & 0o7777 != 0o700 {
            if !tighten {
                return Err(refused("managed folder is not owner-only (0700)"));
            }
            set_descriptor_mode(&directory.fd, &directory.path, 0o700).map_err(io::Error::other)?;
        }
        Ok(directory)
    }

    fn create_child(&self, name: &str) -> io::Result<()> {
        match mkdirat(&self.fd, name, DIR_MODE) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn managed(&self, name: &str, create: bool) -> io::Result<Self> {
        if create {
            self.create_child(name)?;
        }
        self.open_child(name, create)
    }

    pub fn verify_child_owner(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        self.verify_child_owner_as(name, geteuid().as_raw())
    }

    pub fn verify_child_owner_as(&self, name: &std::ffi::OsStr, owner: u32) -> io::Result<()> {
        let fd = openat(&self.fd, name, DIRECTORY_FLAGS, Mode::empty())?;
        check_owner(&fstat(&fd)?, owner)
    }

    pub fn exclusive(&self, name: &str) -> io::Result<Self> {
        mkdirat(&self.fd, name, DIR_MODE)?;
        self.open_child(name, true).inspect_err(|_| {
            let _ = unlinkat(&self.fd, name, AtFlags::REMOVEDIR);
        })
    }

    pub fn new_file(&self, name: &str) -> io::Result<File> {
        let fd = openat(
            &self.fd,
            name,
            LEAF_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            FILE_MODE,
        )?;
        set_descriptor_mode(&fd, &self.path.join(name), 0o600).map_err(io::Error::other)?;
        Ok(File::from(fd))
    }

    pub fn open_lock(&self, name: &str) -> io::Result<File> {
        let fd = openat(
            &self.fd,
            name,
            LEAF_FLAGS | OFlags::RDWR | OFlags::CREATE,
            FILE_MODE,
        )?;
        check_regular(&fstat(&fd)?, geteuid().as_raw())?;
        Ok(File::from(fd))
    }

    pub fn check_auth(&self) -> io::Result<()> {
        self.check_auth_as(geteuid().as_raw())
    }

    pub fn check_auth_as(&self, owner: u32) -> io::Result<()> {
        let Some(stat) =
            absent(statat(&self.fd, AUTH, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from))?
        else {
            return Ok(());
        };
        check_credential(&stat, owner)?;
        let fd = openat(&self.fd, AUTH, LEAF_FLAGS | OFlags::RDONLY, Mode::empty())?;
        let stat = fstat(&fd)?;
        check_credential(&stat, owner)?;
        if stat.st_mode & 0o7777 != 0o600 {
            set_descriptor_mode(&fd, &self.path.join(AUTH), 0o600).map_err(io::Error::other)?;
        }
        Ok(())
    }

    pub fn forget(&self) -> io::Result<bool> {
        self.forget_as(geteuid().as_raw())
    }

    pub fn forget_as(&self, owner: u32) -> io::Result<bool> {
        let Some(stat) =
            absent(statat(&self.fd, AUTH, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from))?
        else {
            return Ok(false);
        };
        check_regular(&stat, owner)?;
        // Only the owner of the profile can replace a verified leaf; unlinkat
        // acts on the directory entry and never follows a replacement link.
        unlinkat(&self.fd, AUTH, AtFlags::empty())?;
        Ok(true)
    }
}

fn check_owner(stat: &Stat, owner: u32) -> io::Result<()> {
    if stat.st_uid != owner {
        return Err(refused("not owned by the effective user"));
    }
    Ok(())
}

fn check_regular(stat: &Stat, owner: u32) -> io::Result<()> {
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(refused("not a regular file (links are refused)"));
    }
    check_owner(stat, owner)
}

fn check_credential(stat: &Stat, owner: u32) -> io::Result<()> {
    check_regular(stat, owner)?;
    if stat.st_nlink != 1 {
        return Err(refused("credential must not have another hard link"));
    }
    Ok(())
}
