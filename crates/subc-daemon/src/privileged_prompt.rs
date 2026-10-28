//! Operator prompt summaries naming the daemon operation, its target, and the
//! OS-resolved process that requested it, within the prompt's text budget.

use crate::caller_identity::CallerIdentity;
use std::path::Path;

const SUMMARY_LIMIT: usize = 200;
const TARGET_LIMIT: usize = 64;

/// Operation facts selected by the daemon's control dispatcher. There is no
/// free-form summary or caller description for a client to supply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivilegedOperation<'a> {
    Restart { module_id: &'a str },
    Swap { module_id: &'a str },
    Reload { module_id: &'a str },
    Rescan,
    SetEnabled { module_id: &'a str, enabled: bool },
    ReleaseReserved { module_id: &'a str },
}

impl PrivilegedOperation<'_> {
    fn description(self) -> String {
        let (verb, target) = match self {
            Self::Restart { module_id } => ("Restart module", module_id),
            Self::Swap { module_id } => ("Swap module", module_id),
            Self::Reload { module_id } => ("Reload module", module_id),
            Self::SetEnabled { module_id, enabled } => (
                if enabled {
                    "Start module"
                } else {
                    "Stop module"
                },
                module_id,
            ),
            Self::ReleaseReserved { module_id } => ("Release reserved module id", module_id),
            Self::Rescan => {
                // Approval covers the config read after approval, not a preview
                // whose contents might have changed while the prompt was open.
                return "Apply the daemon config file as it reads when approved (rescan)".into();
            }
        };
        format!("{verb} {}", truncate_target(&sanitize(target)))
    }
}

/// Build a summary from a typed operation and an OS-resolved connection caller.
/// The path, rather than the operation or pid, pays for the 200-scalar budget.
pub fn prompt_text(operation: PrivilegedOperation<'_>, caller: &CallerIdentity) -> String {
    match caller.pid().zip(caller.executable()) {
        Some((pid, path)) => resolved_prompt_text(operation, pid, path),
        None => format!(
            "{} — requested by an unidentified local process",
            operation.description()
        ),
    }
}

fn resolved_prompt_text(operation: PrivilegedOperation<'_>, pid: u32, path: &Path) -> String {
    let prefix = format!("{} — requested by pid {pid} (", operation.description());
    let path_budget = SUMMARY_LIMIT - prefix.chars().count() - 1;
    let path = shorten_path(&sanitize(&path.to_string_lossy()), path_budget);
    format!("{prefix}{path})")
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if forbidden(c) { '?' } else { c })
        .collect()
}

fn forbidden(c: char) -> bool {
    // The operator.confirm API rejects control and invisible format characters
    // in operator_confirm::valid_summary. Replace those and other invisible
    // format characters so a module id or executable cannot hide or reorder
    // any part of the prompt.
    c.is_control()
        || matches!(c,
            '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{115f}' | '\u{1160}' |
            '\u{17b4}' | '\u{17b5}' | '\u{180b}'..='\u{180f}' |
            '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' |
            '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' |
            '\u{feff}' | '\u{ffa0}' | '\u{fff9}'..='\u{fffb}' |
            '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' |
            '\u{e0000}'..='\u{e0fff}')
}

fn truncate_target(target: &str) -> String {
    if target.chars().count() <= TARGET_LIMIT {
        target.to_owned()
    } else {
        target.chars().take(TARGET_LIMIT - 1).chain(['…']).collect()
    }
}

fn shorten_path(path: &str, budget: usize) -> String {
    let count = path.chars().count();
    if count <= budget {
        return path.to_owned();
    }
    let component = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if component.chars().count() < budget {
        // Keep as much of the suffix as fits, including the whole final component.
        std::iter::once('…')
            .chain(path.chars().skip(count - (budget - 1)))
            .collect()
    } else {
        // Both markers matter: directories were omitted and the filename was cut.
        std::iter::once('…')
            .chain(component.chars().take(budget - 2))
            .chain(['…'])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(operation: PrivilegedOperation<'_>) -> String {
        resolved_prompt_text(operation, 4121, Path::new("/bin/node"))
    }

    #[test]
    fn operation_and_target_precede_the_resolved_caller() {
        let cases = [
            (
                PrivilegedOperation::Restart { module_id: "aft" },
                "Restart module aft",
            ),
            (
                PrivilegedOperation::Swap { module_id: "aft" },
                "Swap module aft",
            ),
            (
                PrivilegedOperation::Reload { module_id: "aft" },
                "Reload module aft",
            ),
            (
                PrivilegedOperation::SetEnabled {
                    module_id: "aft",
                    enabled: true,
                },
                "Start module aft",
            ),
            (
                PrivilegedOperation::SetEnabled {
                    module_id: "aft",
                    enabled: false,
                },
                "Stop module aft",
            ),
            (
                PrivilegedOperation::ReleaseReserved { module_id: "aft" },
                "Release reserved module id aft",
            ),
        ];
        for (operation, description) in cases {
            assert_eq!(
                resolved(operation),
                format!("{description} — requested by pid 4121 (/bin/node)")
            );
        }
    }

    #[test]
    fn rescan_covers_the_config_read_after_approval_without_a_module_count() {
        assert_eq!(
            resolved(PrivilegedOperation::Rescan),
            "Apply the daemon config file as it reads when approved (rescan) — requested by pid 4121 (/bin/node)"
        );
    }

    #[test]
    fn failed_lookup_names_an_unidentified_local_process() {
        assert_eq!(
            prompt_text(
                PrivilegedOperation::Restart { module_id: "aft" },
                &CallerIdentity::default()
            ),
            "Restart module aft — requested by an unidentified local process"
        );
    }

    /// Holds the connecting end of the caller socket in its own process. On
    /// Linux and macOS the lookup leaves a caller unresolved when it finds
    /// more than one process holding the socket, and on Unix a test process
    /// running other tests in parallel lends copies of its descriptors to
    /// every child it spawns until that child execs. A connecting socket that
    /// lives in the test process can therefore read as unidentified whenever
    /// another test happens to be spawning; one that lives here cannot.
    #[test]
    #[ignore = "re-exec socket owner fixture"]
    fn prompt_caller_child_holds_socket() {
        use std::io::{Read, Write};
        let address = std::env::var("SUBC_TEST_PROMPT_CALLER_ADDRESS").unwrap();
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        socket.write_all(b"R").unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    }

    struct KilledOnDrop(std::process::Child);

    impl Drop for KilledOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[tokio::test]
    async fn resolved_connection_identity_flows_into_the_public_prompt_builder() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let caller = KilledOnDrop(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "privileged_prompt::tests::prompt_caller_child_holds_socket",
                    "--ignored",
                ])
                .env(
                    "SUBC_TEST_PROMPT_CALLER_ADDRESS",
                    listener.local_addr().unwrap().to_string(),
                )
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let (mut accepted, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                .await
                .expect("socket owner child connects")
                .unwrap();
        let mut ready = [0];
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            accepted.read_exact(&mut ready),
        )
        .await
        .expect("socket owner child signals readiness")
        .unwrap();
        assert_eq!(&ready, b"R");
        let accepted = accepted.into_std().unwrap();

        // Another process holding a copy of everything this test process
        // holds, as a child mid-spawn does. It must not make the caller
        // ambiguous, because the caller's socket is not among those copies.
        #[cfg(unix)]
        let _mid_spawn_copy = KilledOnDrop(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                    accepted.try_clone().unwrap(),
                )))
                .spawn()
                .unwrap(),
        );

        let accepted = tokio::net::TcpStream::from_std(accepted).unwrap();
        let caller_identity = CallerIdentity::lookup(&accepted);
        let summary = prompt_text(
            PrivilegedOperation::Restart { module_id: "aft" },
            &caller_identity,
        );
        assert!(
            summary.starts_with(&format!(
                "Restart module aft — requested by pid {} (",
                caller.0.id()
            )),
            "{summary}"
        );
        let executable = std::env::current_exe().unwrap();
        let filename = executable.file_name().unwrap().to_string_lossy();
        assert!(summary.ends_with(&format!("{filename})")), "{summary}");
        drop(accepted);
    }

    #[test]
    fn target_limits_count_scalars_and_mark_only_a_cut_target() {
        for count in [63, 64, 65, 300] {
            let target = "界".repeat(count);
            let expected = if count <= 64 {
                target.clone()
            } else {
                format!("{}…", "界".repeat(63))
            };
            assert_eq!(
                resolved(PrivilegedOperation::Restart { module_id: &target }),
                format!("Restart module {expected} — requested by pid 4121 (/bin/node)")
            );
        }
    }

    #[test]
    fn controls_zero_width_and_bidi_are_replaced_in_target_and_path() {
        let bad: String = (0..=0x9f)
            .filter_map(char::from_u32)
            .filter(|c| c.is_control())
            .chain(['\u{00ad}', '\u{034f}', '\u{061c}', '\u{180e}', '\u{feff}'])
            .chain(('\u{200b}'..='\u{200f}').chain('\u{2028}'..='\u{202e}'))
            .chain('\u{2060}'..='\u{206f}')
            .collect();
        for c in bad.chars() {
            let target = format!("a{c}ft");
            let path = format!("/bin/n{c}de");
            assert_eq!(
                resolved_prompt_text(
                    PrivilegedOperation::Restart { module_id: &target },
                    4121,
                    Path::new(&path)
                ),
                "Restart module a?ft — requested by pid 4121 (/bin/n?de)"
            );
        }
    }

    #[test]
    fn path_shortening_keeps_the_final_component_for_unix_and_windows_paths() {
        assert_eq!(shorten_path("/bin/node", 9), "/bin/node");
        assert_eq!(shorten_path("/long/directory/bin/node", 10), "…/bin/node");
        assert_eq!(
            shorten_path(r"C:\long\directory\bin\node.exe", 14),
            r"…\bin\node.exe"
        );
        assert_eq!(
            shorten_path("/long/path/界界界界界界界界界界", 10),
            "…界界界界界界界界…"
        );
        assert_eq!(shorten_path("/path/abcdefghij", 10), "…abcdefgh…");
    }

    fn oversized_facts() -> (String, String) {
        let target = format!("{}\u{202e}{}", "界".repeat(31), "界".repeat(268));
        let path = format!("/{}\u{200b}/bin/node", "路".repeat(489));
        assert_eq!(target.chars().count(), 300);
        assert_eq!(path.chars().count(), 500);
        (target, path)
    }

    #[test]
    fn oversized_unicode_facts_fit_without_losing_operation_or_pid() {
        let (target, path) = oversized_facts();
        let summary = resolved_prompt_text(
            PrivilegedOperation::Restart { module_id: &target },
            4121,
            Path::new(&path),
        );
        assert_eq!(summary.chars().count(), 200);
        assert!(summary.starts_with(&format!(
            "Restart module {}?{}… — requested by pid 4121 (…",
            "界".repeat(31),
            "界".repeat(31)
        )));
        assert!(summary.ends_with("/bin/node)"));
        assert!(!summary.chars().any(forbidden));

        let long_filename = format!("/{}", "路".repeat(499));
        for operation in [
            PrivilegedOperation::ReleaseReserved { module_id: &target },
            PrivilegedOperation::Rescan,
        ] {
            let summary = resolved_prompt_text(operation, u32::MAX, Path::new(&long_filename));
            assert_eq!(summary.chars().count(), 200);
            assert!(summary.contains(" — requested by pid 4294967295 (…"));
            assert!(summary.ends_with("…)"));
        }
    }

    #[tokio::test]
    async fn oversized_unicode_summary_passes_operator_confirm_admission_rule() {
        use crate::{
            forwarding::{ChannelFlow, ModuleEndpointId, RouteBinding},
            operator_confirm::{
                OperatorConfirms, OperatorLimits, OperatorProvider, OperatorWithdraw,
                ProviderResult,
            },
            router::{FrameSink, RouteCtx},
            ConnectionId, Frame,
        };
        use std::sync::Arc;
        use subc_protocol::{
            error_codes, session::OperatorConfirmRequest, Flags, FrameType, Principal, Priority,
            PROTOCOL_VERSION,
        };

        struct UnavailableProvider;
        impl OperatorProvider for UnavailableProvider {
            fn prompt(
                &self,
                _: &str,
                _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
            ) -> ProviderResult {
                ProviderResult::Unavailable
            }
        }

        let confirms = Arc::new(OperatorConfirms::default());
        confirms.configure(Arc::new(UnavailableProvider), OperatorLimits::default());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(1),
            egress: FrameSink::new(tx),
        };
        let binding = RouteBinding {
            client_connection_id: ConnectionId::new(2),
            client_sink: ctx.egress.clone(),
            client_negotiated_ver: PROTOCOL_VERSION,
            client_channel: 1,
            client_epoch: 1,
            module_id: "fixture".into(),
            module_endpoint: ModuleEndpointId {
                connection_id: ctx.connection_id,
                generation: 1,
            },
            module_sink: ctx.egress.clone(),
            module_negotiated_ver: PROTOCOL_VERSION,
            module_channel: 1,
            module_epoch: 1,
            principal: Principal::Direct,
            harness: "fixture".into(),
            build_class: None,
            operator_app: None,
            project_root: None,
            bound_at: tokio::time::Instant::now(),
            flow: Arc::new(ChannelFlow::new(64 * 1024)),
            scope: None,
        };
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            b"{}".to_vec(),
        )
        .unwrap();

        // Use operator.confirm admission to check its summary length and character
        // restrictions without duplicating the validation predicate. Rejections of
        // overlong and bidi-bearing summaries prove the fixture reaches that gate.
        for invalid in ["界".repeat(201), "hidden\u{202e}text".into()] {
            let refusal = confirms.admit(
                &ctx,
                frame.clone(),
                "fixture".into(),
                true,
                OperatorConfirmRequest::new(invalid, 1, 1),
                Some(&binding),
            );
            assert_eq!(refusal.len(), 1);
            let body: serde_json::Value = serde_json::from_slice(&refusal[0].body).unwrap();
            assert_eq!(body["code"], error_codes::OPERATOR_SUMMARY_INVALID);
        }

        let (target, path) = oversized_facts();
        let summary = resolved_prompt_text(
            PrivilegedOperation::Restart { module_id: &target },
            4121,
            Path::new(&path),
        );
        assert!(confirms
            .admit(
                &ctx,
                frame,
                "fixture".into(),
                true,
                OperatorConfirmRequest::new(summary, 1, 1),
                Some(&binding),
            )
            .is_empty());
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(4), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&outcome.frame.body).unwrap();
        assert_eq!(body["code"], error_codes::OPERATOR_PRESENCE_UNAVAILABLE);
        assert_eq!(body["detail"]["reason"], "provider_error");
    }
}
