//! Offline install tooling for the local `nats-server`: the payloads of the install root
//! ceremony, and the server configuration written from the signed result
//! (`docs/designs/nats-install-trust-chain.md`, sections 1-5 and 7).
//!
//! - `ck-bus install-plan` creates (or reads back) the system account identity and
//!   writes the signing inputs the root must sign, printing each one's path, sha256 and
//!   decoded claims for approval. When the stored JWTs already carry the claims it would
//!   build, it prints "no ceremony needed" and writes nothing.
//! - `ck-bus install-apply` verifies the root's signatures and writes `operator.jwt`,
//!   `server.conf` and an empty resolver directory, then prints ck-bus's three
//!   environment values.
//!
//! Both run without `SUBC_MODULE_ID` and never reach the daemon or the vault. `ck setup`
//! drives them. Output is one JSON object on stdout; a refusal is one line on stderr and
//! exit status 1.

mod conf;
mod payload;
mod seeds;

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::bootstrap::account_jwt::decode_claims;
use crate::bootstrap::config::{
    check_loopback_url, NATS_URL_ENV, OPERATOR_JWT_ENV, SYSTEM_ACCOUNT_ENV,
};
use crate::credentials::nkey::{encode_public, NkeyRole};
use conf::ServerConf;
use payload::{Payload, PinnedKeys};

/// The default client port. Not the stock 4222, so a developer's own nats-server on the
/// default port does not collide with the supervised one; and below every common
/// ephemeral range (Linux 32768+, macOS and Windows 49152+), so an outgoing connection
/// cannot be holding it when the server starts.
pub const DEFAULT_PORT: u16 = 14222;

const SYSTEM_ACCOUNT_FILE: &str = "system_account";
const OPERATOR_JWT_FILE: &str = "operator.jwt";
const SERVER_CONF_FILE: &str = "server.conf";
const JWT_DIR: &str = "jwt";
const JS_DIR: &str = "js";
const CEREMONY_DIR: &str = "ceremony";
pub const NO_CEREMONY_NEEDED: &str = "no ceremony needed";

/// Runs `install-plan` or `install-apply` when `args` (without the program name) names
/// one, returning the process exit status; `None` for any other invocation.
pub fn run(args: &[String]) -> Option<i32> {
    let (command, rest) = args.split_first()?;
    let result = match command.as_str() {
        "install-plan" => Flags::parse(rest).and_then(|flags| plan(&flags)),
        "install-apply" => Flags::parse(rest).and_then(|flags| apply(&flags)),
        _ => return None,
    };
    Some(match result {
        Ok(output) => {
            println!("{output:#}");
            0
        }
        Err(error) => {
            eprintln!("ck-bus {command}: {error}");
            1
        }
    })
}

struct Flags(Vec<(String, String)>);

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            let name = flag
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument {flag:?}"))?;
            let value = iter
                .next()
                .ok_or_else(|| format!("--{name} needs a value"))?;
            if pairs.iter().any(|(seen, _)| seen == name) {
                return Err(format!("--{name} given twice"));
            }
            pairs.push((name.to_string(), value.clone()));
        }
        Ok(Self(pairs))
    }

    fn optional(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.optional(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn only(&self, allowed: &[&str]) -> Result<(), String> {
        match self
            .0
            .iter()
            .find(|(flag, _)| !allowed.contains(&flag.as_str()))
        {
            Some((flag, _)) => Err(format!("unknown flag --{flag}")),
            None => Ok(()),
        }
    }

    fn nats_dir(&self) -> Result<PathBuf, String> {
        let dir = self.required("nats-dir")?;
        std::path::absolute(dir).map_err(|error| format!("--nats-dir {dir}: {error}"))
    }
}

/// A raw Ed25519 public key given as 64 hex characters, as the vault prints it.
fn public_key_hex(flags: &Flags, name: &str, role: NkeyRole) -> Result<String, String> {
    let hex = flags.required(name)?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|at| {
            hex.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("--{name} must be 64 hex characters (a raw Ed25519 public key)"))?;
    Ok(encode_public(role, &key))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes `content` to `path` through a temporary file in the same directory and a
/// rename, so a reader never sees a partial file.
fn write_atomic(path: &Path, content: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("a written path has a parent")?;
    let name = path
        .file_name()
        .ok_or("a written path has a file name")?
        .to_string_lossy();
    let temp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    result.map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("write {}: {error}", path.display())
    })
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn is_account_key(value: &str) -> bool {
    value.starts_with('A') && KeyPair::from_public_key(value).is_ok()
}

/// The system account id: read back from `<nats dir>/system_account`, else adopted from
/// a stored operator JWT, else generated. The stored operator JWT is adopted even when
/// it no longer verifies, because a root rotation keeps the account ids. The identity key pair is
/// generated in memory and only its public half is kept: nobody holds the account's seed,
/// and user JWTs are signed by the system account signing key instead. The id is never
/// generated twice, because an account id cannot change.
fn system_account_id(nats_dir: &Path) -> Result<String, String> {
    let path = nats_dir.join(SYSTEM_ACCOUNT_FILE);
    if let Some(text) = read_optional(&path)? {
        let id = text.trim();
        if !is_account_key(id) {
            return Err(format!(
                "{} does not hold an account public key",
                path.display()
            ));
        }
        return Ok(id.to_string());
    }
    let adopted = read_optional(&nats_dir.join(OPERATOR_JWT_FILE))?
        .and_then(|jwt| decode_claims(&jwt))
        .and_then(|claims| {
            claims["nats"]["system_account"]
                .as_str()
                .map(str::to_string)
        })
        .filter(|id| is_account_key(id));
    let id = adopted.unwrap_or_else(|| {
        let identity = KeyPair::new_account();
        identity.public_key()
    });
    write_atomic(&path, format!("{id}\n").as_bytes())?;
    Ok(id)
}

/// Why a stored payload needs a ceremony, or `None` when it already carries the claims
/// that would be built.
fn stored_verdict(stored: Option<String>, root: &str, planned: &Value) -> Option<String> {
    let Some(jwt) = stored else {
        return Some("absent".to_string());
    };
    match payload::verify_stored(&jwt, root) {
        Err(error) => Some(format!("the stored JWT is refused: {error}")),
        Ok(claims) => {
            let differs = payload::differing_claims(&claims, planned);
            (!differs.is_empty()).then(|| format!("claims differ: {}", differs.join(", ")))
        }
    }
}

fn stored_system_account_jwt(
    nats_dir: &Path,
    system_account: &str,
) -> Result<Option<String>, String> {
    Ok(read_optional(&nats_dir.join(SERVER_CONF_FILE))?
        .and_then(|conf| conf::preloaded_jwt(&conf, system_account)))
}

fn plan(flags: &Flags) -> Result<Value, String> {
    flags.only(&[
        "nats-dir",
        "root-pub",
        "signer-pub",
        "sysaccount-pub",
        "out",
    ])?;
    let nats_dir = flags.nats_dir()?;
    let keys = PinnedKeys {
        root: public_key_hex(flags, "root-pub", NkeyRole::Operator)?,
        signer: public_key_hex(flags, "signer-pub", NkeyRole::Operator)?,
        sysaccount: public_key_hex(flags, "sysaccount-pub", NkeyRole::Account)?,
    };
    let out = match flags.optional("out") {
        Some(out) => std::path::absolute(out).map_err(|error| format!("--out {out}: {error}"))?,
        None => nats_dir.join(CEREMONY_DIR),
    };
    fs::create_dir_all(&nats_dir)
        .map_err(|error| format!("create {}: {error}", nats_dir.display()))?;
    let system_account = system_account_id(&nats_dir)?;

    let issued_at = unix_now();
    let planned = [
        (
            Payload::Operator,
            payload::operator_claims(&keys, &system_account, issued_at),
            read_optional(&nats_dir.join(OPERATOR_JWT_FILE))?,
        ),
        (
            Payload::SystemAccount,
            payload::system_account_claims(&keys, &system_account, issued_at),
            stored_system_account_jwt(&nats_dir, &system_account)?,
        ),
    ];
    let mut needed = Vec::new();
    for (kind, claims, stored) in planned {
        if let Some(reason) = stored_verdict(stored, &keys.root, &claims) {
            needed.push((kind, claims, reason));
        }
    }
    if needed.is_empty() {
        return Ok(json!({
            "status": NO_CEREMONY_NEEDED,
            "system_account": system_account,
        }));
    }

    fs::create_dir_all(&out).map_err(|error| format!("create {}: {error}", out.display()))?;
    let mut payloads = Vec::new();
    for (kind, claims, reason) in needed {
        let path = out.join(format!("{}.input", kind.label()));
        let input = payload::signing_input(&claims);
        seeds::refuse_seed_in_content(&path, &input)?;
        write_atomic(&path, input.as_bytes())?;
        // What is shown for approval is decoded from the file just written, and only if
        // it re-encodes to exactly those bytes.
        let written =
            fs::read(&path).map_err(|error| format!("read back {}: {error}", path.display()))?;
        let decoded = payload::decode_canonical(&written)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        payloads.push(json!({
            "payload": kind.label(),
            "reason": reason,
            "path": path.display().to_string(),
            "sha256": sha256_hex(&written),
            "claims": payload::summary(kind, &decoded),
        }));
    }
    Ok(json!({
        "status": "ceremony needed",
        "system_account": system_account,
        "payloads": payloads,
    }))
}

/// One root-signed payload, verified.
struct Signed {
    jwt: String,
    claims: Value,
}

/// Reads a signing input and its signature, and verifies the signature under the root.
fn signed_from_flags(
    flags: &Flags,
    kind: Payload,
    input_flag: &str,
    sig_flag: &str,
    root: &str,
) -> Result<Option<Signed>, String> {
    let (input_path, signature) = match (flags.optional(input_flag), flags.optional(sig_flag)) {
        (None, None) => return Ok(None),
        (Some(input), Some(signature)) => (input, signature),
        _ => return Err(format!("--{input_flag} and --{sig_flag} go together")),
    };
    let input =
        fs::read(input_path).map_err(|error| format!("--{input_flag} {input_path}: {error}"))?;
    let claims = payload::decode_canonical(&input)
        .map_err(|error| format!("--{input_flag} {input_path}: {error}"))?;
    let raw = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .filter(|raw| raw.len() == 64)
        .ok_or_else(|| format!("--{sig_flag} is not a 64-byte unpadded base64url signature"))?;
    payload::verify(root, &input, &raw)
        .map_err(|error| format!("the {} payload: {error}", kind.label()))?;
    let input = String::from_utf8(input).map_err(|_| "the signing input is not ASCII")?;
    Ok(Some(Signed {
        jwt: format!("{input}.{signature}"),
        claims,
    }))
}

/// A payload given on the command line, or else the stored one, which must still verify
/// under the root: a payload whose claims did not change is kept, never re-signed.
fn signed_or_stored(
    given: Option<Signed>,
    stored: Option<String>,
    kind: Payload,
    root: &str,
) -> Result<Signed, String> {
    if let Some(given) = given {
        return Ok(given);
    }
    let label = kind.label();
    let jwt = stored.ok_or_else(|| {
        format!("no stored {label} JWT to keep; its input and signature are required")
    })?;
    let claims = payload::verify_stored(&jwt, root)
        .map_err(|error| format!("the stored {label} JWT: {error}"))?;
    Ok(Signed {
        jwt: jwt.trim().to_string(),
        claims,
    })
}

fn apply(flags: &Flags) -> Result<Value, String> {
    flags.only(&[
        "nats-dir",
        "root-pub",
        "operator-input",
        "operator-sig",
        "sysaccount-input",
        "sysaccount-sig",
        "port",
    ])?;
    let nats_dir = flags.nats_dir()?;
    let root = public_key_hex(flags, "root-pub", NkeyRole::Operator)?;
    let port = match flags.optional("port") {
        None => DEFAULT_PORT,
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("--port {port} is not a port between 1 and 65535"))?,
    };

    // Every check comes before the first write.
    let operator = signed_from_flags(
        flags,
        Payload::Operator,
        "operator-input",
        "operator-sig",
        &root,
    )?;
    let system = signed_from_flags(
        flags,
        Payload::SystemAccount,
        "sysaccount-input",
        "sysaccount-sig",
        &root,
    )?;
    let system_account_path = nats_dir.join(SYSTEM_ACCOUNT_FILE);
    let system_account = read_optional(&system_account_path)?
        .map(|text| text.trim().to_string())
        .filter(|id| is_account_key(id))
        .ok_or_else(|| {
            format!(
                "{} is absent or not an account key; run install-plan first",
                system_account_path.display()
            )
        })?;
    let operator_path = nats_dir.join(OPERATOR_JWT_FILE);
    let operator = signed_or_stored(
        operator,
        read_optional(&operator_path)?,
        Payload::Operator,
        &root,
    )?;
    let system = signed_or_stored(
        system,
        stored_system_account_jwt(&nats_dir, &system_account)?,
        Payload::SystemAccount,
        &root,
    )?;
    payload::check_shape(Payload::Operator, &operator.claims, &root, &system_account)?;
    payload::check_shape(
        Payload::SystemAccount,
        &system.claims,
        &root,
        &system_account,
    )?;

    let conf_path = nats_dir.join(SERVER_CONF_FILE);
    let jwt_dir = nats_dir.join(JWT_DIR);
    let server_conf = ServerConf {
        port,
        js_dir: &nats_dir.join(JS_DIR),
        operator_jwt: &operator_path,
        jwt_dir: &jwt_dir,
        system_account: &system_account,
        system_account_jwt: &system.jwt,
    };
    let url = server_conf.url();
    check_loopback_url(&url)?;
    let rendered = server_conf.render()?;
    let writes = [(&operator_path, &operator.jwt), (&conf_path, &rendered)];
    for (path, content) in writes {
        seeds::refuse_seed_in_content(path, content)?;
    }

    // The resolver directory is created empty and never touched again: the server stores
    // the preload there at start, and runtime claims updates land there too.
    if !jwt_dir.is_dir() {
        fs::create_dir(&jwt_dir)
            .map_err(|error| format!("create {}: {error}", jwt_dir.display()))?;
    }
    for (path, content) in writes {
        write_atomic(path, content.as_bytes())?;
    }
    seeds::refuse_seeds_in_files(&[
        operator_path.clone(),
        conf_path.clone(),
        system_account_path,
    ])?;

    Ok(json!({
        "status": "applied",
        "server_conf": conf_path.display().to_string(),
        "env": {
            NATS_URL_ENV: url,
            OPERATOR_JWT_ENV: operator_path.display().to_string(),
            SYSTEM_ACCOUNT_ENV: system_account,
        },
    }))
}
