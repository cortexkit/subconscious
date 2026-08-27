//! Producer-side golden vectors for prefrontal-signed agent assertion tokens.
//!
//! The corpus is deliberately signed and verified here so a malformed fixture cannot
//! become a shared source of disagreement between independent verifier sites.

use std::{cmp::Ordering, error::Error, fmt};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const VERIFY_NOW: i64 = 1_800_000_000;

/// This fixed 32-byte private key is a conformance fixture, not a secret. It must
/// never be reused for a production prefrontal signing key.
pub const FIXTURE_SIGNING_KEY_BYTES: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

const REQUIRED_CLAIMS: [&str; 10] = [
    "v",
    "agent_id",
    "surface",
    "handle",
    "installation_id",
    "scope",
    "binding_generation",
    "jti",
    "iat",
    "exp",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expectation {
    Valid,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    InvalidSignature,
    ClaimsNotObject,
    MissingClaim,
    UnknownClaim,
    VType,
    VMismatch,
    AgentIdType,
    SurfaceType,
    HandleType,
    InstallationIdType,
    ScopeType,
    BindingGenerationType,
    JtiType,
    IatType,
    ExpType,
    Expired,
    FutureIat,
    EmptyScope,
    ScopeMismatch,
    SurfaceMismatch,
}

impl RefusalReason {
    pub const ALL: [Self; 20] = [
        Self::InvalidSignature,
        Self::ClaimsNotObject,
        Self::MissingClaim,
        Self::UnknownClaim,
        Self::VType,
        Self::VMismatch,
        Self::AgentIdType,
        Self::SurfaceType,
        Self::HandleType,
        Self::InstallationIdType,
        Self::ScopeType,
        Self::BindingGenerationType,
        Self::JtiType,
        Self::IatType,
        Self::ExpType,
        Self::Expired,
        Self::FutureIat,
        Self::EmptyScope,
        Self::ScopeMismatch,
        Self::SurfaceMismatch,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSignature => "invalid_signature",
            Self::ClaimsNotObject => "claims_not_object",
            Self::MissingClaim => "missing_claim",
            Self::UnknownClaim => "unknown_claim",
            Self::VType => "v_type",
            Self::VMismatch => "v_mismatch",
            Self::AgentIdType => "agent_id_type",
            Self::SurfaceType => "surface_type",
            Self::HandleType => "handle_type",
            Self::InstallationIdType => "installation_id_type",
            Self::ScopeType => "scope_type",
            Self::BindingGenerationType => "binding_generation_type",
            Self::JtiType => "jti_type",
            Self::IatType => "iat_type",
            Self::ExpType => "exp_type",
            Self::Expired => "expired",
            Self::FutureIat => "future_iat",
            Self::EmptyScope => "empty_scope",
            Self::ScopeMismatch => "scope_mismatch",
            Self::SurfaceMismatch => "surface_mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusMeta {
    pub vector_set_version: u8,
    pub claim_shape_v: u8,
    pub pubkey_hex: String,
    pub verify_now: i64,
    pub reasons: Vec<RefusalReason>,
    pub rule_3_scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyContext {
    pub surface: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vector {
    pub name: String,
    pub expect: Expectation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<RefusalReason>,
    pub verify_context: VerifyContext,
    pub claims: Value,
    pub signature_hex: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Corpus {
    pub meta: CorpusMeta,
    pub vectors: Vec<Vector>,
}

pub fn fixture_signing_key() -> SigningKey {
    SigningKey::from_bytes(&FIXTURE_SIGNING_KEY_BYTES)
}

pub fn fixture_public_key_hex() -> String {
    hex::encode(fixture_signing_key().verifying_key().as_bytes())
}

/// The vector claims use only strings, safe JSON integers, arrays, and objects.
///
/// This deliberately rejects floating-point numbers rather than pretending to be a
/// general-purpose JCS implementation. RFC 8785's ECMAScript number formatting is
/// outside the claim domain, while rejecting it makes an accidental domain expansion
/// fail loudly instead of generating cross-language signatures with undefined bytes.
pub fn canonicalize(claims: &Value) -> Result<Vec<u8>, CanonicalizationError> {
    let mut output = Vec::new();
    write_canonical(claims, &mut output)?;
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalizationError {
    NonIntegerNumber,
    IntegerOutOfRange,
    StringSerialization(String),
}

impl fmt::Display for CanonicalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonIntegerNumber => {
                formatter.write_str("JCS claim canonicalization rejects non-integer numbers")
            }
            Self::IntegerOutOfRange => formatter.write_str(
                "JCS claim canonicalization rejects integers outside the ECMAScript safe range",
            ),
            Self::StringSerialization(error) => {
                write!(formatter, "JSON string serialization failed: {error}")
            }
        }
    }
}

impl Error for CanonicalizationError {}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), CanonicalizationError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::String(string) => write_json_string(string, output)?,
        Value::Number(number) => write_json_integer(number, output)?,
        Value::Array(entries) => {
            output.push(b'[');
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(entry, output)?;
            }
            output.push(b']');
        }
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by(|(left, _), (right, _)| utf16_code_unit_cmp(left, right));
            output.push(b'{');
            for (index, (name, entry)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_json_string(name, output)?;
                output.push(b':');
                write_canonical(entry, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn write_json_string(value: &str, output: &mut Vec<u8>) -> Result<(), CanonicalizationError> {
    let serialized = serde_json::to_string(value)
        .map_err(|error| CanonicalizationError::StringSerialization(error.to_string()))?;
    output.extend_from_slice(serialized.as_bytes());
    Ok(())
}

fn write_json_integer(
    value: &serde_json::Number,
    output: &mut Vec<u8>,
) -> Result<(), CanonicalizationError> {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

    if let Some(integer) = value.as_i64() {
        if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&integer) {
            return Err(CanonicalizationError::IntegerOutOfRange);
        }
        output.extend_from_slice(integer.to_string().as_bytes());
        return Ok(());
    }
    if let Some(integer) = value.as_u64() {
        if integer > MAX_SAFE_INTEGER as u64 {
            return Err(CanonicalizationError::IntegerOutOfRange);
        }
        output.extend_from_slice(integer.to_string().as_bytes());
        return Ok(());
    }
    Err(CanonicalizationError::NonIntegerNumber)
}

fn utf16_code_unit_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

pub fn verify(
    claims: &Value,
    signature_hex: &str,
    public_key: &VerifyingKey,
    verify_now: i64,
    context: &VerifyContext,
) -> Result<(), RefusalReason> {
    let signature_bytes =
        hex::decode(signature_hex).map_err(|_| RefusalReason::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| RefusalReason::InvalidSignature)?;
    let canonical = canonicalize(claims).map_err(|_| RefusalReason::InvalidSignature)?;
    public_key
        .verify(&canonical, &signature)
        .map_err(|_| RefusalReason::InvalidSignature)?;

    let object = claims.as_object().ok_or(RefusalReason::ClaimsNotObject)?;
    for required in REQUIRED_CLAIMS {
        if !object.contains_key(required) {
            return Err(RefusalReason::MissingClaim);
        }
    }
    if object
        .keys()
        .any(|key| !REQUIRED_CLAIMS.contains(&key.as_str()))
    {
        return Err(RefusalReason::UnknownClaim);
    }

    if !is_json_integer(object, "v") {
        return Err(RefusalReason::VType);
    }
    if object.get("v").and_then(Value::as_i64) != Some(1) {
        return Err(RefusalReason::VMismatch);
    }
    if !is_json_string(object, "agent_id") {
        return Err(RefusalReason::AgentIdType);
    }
    if !is_json_string(object, "surface") {
        return Err(RefusalReason::SurfaceType);
    }
    if !is_json_string(object, "handle") {
        return Err(RefusalReason::HandleType);
    }
    if !is_json_integer(object, "installation_id") {
        return Err(RefusalReason::InstallationIdType);
    }
    if !is_json_integer(object, "binding_generation") {
        return Err(RefusalReason::BindingGenerationType);
    }
    if !is_json_string(object, "jti") {
        return Err(RefusalReason::JtiType);
    }
    if !is_json_integer(object, "iat") {
        return Err(RefusalReason::IatType);
    }
    if !is_json_integer(object, "exp") {
        return Err(RefusalReason::ExpType);
    }

    let scope = object
        .get("scope")
        .and_then(Value::as_array)
        .filter(|entries| entries.iter().all(Value::is_string))
        .ok_or(RefusalReason::ScopeType)?;
    if scope.is_empty() {
        return Err(RefusalReason::EmptyScope);
    }

    let exp = object
        .get("exp")
        .and_then(Value::as_i64)
        .expect("checked integer");
    if exp <= verify_now {
        return Err(RefusalReason::Expired);
    }
    let iat = object
        .get("iat")
        .and_then(Value::as_i64)
        .expect("checked integer");
    if iat > verify_now + 60 {
        return Err(RefusalReason::FutureIat);
    }

    if !scope
        .iter()
        .any(|entry| entry.as_str() == Some(&context.action))
    {
        return Err(RefusalReason::ScopeMismatch);
    }
    if object.get("surface").and_then(Value::as_str) != Some(&context.surface) {
        return Err(RefusalReason::SurfaceMismatch);
    }

    Ok(())
}

pub fn verify_vector(corpus: &Corpus, vector: &Vector) -> Result<(), RefusalReason> {
    let public_key_bytes =
        hex::decode(&corpus.meta.pubkey_hex).map_err(|_| RefusalReason::InvalidSignature)?;
    let public_key_bytes: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| RefusalReason::InvalidSignature)?;
    let public_key =
        VerifyingKey::from_bytes(&public_key_bytes).map_err(|_| RefusalReason::InvalidSignature)?;
    verify(
        &vector.claims,
        &vector.signature_hex,
        &public_key,
        corpus.meta.verify_now,
        &vector.verify_context,
    )
}

pub fn build_corpus() -> Corpus {
    let signing_key = fixture_signing_key();
    let github_apply = verify_context("github", "manifest.apply");
    let mut vectors = Vec::new();

    let minimal = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "agent-token-minimal",
    );
    vectors.push(signed_vector(
        "a-valid-minimal-realistic",
        Expectation::Valid,
        None,
        github_apply.clone(),
        minimal,
        &signing_key,
    ));

    let full_shape = claims(
        "0042",
        "plexus[bot]",
        vec!["manifest.apply", "issue.comment"],
        "12345",
    );
    vectors.push(signed_vector(
        "b-valid-full-shape-numeric-looking-strings",
        Expectation::Valid,
        None,
        github_apply.clone(),
        full_shape,
        &signing_key,
    ));

    // The claim set is closed, so it cannot add a Unicode field-name witness. The
    // source order is nevertheless deliberately non-JCS; the RFC Unicode ordering
    // witness is proved separately in `rfc_8785_worked_examples` below.
    let jcs_witness = reordered_claims(claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "agent-token-jcs-order",
    ));
    vectors.push(signed_vector(
        "c-valid-jcs-ordering-required",
        Expectation::Valid,
        None,
        github_apply.clone(),
        jcs_witness,
        &signing_key,
    ));

    let signature_source = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "agent-token-signature-source",
    );
    let mut bit_flipped = signed_vector(
        "d-refuse-signature-bit-flipped",
        Expectation::Refuse,
        Some(RefusalReason::InvalidSignature),
        github_apply.clone(),
        signature_source.clone(),
        &signing_key,
    );
    let mut signature = hex::decode(&bit_flipped.signature_hex).expect("generator produces hex");
    signature[0] ^= 0x01;
    bit_flipped.signature_hex = hex::encode(signature);
    vectors.push(bit_flipped);

    let signed_then_mutated = signed_vector(
        "e-refuse-claims-mutated-after-signing",
        Expectation::Refuse,
        Some(RefusalReason::InvalidSignature),
        github_apply.clone(),
        signature_source.clone(),
        &signing_key,
    );
    let mut signed_then_mutated = signed_then_mutated;
    signed_then_mutated
        .claims
        .as_object_mut()
        .expect("claims object")
        .insert("handle".into(), Value::String("forged[bot]".into()));
    vectors.push(signed_then_mutated);

    let other_key = SigningKey::from_bytes(&[0x55; 32]);
    vectors.push(signed_vector(
        "f-refuse-different-signer",
        Expectation::Refuse,
        Some(RefusalReason::InvalidSignature),
        github_apply.clone(),
        signature_source,
        &other_key,
    ));

    vectors.push(type_trap_vector(
        "g-refuse-installation-id-string",
        "installation_id",
        Value::String("12345678".into()),
        RefusalReason::InstallationIdType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "h-refuse-binding-generation-string",
        "binding_generation",
        Value::String("3".into()),
        RefusalReason::BindingGenerationType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "i-refuse-iat-string",
        "iat",
        Value::String((VERIFY_NOW - 10).to_string()),
        RefusalReason::IatType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "j-refuse-exp-string",
        "exp",
        Value::String((VERIFY_NOW + 600).to_string()),
        RefusalReason::ExpType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "k-refuse-v-string",
        "v",
        Value::String("1".into()),
        RefusalReason::VType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "l-refuse-agent-id-number",
        "agent_id",
        Value::from(42),
        RefusalReason::AgentIdType,
        &github_apply,
        &signing_key,
    ));
    vectors.push(type_trap_vector(
        "m-refuse-jti-number",
        "jti",
        Value::from(12345),
        RefusalReason::JtiType,
        &github_apply,
        &signing_key,
    ));

    let mut missing_scope = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "missing-scope",
    );
    missing_scope
        .as_object_mut()
        .expect("claims object")
        .remove("scope");
    vectors.push(signed_vector(
        "n-refuse-missing-required-scope",
        Expectation::Refuse,
        Some(RefusalReason::MissingClaim),
        github_apply.clone(),
        missing_scope,
        &signing_key,
    ));

    let mut unknown_claim = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "unknown-claim",
    );
    unknown_claim
        .as_object_mut()
        .expect("claims object")
        .insert("admin".into(), Value::Bool(true));
    vectors.push(signed_vector(
        "o-refuse-unknown-extra-claim",
        Expectation::Refuse,
        Some(RefusalReason::UnknownClaim),
        github_apply.clone(),
        unknown_claim,
        &signing_key,
    ));

    let mut expired = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "expired-token",
    );
    expired
        .as_object_mut()
        .expect("claims object")
        .insert("exp".into(), Value::from(VERIFY_NOW));
    vectors.push(signed_vector(
        "p-refuse-expired",
        Expectation::Refuse,
        Some(RefusalReason::Expired),
        github_apply.clone(),
        expired,
        &signing_key,
    ));

    let mut future_iat = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "future-iat",
    );
    future_iat
        .as_object_mut()
        .expect("claims object")
        .insert("iat".into(), Value::from(VERIFY_NOW + 61));
    vectors.push(signed_vector(
        "q-refuse-future-iat-beyond-skew",
        Expectation::Refuse,
        Some(RefusalReason::FutureIat),
        github_apply.clone(),
        future_iat,
        &signing_key,
    ));

    let empty_scope = claims("plexus", "plexus[bot]", Vec::new(), "empty-scope");
    vectors.push(signed_vector(
        "r-refuse-empty-scope",
        Expectation::Refuse,
        Some(RefusalReason::EmptyScope),
        github_apply.clone(),
        empty_scope,
        &signing_key,
    ));

    let mut wrong_surface = claims(
        "plexus",
        "plexus[bot]",
        vec!["manifest.apply"],
        "gitlab-surface",
    );
    wrong_surface
        .as_object_mut()
        .expect("claims object")
        .insert("surface".into(), Value::String("gitlab".into()));
    vectors.push(signed_vector(
        "s-refuse-surface-mismatch",
        Expectation::Refuse,
        Some(RefusalReason::SurfaceMismatch),
        github_apply,
        wrong_surface,
        &signing_key,
    ));

    Corpus {
        meta: CorpusMeta {
            vector_set_version: 1,
            claim_shape_v: 1,
            pubkey_hex: fixture_public_key_hex(),
            verify_now: VERIFY_NOW,
            // Reason strings are corpus API: external verifiers map them to local
            // refusal codes, so changing one would silently break their fixtures.
            reasons: RefusalReason::ALL.to_vec(),
            // Rule 3 depends on a verifier-local registry watermark and no fixed
            // corpus can model cold-start registry state without lying about it.
            rule_3_scope:
                "out_of_corpus_scope: requires verifier-local binding_generation watermark state"
                    .into(),
        },
        vectors,
    }
}

pub fn render_corpus() -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&build_corpus()).expect("corpus always serializes");
    bytes.push(b'\n');
    bytes
}

fn verify_context(surface: &str, action: &str) -> VerifyContext {
    VerifyContext {
        surface: surface.into(),
        action: action.into(),
    }
}

fn claims(agent_id: &str, handle: &str, scope: Vec<&str>, jti: &str) -> Value {
    // This is JCS key order so only vector (c) is sensitive to a naive serializer.
    let mut object = Map::new();
    object.insert("agent_id".into(), Value::String(agent_id.into()));
    object.insert("binding_generation".into(), Value::from(3));
    object.insert("exp".into(), Value::from(VERIFY_NOW + 600));
    object.insert("handle".into(), Value::String(handle.into()));
    object.insert("iat".into(), Value::from(VERIFY_NOW - 10));
    object.insert("installation_id".into(), Value::from(12_345_678));
    object.insert("jti".into(), Value::String(jti.into()));
    object.insert(
        "scope".into(),
        Value::Array(
            scope
                .into_iter()
                .map(|entry| Value::String(entry.into()))
                .collect(),
        ),
    );
    object.insert("surface".into(), Value::String("github".into()));
    object.insert("v".into(), Value::from(1));
    Value::Object(object)
}

fn reordered_claims(claims: Value) -> Value {
    let source = claims.as_object().expect("claims object");
    let mut reordered = Map::new();
    for name in [
        "surface",
        "scope",
        "v",
        "jti",
        "installation_id",
        "handle",
        "exp",
        "iat",
        "binding_generation",
        "agent_id",
    ] {
        reordered.insert(
            name.into(),
            source.get(name).expect("required claim").clone(),
        );
    }
    Value::Object(reordered)
}

fn signed_vector(
    name: &str,
    expect: Expectation,
    reason: Option<RefusalReason>,
    verify_context: VerifyContext,
    claims: Value,
    signing_key: &SigningKey,
) -> Vector {
    let canonical = canonicalize(&claims).expect("claims are JCS-canonicalizable");
    Vector {
        name: name.into(),
        expect,
        reason,
        verify_context,
        claims,
        signature_hex: hex::encode(signing_key.sign(&canonical).to_bytes()),
    }
}

fn type_trap_vector(
    name: &str,
    field: &str,
    defect: Value,
    reason: RefusalReason,
    verify_context: &VerifyContext,
    signing_key: &SigningKey,
) -> Vector {
    let mut defective = claims("plexus", "plexus[bot]", vec!["manifest.apply"], name);
    defective
        .as_object_mut()
        .expect("claims object")
        .insert(field.into(), defect);
    // Type traps are signed after the defect is introduced. Otherwise a verifier
    // would reject the signature first and this vector would prove no type rule.
    signed_vector(
        name,
        Expectation::Refuse,
        Some(reason),
        verify_context.clone(),
        defective,
        signing_key,
    )
}

fn is_json_integer(object: &Map<String, Value>, field: &str) -> bool {
    object
        .get(field)
        .is_some_and(|value| value.is_i64() || value.is_u64())
}

fn is_json_string(object: &Map<String, Value>, field: &str) -> bool {
    object.get(field).is_some_and(Value::is_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/vectors/agent_token_vectors_v1.json"
    ));

    #[test]
    fn corpus_reference_walk_matches_every_named_expectation() {
        let corpus: Corpus = serde_json::from_str(CORPUS).expect("checked-in corpus parses");
        for vector in &corpus.vectors {
            let actual = verify_vector(&corpus, vector);
            match vector.expect {
                Expectation::Valid => assert_eq!(actual, Ok(()), "{}", vector.name),
                Expectation::Refuse => assert_eq!(
                    actual,
                    Err(vector.reason.expect("refusal reason")),
                    "{}",
                    vector.name
                ),
            }
        }
    }

    #[test]
    fn checked_in_corpus_is_current_generator_output() {
        assert_eq!(
            CORPUS.as_bytes(),
            render_corpus(),
            "run `cargo run -p agent-token-vectors --bin generate`"
        );
    }

    #[test]
    fn vector_c_requires_jcs_not_plain_serde_json_ordering() {
        let corpus: Corpus = serde_json::from_str(CORPUS).expect("checked-in corpus parses");
        let vector = corpus
            .vectors
            .iter()
            .find(|vector| vector.name == "c-valid-jcs-ordering-required")
            .expect("vector c");
        assert_ne!(
            canonicalize(&vector.claims).expect("JCS"),
            serde_json::to_vec(&vector.claims).expect("plain JSON"),
            "vector c must distinguish JCS from source-order serde_json serialization"
        );
    }

    #[test]
    fn rule_four_refuses_an_action_outside_the_signed_scope() {
        let corpus: Corpus = serde_json::from_str(CORPUS).expect("checked-in corpus parses");
        let vector = corpus.vectors.first().expect("minimal valid vector");
        let wrong_action = VerifyContext {
            surface: "github".into(),
            action: "issue.comment".into(),
        };
        let public_key = fixture_signing_key().verifying_key();
        assert_eq!(
            verify(
                &vector.claims,
                &vector.signature_hex,
                &public_key,
                corpus.meta.verify_now,
                &wrong_action,
            ),
            Err(RefusalReason::ScopeMismatch)
        );
    }

    #[test]
    fn rfc_8785_worked_examples() {
        fn assert_canonical(value: Value, expected: &str) {
            let observed = String::from_utf8(canonicalize(&value).expect("JCS claim domain"))
                .expect("JCS is UTF-8");
            assert_eq!(
                observed,
                expected,
                "observed UTF-8 bytes: {:02x?}\nexpected UTF-8 bytes: {:02x?}",
                observed.as_bytes(),
                expected.as_bytes(),
            );
        }

        // RFC 8785 §3.2.2's string and literal values from its serialization example.
        assert_canonical(
            Value::String("€$\u{000f}\nA'B\"\\\\\"/".into()),
            r#""€$\u000f\nA'B\"\\\\\"/""#,
        );
        assert_canonical(
            Value::Array(vec![Value::Null, Value::Bool(true), Value::Bool(false)]),
            "[null,true,false]",
        );

        // RFC 8785 §3.2.3's Unicode member-name sorting example.
        let sorting_example: Value = serde_json::from_str(
            r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\ufb33":"Hebrew Letter Dalet With Dagesh","1":"One","\ud83d\ude00":"Emoji: Grinning Face","\u0080":"Control","\u00f6":"Latin Small Letter O With Diaeresis"}"#,
        )
        .expect("RFC example parses");
        assert_canonical(
            sorting_example,
            r#"{"\r":"Carriage Return","1":"One","":"Control","ö":"Latin Small Letter O With Diaeresis","€":"Euro Sign","😀":"Emoji: Grinning Face","דּ":"Hebrew Letter Dalet With Dagesh"}"#,
        );

        assert_eq!(
            canonicalize(&serde_json::json!(1.5)),
            Err(CanonicalizationError::NonIntegerNumber),
            "the corpus canonicalizer must refuse numbers outside its integer claim domain"
        );
    }
}
