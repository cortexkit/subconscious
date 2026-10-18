//! User-JWT assertions over the shared private broker implementation.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
pub use cortexkit_bus_testkit::trust::unix_now;
use nkeys::KeyPair;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub fn nats_server_bin() -> Result<(PathBuf, String), (&'static str, String)> {
    cortexkit_bus_testkit::nats_server_bin().map_err(|error| {
        let gate = if matches!(error, cortexkit_bus_testkit::RigError::MissingBinary { .. }) {
            "nats-server-absent"
        } else {
            "nats-server-too-old"
        };
        (gate, error.to_string())
    })
}

pub struct NatsFixture {
    pub dir: PathBuf,
    pub log: PathBuf,
    pub url: String,
    pub version: String,
    pub account_public: String,
    pub trusted_issuers: BTreeSet<String>,
    server: cortexkit_bus_testkit::BusServer,
}
impl NatsFixture {
    pub async fn start(bin: &Path, version: String, dir: &Path, account_signing_key: &str) -> Self {
        let account_public = KeyPair::new_account().public_key();
        let server = cortexkit_bus_testkit::BusServer::start(cortexkit_bus_testkit::BusOptions {
            binary: Some(bin.to_path_buf()),
            directory: Some(dir.to_path_buf()),
            preload: vec![cortexkit_bus_testkit::PreloadedAccount {
                public_key: account_public.clone(),
                signing_keys: vec![account_signing_key.to_string()],
            }],
            ..Default::default()
        })
        .await
        .expect("private NATS server");
        Self {
            dir: server.dir.clone(),
            log: server.log.clone(),
            url: server.url.clone(),
            version,
            account_public,
            trusted_issuers: BTreeSet::from([account_signing_key.to_string()]),
            server,
        }
    }
    /// Fails the run unless `jwt` is signed, under its own `iss`, by an issuer the harness
    /// wrote into this server's config. An arm that authenticates with anything else
    /// would prove nothing about the signer.
    pub fn assert_harness_signed(&self, jwt: &str) {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT has three parts");
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[1])
                .expect("JWT claims are base64url"),
        )
        .expect("JWT claims are JSON");
        let issuer = claims["iss"].as_str().expect("JWT names its issuer");
        assert!(
            self.trusted_issuers.contains(issuer),
            "JWT issuer {issuer} is not a key the harness wrote into the server config"
        );
        let signature = URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("JWT signature is base64url");
        KeyPair::from_public_key(issuer)
            .expect("issuer is an nkey")
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the JWT signature must verify under its harness-written issuer");
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub async fn stop(self) {
        self.server.stop().await;
    }
}
