//! Client-facing subc channel-0 control wire shapes.
//!
//! This crate is the client ↔ subc control-plane boundary. It depends only on
//! [`subc-protocol`] for shared primitives such as `RouteTarget`,
//! `BindIdentity`, and `ConfigTier`; clients can use it without depending on the
//! daemon implementation.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use subc_protocol::{manifest::ProviderRole, session::ConfigTier, BindIdentity, RouteTarget};

/// Reserved dotted operation prefixes for the v0.4 control vocabulary.
pub mod ops {
    pub const SERVER: &str = "server.";
    pub const CATALOG: &str = "catalog.";
    pub const ROUTE: &str = "route.";
    pub const SUPERVISOR: &str = "supervisor.";
    pub const SCHEDULER: &str = "scheduler.";
    pub const CONFIG: &str = "config.";
    pub const WATCH: &str = "watch.";

    pub const SERVER_DESCRIBE: &str = "server.describe";
    pub const CATALOG_LIST: &str = "catalog.list";
    pub const ROUTE_OPEN: &str = "route.open";
    pub const ROUTE_POLL: &str = "route.poll";
    pub const SUPERVISOR_LIST: &str = "supervisor.list";
    pub const SUPERVISOR_RESTART: &str = "supervisor.restart";
    pub const SUPERVISOR_SET_ENABLED: &str = "supervisor.set_enabled";
}

/// Client-originated channel-0 control RPC body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ClientControlRequest {
    #[serde(rename = "server.describe")]
    ServerDescribe {},
    #[serde(rename = "catalog.list")]
    CatalogList {
        #[serde(default)]
        module_id: Option<String>,
    },
    #[serde(rename = "route.open")]
    RouteOpen {
        target: RouteTarget,
        identity: BindIdentity,
        #[serde(default)]
        config: Vec<ConfigTier>,
    },
    #[serde(rename = "route.poll")]
    RoutePoll { route_channel: u16, kind: PollKind },
    #[serde(rename = "supervisor.list")]
    SupervisorList {},
    #[serde(rename = "supervisor.restart")]
    SupervisorRestart { module_id: String },
    #[serde(rename = "supervisor.set_enabled")]
    SupervisorSetEnabled { module_id: String, enabled: bool },
}

/// subc's channel-0 response body for client control RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ClientControlResponse {
    #[serde(rename = "server.describe")]
    ServerDescribe {
        protocol_ver: u8,
        subc_ops: Vec<String>,
        capabilities: Vec<String>,
    },
    #[serde(rename = "catalog.list")]
    CatalogList {
        generation: u64,
        modules: Vec<CatalogEntry>,
        subc_ops: Vec<String>,
    },
    #[serde(rename = "route.open")]
    RouteOpen { route_channel: u16 },
    #[serde(rename = "route.poll")]
    RoutePoll {
        status: Option<String>,
        live: Option<bool>,
    },
    #[serde(rename = "supervisor.list")]
    SupervisorList {
        generation: u64,
        modules: Vec<SupervisorEntry>,
    },
    #[serde(rename = "supervisor.ack")]
    SupervisorAck { module_id: String, applied: bool },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PollKind {
    Status,
    Liveness,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogEntry {
    pub module_id: String,
    pub roles: Vec<ProviderRole>,
    pub control_ops: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorEntry {
    pub module_id: String,
    pub state: String,
    pub enabled: bool,
    pub live: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{BindIdentity, RouteTarget};

    #[test]
    fn route_poll_uses_kind_field() {
        let body = serde_json::to_value(ClientControlRequest::RoutePoll {
            route_channel: 7,
            kind: PollKind::Status,
        })
        .unwrap();

        assert_eq!(body["op"], "route.poll");
        assert_eq!(body["kind"], "status");
        assert!(body.get("op").is_some());
    }

    #[test]
    fn route_open_is_internally_tagged() {
        let request = ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: "aft".to_string(),
            },
            identity: BindIdentity {
                project_root: "/tmp/project".into(),
                harness: "opencode".to_string(),
                session: "session-1".to_string(),
            },
            config: Vec::new(),
        };

        let body = serde_json::to_value(request).unwrap();
        assert_eq!(body["op"], "route.open");
        assert_eq!(body["target"]["kind"], "tool_provider");
    }
}
