//! Transitional compatibility JSON project fields for targets that do not
//! declare `bind-no-project/v1`. Prefer the opener's canonical root and preserve
//! its project ID, including absence. Rootless openers use the admitted scope's
//! workdir, else `/`, and the opener ID, else the scope ID, else no ID. The `/`
//! compatibility fallback does not supply an internal project root: without an
//! opener root or scope workdir, internal root attribution remains absent.

use std::path::PathBuf;

use serde::{ser::SerializeMap, Serialize, Serializer};
use subc_protocol::{
    manifest::CapabilityDeclarations,
    peer::{encode_route_bind, RouteBindPeer},
    scope::{ScopeStamp, BIND_NO_PROJECT_CAPABILITY},
    session::ModuleControlRequest,
};

use crate::identity::ProjectRootId;

#[derive(Debug, Default)]
pub(crate) struct LegacyProject {
    pub root: Option<ProjectRootId>,
    pub id: Option<String>,
}

impl LegacyProject {
    /// Read only the retired fields, from the same object as BindIdentity.
    /// If `identity.project_root` is present, it must decode as a path and pass
    /// path validation. JSON `null` is rejected; omit the field for a rootless
    /// opener.
    pub fn from_open(body: &[u8]) -> Result<Self, (&'static str, String)> {
        let body: serde_json::Value =
            serde_json::from_slice(body).map_err(|err| ("invalid_request", err.to_string()))?;
        let identity = &body["identity"];
        let root = identity
            .get("project_root")
            .map(|value| {
                let path: PathBuf = serde_json::from_value(value.clone())
                    .map_err(|err| ("invalid_project_root", err.to_string()))?;
                ProjectRootId::from_path_allowing_missing(&path)
                    .map_err(|err| ("invalid_project_root", err.to_string()))
            })
            .transpose()?;
        let id = identity
            .get("project_id")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|err| ("invalid_request", err.to_string()))?
            .flatten();
        Ok(Self { root, id })
    }

    pub fn effective_root(
        &self,
        stamp: Option<&ScopeStamp>,
    ) -> Result<Option<ProjectRootId>, crate::identity::IdentityError> {
        if let Some(root) = &self.root {
            return Ok(Some(root.clone()));
        }
        stamp
            .and_then(|stamp| stamp.workdir.as_ref())
            .map(ProjectRootId::from_path_allowing_missing)
            .transpose()
    }

    pub fn for_target(self, stamp: Option<&ScopeStamp>) -> LegacyBindProject {
        // A legacy producer's absent id must stay absent for its whole session;
        // adding the scope id would fork state keyed on the old identity.
        let id = if self.root.is_some() {
            self.id
        } else {
            self.id
                .or_else(|| stamp.and_then(|stamp| stamp.project_id.clone()))
        };
        LegacyBindProject {
            root: self
                .root
                .map(|root| root.as_path().to_path_buf())
                .or_else(|| {
                    stamp
                        .and_then(|stamp| stamp.workdir.as_ref())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from("/")),
            id,
        }
    }
}

pub(crate) struct LegacyBindProject {
    root: PathBuf,
    id: Option<String>,
}

/// For targets without `bind-no-project/v1`, preserve the pre-0.34 JSON field
/// order and encoding. With an opener-supplied root and otherwise identical bind
/// metadata, preserve legacy bind bytes after root canonicalization. Re-encoding
/// through a JSON value would sort keys and change those bytes.
pub(crate) fn encode_bind(
    bind: &ModuleControlRequest,
    peer: &RouteBindPeer,
    project: &LegacyBindProject,
    capabilities: Option<&CapabilityDeclarations>,
) -> serde_json::Result<Vec<u8>> {
    if capabilities.is_some_and(|caps| {
        caps.provides
            .iter()
            .any(|cap| cap == BIND_NO_PROJECT_CAPABILITY)
    }) {
        return encode_route_bind(bind, peer);
    }
    struct CompatBind<'a>(&'a ModuleControlRequest, &'a LegacyBindProject);
    impl Serialize for CompatBind<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let ModuleControlRequest::RouteBind {
                route_channel,
                epoch,
                target,
                identity,
                principal,
                consumer_capabilities,
                role_versions,
                admission_facts,
                scope,
                call_mode,
            } = self.0
            else {
                return Err(serde::ser::Error::custom("expected route.bind"));
            };
            #[derive(Serialize)]
            struct Identity<'a> {
                project_root: &'a PathBuf,
                harness: &'a str,
                session: &'a str,
                #[serde(skip_serializing_if = "Option::is_none")]
                project_id: &'a Option<String>,
            }
            let mut map = serializer.serialize_map(None)?;
            map.serialize_entry("op", "route.bind")?;
            map.serialize_entry("route_channel", route_channel)?;
            map.serialize_entry("epoch", epoch)?;
            map.serialize_entry("target", target)?;
            map.serialize_entry(
                "identity",
                &Identity {
                    project_root: &self.1.root,
                    harness: &identity.harness,
                    session: &identity.session,
                    project_id: &self.1.id,
                },
            )?;
            macro_rules! optional {
                ($($field:ident),* $(,)?) => {$(
                    if let Some(value) = $field {
                        map.serialize_entry(stringify!($field), value)?;
                    }
                )*};
            }
            optional!(
                principal,
                consumer_capabilities,
                role_versions,
                admission_facts,
                scope,
                call_mode
            );
            map.end()
        }
    }
    #[derive(Serialize)]
    struct WithPeer<'a> {
        #[serde(flatten)]
        bind: CompatBind<'a>,
        #[serde(flatten)]
        peer: &'a RouteBindPeer,
    }
    serde_json::to_vec(&WithPeer {
        bind: CompatBind(bind, project),
        peer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{scope::ScopeKind, BindIdentity, Principal, RouteTarget};

    fn stamp() -> ScopeStamp {
        ScopeStamp::new(Principal::Direct, "session", 1, ScopeKind::Head)
            .with_workdir(Some("/scope-project".into()))
            .with_project_id(Some("pj-scope".into()))
    }

    fn bind(scope: Option<ScopeStamp>) -> ModuleControlRequest {
        ModuleControlRequest::RouteBind {
            route_channel: 2,
            epoch: 3,
            target: RouteTarget::ToolProvider {
                module_id: "test".into(),
            },
            identity: BindIdentity::new("test", "session"),
            principal: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope,
            call_mode: None,
        }
    }

    /// The form the daemon writes for an opener root. On Windows it is not
    /// `std::fs::canonicalize`'s `\\?\` form, so compare against this.
    fn canonical_current_dir() -> PathBuf {
        ProjectRootId::from_path_allowing_missing(std::env::current_dir().unwrap())
            .unwrap()
            .into_path_buf()
    }

    fn legacy(body: serde_json::Value) -> LegacyProject {
        LegacyProject::from_open(&serde_json::to_vec(&body).unwrap()).unwrap()
    }

    fn identity(project: LegacyProject, stamp: Option<&ScopeStamp>) -> serde_json::Value {
        let bytes = encode_bind(
            &bind(None),
            &RouteBindPeer::new(),
            &project.for_target(stamp),
            None,
        )
        .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["identity"].clone()
    }

    #[test]
    fn opener_root_preserves_legacy_bind_bytes() {
        let root = canonical_current_dir();
        let project = legacy(
            serde_json::json!({"identity": {"project_root": root, "project_id": "pj-opener"}}),
        );
        let actual = encode_bind(
            &bind(None),
            &RouteBindPeer::new(),
            &project.for_target(Some(&stamp())),
            None,
        )
        .unwrap();
        let expected = format!(
            "{{\"op\":\"route.bind\",\"route_channel\":2,\"epoch\":3,\"target\":{{\"kind\":\"tool_provider\",\"module_id\":\"test\"}},\"identity\":{{\"project_root\":{},\"harness\":\"test\",\"session\":\"session\",\"project_id\":\"pj-opener\"}}}}",
            serde_json::to_string(&root).unwrap(),
        );
        assert_eq!(actual, expected.as_bytes());
    }

    #[test]
    fn opener_root_without_id_preserves_legacy_bytes_despite_stamp_id() {
        let root = canonical_current_dir();
        let project = legacy(serde_json::json!({"identity": {"project_root": root}}));
        let actual = encode_bind(
            &bind(None),
            &RouteBindPeer::new(),
            &project.for_target(Some(&stamp())),
            None,
        )
        .unwrap();
        let expected = format!(
            "{{\"op\":\"route.bind\",\"route_channel\":2,\"epoch\":3,\"target\":{{\"kind\":\"tool_provider\",\"module_id\":\"test\"}},\"identity\":{{\"project_root\":{},\"harness\":\"test\",\"session\":\"session\"}}}}",
            serde_json::to_string(&root).unwrap(),
        );
        assert_eq!(actual, expected.as_bytes());
    }

    #[test]
    fn legacy_projection_preserves_optional_metadata_and_all_peer_combinations() {
        use subc_protocol::peer::{ObservedPeer, OperatorApp};

        let stamp = stamp();
        let mut request = bind(Some(stamp.clone()));
        let ModuleControlRequest::RouteBind {
            principal,
            consumer_capabilities,
            role_versions,
            admission_facts,
            call_mode,
            ..
        } = &mut request
        else {
            unreachable!()
        };
        *principal = Some(Principal::Direct);
        *consumer_capabilities = Some(vec!["roots".into()]);
        *role_versions = Some(std::collections::BTreeMap::from([(
            "tool-provider".into(),
            "v1".into(),
        )]));
        *admission_facts = Some(serde_json::Value::Null);
        *call_mode = Some(subc_protocol::call_mode::derive_call_mode(Some(&stamp)));
        let project = LegacyBindProject {
            root: PathBuf::from("/project"),
            id: Some("pj-opener".into()),
        };
        for observed in [None, Some(ObservedPeer::observed_uid(501))] {
            for app in [None, Some(OperatorApp::new("editor", 4242))] {
                let peer = RouteBindPeer::new()
                    .with_observed_peer(observed.clone())
                    .with_operator_app(app);
                let modern =
                    String::from_utf8(encode_route_bind(&request, &peer).unwrap()).unwrap();
                let expected = modern.replace(
                    r#""identity":{"harness":"test","session":"session"}"#,
                    r#""identity":{"project_root":"/project","harness":"test","session":"session","project_id":"pj-opener"}"#,
                );
                assert_ne!(
                    modern, expected,
                    "the expected legacy identity must be inserted"
                );
                assert_eq!(
                    encode_bind(&request, &peer, &project, None).unwrap(),
                    expected.as_bytes()
                );
            }
        }
    }

    #[test]
    fn scoped_rootless_opener_gets_stamp_workdir() {
        let stamp = stamp();
        assert_eq!(
            identity(LegacyProject::default(), Some(&stamp))["project_root"],
            "/scope-project"
        );
    }

    #[test]
    fn unscoped_rootless_opener_gets_no_project_placeholder() {
        let project = LegacyProject::default();
        assert!(project.effective_root(None).unwrap().is_none());
        assert_eq!(identity(project, None)["project_root"], "/");
    }

    #[test]
    fn declaring_target_gets_no_project_fields() {
        let caps = CapabilityDeclarations {
            provides: vec![BIND_NO_PROJECT_CAPABILITY.into()],
            requires: vec![],
            must_never_reach: vec![],
        };
        let project = legacy(
            serde_json::json!({"identity": {"project_root": std::env::current_dir().unwrap(), "project_id": "pj-opener"}}),
        );
        let bytes = encode_bind(
            &bind(None),
            &RouteBindPeer::new(),
            &project.for_target(Some(&stamp())),
            Some(&caps),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["identity"],
            serde_json::json!({"harness": "test", "session": "session"})
        );
    }

    #[test]
    fn project_id_prefers_opener() {
        let project = legacy(serde_json::json!({"identity": {"project_id": "pj-opener"}}));
        assert_eq!(identity(project, Some(&stamp()))["project_id"], "pj-opener");
    }

    #[test]
    fn project_id_falls_back_to_stamp() {
        assert_eq!(
            identity(LegacyProject::default(), Some(&stamp()))["project_id"],
            "pj-scope"
        );
    }

    #[test]
    fn project_id_is_absent_without_opener_or_stamp_id() {
        assert!(identity(LegacyProject::default(), None)
            .get("project_id")
            .is_none());
    }

    #[test]
    fn invalid_present_root_is_refused() {
        let dir = cortexkit_test_support::ScratchDir::new("invalid-legacy-root");
        let mut roots = vec![serde_json::Value::Null, serde_json::json!(7)];
        // A `..` after a missing component can't be resolved on Unix. Windows
        // resolves `..` lexically, so there the same path is valid.
        if cfg!(unix) {
            roots.push(serde_json::json!(dir.path().join("missing").join("..")));
        }
        for root in roots {
            let body = serde_json::to_vec(&serde_json::json!({"identity": {"project_root": root}}))
                .unwrap();
            assert_eq!(
                LegacyProject::from_open(&body).unwrap_err().0,
                "invalid_project_root"
            );
        }
    }
}
