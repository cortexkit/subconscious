use super::*;
use subc_control::config::ConfigControlRequest as Request;

pub(super) const HELP: &str = "ck config — configuration gate\n\nusage:\n  ck config status|plan|stamp|unstamp|history\n  ck config apply <digest>\n  ck config rollback <generation>\n  ck config place-program <module> <path> <sha256>\n\nPlans, stamps, unstamps and rollbacks only prepare a view. Apply its digest to\nrequest approval on a trusted surface; terminal output is not approval.\nplace-program is available only with the daemon's dev-placement feature.\n";

impl CkClient {
    pub(super) async fn connect_config_local(
        resolved: ResolvedConnection,
    ) -> Result<Self, CkError> {
        let failure = |source: String| CkError::Connection {
            path: resolved.path.clone(),
            source,
        };
        // Read the endpoint and key from one owner-checked file. TCP possession of
        // that key is not the kernel-observed owner identity config operations need.
        let read = connection_file::read_for_client_with_local_endpoint(&resolved.path)
            .map_err(|error| failure(error.to_string()))?;
        let endpoint = read.local_endpoint.ok_or_else(|| failure("the daemon published no local endpoint (its local listener may have failed); config commands need it; check `ck daemon` and the daemon log, then retry".into()))?;
        let mut stream = time::timeout(CONNECT_TIMEOUT, connect_local(&endpoint)).await
            .map_err(|_| failure(format!("local connect {endpoint:?}: timed out after {CONNECT_TIMEOUT:?}")))?
            .map_err(|error| failure(format!("local connect {endpoint:?}: {error}; check `ck daemon` and the daemon log, then retry")))?;
        authenticate_client(&mut stream, &read.info, AUTH_DEADLINE)
            .await
            .map_err(|error| failure(format!("local authenticate: {error}")))?;
        Ok(Self {
            path: resolved.path,
            info: read.info,
            stream,
            next_corr: 1,
        })
    }
}

async fn connect_local(
    endpoint: &connection_file::LocalEndpoint,
) -> io::Result<Box<dyn ControlStream>> {
    match endpoint {
        #[cfg(unix)]
        connection_file::LocalEndpoint::Unix { path } => {
            Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
        }
        #[cfg(windows)]
        connection_file::LocalEndpoint::Pipe { name } => Ok(Box::new(
            tokio::net::windows::named_pipe::ClientOptions::new().open(name)?,
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported local endpoint kind",
        )),
    }
}

pub(super) fn parse(tail: &[OsString]) -> Result<Command, CkError> {
    let strings: Vec<_> = tail.iter().map(|arg| arg.to_string_lossy()).collect();
    let words: Vec<_> = strings.iter().map(|arg| arg.as_ref()).collect();
    let request = match words.as_slice() {
        [] | ["help" | "-h" | "--help"] => return Ok(Command::Help(HELP.into())),
        [verb] => match *verb {
            "status" => Request::Status {},
            "plan" => Request::Plan {},
            "stamp" => Request::Stamp {},
            "unstamp" => Request::Unstamp {},
            "history" => Request::History {},
            _ => return Err(CkError::Usage(HELP.into())),
        },
        [verb, digest] if *verb == "apply" => Request::Apply {
            digest: serde_json::from_value(json!(digest))
                .map_err(|error| CkError::Usage(format!("invalid digest: {error}\n{HELP}")))?,
        },
        [verb, generation] if *verb == "rollback" => Request::Rollback {
            generation: generation
                .parse()
                .map_err(|_| CkError::Usage(HELP.into()))?,
        },
        [verb, module, path, digest] if *verb == "place-program" => Request::PlaceProgram {
            module_id: module.to_string(),
            path: path.to_string(),
            sha256: serde_json::from_value(json!(if digest.starts_with("sha256:") {
                digest.to_string()
            } else {
                format!("sha256:{digest}")
            }))
            .map_err(|error| CkError::Usage(format!("invalid sha256: {error}\n{HELP}")))?,
        },
        _ => return Err(CkError::Usage(HELP.into())),
    };
    Ok(Command::Config(request))
}

pub(super) async fn run(
    client: &mut CkClient,
    request: Request,
    json_output: JsonOutput,
) -> Result<(), CkError> {
    let applies = matches!(request, Request::Apply { .. });
    let places = matches!(request, Request::PlaceProgram { .. });
    let timeout = if applies {
        CONTROL_RESPONSE_TIMEOUT.saturating_add(OPERATOR_RESPONSE_BUDGET)
    } else {
        CONTROL_RESPONSE_TIMEOUT
    };
    let reply = match client.rpc_value_with_timeout(request, timeout).await {
        Err(CkError::ResponseTimeout { timeout }) if applies || places => {
            return Err(CkError::OutcomeUnknown {
                operation: "configuration commit".into(),
                verify_command: "ck config status; ck config history".into(),
                timeout,
            });
        }
        result => result?,
    };
    // Keep the complete daemon view visible, including new diagnostic fields.
    // A summary must not hide files included in the daemon's program-identity
    // check, or any protection gap, from the person reviewing the plan.
    if json_output.enabled {
        print_json(&reply)
    } else {
        let mut view = reply;
        human_channels(&mut view);
        print_json(&view)
    }
}

fn human_channels(value: &mut Value) {
    if let Some(generations) = value.get_mut("generations").and_then(Value::as_array_mut) {
        for generation in generations {
            describe_channel(generation.get_mut("approval_channel"));
        }
    }
    if let Some(records) = value
        .pointer_mut("/status/approval_channels")
        .and_then(Value::as_array_mut)
    {
        for record in records {
            describe_channel(record.get_mut("channel"));
        }
    }
}

fn describe_channel(value: Option<&mut Value>) {
    if let Some(value) = value {
        if let Ok(channel) =
            serde_json::from_value::<subc_control::config::ApprovalChannel>(value.clone())
        {
            *value = json!(channel.description());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_cli_parses_all_operations_strictly() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for args in [
            vec!["status"],
            vec!["plan"],
            vec!["stamp"],
            vec!["unstamp"],
            vec!["history"],
            vec!["apply", &digest],
            vec!["rollback", "4"],
            vec!["place-program", "demo", "/a path/program", &digest],
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            let Command::Config(request) = parse(&args).unwrap() else {
                panic!("not config")
            };
            assert!(request.op().starts_with("config."));
            let mut extra = args;
            extra.push("extra".into());
            assert!(parse(&extra).is_err());
        }
        for args in [
            vec!["apply", "bad"],
            vec!["rollback", "-1"],
            vec!["place-program"],
        ] {
            assert!(parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn config_cli_human_history_names_unapproved_placement() {
        let mut value = json!({"generations":[{"approval_channel":"development_placement"}], "status":{"approval_channels":[{"channel":"development_placement"}]}});
        human_channels(&mut value);
        assert_eq!(
            value["generations"][0]["approval_channel"],
            "development placement (no approval)"
        );
        assert_eq!(
            value["status"]["approval_channels"][0]["channel"],
            "development placement (no approval)"
        );
    }
}
