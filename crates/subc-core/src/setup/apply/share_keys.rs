use super::{broca, vault};
use crate::setup::runtime::{CommandResult, CommandRunner};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path, thread};

const MEMBER_PREFIX: &str = "fed-computer:";
const TRANSFER_KEY: &str = "kem:vault-transfer:1";

#[derive(Deserialize)]
struct Roster {
    #[serde(rename = "self")]
    local: DeviceIdentity,
    devices: Vec<Device>,
}

#[derive(Deserialize)]
struct DeviceIdentity {
    fingerprint: String,
}

#[derive(Deserialize)]
struct Device {
    name: String,
    fingerprint: String,
    device_class: String,
    trust: String,
}

fn fingerprint(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn member(value: &str) -> String {
    format!("{MEMBER_PREFIX}{value}")
}

fn resolve<'a>(roster: &'a Roster, with: &str) -> Result<(&'a Device, String), String> {
    let local = &roster.local.fingerprint;
    if !fingerprint(local) || local != &local.to_ascii_lowercase() {
        return Err("Callosum returned an invalid fingerprint for this computer.".into());
    }
    let explicit = if let Some(value) = with.strip_prefix(MEMBER_PREFIX) {
        if !fingerprint(value) {
            return Err(format!("{with:?} is not a valid computer member ID."));
        }
        Some(value)
    } else {
        fingerprint(with).then_some(with)
    };
    let matching = roster
        .devices
        .iter()
        .filter(|row| {
            row.device_class == "computer"
                && row.trust == "verified"
                && row.fingerprint != *local
                && match explicit {
                    Some(value) => row.fingerprint.eq_ignore_ascii_case(value),
                    None => row.name == with,
                }
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [row]
            if fingerprint(&row.fingerprint)
                && row.fingerprint == row.fingerprint.to_ascii_lowercase() =>
        {
            Ok((row, member(local)))
        }
        [_] => Err("Callosum returned an invalid fingerprint for the paired computer.".into()),
        [] => Err(format!(
            "No other verified paired computer matches {with:?}."
        )),
        rows => {
            let fingerprints = rows
                .iter()
                .map(|row| row.fingerprint.as_str())
                .collect::<Vec<_>>();
            Err(format!("More than one verified paired computer matches {with:?}. Pass one of their fingerprints: {}.", fingerprints.join(", ")))
        }
    }
}

fn checked(result: CommandResult, command: &str) -> Result<String, String> {
    if result.exit_code == Some(0) {
        Ok(result.stdout)
    } else {
        Err(format!(
            "{command} failed{}: {}",
            vault::exit_description(&result),
            result.explanation()
        ))
    }
}

fn check_receiving(
    auth: &mut vault::Auth<'_>,
    records: &[vault::Credential],
    local: &str,
    peer: &str,
) -> Result<(), String> {
    let members = checked(
        auth.retry_lease(&["member", "list"])?,
        "ck auth member list",
    )?;
    let mut missing = Vec::new();
    if !records
        .iter()
        .any(|row| row.id == TRANSFER_KEY && row.state == "active")
    {
        missing.push(format!("This computer needs an active vault transfer key. Run `ck auth transfer init --self {local}`."));
    }
    if !members.lines().any(|row| {
        let mut fields = row.split_whitespace();
        fields.next() == Some(peer) && fields.next() == Some("transfer_deliver")
    }) {
        missing.push(format!("This computer does not allow transfers from {peer}. Run `ck auth member grant --member {peer} --operation transfer_deliver`."));
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join("\n"))
    }
}

fn send_receipt<'a>(stdout: &'a str, credential: &str, to: &str) -> Option<(&'a str, &'a str)> {
    let mut lines = stdout.trim().lines();
    let fields = lines.next()?.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        [transfer, id, "to", recipient, "reply", code]
            if *id == credential
                && *recipient == to
                && !transfer.starts_with('-')
                && lines.next().is_none() =>
        {
            Some((transfer, code))
        }
        _ => None,
    }
}

fn send(auth: &mut vault::Auth<'_>, credential: &str, to: &str) -> Result<String, String> {
    let fresh = ["transfer", "send", "--id", credential, "--to", to];
    let mut resume = None::<String>;
    let mut restarted = false;
    loop {
        let result = if let Some(transfer) = &resume {
            auth.retry_lease(&["transfer", "send", "--resume", transfer])?
        } else {
            auth.retry_lease(&fresh)?
        };
        let unknown = || {
            format!("Send outcome unknown for {credential}. Run `ck setup share-keys` on the other computer to accept it if it arrived.")
        };
        if result.exit_code == Some(5) {
            // The vault does not return a transfer ID when the outcome is unknown.
            return Err(unknown());
        }
        let action = match &resume {
            Some(transfer) => format!("ck auth transfer send --resume {transfer} for {credential}"),
            None => format!("ck auth transfer send for {credential}"),
        };
        let stdout = checked(result, &action)?;
        let (transfer, code) = send_receipt(&stdout, credential, to).ok_or_else(unknown)?;
        if resume.as_ref().is_some_and(|id| id != transfer) {
            return Err(unknown());
        }
        match code {
            "pending" => return Ok(format!("Sent {credential} ({transfer}); it is still pending on the other computer.")),
            "accepted" => return Ok(format!("Sent {credential} ({transfer}); the other computer accepted it.")),
            "rejected" => return Ok(format!("The other computer rejected {credential} ({transfer}); it may already hold that ID.")),
            "expired" if !restarted => {
                restarted = true;
                resume = None;
            }
            "expired" => return Err(format!("The new transfer of {credential} also expired.")),
            _ if resume.is_none() => {
                resume = Some(transfer.into());
            }
            _ => return Err(format!("The other computer refused {credential} ({transfer}): {code}.")),
        }
    }
}

struct Incoming {
    transfer: String,
    key: u16,
}

fn incoming(auth: &mut vault::Auth<'_>, from: &str) -> Result<Vec<Incoming>, String> {
    let stdout = checked(
        auth.retry_lease(&["transfer", "receive"])?,
        "ck auth transfer receive",
    )?;
    let mut pending = Vec::new();
    for row in stdout.lines() {
        let mut fields = row.split_whitespace();
        let (Some(transfer), Some("from"), Some(sender), Some(credential)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if sender != from {
            continue;
        }
        let Some(suffix) = credential.strip_prefix(broca::PREFIX) else {
            continue;
        };
        let key = broca::parse_id(suffix)
            .filter(|_| {
                !transfer.starts_with('-')
                    && fields.next() == Some("opaque")
                    && fields.next().is_none()
            })
            .ok_or_else(|| {
                format!("Invalid incoming Broca deployment key or transfer row: {row}")
            })?;
        pending.push(Incoming {
            transfer: transfer.into(),
            key,
        });
    }
    Ok(pending)
}

fn settle(
    auth: &mut vault::Auth<'_>,
    row: &Incoming,
    from: &str,
    held: &mut BTreeMap<u16, bool>,
) -> Result<String, String> {
    let credential = broca::id(row.key);
    let transfer = &row.transfer;
    let verb = if held.contains_key(&row.key) {
        "reject"
    } else {
        let grant = auth.retry_lease(&vault::grant_args(
            broca::PRINCIPAL,
            "exact",
            &credential,
            "read",
        ))?;
        checked(
            grant,
            &format!("ck auth grant Broca read on {credential}; left {transfer} pending"),
        )?;
        "accept"
    };
    let result = auth.retry_lease(&["transfer", verb, transfer, "--from", from])?;
    let action = format!("ck auth transfer {verb} {transfer}");
    if result.exit_code == Some(5) {
        *held = broca::deployment_records(&auth.list()?)?;
        let still_pending = incoming(auth, from)?
            .iter()
            .any(|row| row.transfer == *transfer);
        let observation = if still_pending {
            "still pending"
        } else {
            "no longer pending"
        };
        return Err(format!("The outcome of `{action}` for {credential} is unknown. The transfer is {observation}; no retry was attempted."));
    }
    let stdout = checked(result, &action)?;
    match (verb, stdout.trim()) {
        ("reject", reply) if reply.split_whitespace().eq([transfer.as_str(), "rejected"]) => Ok(
            format!("Rejected {credential} ({transfer}) because this vault already holds that ID."),
        ),
        ("accept", reply) if reply.starts_with("accepted ") => {
            held.insert(row.key, true);
            Ok(format!(
                "Accepted {credential} ({transfer}) and granted Broca read access."
            ))
        }
        ("accept", reply)
            if reply.starts_with("already ") && reply.ends_with("; nothing was written") =>
        {
            *held = broca::deployment_records(&auth.list()?)?;
            Ok(format!("Transfer {transfer} for {credential} was {reply}."))
        }
        _ => Err(format!(
            "`{action}` returned an unexpected reply: {}",
            stdout.trim()
        )),
    }
}

pub fn share_keys(
    with: &str,
    auth_program: &Path,
    runner: &mut dyn CommandRunner,
    mut report: impl FnMut(String),
) -> Result<(), String> {
    let stdout = checked(runner.callosum_devices()?, "callosum.devices_read")?;
    let roster: Roster = serde_json::from_str(&stdout)
        .map_err(|error| format!("Invalid callosum.devices_read reply: {error}"))?;
    let (other, local) = resolve(&roster, with)?;
    let peer = member(&other.fingerprint);
    let mut auth = vault::Auth {
        program: auth_program,
        runner,
        wait: thread::sleep,
    };
    let records = auth.list()?;
    check_receiving(&mut auth, &records, &local, &peer)?;
    let mut held = broca::deployment_records(&records)?;
    let mut issues = Vec::new();
    for &key in held.keys() {
        match send(&mut auth, &broca::id(key), &peer) {
            Ok(line) => report(line),
            Err(error) => issues.push(error),
        }
    }
    match incoming(&mut auth, &peer) {
        Ok(rows) => {
            for row in rows {
                match settle(&mut auth, &row, &peer, &mut held) {
                    Ok(line) => report(line),
                    Err(error) => issues.push(error),
                }
            }
        }
        Err(error) => issues.push(error),
    }
    if let Some((&key, _)) = held.iter().find(|(_, active)| **active) {
        report(format!("This vault's lowest active deployment key is {}. Once both computers hold the same keys, Broca uses that key for new sessions if it can read it and no key is explicitly configured.", broca::id(key)));
    } else {
        report("This vault has no active Broca deployment key.".into());
    }
    report(format!("Run `ck setup share-keys --with {local}` on {name:?} too. If return transfers arrive later, run this command here again.", name = other.name));
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues.join("\n"))
    }
}

#[cfg(test)]
#[path = "share_keys_tests.rs"]
mod tests;
