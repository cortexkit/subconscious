//! Facts about the caller's current connection, not a claim supplied by the client.
//!
//! Recognition is a snapshot: revalidation can change whether a later call or
//! route bind carries an operator app. This query grants nothing and never prompts.

use serde::{Deserialize, Serialize};
use subc_protocol::peer::OperatorApp;

/// Ask the daemon what it knows about this connection.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionDescribeRequest {
    op: ConnectionDescribeOp,
}

impl ConnectionDescribeRequest {
    pub fn new() -> Self {
        Self {
            op: ConnectionDescribeOp::Describe,
        }
    }
}

impl Default for ConnectionDescribeRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// The daemon's current view of the caller's connection.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionDescribeReply {
    op: ConnectionDescribeOp,
    /// True for a Unix socket or Windows named pipe, false for loopback TCP.
    pub local: bool,
    /// The app currently recognised for this connection, omitted when none is.
    /// `route.bind` sends it only to targets that advertise `operator-app/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_app: Option<OperatorApp>,
}

impl ConnectionDescribeReply {
    pub fn new(local: bool, operator_app: Option<OperatorApp>) -> Self {
        Self {
            op: ConnectionDescribeOp::Describe,
            local,
            operator_app,
        }
    }
}

// A private, required tag prevents accepting a reply for a different operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ConnectionDescribeOp {
    #[serde(rename = "connection.describe")]
    Describe,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn connection_describe_bodies_require_the_op() {
        for op in [None, Some("app.open")] {
            let mut request = json!({});
            let mut reply = json!({"local": true});
            if let Some(op) = op {
                request["op"] = json!(op);
                reply["op"] = json!(op);
            }
            assert!(serde_json::from_value::<ConnectionDescribeRequest>(request).is_err());
            assert!(serde_json::from_value::<ConnectionDescribeReply>(reply).is_err());
        }
    }

    #[test]
    fn connection_describe_wire_shapes() {
        assert_eq!(
            serde_json::to_value(ConnectionDescribeRequest::new()).unwrap(),
            json!({"op": "connection.describe"})
        );
        assert_eq!(
            serde_json::to_value(ConnectionDescribeReply::new(false, None)).unwrap(),
            json!({"op": "connection.describe", "local": false})
        );
        let reply = ConnectionDescribeReply::new(true, Some(OperatorApp::new("fixture", 42)));
        let wire = json!({"op": "connection.describe", "local": true, "operator_app": {"app": "fixture", "pid": 42}});
        assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ConnectionDescribeReply>(wire).unwrap(),
            reply
        );
    }
}
