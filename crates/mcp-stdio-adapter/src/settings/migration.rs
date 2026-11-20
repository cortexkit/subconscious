use std::{
    error::Error,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use subc_client_rs::config::{Layer, LayerValue, ValidationContext};
use subc_jsonc::{apply_edits, Edit};
use subc_os::{
    open_directory_nofollow_all, open_regular_nofollow, OpenRegularError, RetainedTempFile,
};

use super::{parse_effective, parse_jsonc, pointer, LEGACY_FILE, MODULE_ID, SCHEMA, USER_FILE};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub(super) fn convert(config_dir: &Path, data_dir: &Path) -> Result<Option<String>> {
    convert_with(config_dir, data_dir, || {})
}

fn convert_with(
    config_dir: &Path,
    data_dir: &Path,
    before_remove: impl FnOnce(),
) -> Result<Option<String>> {
    let legacy_path = config_dir.join(LEGACY_FILE);
    let user_path = config_dir.join(USER_FILE);
    if exists(&user_path)? {
        return Ok(exists(&legacy_path)?.then(|| {
            format!(
                "{} is ignored; edit {} instead",
                legacy_path.display(),
                user_path.display()
            )
        }));
    }
    let Some(source) = read_legacy(&legacy_path)? else {
        return Ok(None);
    };
    let (converted, pointers) = converted_document(&legacy_path, &source)?;

    if !data_dir.is_absolute() {
        return Err(
            "set XDG_DATA_HOME to an absolute path before converting MCP adapter settings".into(),
        );
    }
    fs::create_dir_all(data_dir)?;
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let backup = data_dir.join(format!(
        "{LEGACY_FILE}.{}.{}.bak",
        std::process::id(),
        unique
    ));
    let mut backup_file = private_file(&backup)?;
    backup_file.write_all(source.as_bytes())?;
    backup_file.sync_all()?;
    drop(backup_file);

    // The converted file becomes the user's settings file; on a protected
    // install the daemon applies it only after the person approves it. Publish
    // atomically, but never overwrite a settings file that appeared while the
    // converter was working.
    let parent = open_directory_nofollow_all(&fs::canonicalize(config_dir)?)?;
    let mut staged = RetainedTempFile::create_for_publish(&parent)?;
    staged.file_mut().write_all(converted.as_bytes())?;
    staged.publish(OsStr::new(USER_FILE))?;
    tracing::info!(old = %legacy_path.display(), new = %user_path.display(), backup = %backup.display(), changed_pointers = ?pointers, "converted MCP adapter settings");

    before_remove();
    match read_legacy(&legacy_path) {
        Ok(Some(current)) if current == source => fs::remove_file(&legacy_path)?,
        Ok(None) => {}
        _ => {
            return Ok(Some(format!(
                "{} changed during conversion and was left in place; edit {} instead",
                legacy_path.display(),
                user_path.display()
            )))
        }
    }
    Ok(None)
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn read_legacy(path: &Path) -> Result<Option<String>> {
    let mut file = match open_regular_nofollow(path) {
        Ok(file) => file,
        Err(OpenRegularError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    let mut source = String::new();
    file.read_to_string(&mut source)?;
    Ok(Some(source))
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
fn private_file(path: &Path) -> io::Result<File> {
    subc_os::windows_acl::create_private_file(path)
}

type CredentialRename = ([String; 5], Value);

pub(super) fn legacy_document(path: &Path, source: &str) -> Result<(Value, Vec<CredentialRename>)> {
    let legacy = parse_jsonc(path, source)?;
    let mut candidate = json!({"servers": legacy});
    let mut renames = Vec::new();
    if let Some(servers) = candidate["servers"].as_object_mut() {
        for (server, config) in servers {
            let Some(env) = config.get_mut("env").and_then(Value::as_object_mut) else {
                continue;
            };
            for (name, binding) in env {
                let Some(binding) = binding.as_object_mut() else {
                    continue;
                };
                if !binding.contains_key("handle") {
                    continue;
                }
                if binding.len() != 1 {
                    return Err(format!(
                        "{}: legacy environment binding must contain just one handle or literal",
                        path.display()
                    )
                    .into());
                }
                let handle = binding.remove("handle").expect("checked above");
                let reference = json!({"vault_ref":handle});
                binding.insert("credential_ref".to_string(), reference.clone());
                renames.push((
                    [
                        "servers".to_string(),
                        server.clone(),
                        "env".to_string(),
                        name.clone(),
                        "handle".to_string(),
                    ],
                    reference,
                ));
            }
        }
    }
    Ok((candidate, renames))
}

fn converted_document(path: &Path, source: &str) -> Result<(String, Vec<String>)> {
    let (candidate, renames) = legacy_document(path, source)?;
    let tuple = SCHEMA
        .review_tuple(
            &[LayerValue::new(Layer::User, candidate.clone())],
            &ValidationContext::new(MODULE_ID, None),
        )
        .map_err(|error| {
            format!(
                "invalid legacy settings in {} at {}",
                path.display(),
                error.path
            )
        })?;
    parse_effective(&tuple.effective)?;

    let mut edits = vec![Edit::WrapRoot { key: "servers" }];
    let mut pointers = vec!["/servers".into()];
    let paths: Vec<_> = renames
        .iter()
        .map(|(path, _)| path.each_ref().map(String::as_str))
        .collect();
    let mut renamed_paths = paths.clone();
    for path in &mut renamed_paths {
        path[4] = "credential_ref";
    }
    for ((old, new), (_, reference)) in paths.iter().zip(&renamed_paths).zip(&renames) {
        edits.push(Edit::RenameMember {
            path: old,
            key: "credential_ref",
        });
        edits.push(Edit::SetMember {
            path: new,
            value: reference,
        });
        pointers.push(old.iter().fold(String::new(), |p, key| pointer(&p, key)));
        pointers.push(new.iter().fold(String::new(), |p, key| pointer(&p, key)));
    }
    let mut text = source.to_owned();
    apply_edits(&mut text, &edits)?;
    if parse_jsonc(path, &text)? != candidate {
        return Err(format!("could not safely convert {}", path.display()).into());
    }
    Ok((text, pointers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    const OLD: &str = concat!(
        "/* user file header */\r\n{\r\n",
        "  // keep the server entry\r\n",
        "  \"demo\": { \"command\": \"demo\", \"env\": {\r\n",
        "    \"TOKEN\": { /* inside binding */ \"h\\u0061ndle\" /* beside key */: \"vault:demo\" }, // after binding\r\n",
        "    \"LEVEL\": { \"value\": \"debug\" },\r\n",
        "  } },\r\n",
        "}\r\n// user file footer\r\n",
    );

    #[test]
    fn migration_keeps_comments_backs_up_private_bytes_removes_source_and_runs_once() {
        let root = ScratchDir::new("mcp-migrate");
        let data = root.join("module-data");
        let config = root.join("cortexkit");
        fs::create_dir(&config).unwrap();
        let legacy = config.join(LEGACY_FILE);
        fs::write(&legacy, OLD).unwrap();
        assert_eq!(convert(&config, &data).unwrap(), None);
        let user = config.join(USER_FILE);
        let converted = fs::read_to_string(&user).unwrap();
        let actual = parse_jsonc(&user, &converted).unwrap();
        assert_eq!(
            actual,
            json!({"servers":{"demo":{"command":"demo","env":{
                "TOKEN":{"credential_ref":{"vault_ref":"vault:demo"}},"LEVEL":{"value":"debug"}
            }}}})
        );
        let comments = [
            "/* user file header */",
            "// keep the server entry",
            "/* inside binding */",
            "/* beside key */",
            "// after binding",
            "// user file footer",
        ];
        let mut text = converted.as_str();
        for comment in comments {
            text = text
                .split_once(comment)
                .expect("original comment in order")
                .1;
        }
        assert!(converted.contains("\"LEVEL\": { \"value\": \"debug\" },\r\n"));
        assert!(!legacy.exists());
        let backups: Vec<_> = fs::read_dir(&data)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), OLD.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(convert(&config, &data).unwrap(), None);
        assert_eq!(fs::read(&user).unwrap(), converted.as_bytes());
        assert_eq!(fs::read_dir(&data).unwrap().count(), 1);
    }

    #[test]
    fn migration_preserves_an_intervening_legacy_edit_and_reports_the_canonical_path() {
        let root = ScratchDir::new("mcp-migrate-race");
        let data = root.join("module-data");
        let config = root.join("cortexkit");
        fs::create_dir(&config).unwrap();
        let legacy = config.join(LEGACY_FILE);
        let user = config.join(USER_FILE);
        fs::write(&legacy, OLD).unwrap();
        let edited = r#"{"edited": {"command":"do-not-overwrite"}}"#;
        let notice = convert_with(&config, &data, || {
            assert!(user.is_file());
            fs::write(&legacy, edited).unwrap();
        })
        .unwrap()
        .unwrap();
        assert!(notice.contains("changed during conversion"));
        assert!(notice.contains(&user.display().to_string()));
        assert_eq!(fs::read_to_string(&legacy).unwrap(), edited);
        let canonical = fs::read(&user).unwrap();
        let notice = convert(&config, &data).unwrap().unwrap();
        assert!(notice.contains("is ignored"));
        assert!(notice.contains(&user.display().to_string()));
        assert_eq!(fs::read(&user).unwrap(), canonical);
        assert_eq!(fs::read_to_string(&legacy).unwrap(), edited);
        assert_eq!(fs::read_dir(&data).unwrap().count(), 1);
    }

    #[test]
    fn migration_does_not_write_files_for_an_unrecognized_legacy_shape() {
        let root = ScratchDir::new("mcp-invalid-old");
        let data = root.join("module-data");
        let config = root.join("cortexkit");
        fs::create_dir(&config).unwrap();
        let legacy = config.join(LEGACY_FILE);
        let input = r#"{"demo":{"command":"demo","env":{"TOKEN":{"handle":42}}}}"#;
        fs::write(&legacy, input).unwrap();
        assert!(convert(&config, &data).is_err());
        assert_eq!(fs::read_to_string(legacy).unwrap(), input);
        assert!(!config.join(USER_FILE).exists());
        assert!(!data.exists());
    }
}
