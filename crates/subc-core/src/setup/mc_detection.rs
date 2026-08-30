use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, Error as SqliteError, ErrorCode, OpenFlags};

/// The target platform determines only the owner-pinned fallback directory.
/// Test and storage overrides take precedence on every platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McPlatform {
    Unix,
    Windows,
}

impl McPlatform {
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// The source selected for the one directory that the detector probes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McDataDirectorySource {
    TestOverride,
    StorageOverride,
    XdgDataHome,
    UserProfileFallback,
}

/// The durable state classification returned by the read-only MC probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McClassification {
    Absent,
    TornState,
    ForeignSqlite,
    Malformed,
    Tier1Empty,
    Tier2,
    InstalledAndLive,
    Unknown,
}

/// Evidence collected without treating a directory or configuration file as an
/// installation marker. A populated `read_only_uri` records the exact URI used
/// to inspect an existing database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McDetectionEvidence {
    pub platform: McPlatform,
    pub data_directory_source: Option<McDataDirectorySource>,
    pub data_directory: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub database_present: bool,
    pub wal_present: Option<bool>,
    pub shm_present: Option<bool>,
    pub read_only_uri: Option<String>,
    pub has_pre_fork_migration: Option<bool>,
    pub durable_row_counts: BTreeMap<String, u64>,
    pub sqlite_error: Option<String>,
}

impl McDetectionEvidence {
    fn unresolved(platform: McPlatform) -> Self {
        Self {
            platform,
            data_directory_source: None,
            data_directory: None,
            database_path: None,
            database_present: false,
            wal_present: None,
            shm_present: None,
            read_only_uri: None,
            has_pre_fork_migration: None,
            durable_row_counts: BTreeMap::new(),
            sqlite_error: None,
        }
    }
}

/// A structured MC standalone-detection result for the setup planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McDetection {
    pub classification: McClassification,
    pub evidence: McDetectionEvidence,
}

/// Environment inputs are captured before probing so tests and callers can
/// resolve the selected location without mutating process-wide environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McEnvironment {
    pub test_data_dir: Option<PathBuf>,
    pub storage_dir: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub user_profile: Option<PathBuf>,
}

impl McEnvironment {
    pub fn from_process() -> Self {
        Self {
            test_data_dir: non_empty_environment_path("MAGIC_CONTEXT_TEST_DATA_DIR"),
            storage_dir: non_empty_environment_path("MAGIC_CONTEXT_STORAGE_DIR"),
            xdg_data_home: non_empty_environment_path("XDG_DATA_HOME"),
            home: non_empty_environment_path("HOME"),
            user_profile: non_empty_environment_path("USERPROFILE"),
        }
    }
}

/// Resolves the MC data directory in the owner-pinned order. Override values
/// are already the directory containing `context.db`; only platform defaults
/// add CortexKit path components.
pub fn resolve_data_directory(
    environment: &McEnvironment,
    platform: McPlatform,
) -> Option<(McDataDirectorySource, PathBuf)> {
    if let Some(directory) = &environment.test_data_dir {
        return Some((McDataDirectorySource::TestOverride, directory.clone()));
    }
    if let Some(directory) = &environment.storage_dir {
        return Some((McDataDirectorySource::StorageOverride, directory.clone()));
    }

    match platform {
        McPlatform::Unix => {
            if let Some(data_home) = &environment.xdg_data_home {
                return Some((
                    McDataDirectorySource::XdgDataHome,
                    data_home.join("cortexkit").join("magic-context"),
                ));
            }
            environment.home.as_ref().map(|home| {
                (
                    McDataDirectorySource::UserProfileFallback,
                    home.join(".local")
                        .join("share")
                        .join("cortexkit")
                        .join("magic-context"),
                )
            })
        }
        McPlatform::Windows => environment.user_profile.as_ref().map(|profile| {
            (
                McDataDirectorySource::UserProfileFallback,
                profile
                    .join(".local")
                    .join("share")
                    .join("cortexkit")
                    .join("magic-context"),
            )
        }),
    }
}

/// Probes the process-selected MC directory with a read-only SQLite URI.
pub fn detect_current() -> McDetection {
    detect(&McEnvironment::from_process(), McPlatform::current())
}

/// Probes one resolved MC directory. This function never creates the directory,
/// writes the database, or opens a writable SQLite connection.
pub fn detect(environment: &McEnvironment, platform: McPlatform) -> McDetection {
    let Some((source, data_directory)) = resolve_data_directory(environment, platform) else {
        return McDetection {
            classification: McClassification::Unknown,
            evidence: McDetectionEvidence::unresolved(platform),
        };
    };

    let database_path = data_directory.join("context.db");
    let wal_path = sidecar_path(&database_path, "-wal");
    let shm_path = sidecar_path(&database_path, "-shm");
    let mut evidence = McDetectionEvidence {
        platform,
        data_directory_source: Some(source),
        data_directory: Some(data_directory),
        database_path: Some(database_path.clone()),
        database_present: false,
        wal_present: path_presence(&wal_path),
        shm_present: path_presence(&shm_path),
        read_only_uri: None,
        has_pre_fork_migration: None,
        durable_row_counts: BTreeMap::new(),
        sqlite_error: None,
    };

    let database_metadata = match fs::metadata(&database_path) {
        Ok(metadata) => {
            evidence.database_present = true;
            metadata
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return match (evidence.wal_present, evidence.shm_present) {
                (Some(true), _) | (_, Some(true)) => McDetection {
                    classification: McClassification::TornState,
                    evidence,
                },
                (Some(false), Some(false)) => McDetection {
                    classification: McClassification::Absent,
                    evidence,
                },
                _ => McDetection {
                    classification: McClassification::Unknown,
                    evidence,
                },
            };
        }
        Err(error) => {
            evidence.sqlite_error = Some(error.to_string());
            return McDetection {
                classification: McClassification::Unknown,
                evidence,
            };
        }
    };

    if !database_metadata.is_file() {
        return McDetection {
            classification: McClassification::Malformed,
            evidence,
        };
    }

    let uri = sqlite_read_only_uri(&database_path);
    evidence.read_only_uri = Some(uri.clone());
    let connection = match Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(connection) => connection,
        Err(error) => return classify_sqlite_error(error, evidence),
    };
    if let Err(error) = connection.busy_timeout(Duration::ZERO) {
        return classify_sqlite_error(error, evidence);
    }

    let has_pre_fork_migration = match connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version < 10000)",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(value) => value != 0,
        Err(error) if is_sqlite_busy(&error) => return classify_sqlite_error(error, evidence),
        Err(error) if lacks_schema_migration_evidence(&error) => {
            return McDetection {
                classification: McClassification::ForeignSqlite,
                evidence,
            };
        }
        Err(error) => return classify_sqlite_error(error, evidence),
    };
    evidence.has_pre_fork_migration = Some(has_pre_fork_migration);
    if !has_pre_fork_migration {
        return McDetection {
            classification: McClassification::ForeignSqlite,
            evidence,
        };
    }

    let mut durable_state_total = 0_u64;
    for table in ["compartments", "memories", "tags"] {
        let count =
            match connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            }) {
                Ok(count) if count >= 0 => count as u64,
                Ok(_) => {
                    return McDetection {
                        classification: McClassification::Malformed,
                        evidence,
                    };
                }
                Err(error) => return classify_sqlite_error(error, evidence),
            };
        durable_state_total = durable_state_total.saturating_add(count);
        evidence.durable_row_counts.insert(table.to_string(), count);
    }

    McDetection {
        classification: if durable_state_total > 0 {
            McClassification::Tier2
        } else {
            McClassification::Tier1Empty
        },
        evidence,
    }
}

fn classify_sqlite_error(error: SqliteError, mut evidence: McDetectionEvidence) -> McDetection {
    evidence.sqlite_error = Some(error.to_string());
    McDetection {
        classification: if is_sqlite_busy(&error) {
            McClassification::InstalledAndLive
        } else {
            McClassification::Malformed
        },
        evidence,
    }
}

fn is_sqlite_busy(error: &SqliteError) -> bool {
    matches!(
        error,
        SqliteError::SqliteFailure(sqlite_error, _)
            if sqlite_error.code == ErrorCode::DatabaseBusy
    )
}

fn lacks_schema_migration_evidence(error: &SqliteError) -> bool {
    let text = error.to_string();
    text.contains("no such table: schema_migrations")
        || text.contains("no such column: version")
        || text.contains("has no column named version")
}

fn path_presence(path: &Path) -> Option<bool> {
    match fs::metadata(path) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = database_path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn non_empty_environment_path(key: &str) -> Option<PathBuf> {
    let value: OsString = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn sqlite_read_only_uri(path: &Path) -> String {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|current_directory| current_directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let normalized = absolute_path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_uri_path(&normalized);
    let prefix = if encoded.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    format!("{prefix}{encoded}?mode=ro")
}

fn percent_encode_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use subc_core::test_support::TestTempDir;

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    fn fixture_environment(directory: &Path) -> McEnvironment {
        McEnvironment {
            test_data_dir: Some(directory.to_path_buf()),
            ..McEnvironment::default()
        }
    }

    fn create_database(directory: &Path, durable_rows: bool) -> (PathBuf, Connection) {
        let database = directory.join("context.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER NOT NULL);
                 INSERT INTO schema_migrations (version) VALUES (1);
                 CREATE TABLE compartments (id INTEGER PRIMARY KEY);
                 CREATE TABLE memories (id INTEGER PRIMARY KEY);
                 CREATE TABLE tags (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        if durable_rows {
            connection
                .execute("INSERT INTO memories (id) VALUES (1)", [])
                .unwrap();
        }
        (database, connection)
    }

    #[test]
    fn data_directory_precedence_treats_overrides_as_context_db_directories() {
        let environment = McEnvironment {
            test_data_dir: Some(PathBuf::from("/fixture/test-context")),
            storage_dir: Some(PathBuf::from("/fixture/storage-context")),
            xdg_data_home: Some(PathBuf::from("/fixture/xdg")),
            home: Some(PathBuf::from("/fixture/home")),
            user_profile: Some(PathBuf::from("/fixture/profile")),
        };
        assert_eq!(
            resolve_data_directory(&environment, McPlatform::Unix),
            Some((
                McDataDirectorySource::TestOverride,
                PathBuf::from("/fixture/test-context")
            ))
        );

        let storage_only = McEnvironment {
            test_data_dir: None,
            ..environment.clone()
        };
        assert_eq!(
            resolve_data_directory(&storage_only, McPlatform::Unix),
            Some((
                McDataDirectorySource::StorageOverride,
                PathBuf::from("/fixture/storage-context")
            ))
        );

        let defaults_only = McEnvironment {
            test_data_dir: None,
            storage_dir: None,
            ..environment
        };
        assert_eq!(
            resolve_data_directory(&defaults_only, McPlatform::Unix),
            Some((
                McDataDirectorySource::XdgDataHome,
                PathBuf::from("/fixture/xdg")
                    .join("cortexkit")
                    .join("magic-context")
            ))
        );
    }

    #[test]
    fn unix_fallback_uses_local_share_when_xdg_is_unset() {
        let environment = McEnvironment {
            home: Some(PathBuf::from("/fixture/home")),
            ..McEnvironment::default()
        };
        assert_eq!(
            resolve_data_directory(&environment, McPlatform::Unix),
            Some((
                McDataDirectorySource::UserProfileFallback,
                PathBuf::from("/fixture/home")
                    .join(".local")
                    .join("share")
                    .join("cortexkit")
                    .join("magic-context")
            ))
        );
    }

    #[test]
    fn windows_no_override_uses_the_owner_pinned_local_share_default() {
        let environment = McEnvironment {
            xdg_data_home: Some(PathBuf::from("C:\\ignored-xdg")),
            user_profile: Some(PathBuf::from("C:\\Users\\operator")),
            ..McEnvironment::default()
        };
        assert_eq!(
            resolve_data_directory(&environment, McPlatform::Windows),
            Some((
                McDataDirectorySource::UserProfileFallback,
                PathBuf::from("C:\\Users\\operator")
                    .join(".local")
                    .join("share")
                    .join("cortexkit")
                    .join("magic-context")
            ))
        );
    }

    #[test]
    fn tier_two_uses_a_read_only_uri_and_preserves_database_directory_contents() {
        let directory = fixture_dir("tier-two");
        let (_database, connection) = create_database(&directory, true);
        drop(connection);
        let before = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();

        let detection = detect(&fixture_environment(directory.path()), McPlatform::Unix);

        let after = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(detection.classification, McClassification::Tier2);
        assert_eq!(
            detection.evidence.database_path,
            Some(directory.join("context.db"))
        );
        assert!(detection
            .evidence
            .read_only_uri
            .as_deref()
            .is_some_and(|uri| uri.ends_with("?mode=ro")));
        assert_eq!(detection.evidence.durable_row_counts["memories"], 1);
        assert_eq!(before, after);
    }

    #[test]
    fn tier_one_empty_is_not_tier_two() {
        let directory = fixture_dir("tier-one-empty");
        let (_database, connection) = create_database(directory.path(), false);
        drop(connection);

        let detection = detect(&fixture_environment(directory.path()), McPlatform::Unix);

        assert_eq!(detection.classification, McClassification::Tier1Empty);
        assert_eq!(detection.evidence.durable_row_counts["compartments"], 0);
        assert_eq!(detection.evidence.durable_row_counts["memories"], 0);
        assert_eq!(detection.evidence.durable_row_counts["tags"], 0);
    }

    #[test]
    fn foreign_and_malformed_sqlite_files_never_qualify() {
        let foreign_directory = fixture_dir("foreign");
        let foreign_database = foreign_directory.join("context.db");
        Connection::open(&foreign_database)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
            .unwrap();
        let foreign = detect(
            &fixture_environment(foreign_directory.path()),
            McPlatform::Unix,
        );
        assert_eq!(foreign.classification, McClassification::ForeignSqlite);

        let malformed_directory = fixture_dir("malformed");
        fs::write(
            malformed_directory.join("context.db"),
            "not a sqlite database",
        )
        .unwrap();
        let malformed = detect(
            &fixture_environment(malformed_directory.path()),
            McPlatform::Unix,
        );
        assert_eq!(malformed.classification, McClassification::Malformed);
    }

    #[test]
    fn directory_and_wal_or_shm_sidecars_without_a_database_are_not_installation_evidence() {
        let empty_directory = fixture_dir("directory-only");
        let empty = detect(
            &fixture_environment(empty_directory.path()),
            McPlatform::Unix,
        );
        assert_eq!(empty.classification, McClassification::Absent);

        let config_only_root = fixture_dir("config-only");
        let config_only_directory = config_only_root.join("mc-data");
        fs::create_dir_all(&config_only_directory).unwrap();
        let config_directory = config_only_root.join(".config").join("cortexkit");
        fs::create_dir_all(&config_directory).unwrap();
        fs::write(
            config_directory.join("magic-context.jsonc"),
            "{\"transport\": \"standalone\"}",
        )
        .unwrap();
        let config_only = detect(
            &fixture_environment(&config_only_directory),
            McPlatform::Unix,
        );
        assert_eq!(config_only.classification, McClassification::Absent);

        let torn_directory = fixture_dir("torn-wal");
        fs::write(torn_directory.join("context.db-wal"), []).unwrap();
        let torn = detect(
            &fixture_environment(torn_directory.path()),
            McPlatform::Unix,
        );
        assert_eq!(torn.classification, McClassification::TornState);
        assert!(torn.evidence.wal_present == Some(true));
    }

    #[test]
    fn sqlite_busy_is_installed_and_live_without_an_automatic_offer() {
        let directory = fixture_dir("busy");
        let (_database, connection) = create_database(directory.path(), true);
        connection.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let detection = detect(&fixture_environment(directory.path()), McPlatform::Unix);

        assert_eq!(detection.classification, McClassification::InstalledAndLive);
        connection.execute_batch("ROLLBACK").unwrap();
        drop(connection);
    }

    #[test]
    fn sqlite_uri_escapes_path_delimiters_before_appending_mode_ro() {
        let uri = sqlite_read_only_uri(Path::new("fixture #?%.db"));
        assert!(uri.contains("%20%23%3F%25.db?mode=ro"));
    }
}
