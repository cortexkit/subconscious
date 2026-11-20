//! Operator requests for the daemon's agent policy trust root.

use serde::{Deserialize, Serialize};

pub const SET_OP: &str = "policy_root.set";
pub const RECOVER_OP: &str = "policy_root.recover";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPolicyRoot {
    pub key_id: String,
    pub public_key_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverPolicyRoot {
    pub body_b64: String,
}

#[cfg(test)]
mod tests {
    use crate::{ClientControlRequest, ClientControlResponse};

    #[test]
    fn policy_root_client_wire() {
        for request in [
            r#"{"op":"policy_root.get"}"#,
            r#"{"op":"policy_root.set","key_id":"0123456789abcdef","public_key_hex":"00"}"#,
            r#"{"op":"policy_root.recover","body_b64":"e30="}"#,
        ] {
            let decoded: ClientControlRequest = serde_json::from_str(request).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), request);
            let extra = request.replace('}', ",\"extra\":true}");
            assert!(serde_json::from_str::<ClientControlRequest>(&extra).is_err());
        }
        let reply = r#"{"op":"policy_root.recover","digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","state":"pending"}"#;
        let decoded: ClientControlResponse = serde_json::from_str(reply).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), reply);
    }
}
