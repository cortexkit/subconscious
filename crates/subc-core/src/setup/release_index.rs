use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use hex_literal::hex;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Generation 1 verifying key for the CortexKit release index.
///
/// A later generation ships in a new `ck` before the index worker switches
/// keys; this binary will not accept a document signed by any other key.
pub const RELEASE_INDEX_PUBKEY: [u8; 32] =
    hex!("abee088dcc7cc2a0ddcf979ae75b58437fb50c8a0f931306b6f9db055d676897");

/// Key generation expected by this `ck` when verifying `index.json.sig`.
pub const RELEASE_INDEX_KEY_GENERATION: u32 = 1;

pub const DEFAULT_RELEASE_INDEX_URL: &str = "https://cortexkit.io/releases/v1/index.json";

/// An index older than this is refused: the worker regenerates daily, so a
/// stale document means the ingester is down, not that nothing was released.
pub const INDEX_FRESHNESS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The signed release document CortexKit publishes. Unknown fields are
/// ignored so the index can grow additively without breaking older `ck`.
#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseIndex {
    #[allow(dead_code)]
    pub schema: u32,
    #[allow(dead_code)]
    pub channel: String,
    pub generated_at_ms: u64,
    #[serde(default)]
    pub components: BTreeMap<String, IndexComponent>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexComponent {
    pub release: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, BTreeMap<String, IndexAsset>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexAsset {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub reports: Option<String>,
}

/// Typed refusals for a signed index. None of them install anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexRefusal {
    Unreachable { url: String, reason: String },
    SignatureInvalid { url: String, key_generation: u32 },
    Malformed { url: String, reason: String },
    Stale { url: String, generated_at_ms: u64 },
}

impl fmt::Display for IndexRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { url, reason } => {
                write!(formatter, "index_unreachable: {url}: {reason}")
            }
            Self::SignatureInvalid {
                url,
                key_generation,
            } => write!(
                formatter,
                "index_signature_invalid: {url} (expected key generation {key_generation})"
            ),
            Self::Malformed { url, reason } => {
                write!(formatter, "index_malformed: {url}: {reason}")
            }
            Self::Stale {
                url,
                generated_at_ms,
            } => write!(
                formatter,
                "index_stale: {url} (generated_at_ms={generated_at_ms})"
            ),
        }
    }
}

impl std::error::Error for IndexRefusal {}

pub fn index_url() -> String {
    std::env::var("CK_RELEASE_INDEX_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_INDEX_URL.to_string())
}

/// Download `index.json` in one request, read the Ed25519 signature from the
/// `X-CortexKit-Signature-Ed25519` response header, verify it against the
/// embedded generation-1 key, parse, and refuse a stale document.
pub fn fetch_index(url: &str, deadline: Duration) -> Result<ReleaseIndex, IndexRefusal> {
    // This gate keeps the environment-supplied fixture key out of the shipped
    // verifier. Reading it in production would let runtime configuration replace
    // the embedded key that authenticates the release index.
    #[cfg(feature = "test-support")]
    if let Some(key) = test_release_index_key() {
        return fetch_index_with_verifying_key(url, &key, RELEASE_INDEX_KEY_GENERATION, deadline);
    }
    fetch_index_with_verifying_key(
        url,
        &RELEASE_INDEX_PUBKEY,
        RELEASE_INDEX_KEY_GENERATION,
        deadline,
    )
}

#[cfg(feature = "test-support")]
fn test_release_index_key() -> Option<[u8; 32]> {
    let encoded = std::env::var("CK_TEST_RELEASE_INDEX_PUBKEY").ok()?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    decoded.try_into().ok()
}

/// How long an installer may wait for the index. Generous: an install is an
/// attended operation, and a slow link is not a reason to refuse it.
pub const INSTALL_INDEX_DEADLINE: Duration = Duration::from_secs(30);

/// Same resolver `ck` uses, with an injected verifying key so tests can
/// exercise the accept path without the production private key.
pub fn fetch_index_with_verifying_key(
    url: &str,
    pubkey: &[u8; 32],
    key_generation: u32,
    deadline: Duration,
) -> Result<ReleaseIndex, IndexRefusal> {
    fetch_index_at(url, pubkey, key_generation, unix_now_ms(), deadline)
}

const SIGNATURE_HEADER: &str = "X-CortexKit-Signature-Ed25519";

fn fetch_index_at(
    url: &str,
    pubkey: &[u8; 32],
    key_generation: u32,
    now_ms: u64,
    deadline: Duration,
) -> Result<ReleaseIndex, IndexRefusal> {
    let index_path = temporary_path("index.json");
    let header_path = temporary_path("index.headers");
    let signature = match download_index(url, &index_path, &header_path, deadline) {
        Ok(signature) => signature,
        Err(reason) => {
            let _ = fs::remove_file(&index_path);
            let _ = fs::remove_file(&header_path);
            return Err(IndexRefusal::Unreachable {
                url: url.to_string(),
                reason,
            });
        }
    };
    let index_bytes = match fs::read(&index_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = fs::remove_file(&index_path);
            let _ = fs::remove_file(&header_path);
            return Err(IndexRefusal::Unreachable {
                url: url.to_string(),
                reason: error.to_string(),
            });
        }
    };
    let _ = fs::remove_file(&index_path);
    let _ = fs::remove_file(&header_path);
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return Err(IndexRefusal::SignatureInvalid {
            url: url.to_string(),
            key_generation,
        });
    };
    verify_and_parse(
        &index_bytes,
        signature.trim(),
        url,
        pubkey,
        key_generation,
        now_ms,
    )
}

/// The transport enforces `deadline` itself. A caller that merely stops
/// waiting (the dashboard's 800 ms budget) would leave the child fetching a
/// hanging source until the source gives up; on Windows a child that
/// outlives `ck` also keeps the inherited stdio handles open, so whoever ran
/// `ck` waits with it. Bounding the child bounds everything downstream.
fn download_index(
    url: &str,
    body_path: &Path,
    header_path: &Path,
    deadline: Duration,
) -> Result<Option<String>, String> {
    let body = body_path.to_string_lossy().into_owned();
    let headers = header_path.to_string_lossy().into_owned();
    // curl takes fractional seconds; Invoke-WebRequest takes whole seconds
    // and treats 0 as infinite, so the floor is one second there.
    let curl_seconds = format!("{:.3}", deadline.as_secs_f64().max(0.001));
    let powershell_seconds = deadline.as_secs().max(1);
    let (program, args) = if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!(
                    "$r = Invoke-WebRequest -Uri '{url}' -OutFile '{body}' -PassThru -UseBasicParsing -TimeoutSec {powershell_seconds}; $lines = @(); foreach ($key in $r.Headers.Keys) {{ $lines += \"${{key}}: $($r.Headers[$key])\" }}; Set-Content -LiteralPath '{headers}' -Value ($lines -join \"`n\")"
                ),
            ],
        )
    } else {
        (
            "curl",
            vec![
                "--fail".to_string(),
                "--location".to_string(),
                "--silent".to_string(),
                "--show-error".to_string(),
                "--max-time".to_string(),
                curl_seconds,
                "--dump-header".to_string(),
                headers,
                "--output".to_string(),
                body,
                url.to_string(),
            ],
        )
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(format!("could not download {url}: {stderr} {stdout}"));
    }
    let header_text = fs::read_to_string(header_path).unwrap_or_default();
    Ok(signature_from_dump_header(&header_text))
}

/// Last matching header wins so a redirected dump still yields the final
/// response's signature rather than an intermediate hop's.
fn signature_from_dump_header(contents: &str) -> Option<String> {
    let mut found = None;
    for line in contents.lines() {
        let line = line.trim();
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case(SIGNATURE_HEADER) {
            found = Some(value.trim().to_string());
        }
    }
    found.filter(|value| !value.is_empty())
}

fn verify_and_parse(
    index_bytes: &[u8],
    signature_base64: &str,
    url: &str,
    pubkey: &[u8; 32],
    key_generation: u32,
    now_ms: u64,
) -> Result<ReleaseIndex, IndexRefusal> {
    let signature_invalid = || IndexRefusal::SignatureInvalid {
        url: url.to_string(),
        key_generation,
    };
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_base64.as_bytes())
        .map_err(|_| signature_invalid())?;
    let signature_array: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| signature_invalid())?;
    let signature = Signature::from_bytes(&signature_array);
    let verifying_key = VerifyingKey::from_bytes(pubkey).map_err(|_| signature_invalid())?;
    verifying_key
        .verify_strict(index_bytes, &signature)
        .map_err(|_| signature_invalid())?;

    let index: ReleaseIndex =
        serde_json::from_slice(index_bytes).map_err(|error| IndexRefusal::Malformed {
            url: url.to_string(),
            reason: error.to_string(),
        })?;
    if now_ms.saturating_sub(index.generated_at_ms) > INDEX_FRESHNESS.as_millis() as u64 {
        return Err(IndexRefusal::Stale {
            url: url.to_string(),
            generated_at_ms: index.generated_at_ms,
        });
    }
    Ok(index)
}

pub(super) fn download(url: &str, destination: &Path) -> Result<(), String> {
    let destination = destination.to_string_lossy().into_owned();
    let (program, args) = if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Invoke-WebRequest -Uri '{url}' -OutFile '{destination}' -UseBasicParsing"),
            ],
        )
    } else {
        (
            "curl",
            vec![
                "--fail".to_string(),
                "--location".to_string(),
                "--silent".to_string(),
                "--show-error".to_string(),
                "--output".to_string(),
                destination,
                url.to_string(),
            ],
        )
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(format!("could not download {url}: {stderr} {stdout}"))
    }
}

fn temporary_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ck-setup-{name}-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos()
    ))
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x42; 32])
    }

    fn test_pubkey(key: &SigningKey) -> [u8; 32] {
        key.verifying_key().to_bytes()
    }

    fn sign_b64(key: &SigningKey, bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(key.sign(bytes).to_bytes())
    }

    fn fresh_ms() -> u64 {
        unix_now_ms()
    }

    fn core_index(generated_at_ms: u64) -> serde_json::Value {
        let sha = "ab".repeat(32);
        let asset = |name: &str, reports: Option<&str>| {
            json!({
                "url": format!("http://127.0.0.1/{name}.zip"),
                "sha256": sha,
                "bytes": 1,
                "reports": reports,
            })
        };
        let target_assets = json!({
            "ck": asset("ck", Some("0.16.0")),
            "ck-subc": asset("ck-subc", Some("0.16.0")),
            "ck-subc-mcp": asset("ck-subc-mcp", None),
        });
        json!({
            "schema": 1,
            "channel": "alpha",
            "generated_at_ms": generated_at_ms,
            "components": {
                "core": {
                    "repository": "cortexkit/subconscious",
                    "release": "subc-core-v0.16.0",
                    "published_at_ms": 1_788_400_000_000_u64,
                    "version": "0.16.0",
                    "train": null,
                    "assets": {
                        "darwin-arm64": target_assets.clone(),
                        "linux-x64": target_assets.clone(),
                        "windows-x64": target_assets,
                    }
                }
            }
        })
    }

    fn parse_with(
        bytes: &[u8],
        signature: &str,
        pubkey: &[u8; 32],
        now_ms: u64,
    ) -> Result<ReleaseIndex, IndexRefusal> {
        verify_and_parse(
            bytes,
            signature,
            "https://cortexkit.io/releases/v1/index.json",
            pubkey,
            RELEASE_INDEX_KEY_GENERATION,
            now_ms,
        )
    }

    struct Served {
        body: Vec<u8>,
        extra_headers: Vec<(String, String)>,
    }

    fn spawn_http(files: BTreeMap<String, Served>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            listener.set_nonblocking(false).expect("blocking listener");
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0_u8; 4096];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/");
                if let Some(served) = files.get(path) {
                    let mut header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n",
                        served.body.len()
                    );
                    for (name, value) in &served.extra_headers {
                        header.push_str(&format!("{name}: {value}\r\n"));
                    }
                    header.push_str("\r\n");
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&served.body);
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                }
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn valid_index_is_accepted() {
        let key = test_signing_key();
        let bytes = serde_json::to_vec(&core_index(fresh_ms())).unwrap();
        let index = parse_with(
            &bytes,
            &sign_b64(&key, &bytes),
            &test_pubkey(&key),
            fresh_ms(),
        )
        .expect("valid signed index");
        assert_eq!(index.schema, 1);
        assert_eq!(index.channel, "alpha");
        assert!(index.components.contains_key("core"));
    }

    #[test]
    fn tampered_byte_is_signature_invalid() {
        let key = test_signing_key();
        let mut bytes = serde_json::to_vec(&core_index(fresh_ms())).unwrap();
        let signature = sign_b64(&key, &bytes);
        bytes[0] ^= 0x01;
        let error = parse_with(&bytes, &signature, &test_pubkey(&key), fresh_ms()).unwrap_err();
        assert!(
            matches!(
                error,
                IndexRefusal::SignatureInvalid {
                    key_generation: 1,
                    ..
                }
            ),
            "{error}"
        );
        assert!(
            error.to_string().contains("index_signature_invalid"),
            "{error}"
        );
    }

    #[test]
    fn missing_signature_header_is_signature_invalid() {
        let key = test_signing_key();
        let bytes = serde_json::to_vec(&core_index(fresh_ms())).unwrap();
        let base = spawn_http(BTreeMap::from([(
            "/index.json".to_string(),
            Served {
                body: bytes,
                extra_headers: Vec::new(),
            },
        )]));
        let error = fetch_index_with_verifying_key(
            &format!("{base}/index.json"),
            &test_pubkey(&key),
            RELEASE_INDEX_KEY_GENERATION,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                IndexRefusal::SignatureInvalid {
                    key_generation: 1,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn eight_day_old_index_is_stale() {
        let key = test_signing_key();
        let now = fresh_ms();
        let generated_at_ms = now - (8 * 24 * 60 * 60 * 1000);
        let bytes = serde_json::to_vec(&core_index(generated_at_ms)).unwrap();
        let error =
            parse_with(&bytes, &sign_b64(&key, &bytes), &test_pubkey(&key), now).unwrap_err();
        assert!(
            matches!(error, IndexRefusal::Stale { generated_at_ms: ts, .. } if ts == generated_at_ms),
            "{error}"
        );
        assert!(error.to_string().contains("index_stale"), "{error}");
    }

    #[test]
    fn unparsable_payload_is_malformed() {
        let key = test_signing_key();
        let bytes = b"not json";
        let error = parse_with(
            bytes,
            &sign_b64(&key, bytes),
            &test_pubkey(&key),
            fresh_ms(),
        )
        .unwrap_err();
        assert!(matches!(error, IndexRefusal::Malformed { .. }), "{error}");
        assert!(error.to_string().contains("index_malformed"), "{error}");
    }

    #[test]
    fn http_fetch_accepts_a_test_signed_index() {
        let key = test_signing_key();
        let bytes = serde_json::to_vec(&core_index(fresh_ms())).unwrap();
        let signature = sign_b64(&key, &bytes);
        let base = spawn_http(BTreeMap::from([(
            "/index.json".to_string(),
            Served {
                body: bytes,
                extra_headers: vec![(SIGNATURE_HEADER.to_string(), signature)],
            },
        )]));
        let index = fetch_index_with_verifying_key(
            &format!("{base}/index.json"),
            &test_pubkey(&key),
            RELEASE_INDEX_KEY_GENERATION,
            Duration::from_secs(10),
        )
        .expect("signed fixture");
        assert_eq!(index.components["core"].release, "subc-core-v0.16.0");
    }

    /// A valid Ed25519 signature over `{"schema":1}` made by the RFC 8032
    /// test key, not the production index key. Proves this binary verifies
    /// against the embedded generation-1 public key rather than accepting
    /// any well-formed signature.
    #[test]
    fn embedded_production_key_refuses_a_hardcoded_foreign_signature() {
        let message = b"{\"schema\":1}";
        let signature =
            "fjYZ87Tka7M+yJ+lmjD7vjSjflypCGi2KIvmSktgssO79FN8/mntGhobmTCwYDeQRAEAu7oDdv7zrAkI9N9uDA==";
        let error = verify_and_parse(
            message,
            signature,
            "https://cortexkit.io/releases/v1/index.json",
            &RELEASE_INDEX_PUBKEY,
            RELEASE_INDEX_KEY_GENERATION,
            fresh_ms(),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                IndexRefusal::SignatureInvalid {
                    key_generation: 1,
                    ..
                }
            ),
            "{error}"
        );
        let rendered = error.to_string();
        assert!(rendered.contains("index_signature_invalid"), "{rendered}");
        assert!(rendered.contains("generation 1"), "{rendered}");
    }

    /// The transport, not the caller, owns the deadline: a source that
    /// accepts and then never answers must produce `index_unreachable`
    /// within the deadline, with the fetch child gone. A caller that merely
    /// stops awaiting leaves the child running against the hang — and on
    /// Windows a child that outlives `ck` holds its inherited stdio open,
    /// so whoever ran `ck` waits the whole hang with it. This is the CI
    /// failure that made the dashboard's 800 ms budget read as 10 s.
    #[test]
    fn a_hanging_source_refuses_within_the_transport_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // Hold the connection open, answering nothing, until released.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            drop(stream);
        });
        let key = test_signing_key();
        let started = std::time::Instant::now();
        let error = fetch_index_with_verifying_key(
            &format!("http://{addr}/index.json"),
            &test_pubkey(&key),
            RELEASE_INDEX_KEY_GENERATION,
            Duration::from_millis(800),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        assert!(
            matches!(error, IndexRefusal::Unreachable { .. }),
            "a hang is unreachable, never a signature or parse verdict: {error}"
        );
        // Discriminating margin, not a latency claim: a transport that ignores
        // its deadline cannot return before the 30 s hold. One that honors it
        // is back in about a second on unix; on Windows the bound starts only
        // after powershell.exe itself has started, which on a cold runner is
        // several seconds on its own.
        assert!(
            elapsed < Duration::from_secs(20),
            "transport ignored its deadline: {elapsed:?}"
        );
    }

    #[test]
    fn index_url_override_is_the_only_env_knob() {
        let original = std::env::var("CK_RELEASE_INDEX_URL").ok();
        std::env::set_var("CK_RELEASE_INDEX_URL", "http://127.0.0.1:9/index.json");
        assert_eq!(index_url(), "http://127.0.0.1:9/index.json");
        std::env::remove_var("CK_RELEASE_INDEX_URL");
        assert_eq!(index_url(), DEFAULT_RELEASE_INDEX_URL);
        if let Some(value) = original {
            std::env::set_var("CK_RELEASE_INDEX_URL", value);
        }
    }
}
