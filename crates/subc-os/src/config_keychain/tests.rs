use super::*;
use testing::FileKeyStore;

fn store(path: std::path::PathBuf) -> FileKeyStore {
    FileKeyStore::open_for_executable(path, std::path::Path::new("ckdev-keychain-test")).unwrap()
}

#[test]
fn key_debug_is_redacted_including_nested_state() {
    let state = KeychainState {
        key: StoreKey::new([0xa5; 32]),
        anchor: AnchorState::default(),
    };
    assert_eq!(format!("{:?}", state.key), "StoreKey([REDACTED])");
    let debug = format!("{state:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("165"));
    assert!(!debug.contains("a5"));
}

#[test]
fn versioned_encoding_rejects_truncation_unknown_version_and_bad_anchor() {
    let state = KeychainState {
        key: StoreKey::new([0x71; 32]),
        anchor: AnchorState {
            current: Some(StoreAnchor {
                revision: 12,
                sha256: [0x93; 32],
            }),
            high_water: 12,
        },
    };
    let bytes = state.encode();
    assert_eq!(bytes.len(), 89);
    assert_eq!(&bytes[..8], b"SUBCKEY1");
    assert_eq!(&bytes[40..48], &12u64.to_be_bytes());
    let decoded = KeychainState::decode(&bytes).unwrap();
    assert_eq!(decoded.key.as_bytes(), &[0x71; 32]);
    assert_eq!(decoded.anchor, state.anchor);
    for length in 0..bytes.len() {
        assert!(matches!(
            KeychainState::decode(&bytes[..length]),
            Err(KeychainError::InvalidData)
        ));
    }
    for (offset, value) in [(7, b'2'), (48, 2), (47, 11), (56, 0)] {
        let mut invalid = bytes.clone();
        invalid[offset] = value;
        assert!(matches!(
            KeychainState::decode(&invalid),
            Err(KeychainError::InvalidData)
        ));
    }
    let mut invalid = bytes.clone();
    invalid.push(0);
    assert!(matches!(
        KeychainState::decode(&invalid),
        Err(KeychainError::InvalidData)
    ));
}

#[test]
fn file_key_store_retains_key_and_high_water_through_unstamp_and_restamp() {
    let root = cortexkit_test_support::ScratchDir::new("config-keychain");
    let path = root.join("key-store");
    let store = store(path.clone());
    assert!(store.load().unwrap().is_none());
    store.create(StoreKey::new([0x39; 32])).unwrap();
    assert!(matches!(
        store.create(StoreKey::new([0x42; 32])),
        Err(KeychainError::AlreadyExists)
    ));
    let anchor = StoreAnchor {
        revision: 7,
        sha256: [0xa1; 32],
    };
    store.update_anchor(anchor).unwrap();
    store.update_anchor(anchor).unwrap();
    assert!(matches!(
        store.update_anchor(StoreAnchor {
            revision: 7,
            sha256: [0xb1; 32]
        }),
        Err(KeychainError::RevisionConflict)
    ));
    assert!(matches!(
        store.update_anchor(StoreAnchor {
            revision: 6,
            ..anchor
        }),
        Err(KeychainError::RevisionConflict)
    ));
    store.clear_anchor(8).unwrap();
    drop(store);
    let store = self::store(path);
    let state = store.load().unwrap().unwrap();
    assert_eq!(state.key.as_bytes(), &[0x39; 32]);
    assert_eq!(
        state.anchor,
        AnchorState {
            current: None,
            high_water: 8
        }
    );
    assert!(matches!(
        store.update_anchor(StoreAnchor {
            revision: 8,
            ..anchor
        }),
        Err(KeychainError::RevisionConflict)
    ));
    assert!(matches!(
        store.clear_anchor(7),
        Err(KeychainError::RevisionConflict)
    ));
    store
        .update_anchor(StoreAnchor {
            revision: 9,
            ..anchor
        })
        .unwrap();
    let state = store.load().unwrap().unwrap();
    assert_eq!(state.anchor.high_water, 9);
    assert_eq!(state.anchor.current.unwrap().revision, 9);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.join("key-store"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn file_key_store_denial_update_failure_corruption_and_deletion_are_distinct() {
    let root = cortexkit_test_support::ScratchDir::new("config-keychain-failure");
    let path = root.join("key-store");
    let mut store = store(path.clone());
    assert!(matches!(
        store.update_anchor(StoreAnchor {
            revision: 1,
            sha256: [1; 32]
        }),
        Err(KeychainError::Missing)
    ));
    store.create(StoreKey::new([0x83; 32])).unwrap();
    let original = std::fs::read(&path).unwrap();
    store.set_access_denied(true);
    assert!(matches!(store.load(), Err(KeychainError::Denied)));
    assert!(matches!(store.clear_anchor(1), Err(KeychainError::Denied)));
    assert!(matches!(
        store.create(StoreKey::new([0x84; 32])),
        Err(KeychainError::Denied)
    ));
    assert!(matches!(store.delete(), Err(KeychainError::Denied)));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    store.set_access_denied(false);
    store.set_update_failure(true);
    let anchor = StoreAnchor {
        revision: 1,
        sha256: [0x11; 32],
    };
    assert!(matches!(
        store.update_anchor(anchor),
        Err(KeychainError::Unavailable)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    store.set_update_failure(false);
    store.update_anchor(anchor).unwrap();
    assert_eq!(store.load().unwrap().unwrap().anchor.current, Some(anchor));
    std::fs::write(&path, b"invalid").unwrap();
    assert!(matches!(store.load(), Err(KeychainError::InvalidData)));
    assert!(matches!(
        store.clear_anchor(2),
        Err(KeychainError::InvalidData)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
    store.delete().unwrap();
    assert!(store.load().unwrap().is_none());
    assert!(matches!(store.delete(), Err(KeychainError::Missing)));
    store.create(StoreKey::new([0x84; 32])).unwrap();
    let recreated = store.load().unwrap().unwrap();
    assert_eq!(recreated.anchor, AnchorState::default());
    assert_eq!(recreated.key.as_bytes(), &[0x84; 32]);
}

#[test]
fn file_key_store_refuses_non_development_executables() {
    let path = std::path::PathBuf::from("must-not-be-created");
    for executable in ["ck", "subc", "prefix-ckdev-test", "ckdev", ""] {
        assert!(matches!(
            FileKeyStore::open_for_executable(path.clone(), std::path::Path::new(executable)),
            Err(KeychainError::Unsupported)
        ));
    }
    assert!(matches!(
        FileKeyStore::open(path),
        Err(KeychainError::Unsupported)
    ));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn native_keychain_is_unsupported_off_macos() {
    assert!(matches!(
        KeychainStore::open("unused-test-service"),
        Err(KeychainError::Unsupported)
    ));
}
