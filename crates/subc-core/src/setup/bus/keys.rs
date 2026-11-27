use super::{valid_half, Auth, Credential};

pub(super) const SIGNER: &str = "signing:ck-bus-operator-signer:1";
pub(super) const SYSACCOUNT: &str = "signing:ck-bus-sysaccount:1";
pub(super) const ACCOUNT: &str = "signing:ck-bus-account:1";
const IDS: [&str; 3] = [SIGNER, SYSACCOUNT, ACCOUNT];

pub(super) struct BusKeys {
    pub signer_pub: String,
    pub sysaccount_pub: String,
    pub changed: bool,
}

pub(super) fn check_active(listed: &[Credential]) -> Result<(), String> {
    for id in IDS {
        if listed_non_active(listed, id) {
            return Err(format!(
                "{id} is in the vault but not active; nothing was changed"
            ));
        }
    }
    Ok(())
}

pub(super) fn ensure(auth: &mut Auth<'_>, listed: &[Credential]) -> Result<BusKeys, String> {
    let (signer_pub, minted_signer) = ensure_key(auth, listed, SIGNER, true)?;
    let (sysaccount_pub, minted_sysaccount) = ensure_key(auth, listed, SYSACCOUNT, true)?;
    let (_, minted_account) = ensure_key(auth, listed, ACCOUNT, false)?;
    let mut changed = minted_signer || minted_sysaccount || minted_account;
    for id in IDS {
        for op in ["sign", "read"] {
            changed |= auth.grant("reserved:ckbus", "exact", id, op)?;
        }
    }
    Ok(BusKeys {
        signer_pub,
        sysaccount_pub,
        changed,
    })
}

fn listed_non_active(listed: &[Credential], id: &str) -> bool {
    listed
        .iter()
        .any(|key| key.id == id && key.state != "active")
}

fn ensure_key(
    auth: &mut Auth<'_>,
    listed: &[Credential],
    id: &str,
    need_half: bool,
) -> Result<(String, bool), String> {
    let mint = || format!("ck auth mint-signing-key --id {id}");
    let mut changed = false;
    if !listed.iter().any(|key| key.id == id) {
        let result = auth
            .retry_lease(&["mint-signing-key", "--id", id])
            .map_err(|error| format!("{} failed: {error}", mint()))?;
        match result.exit_code {
            Some(0) if need_half => {
                return public_half(&result.stdout)
                    .map(|half| (half, true))
                    .ok_or_else(|| format!("{} did not return a valid public key", mint()));
            }
            Some(0) => return Ok((String::new(), true)),
            Some(1)
                if result.stderr.lines().any(|line| {
                    line.trim_start()
                        .starts_with("error: credential already exists")
                }) => {}
            Some(5) => {
                let unknown =
                    || format!("{} ended with an unknown outcome; rerun ck setup", mint());
                let listed = auth.list().map_err(|_| unknown())?;
                if listed_non_active(&listed, id) {
                    return Err(format!(
                        "{id} is in the vault but not active; rerun ck setup"
                    ));
                }
                if !listed
                    .iter()
                    .any(|key| key.id == id && key.state == "active")
                {
                    return Err(unknown());
                }
                // The mint may have taken effect before the reply was lost.
                changed = true;
            }
            Some(code) => return Err(format!("{} failed with exit {code}", mint())),
            None => return Err(format!("{} failed without an exit code", mint())),
        }
    }
    let half = if need_half {
        read_public_half(auth, id)?
    } else {
        String::new()
    };
    Ok((half, changed))
}

fn read_public_half(auth: &mut Auth<'_>, id: &str) -> Result<String, String> {
    auth.retry_lease(&["public-key", "--id", id])
        .ok()
        .filter(|result| result.exit_code == Some(0))
        .and_then(|result| public_half(&result.stdout))
        .ok_or_else(|| {
            format!("setup needs ck auth public-key to read {id}; run ck upgrade, then ck setup")
        })
}

fn public_half(stdout: &str) -> Option<String> {
    let mut halves = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("public_key_hex "));
    let half = halves.next()?;
    (valid_half(half) && halves.next().is_none()).then(|| half.to_owned())
}
