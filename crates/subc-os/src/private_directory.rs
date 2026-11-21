use std::{io, path::Path};

/// Create one directory with access restricted to its owner. The parent must
/// already exist. An existing leaf, including a directory, returns
/// [`io::ErrorKind::AlreadyExists`] and is left untouched.
pub fn create_private_directory_exclusive(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        crate::windows_acl::create_private_directory_exclusive(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn private_directory_is_exclusive_and_leaves_a_collision_untouched() {
        let scratch = cortexkit_test_support::ScratchDir::new("private-directory");
        let target = scratch.path().join("stage");
        create_private_directory_exclusive(&target).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        #[cfg(windows)]
        crate::windows_acl::verify_private_dir(&target).unwrap();

        let sentinel = target.join("already-present");
        std::fs::write(&sentinel, "keep").unwrap();
        assert_eq!(
            create_private_directory_exclusive(&target)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
    }
}
