//! Semantic service-definition comparisons and conservative legacy ownership.
use std::collections::BTreeMap;

use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{json, Value};

use super::runtime::RuntimePlatform;

pub(super) fn canonical(platform: RuntimePlatform, text: &str) -> Result<String, String> {
    let value = match platform {
        RuntimePlatform::Linux => unit(text)?,
        RuntimePlatform::Macos | RuntimePlatform::Windows => {
            let document = Document::parse_with_options(
                text,
                ParsingOptions {
                    allow_dtd: true,
                    ..ParsingOptions::default()
                },
            )
            .map_err(|e| format!("invalid service XML: {e}"))?;
            if platform == RuntimePlatform::Macos {
                let root = document.root_element();
                if !root.has_tag_name("plist") {
                    return Err("expected plist".into());
                }
                plist(
                    root.children()
                        .find(Node::is_element)
                        .ok_or("empty plist")?,
                )?
            } else {
                task(document.root_element())?
            }
        }
    };
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

fn plist(node: Node<'_, '_>) -> Result<Value, String> {
    Ok(match node.tag_name().name() {
        "dict" => {
            let mut children = node.children().filter(Node::is_element);
            let mut values = BTreeMap::new();
            while let Some(key) = children.next() {
                let value = children.next().ok_or("odd plist dictionary")?;
                if !key.has_tag_name("key") {
                    return Err("expected plist key".into());
                }
                let key = key.text().unwrap_or_default();
                if values.insert(key, plist(value)?).is_some() {
                    return Err("duplicate plist key".into());
                }
            }
            json!(values)
        }
        "array" => Value::Array(
            node.children()
                .filter(Node::is_element)
                .map(plist)
                .collect::<Result<_, _>>()?,
        ),
        "true" => json!(true),
        "false" => json!(false),
        "integer" => json!(node
            .text()
            .unwrap_or_default()
            .trim()
            .parse::<i64>()
            .map_err(|e| e.to_string())?),
        "string" => json!(node.text().unwrap_or_default()),
        other => return Err(format!("unsupported plist value {other}")),
    })
}

fn task(node: Node<'_, '_>) -> Result<Value, String> {
    let name = node.tag_name().name();
    let mut attributes = BTreeMap::new();
    for attr in node.attributes() {
        attributes.insert(
            format!("{}:{}", attr.namespace().unwrap_or_default(), attr.name()),
            attr.value(),
        );
    }
    let mut children = Vec::new();
    for child in node.children().filter(Node::is_element) {
        // Task Scheduler exports optional schema defaults and registration metadata
        // that were not in the submitted XML. Ignore only these known defaults;
        // non-default values remain visible as user modifications.
        let child_name = child.tag_name().name();
        let value = child.text().unwrap_or_default().trim();
        let default = match (name, child_name) {
            ("RegistrationInfo", "Date" | "Author") => true,
            ("Settings", "AllowHardTerminate" | "AllowStartOnDemand" | "Enabled") => {
                value == "true"
            }
            ("Settings", "Hidden" | "RunOnlyIfIdle") => value == "false",
            (
                "Settings",
                "UseUnifiedSchedulingEngine" | "DisallowStartOnRemoteAppSession" | "Volatile",
            ) => value == "false",
            ("Settings", "DisallowStartIfOnBatteries" | "StopIfGoingOnBatteries") => {
                value == "true"
            }
            ("Settings", "StartWhenAvailable" | "RunOnlyIfNetworkAvailable" | "WakeToRun") => {
                value == "false"
            }
            ("Settings", "ExecutionTimeLimit") => value == "PT72H",
            ("Settings", "MultipleInstancesPolicy") => value == "IgnoreNew",
            ("Settings", "Priority") => value == "7",
            ("IdleSettings", "Duration") => value == "PT10M",
            ("IdleSettings", "WaitTimeout") => value == "PT1H",
            ("IdleSettings", "StopOnIdleEnd") => value == "true",
            ("IdleSettings", "RestartOnIdle") => value == "false",
            ("Settings", "DeleteExpiredTaskAfter") => value.is_empty(),
            ("LogonTrigger", "StartBoundary" | "EndBoundary") => value.is_empty(),
            _ => false,
        };
        if !default {
            let parsed = task(child)?;
            if matches!(child_name, "Settings" | "IdleSettings")
                && parsed["children"].as_array().is_some_and(Vec::is_empty)
            {
                continue;
            }
            children.push(parsed);
        }
    }
    // These containers use xs:all; array/sequence-constrained containers keep
    // their order, so argument/action reordering cannot look like formatting.
    if matches!(
        name,
        "Task" | "Settings" | "RestartOnFailure" | "RegistrationInfo" | "Principal"
    ) {
        children.sort_by_key(Value::to_string);
    }
    let mut text = if children.is_empty() {
        node.text().unwrap_or_default().trim().to_string()
    } else {
        String::new()
    };
    if name == "UserId" {
        text.make_ascii_lowercase();
    }
    Ok(
        json!({"name": name, "namespace": node.tag_name().namespace().unwrap_or_default(),
        "attributes": attributes, "text": text, "children": children}),
    )
}

fn unit(text: &str) -> Result<Value, String> {
    let mut section = String::new();
    let mut values: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
        } else {
            let (key, value) = line.split_once('=').ok_or("invalid systemd directive")?;
            values
                .entry(section.clone())
                .or_default()
                .entry(key.trim().to_string())
                .or_default()
                .push(value.trim().to_string());
        }
    }
    Ok(json!(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_comparison_sorts_keys_but_preserves_types_and_array_order() {
        let a = "<plist><dict><key>a</key><integer>10</integer><key>b</key><array><string>x&amp;y</string><false/></array></dict></plist>";
        let b = "<plist>\n<dict><key>b</key><array><string>x&#38;y</string><false /></array><key>a</key><integer>010</integer></dict></plist>";
        let parse = |text: &str| canonical(RuntimePlatform::Macos, text).unwrap();
        assert_eq!(parse(a), parse(b));
        assert_ne!(
            parse(a),
            parse(&a.replace("<integer>10</integer>", "<string>10</string>"))
        );
        assert_ne!(
            parse(a),
            parse(&a.replace(
                "<string>x&amp;y</string><false/>",
                "<false/><string>x&amp;y</string>"
            ))
        );
    }

    #[test]
    fn task_export_defaults_do_not_hide_nondefault_edits() {
        let submitted = "<Task><RegistrationInfo><URI>x</URI></RegistrationInfo><Settings><RestartOnFailure><Interval>PT1M</Interval><Count>255</Count></RestartOnFailure></Settings></Task>";
        let exported = "<Task><Settings><AllowHardTerminate>true</AllowHardTerminate><Priority>7</Priority><IdleSettings><Duration>PT10M</Duration><WaitTimeout>PT1H</WaitTimeout><StopOnIdleEnd>true</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><RestartOnFailure><Count>255</Count><Interval>PT1M</Interval></RestartOnFailure></Settings><RegistrationInfo><Author>someone</Author><URI>x</URI><Date>2026-01-01</Date></RegistrationInfo></Task>";
        let parse = |text: &str| canonical(RuntimePlatform::Windows, text).unwrap();
        assert_eq!(parse(submitted), parse(exported));
        assert_ne!(
            parse(submitted),
            parse(&exported.replace("<Priority>7</Priority>", "<Priority>8</Priority>"))
        );
        assert_ne!(
            parse(submitted),
            parse(&exported.replace("<Count>255</Count>", "<Count>1</Count>"))
        );
    }

    #[test]
    fn unit_comparison_preserves_repeated_directives_and_real_value_changes() {
        let parse = |text: &str| canonical(RuntimePlatform::Linux, text).unwrap();
        let a = "[Service]\nRestart=on-failure\nEnvironment=a\nEnvironment=b\n";
        let b = "# comment\n[Service]\nEnvironment = a\nEnvironment = b\nRestart = on-failure\n";
        assert_eq!(parse(a), parse(b));
        assert_ne!(
            parse(a),
            parse(&a.replace(
                "Environment=a\nEnvironment=b",
                "Environment=b\nEnvironment=a"
            ))
        );
        assert_ne!(parse(a), parse(&a.replace("on-failure", "always")));
    }
}
