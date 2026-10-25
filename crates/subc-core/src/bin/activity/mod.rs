use std::{ffi::OsString, io::Write, path::Path};

use subc_client_rs::{ConsumerOptions, SubcConsumer};
use subc_control::{ActivityEntry, ActivityQueryRequest};

use super::{discover_connection_file, print_table, CkError, Command, JsonOutput};

pub(super) const HELP: &str = "ck activity — consequential agent and module events

usage: ck [--json] activity [<filters>] [--follow]

filters (exact matches, combined):
  --agent <id>          daemon-stamped agent id
  --project <path>      daemon-stamped project root
  --module <id>         recording module
  --kind <kind>         event kind, such as subc.module_restarted
  --since-ms <ms>       inclusive Unix epoch milliseconds
  --until-ms <ms>       inclusive Unix epoch milliseconds
  --limit <n>           newest entries to show initially (default 100, minimum 1)

  --json               entries as a JSON array; with --follow, one entry per line
  --follow, -f         query, then follow from its head, applying the same filters
                       (limit applies only to the initial query)

Times in the table are UTC. The initial query is newest first by sequence;
followed entries arrive in append order. A disconnected or refused feed exits
with an error; rerun the command to query retained history.";

pub(super) struct Options {
    query: ActivityQueryRequest,
    follow: bool,
}

pub(super) fn parse(tail: &[OsString]) -> Result<Command, CkError> {
    if tail
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        return Ok(Command::Help(HELP.into()));
    }
    let mut query = ActivityQueryRequest {
        agent: None,
        project: None,
        module: None,
        kind: None,
        since_ms: None,
        until_ms: None,
        limit: 100,
        cursor: None,
    };
    let mut follow = false;
    let mut args = tail.iter();
    while let Some(arg) = args.next() {
        let flag = arg
            .to_str()
            .ok_or_else(|| CkError::Usage("activity flags must be UTF-8".into()))?;
        if flag == "--follow" || flag == "-f" {
            follow = true;
            continue;
        }
        if !matches!(
            flag,
            "--agent"
                | "--project"
                | "--module"
                | "--kind"
                | "--since-ms"
                | "--until-ms"
                | "--limit"
        ) {
            return Err(CkError::Usage(format!(
                "unknown activity argument '{flag}'\n\n{HELP}"
            )));
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CkError::Usage(format!("{flag} needs a value")))?;
        if flag == "--project" {
            query.project = Some(value.into());
            continue;
        }
        let value = value
            .to_str()
            .ok_or_else(|| CkError::Usage(format!("{flag} needs a UTF-8 value")))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| CkError::Usage(format!("{flag} needs an unsigned integer")))
        };
        match flag {
            "--agent" => query.agent = Some(value.into()),
            "--module" => query.module = Some(value.into()),
            "--kind" => query.kind = Some(value.into()),
            "--since-ms" => query.since_ms = Some(number()?),
            "--until-ms" => query.until_ms = Some(number()?),
            "--limit" => {
                query.limit = u32::try_from(number()?)
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        CkError::Usage("--limit needs an integer from 1 to 4294967295".into())
                    })?;
            }
            _ => unreachable!(),
        }
    }
    if matches!((query.since_ms, query.until_ms), (Some(since), Some(until)) if since > until) {
        return Err(CkError::Usage(
            "--since-ms must not exceed --until-ms".into(),
        ));
    }
    Ok(Command::Activity(Options { query, follow }))
}

pub(super) async fn run(
    options: &Options,
    subc: Option<&Path>,
    json: JsonOutput,
    ckni: bool,
    verbose: bool,
) -> Result<(), CkError> {
    let resolved = discover_connection_file(subc)?;
    let consumer = SubcConsumer::connect(&resolved.path, ConsumerOptions::default())
        .await
        .map_err(|error| CkError::Connection {
            path: resolved.path,
            source: error.to_string(),
        })?;
    let result = display(&consumer, options, json, ckni, verbose).await;
    consumer.close().await;
    result
}

async fn display(
    consumer: &SubcConsumer,
    options: &Options,
    json: JsonOutput,
    ckni: bool,
    verbose: bool,
) -> Result<(), CkError> {
    let reply = consumer
        .query_activity(options.query.clone())
        .await
        .map_err(|error| CkError::Rejected {
            code: error.code().map(str::to_string),
            message: format!("activity query failed: {error}"),
        })?;
    if json.enabled && !options.follow {
        json.print(&serde_json::to_value(&reply.entries)?)?;
        return Ok(());
    }
    if json.enabled {
        for entry in &reply.entries {
            print_stream_json(entry, json)?;
        }
    } else if ckni {
        print_table(
            &super::ckni::activity_headers(verbose),
            reply
                .entries
                .iter()
                .map(|entry| plain_row(entry, verbose))
                .collect(),
        );
    } else {
        print_table(
            &["time (UTC)", "module", "actor", "summary"],
            reply.entries.iter().map(row).collect(),
        );
    }
    flush()?;
    if !options.follow {
        return Ok(());
    }

    #[cfg(feature = "test-support")]
    test_handoff(&reply.head).await?;

    // The scan's head covers all entries, not just the filtered/limited page.
    // Starting there replays appends made while the query output was rendered.
    let mut stream = consumer
        .subscribe_activity(Some(reply.head))
        .await
        .map_err(|error| CkError::Rejected {
            code: error.code().map(str::to_string),
            message: format!("activity follow failed: {error}"),
        })?;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| CkError::Message(format!("activity interrupt failed: {error}")))?;
                let _ = stream.unsubscribe();
                return Ok(());
            }
            next = stream.next() => {
                let entry = next.map_err(|error| CkError::Rejected {
                    code: error.code().map(str::to_string),
                    message: format!("activity follow failed: {error}"),
                })?
                    .ok_or_else(|| CkError::Message("activity feed ended; rerun ck activity to query retained history".into()))?;
                // Subscription bodies have no filters; retain query semantics locally.
                if matches(&options.query, &entry) {
                    if json.enabled { print_stream_json(&entry, json)?; }
                    else if ckni {
                        print_table(&super::ckni::activity_headers(verbose), vec![plain_row(&entry, verbose)]);
                    }
                    else { println!("{}", row(&entry).join("  ")); }
                    flush()?;
                }
            }
        }
    }
}

fn matches(query: &ActivityQueryRequest, entry: &ActivityEntry) -> bool {
    query
        .agent
        .as_ref()
        .is_none_or(|agent| entry.actor.agent_id.as_ref() == Some(agent))
        && query
            .project
            .as_ref()
            .is_none_or(|project| entry.actor.project_root.as_ref() == Some(project))
        && query
            .module
            .as_ref()
            .is_none_or(|module| &entry.module == module)
        && query.kind.as_ref().is_none_or(|kind| &entry.kind == kind)
        && query.since_ms.is_none_or(|since| entry.at_ms >= since)
        && query.until_ms.is_none_or(|until| entry.at_ms <= until)
}

fn row(entry: &ActivityEntry) -> Vec<String> {
    let actor = entry
        .actor
        .agent_id
        .clone()
        .unwrap_or_else(|| match &entry.actor.principal {
            subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
            subc_protocol::Principal::Direct => "direct".into(),
            subc_protocol::Principal::Unverified => "unverified".into(),
        });
    vec![
        timestamp(entry.at_ms),
        cell(&entry.module),
        cell(&actor),
        cell(&entry.summary),
    ]
}

fn plain_row(entry: &ActivityEntry, verbose: bool) -> Vec<String> {
    let actor = entry
        .actor
        .agent_id
        .as_deref()
        .unwrap_or_else(|| match &entry.actor.principal {
            subc_protocol::Principal::Reserved { module_id } => module_id,
            subc_protocol::Principal::Direct => "direct",
            subc_protocol::Principal::Unverified => "unverified",
        });
    let mut row = vec![
        timestamp(entry.at_ms),
        super::ckni::cell(
            entry
                .subject
                .as_ref()
                .map(|subject| subject.id.as_str())
                .unwrap_or(""),
        ),
        super::ckni::cell(&entry.summary),
        super::ckni::cell(actor),
    ];
    if verbose {
        row.extend([
            super::ckni::cell(&entry.kind),
            entry.seq.to_string(),
            super::ckni::json_cell(&serde_json::to_value(&entry.details).unwrap_or_default()),
        ]);
    }
    row
}

// Keep Unicode text readable, but never let ledger text inject terminal controls
// or split a table row. JSON serialization separately escapes control characters.
pub(super) fn cell(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn timestamp(at_ms: u64) -> String {
    let parts = super::utc_parts_from_epoch_secs(at_ms / 1000);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        parts.year,
        parts.month,
        parts.day,
        parts.hour,
        parts.minute,
        (at_ms / 1000) % 60,
        at_ms % 1000
    )
}

fn print_stream_json(entry: &ActivityEntry, json: JsonOutput) -> Result<(), CkError> {
    let line = if json.is_versioned() {
        json.stream_line(&serde_json::to_value(entry)?)?
    } else {
        serde_json::to_string(entry)?
    };
    println!("{line}");
    Ok(())
}

fn flush() -> Result<(), CkError> {
    std::io::stdout()
        .flush()
        .map_err(|error| CkError::Message(format!("activity output failed: {error}")))
}

// Pause only the test-support CLI after the real query, so an isolated daemon
// can append during the handoff instead of relying on scheduler timing.
#[cfg(feature = "test-support")]
async fn test_handoff(head: &subc_control::ActivityCursor) -> Result<(), CkError> {
    let Some(directory) = std::env::var_os("CK_TEST_ACTIVITY_HANDOFF") else {
        return Ok(());
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::write(directory.join("head.json"), serde_json::to_vec(head)?)
        .map_err(|error| CkError::Message(error.to_string()))?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !directory.join("resume").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| CkError::Message("activity test handoff timed out".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_live_filters_match_query_fields_and_inclusive_time_bounds() {
        let entry: ActivityEntry = serde_json::from_value(serde_json::json!({
            "seq":9,"at_ms":100,"daemon_incarnation":"current","module":"worker","kind":"worker.changed",
            "summary":"Updated item id", "details":{},
            "actor":{"principal":{"kind":"direct"},"agent_id":"agent-a","project_root":"project-a"}
        })).unwrap();
        let query: ActivityQueryRequest = serde_json::from_value(serde_json::json!({
            "op":"activity.query","agent":"agent-a","project":"project-a","module":"worker","kind":"worker.changed",
            "since_ms":100,"until_ms":100,"limit":1
        })).unwrap();
        assert!(matches(&query, &entry));
        for field in ["agent", "project", "module", "kind", "since_ms", "until_ms"] {
            let mut value = serde_json::to_value(&query).unwrap();
            value[field] = match field {
                "since_ms" => serde_json::json!(101),
                "until_ms" => serde_json::json!(99),
                _ => serde_json::json!("other"),
            };
            let changed = serde_json::from_value(value).unwrap();
            assert!(!matches(&changed, &entry), "live filter ignored: {field}");
        }
    }
    #[test]
    fn ckni_activity_names_subject_and_actor_and_keeps_details_verbose_only() {
        let mut entry: ActivityEntry = serde_json::from_value(serde_json::json!({
            "daemon_incarnation":"test-incarnation", "seq":7, "at_ms":1001,
            "module":"subc", "kind":"subc.module_restarted", "summary":"module restarted",
            "actor":{"principal":{"kind":"direct"}},
            "subject":{"type":"module", "id":"alpha"}, "details":{"drain_ms":1500},
        }))
        .unwrap();
        assert_eq!(
            plain_row(&entry, false),
            [
                "1970-01-01 00:00:01.001",
                "alpha",
                "module restarted",
                "direct"
            ]
        );
        let verbose = plain_row(&entry, true);
        assert_eq!(
            &verbose[4..],
            ["subc.module_restarted", "7", "{\"drain_ms\":1500}"]
        );
        entry.subject = None;
        entry.actor.principal = subc_protocol::Principal::Reserved {
            module_id: "recorder".into(),
        };
        assert_eq!(plain_row(&entry, false)[1], "-");
        assert_eq!(plain_row(&entry, false)[3], "recorder");
        entry.actor.agent_id = Some("agent-1".into());
        assert_eq!(plain_row(&entry, false)[3], "agent-1");
        let long = "event".repeat(500);
        entry.summary = long.clone();
        assert_eq!(plain_row(&entry, false)[2], long);
    }
}
