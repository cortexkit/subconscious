//! Evidence for a program origin, independent of a stored origin label.
//!
//! A launch witness describes the application launch before the daemon adds its
//! per-spawn registration descriptors, nonces and role. Environment values are
//! hashed rather than retained: inherited credentials must not enter review cards.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use subc_protocol::{peer::CallerOrigin, provider_declarations::ProviderDeclarations};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchWitness {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) environment_digest: subc_config::Digest,
    pub(crate) working_directory: PathBuf,
}

impl LaunchWitness {
    /// Use the same environment builder as the child command, including inherited
    /// values. Configured values alone would miss changes to the daemon's env.
    pub(crate) fn capture(spec: &crate::supervise::ModuleSpec) -> std::io::Result<Self> {
        let mut command = tokio::process::Command::new(&spec.program);
        crate::supervise::apply_child_env(&mut command, spec);
        Self::capture_command(spec, &command)
    }

    pub(crate) fn capture_command(
        spec: &crate::supervise::ModuleSpec,
        command: &tokio::process::Command,
    ) -> std::io::Result<Self> {
        let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
        for (key, value) in command.as_std().get_envs() {
            if let Some(value) = value {
                environment.insert(key.to_owned(), value.to_owned());
            } else {
                environment.remove(key);
            }
        }
        // Native strings are serialized without lossy UTF-8 conversion.
        let environment = environment
            .into_iter()
            .map(|(key, value)| {
                (
                    key.as_encoded_bytes().to_vec(),
                    value.as_encoded_bytes().to_vec(),
                )
            })
            .collect::<Vec<_>>();
        Ok(Self {
            program: spec.program.clone(),
            args: spec.args.clone(),
            environment_digest: subc_config::raw_digest(&serde_json::to_vec(&environment)?),
            working_directory: command
                .as_std()
                .get_current_dir()
                .map(PathBuf::from)
                .map_or_else(std::env::current_dir, Ok)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProgramOriginEvidence {
    pub(crate) origin: CallerOrigin,
    pub(crate) module_id: String,
    pub(crate) published_commit: String,
    pub(crate) program_digest: subc_config::Digest,
    pub(crate) launch: LaunchWitness,
    #[serde(default)]
    pub(crate) hosts_third_party_code: Option<bool>,
    #[serde(default)]
    pub(crate) declarations: ProviderDeclarations,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DevelopmentApproval {
    pub(crate) module_id: String,
    pub(crate) incarnation: String,
    pub(crate) spawn: u64,
    pub(crate) origin: CallerOrigin,
    pub(crate) requested_entries: ProviderDeclarations,
}

/// Release origin needs a signed per-module release record, re-verified at each
/// spawn, including its exact launch manifest, revocation status and the
/// module's minimum approved release version
/// (first-party mark §2 and config-gate §9.3). An origin label is not that evidence.
pub(crate) fn release_origin() -> CallerOrigin {
    CallerOrigin::Other
}

pub(crate) fn derive_origin(
    evidence: Option<&ProgramOriginEvidence>,
    module: &str,
    program_digest: &subc_config::Digest,
    launch: &LaunchWitness,
) -> CallerOrigin {
    let Some(evidence) = evidence else {
        return CallerOrigin::Other;
    };
    if evidence.origin == CallerOrigin::Release {
        return release_origin();
    }
    #[cfg(any(test, feature = "dev-placement"))]
    if evidence.origin == CallerOrigin::Development
        && evidence.module_id == module
        && evidence.program_digest == *program_digest
        && evidence.launch == *launch
        && evidence.published_commit.len() == 40
        && evidence
            .published_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return CallerOrigin::Development;
    }
    let _ = (module, program_digest, launch);
    CallerOrigin::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    fn witness() -> LaunchWitness {
        LaunchWitness {
            program: "/program".into(),
            args: vec!["--serve".into()],
            environment_digest: subc_config::raw_digest(b"env"),
            working_directory: "/work".into(),
        }
    }

    fn evidence() -> ProgramOriginEvidence {
        ProgramOriginEvidence {
            origin: CallerOrigin::Development,
            module_id: "module".into(),
            published_commit: "a".repeat(40),
            program_digest: subc_config::raw_digest(b"program"),
            launch: witness(),
            hosts_third_party_code: Some(false),
            declarations: Default::default(),
        }
    }

    #[test]
    fn origins_rederive_each_spawn_and_refuse_unproven_release() {
        let mut record = evidence();
        let derive = |record: Option<&ProgramOriginEvidence>, launch: &LaunchWitness| {
            derive_origin(
                record,
                "module",
                &subc_config::raw_digest(b"program"),
                launch,
            )
        };
        assert_eq!(derive(Some(&record), &witness()), CallerOrigin::Development);
        assert_eq!(derive(None, &witness()), CallerOrigin::Other);
        record.origin = CallerOrigin::Release;
        assert_eq!(derive(Some(&record), &witness()), CallerOrigin::Other);
        record.origin = CallerOrigin::Development;
        for changed in ["args", "env", "cwd", "program"] {
            let mut launch = witness();
            match changed {
                "args" => launch.args.push("--changed".into()),
                "env" => launch.environment_digest = subc_config::raw_digest(b"changed env"),
                "cwd" => launch.working_directory = "/elsewhere".into(),
                _ => launch.program = "/other-program".into(),
            }
            assert_eq!(
                derive(Some(&record), &launch),
                CallerOrigin::Other,
                "{changed}"
            );
        }
        record.program_digest = subc_config::raw_digest(b"other program");
        assert_eq!(derive(Some(&record), &witness()), CallerOrigin::Other);
        record = evidence();
        record.module_id = "another".into();
        assert_eq!(derive(Some(&record), &witness()), CallerOrigin::Other);
        record = evidence();
        record.published_commit.clear();
        assert_eq!(derive(Some(&record), &witness()), CallerOrigin::Other);
    }

    #[test]
    fn origins_missing_hosting_flag_is_hosting() {
        let mut evidence = evidence();
        for hosts in [Some(false), None, Some(true)] {
            evidence.hosts_third_party_code = hosts;
            let origin = derive_origin(
                Some(&evidence),
                "module",
                &evidence.program_digest,
                &witness(),
            );
            assert_eq!(
                origin,
                CallerOrigin::Development,
                "hosting does not change origin"
            );
            let record =
                crate::spawn_authority::SpawnAuthorityRecord::new("incarnation", 1, "module");
            record.initialize_origin(origin, Default::default(), hosts);
            assert_eq!(record.origin_qualifies_for_mark(), hosts == Some(false));
        }
        let record = crate::spawn_authority::SpawnAuthorityRecord::new("incarnation", 2, "module");
        record.initialize_origin(CallerOrigin::Other, Default::default(), Some(false));
        assert!(
            !record.origin_qualifies_for_mark(),
            "non-hosting alone proves no origin"
        );
    }
}
