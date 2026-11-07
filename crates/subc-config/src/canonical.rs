use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fmt;

/// Domain purposes are fixed protocol strings, not caller-selected labels.
pub const PURPOSE_VALUE: &str = "value";
pub const PURPOSE_SCHEMA: &str = "schema";
pub const PURPOSE_DECLARATION: &str = "declaration";
pub const PURPOSE_REVIEW: &str = "review";
pub const PURPOSE_APPLY_PLAN: &str = "apply-plan";
pub const PURPOSE_PLAN: &str = "plan";
pub const PURPOSE_CHANGESET: &str = "changeset";
pub const PURPOSE_GENERATION: &str = "generation";
pub const PURPOSE_SNAPSHOT: &str = "snapshot";
pub const DIGEST_VERSION: u32 = 1;
const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CanonicalError {
    NumberNotLossless,
    InvalidDigest,
    UnknownPurpose,
    Serialization,
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CanonicalError {}

/// A full SHA-256, encoded as `sha256:` followed by 64 lowercase hex characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub struct Digest(String);

impl Digest {
    pub fn new(encoded: impl Into<String>) -> Result<Self, CanonicalError> {
        let encoded = encoded.into();
        let hex = encoded
            .strip_prefix("sha256:")
            .ok_or(CanonicalError::InvalidDigest)?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CanonicalError::InvalidDigest);
        }
        Ok(Self(encoded))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Digest {
    type Error = CanonicalError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}
impl From<Digest> for String {
    fn from(d: Digest) -> Self {
        d.0
    }
}
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Exact file bytes use plain SHA-256, independently of parsed-value envelopes.
pub fn raw_digest(bytes: &[u8]) -> Digest {
    let hash = Sha256::digest(bytes);
    let mut text = String::from("sha256:");
    for byte in hash {
        use std::fmt::Write;
        write!(text, "{byte:02x}").expect("writing a String");
    }
    Digest(text)
}

/// SHA-256 of the RFC 8785 envelope `{domain, version, value}`.
pub fn digest(purpose: &str, value: &Value) -> Result<Digest, CanonicalError> {
    if ![
        PURPOSE_VALUE,
        PURPOSE_SCHEMA,
        PURPOSE_DECLARATION,
        PURPOSE_REVIEW,
        PURPOSE_APPLY_PLAN,
        PURPOSE_PLAN,
        PURPOSE_CHANGESET,
        PURPOSE_GENERATION,
        PURPOSE_SNAPSHOT,
    ]
    .contains(&purpose)
    {
        return Err(CanonicalError::UnknownPurpose);
    }
    let envelope = serde_json::json!({"domain":format!("subc-config/{purpose}"), "version":DIGEST_VERSION, "value":value});
    Ok(raw_digest(&canonical_json(&envelope)?))
}

/// RFC 8785 with the config-gate lossless-number restriction. Keys sort by UTF-16,
/// not Rust's UTF-8 string order, even when serde_json's preserve_order is unified.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    let mut output = Vec::new();
    write_value(value, &mut output)?;
    Ok(output)
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::String(s) => {
            out.extend(serde_json::to_vec(s).map_err(|_| CanonicalError::Serialization)?)
        }
        Value::Number(n) => {
            let number = n.as_f64().ok_or(CanonicalError::NumberNotLossless)?;
            if !number.is_finite() || (number.fract() == 0.0 && number.abs() > SAFE_INTEGER) {
                return Err(CanonicalError::NumberNotLossless);
            }
            out.extend_from_slice(ecmascript_number(number).as_bytes());
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, item) in a.iter().enumerate() {
                if i != 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(o) => {
            out.push(b'{');
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i != 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).map_err(|_| CanonicalError::Serialization)?);
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

// Ryu supplies the shortest round-tripping digits. ECMAScript places the decimal
// point and switches to scientific notation at 1e-6 and 1e21, unlike ryu's display.
fn ecmascript_number(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let mut buffer = ryu::Buffer::new();
    let printed = buffer.format_finite(n.abs());
    let (mantissa, exponent) = printed.split_once('e').unwrap_or((printed, "0"));
    let exponent: i32 = exponent.parse().expect("ryu exponent");
    let dot = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut point = dot + exponent;
    while digits.starts_with('0') {
        digits.remove(0);
        point -= 1;
    }
    while digits.ends_with('0') {
        digits.pop();
    }
    let mut result = if n < 0.0 {
        "-".to_owned()
    } else {
        String::new()
    };
    let k = digits.len() as i32;
    if k <= point && point <= 21 {
        result.push_str(&digits);
        result.extend(std::iter::repeat_n('0', (point - k) as usize));
    } else if 0 < point && point <= 21 {
        result.push_str(&digits[..point as usize]);
        result.push('.');
        result.push_str(&digits[point as usize..]);
    } else if -6 < point && point <= 0 {
        result.push_str("0.");
        result.extend(std::iter::repeat_n('0', (-point) as usize));
        result.push_str(&digits);
    } else {
        result.push_str(&digits[..1]);
        if k > 1 {
            result.push('.');
            result.push_str(&digits[1..]);
        }
        result.push('e');
        let e = point - 1;
        if e >= 0 {
            result.push('+');
        }
        result.push_str(&e.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_contract_keys_and_envelope() {
        let value = json!({"\u{e000}":1,"\u{10000}":2,"a":["\n",false]});
        assert_eq!(
            String::from_utf8(canonical_json(&value).unwrap()).unwrap(),
            "{\"a\":[\"\\n\",false],\"𐀀\":2,\"\":1}"
        );
        assert_eq!(
            raw_digest(b"abc").as_str(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let expected =
            raw_digest(b"{\"domain\":\"subc-config/value\",\"value\":false,\"version\":1}");
        assert_eq!(digest(PURPOSE_VALUE, &json!(false)).unwrap(), expected);
        assert_ne!(digest(PURPOSE_SCHEMA, &json!(false)).unwrap(), expected);
    }

    #[test]
    fn canonical_contract_rfc8785_appendix_b() {
        // RFC 8785 Appendix B. Integral values outside the agreed config range
        // still exercise ES formatting here, but the public canonicalizer refuses them.
        let cases = [
            (0x0000000000000000, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x8000000000000001, "-5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0xffefffffffffffff, "-1.7976931348623157e+308"),
            (0x4340000000000000, "9007199254740992"),
            (0xc340000000000000, "-9007199254740992"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af5, "9.999999999999997e+22"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
            (0x444b1ae4d6e2ef4e, "999999999999999700000"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x41b3de4355555553, "333333333.3333332"),
            (0x41b3de4355555554, "333333333.33333325"),
            (0x41b3de4355555555, "333333333.3333333"),
            (0x41b3de4355555556, "333333333.3333334"),
            (0x41b3de4355555557, "333333333.33333343"),
            (0xbecbf647612f3696, "-0.0000033333333333333333"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ];
        for (bits, expected) in cases {
            let number = f64::from_bits(bits);
            assert_eq!(ecmascript_number(number), expected, "{bits:016x}");
            if number.fract() == 0.0 && number.abs() > SAFE_INTEGER {
                assert_eq!(
                    canonical_json(&json!(number)),
                    Err(CanonicalError::NumberNotLossless)
                );
            } else {
                assert_eq!(canonical_json(&json!(number)).unwrap(), expected.as_bytes());
            }
        }
        for n in [
            json!(9_007_199_254_740_992_u64),
            json!(-9_007_199_254_740_992_i64),
        ] {
            assert!(canonical_json(&n).is_err());
        }
    }
}
