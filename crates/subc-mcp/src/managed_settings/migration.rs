use super::*;
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{self, Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};
use subc_client_rs::config::{LayerValue, ValidationContext};
use subc_jsonc::{apply_edits, remove_member, Edit};
use subc_os::{
    open_directory_nofollow_all, open_regular_nofollow, OpenRegularError, RetainedTempFile,
};

pub(super) fn convert(config_dir: &Path, data_dir: &Path) -> Result<()> {
    let old = config_dir.join(LEGACY_FILE);
    let Some(source) = read_legacy(&old)? else {
        return Ok(());
    };
    let new = config_dir.join(USER_FILE);
    if exists(&new)? {
        return Err(other_error(format!(
            "both {} and {} exist; reconcile the settings and remove the legacy file",
            old.display(),
            new.display(),
        )));
    }
    let converted = converted_document(&old, &source)?;
    if !data_dir.is_absolute() {
        return Err(other_error(
            "set XDG_DATA_HOME to an absolute path before converting MCP settings",
        ));
    }
    fs::create_dir_all(data_dir)?;
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let backup = data_dir.join(format!("{LEGACY_FILE}.{}.{unique}.bak", process::id()));
    let mut file = private_file(&backup)?;
    file.write_all(source.as_bytes())?;
    file.sync_all()?;
    drop(file);

    // Publish the new user file without overwriting a concurrent edit. On an
    // install whose settings are protected by approval (a stamped config
    // gate), the daemon serves the converted file only after the user
    // approves it, like any other settings edit.
    let parent = open_directory_nofollow_all(&fs::canonicalize(config_dir)?)?;
    let mut staged = RetainedTempFile::create_for_publish(&parent)?;
    staged.file_mut().write_all(converted.as_bytes())?;
    staged.publish(OsStr::new(USER_FILE))?;
    match read_legacy(&old)? {
        Some(current) if current == source => fs::remove_file(&old)?,
        Some(_) => {
            return Err(other_error(format!(
                "{} changed during conversion and was not removed; reconcile it with {}",
                old.display(),
                new.display()
            )))
        }
        None => {}
    }
    tracing::info!(old = %old.display(), new = %new.display(), backup = %backup.display(), "converted MCP gateway settings");
    Ok(())
}

pub(super) fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_legacy(path: &Path) -> Result<Option<String>> {
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

fn converted_document(path: &Path, source: &str) -> Result<String> {
    let document = subc_jsonc::jsonc_to_json(source).map_err(other_error)?;
    let value = serde_json::from_str::<Value>(&document)?;
    let mut converted = source.to_owned();
    let mut legacy = source.to_owned();
    edit_object(&mut converted, &mut legacy, &value, &mut vec![], Node::Root)?;
    let before = settings::parse_gateway_config_doc(&legacy, path)?;
    let after = parse_managed_config(&converted, path)?;
    let normalized = subc_jsonc::jsonc_to_json(&converted).map_err(other_error)?;
    schema::schema()
        .review_tuple(
            &[LayerValue::new(
                Layer::User,
                serde_json::from_str(&normalized)?,
            )],
            &ValidationContext::new("subc-mcp", None),
        )
        .map_err(|error| {
            other_error(format!(
                "cannot convert settings at {}: {:?}",
                error.path, error.code
            ))
        })?;

    // Old nulls could reset a value set outside the app overlay. Removing a
    // reset is safe only if it leaves that app's effective policy unchanged.
    for name in std::iter::once("").chain(before.harness_names()) {
        let old = compose_gateway_config(Some(before.clone()), None, name).0;
        let new = compose_gateway_config(Some(after.clone()), None, name).0;
        if old != new {
            return Err(other_error(format!(
                "cannot remove legacy nulls without changing the effective settings for {}",
                if name.is_empty() {
                    "the default app"
                } else {
                    name
                }
            )));
        }
    }
    keep_removed_comments(source, &mut converted);
    Ok(converted)
}

fn keep_removed_comments(source: &str, converted: &mut String) {
    let mut kept = HashMap::new();
    for comment in comments(converted) {
        *kept.entry(comment).or_insert(0usize) += 1;
    }
    let lost = comments(source)
        .into_iter()
        .filter(|comment| match kept.get_mut(comment) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .collect::<Vec<_>>();
    if !lost.is_empty() {
        // A removed member no longer has a place for comments between its
        // key and value. Keep those as trailing file comments.
        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        for comment in lost {
            if !converted.ends_with('\n') {
                converted.push_str(newline);
            }
            converted.push_str(comment);
            converted.push_str(newline);
        }
    }
}

fn comments(document: &str) -> Vec<&str> {
    let bytes = document.as_bytes();
    let mut comments = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match &bytes[at..] {
            [b'"', ..] => {
                at += 1;
                while at < bytes.len() {
                    match bytes[at] {
                        b'\\' => at += 2,
                        b'"' => {
                            at += 1;
                            break;
                        }
                        _ => at += 1,
                    }
                }
            }
            [b'/', b'/', ..] => {
                let text = &document[at..];
                let end = text.find(['\r', '\n']).unwrap_or(text.len());
                comments.push(&text[..end]);
                at += end;
            }
            [b'/', b'*', ..] => {
                let text = &document[at + 2..];
                let Some(end) = text.find("*/") else { break };
                comments.push(&document[at..at + end + 4]);
                at += end + 4;
            }
            _ => at += 1,
        }
    }
    comments
}

#[derive(Clone, Copy)]
enum Node {
    Root,
    Overlay,
    Harness,
    Providers,
    Provider,
    Prompts,
    Tools,
    Overrides,
    Override,
}

fn edit_object<'a>(
    converted: &mut String,
    legacy: &mut String,
    value: &'a Value,
    path: &mut Vec<&'a str>,
    node: Node,
) -> Result<()> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    for (key, value) in object {
        path.push(key);
        if (matches!(node, Node::Root) && key == "version") || value.is_null() {
            remove_member(converted, path).map_err(other_error)?;
        } else if matches!(node, Node::Root | Node::Overlay)
            && key == "refresh"
            && value.as_str() == Some("on-attach")
        {
            apply_edits(
                converted,
                &[Edit::SetMember {
                    path,
                    value: &json!("on_attach"),
                }],
            )
            .map_err(other_error)?;
        } else if let Some(enabled) = value
            .as_bool()
            .filter(|_| matches!(node, Node::Providers | Node::Overrides))
        {
            apply_edits(
                converted,
                &[Edit::SetMember {
                    path,
                    value: &json!({"enabled": enabled}),
                }],
            )
            .map_err(other_error)?;
            if matches!(node, Node::Providers) {
                apply_edits(
                    legacy,
                    &[Edit::SetMember {
                        path,
                        value: &json!({"enabled": enabled}),
                    }],
                )
                .map_err(other_error)?;
            }
        } else {
            let child = match (node, key.as_str()) {
                (Node::Root, "harness") => Some(Node::Harness),
                (Node::Harness, _) => Some(Node::Overlay),
                (Node::Root | Node::Overlay, "providers") => Some(Node::Providers),
                (Node::Providers, _) => Some(Node::Provider),
                (Node::Root | Node::Overlay, "prompts") => Some(Node::Prompts),
                (Node::Provider, "tools") => Some(Node::Tools),
                (Node::Tools | Node::Prompts, "overrides") => Some(Node::Overrides),
                (Node::Overrides, _) => Some(Node::Override),
                _ => None,
            };
            if let Some(child) = child {
                edit_object(converted, legacy, value, path, child)?;
            }
            let new_key = match (node, key.as_str()) {
                (Node::Root | Node::Overlay, "surfaceMode") => Some("surface_mode"),
                (Node::Prompts | Node::Tools, "defaultEnabled") => Some("default_enabled"),
                _ => None,
            };
            if let Some(key) = new_key {
                apply_edits(converted, &[Edit::RenameMember { path, key }]).map_err(other_error)?;
            }
        }
        path.pop();
    }
    Ok(())
}
