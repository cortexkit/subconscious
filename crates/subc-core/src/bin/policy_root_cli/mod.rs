use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use subc_control::policy_root::{RecoverPolicyRoot, SetPolicyRoot};
use subc_protocol::policy_root::{
    decode_public_key, fingerprint, key_id, GetPolicyRoot, PolicyRootChange,
};

pub(super) const HELP: &str = "Usage:\n  ck policy-root show\n  ck policy-root set --vault-key <name>\n  ck policy-root recover --body <file>\n\nSet and recover prepare a plan. Apply its digest with ck config apply.";

#[derive(Debug)]
pub(super) enum Action {
    Show,
    Set(String),
    Recover(PathBuf),
}

pub(super) fn parse(args: &[OsString]) -> Result<Command, CkError> {
    let action = match args {
        [] => return Ok(Command::Help(HELP.into())),
        [verb] if matches!(verb.to_str(), Some("help" | "-h" | "--help")) => {
            return Ok(Command::Help(HELP.into()))
        }
        [verb] if verb == "show" => Action::Show,
        [verb, option, name] if verb == "set" && option == "--vault-key" => {
            let name = name
                .to_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| CkError::Usage(HELP.into()))?;
            Action::Set(name.into())
        }
        [verb, option, path] if verb == "recover" && option == "--body" => {
            Action::Recover(path.into())
        }
        _ => return Err(CkError::Usage(HELP.into())),
    };
    Ok(Command::PolicyRoot(action))
}

pub(super) async fn run(
    client: &mut CkClient,
    action: Action,
    output: JsonOutput,
) -> Result<(), CkError> {
    let request = match action {
        Action::Show => {
            let value = client
                .rpc_value(ClientControlRequest::PolicyRoot(GetPolicyRoot {}))
                .await?;
            let ClientControlResponse::PolicyRoot(view) = serde_json::from_value(value)? else {
                return Err(CkError::Message("unexpected policy_root.get reply".into()));
            };
            if output.enabled {
                return print_json(&serde_json::to_value(&view)?);
            }
            if let Some(pin) = view.pin {
                let public = decode_public_key(&pin.public_key_hex).ok_or_else(|| {
                    CkError::Message("daemon returned an invalid policy root public key".into())
                })?;
                println!(
                    "Key id: {}\nFingerprint: {}\nRetired key ids: {}",
                    pin.key_id,
                    fingerprint(&public),
                    if pin.retired_key_ids.is_empty() {
                        "none".into()
                    } else {
                        pin.retired_key_ids.join(", ")
                    }
                );
            } else {
                println!("no pin");
            }
            return Ok(());
        }
        Action::Set(name) => ClientControlRequest::PolicyRootSet(read_key(client, &name).await?),
        Action::Recover(path) => {
            ClientControlRequest::PolicyRootRecover(read_recovery(path).await?)
        }
    };
    let reply = client.rpc_value(request).await?;
    let change = match serde_json::from_value::<ClientControlResponse>(reply)? {
        ClientControlResponse::PolicyRootSet(change)
        | ClientControlResponse::PolicyRootRecover(change) => change,
        _ => return Err(CkError::Message("unexpected policy root plan reply".into())),
    };
    print_plan(client, change, output).await
}

async fn print_plan(
    client: &mut CkClient,
    change: PolicyRootChange,
    output: JsonOutput,
) -> Result<(), CkError> {
    use subc_control::config::{ConfigControlRequest, ConfigControlResponse};

    let next = format!("ck config apply {}", change.digest);
    let request = ConfigControlRequest::PlanGet {
        digest: change.digest.clone(),
    };
    let view = client
        .rpc_value_with_timeout(request, CONTROL_RESPONSE_TIMEOUT)
        .await?;
    let ConfigControlResponse::PlanGet { view } = serde_json::from_value(view)? else {
        return Err(CkError::Message(format!(
            "unexpected policy root plan detail; review with {next}"
        )));
    };
    let summary = view
        .plan
        .changeset
        .policy_changes
        .iter()
        .find_map(|row| {
            (row.get("path")?.as_str()? == "/agent_policy_root")
                .then(|| row.get("summary")?.as_str())
                .flatten()
        })
        .ok_or_else(|| {
            CkError::Message(format!(
                "daemon did not return the policy root review; review with {next}"
            ))
        })?;
    if output.enabled {
        print_json(
            &json!({"digest":change.digest,"state":change.state,"review":summary,"apply_command":next}),
        )
    } else {
        println!("{summary}\nPlan digest: {}\nNext: {next}", change.digest);
        Ok(())
    }
}

async fn read_key(client: &mut CkClient, name: &str) -> Result<SetPolicyRoot, CkError> {
    let route = open_management(client, "claustrum", "policy-root").await?;
    let reply = client
        .route_request_value(
            route,
            json!({
                "method":"credential.public_key", "params":{"credential_id": name},
            }),
        )
        .await;
    client.route_goodbye(route).await;
    parse_public_key(&reply?)
}

fn parse_public_key(reply: &Value) -> Result<SetPolicyRoot, CkError> {
    let malformed = || {
        CkError::Message("credential.public_key did not return a valid Ed25519 public key".into())
    };
    let result = reply
        .get("result")
        .filter(|result| result["algorithm"] == "ed25519")
        .ok_or_else(malformed)?;
    let public_key_hex = result["public_key_hex"].as_str().ok_or_else(malformed)?;
    let public = decode_public_key(public_key_hex).ok_or_else(malformed)?;
    let derived = key_id(&public);
    if result["key_id"].as_str() != Some(&derived) {
        return Err(malformed());
    }
    Ok(SetPolicyRoot {
        key_id: derived,
        public_key_hex: public_key_hex.into(),
    })
}

async fn read_recovery(path: PathBuf) -> Result<RecoverPolicyRoot, CkError> {
    use tokio::io::AsyncReadExt;

    const LIMIT: u64 = 768;
    let error = |error: io::Error| {
        CkError::Message(format!(
            "read recovery statement {}: {error}",
            path.display()
        ))
    };
    if !tokio::fs::metadata(&path).await.map_err(error)?.is_file() {
        return Err(CkError::Message("recovery statement must be a file".into()));
    }
    let mut body = Vec::new();
    tokio::fs::File::open(&path)
        .await
        .map_err(error)?
        .take(LIMIT + 1)
        .read_to_end(&mut body)
        .await
        .map_err(error)?;
    if body.len() as u64 > LIMIT {
        return Err(CkError::Message("recovery statement is too large".into()));
    }
    Ok(RecoverPolicyRoot {
        body_b64: STANDARD.encode(body),
    })
}

pub(super) async fn open_management(
    client: &mut CkClient,
    module_id: &str,
    purpose: &str,
) -> Result<RouteHandle, CkError> {
    let request = ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: RouteTarget::ManagementSurface {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new(CK_HARNESS, purpose),
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    };
    let value = client.rpc_value(request).await?;
    match serde_json::from_value::<ClientControlResponse>(value)? {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
            ..
        } => Ok(RouteHandle {
            channel: route_channel,
            epoch: route_epoch,
        }),
        other => Err(CkError::Message(format!(
            "unexpected route.open response: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_root_cli_uses_explicit_set_and_recovery_arguments() {
        let parse =
            |words: &[&str]| super::parse(&words.iter().map(OsString::from).collect::<Vec<_>>());
        assert!(matches!(
            parse(&["show"]).unwrap(),
            Command::PolicyRoot(Action::Show)
        ));
        assert!(
            matches!(parse(&["set", "--vault-key", "signing:demo:1"]).unwrap(), Command::PolicyRoot(Action::Set(name)) if name == "signing:demo:1")
        );
        assert!(
            matches!(parse(&["recover", "--body", "example statement.json"]).unwrap(), Command::PolicyRoot(Action::Recover(path)) if path == Path::new("example statement.json"))
        );
        assert!(parse(&["recover", "example statement.json"]).is_err());
        assert!(parse(&["set", "--vault-key", ""]).is_err());
    }

    #[test]
    fn policy_root_cli_derives_the_id_from_the_vault_public_key() {
        let mut reply = json!({"result": {
            "algorithm":"ed25519", "public_key_hex":"00".repeat(32), "key_id":"66687aadf862bd77",
        }});
        let key = parse_public_key(&reply).unwrap();
        assert_eq!(key.key_id, "66687aadf862bd77");
        reply["result"]["key_id"] = json!("66687aadf862bd78");
        assert!(parse_public_key(&reply).is_err());
    }
}
