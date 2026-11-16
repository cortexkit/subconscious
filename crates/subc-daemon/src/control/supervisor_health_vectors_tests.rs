// These examples use the same response frame encoder as the control handlers.
#[test]
fn supervisor_health_v1_vectors() {
    use subc_control::{
        ChildResourceUnavailableReason, ChildResourceUsage, LaunchSpecAgreement,
        LaunchSpecUnavailableReason, PendingReloadVerdict, ReloadPathAgreement,
        ReloadPathUnavailableReason, RunningImageAgreement, RunningImageUnavailableReason,
        SupervisorEntry, SupervisorHealthEntry, SupervisorHealthStatus,
    };

    let examples = [
        (
            "ok-degraded",
            vec![
                (
                    "northstar",
                    SupervisorHealthStatus::Ok,
                    None,
                    None,
                    Some(1_700_000_000_000),
                ),
                (
                    "bluebird",
                    SupervisorHealthStatus::Degraded,
                    Some("Queue is growing"),
                    Some(serde_json::json!({"queue_depth": 12, "workers": 2})),
                    Some(1_700_000_000_100),
                ),
            ],
        ),
        (
            "failing-unknown",
            vec![
                (
                    "sundial",
                    SupervisorHealthStatus::Failing,
                    Some("Storage unavailable"),
                    None,
                    Some(1_700_000_000_200),
                ),
                (
                    "raincloud",
                    SupervisorHealthStatus::Unknown,
                    None,
                    None,
                    None,
                ),
            ],
        ),
        (
            "disabled",
            vec![(
                "moonstone",
                SupervisorHealthStatus::Unknown,
                None,
                None,
                None,
            )],
        ),
    ];
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/supervisor-health-v1");
    for (name, examples) in examples {
        let health = examples
            .iter()
            .map(
                |(id, status, detail, metrics, probe)| SupervisorHealthEntry {
                    module_id: (*id).to_owned(),
                    status: *status,
                    detail: detail.map(str::to_owned),
                    metrics: metrics.clone(),
                    consecutive_failures: 0,
                    late_answer_count: 0,
                    last_late_answer_latency_ms: None,
                    last_action: matches!(
                        status,
                        SupervisorHealthStatus::Degraded | SupervisorHealthStatus::Failing
                    )
                    .then(|| "report".to_owned()),
                    last_action_ms: if matches!(
                        status,
                        SupervisorHealthStatus::Degraded | SupervisorHealthStatus::Failing
                    ) {
                        *probe
                    } else {
                        None
                    },
                    last_probe_ms: *probe,
                },
            )
            .collect();
        let list = examples
            .iter()
            .map(|(id, status, _, _, probe)| {
                let enabled = name != "disabled";
                let (path, image, spec) = if enabled {
                    (
                        ReloadPathAgreement::Match,
                        RunningImageAgreement::Unavailable {
                            reason: RunningImageUnavailableReason::NotYetMeasured,
                        },
                        LaunchSpecAgreement::Match,
                    )
                } else {
                    (
                        ReloadPathAgreement::Unavailable {
                            reason: ReloadPathUnavailableReason::NotRunning,
                        },
                        RunningImageAgreement::Unavailable {
                            reason: RunningImageUnavailableReason::NotRunning,
                        },
                        LaunchSpecAgreement::Unavailable {
                            reason: LaunchSpecUnavailableReason::NotRunning,
                        },
                    )
                };
                let mut entry = SupervisorEntry::new(
                    *id,
                    if enabled { "running" } else { "disabled" },
                    enabled,
                    enabled,
                    *status,
                )
                .with_launch_nonce_env(Some(false))
                .with_pending_reload(Some(
                    PendingReloadVerdict::new(path, image).with_spec(Some(spec)),
                ))
                .with_last_probe_ms(*probe)
                .with_restart_count(Some(0))
                .with_max_restarts(Some(5))
                .with_lifetime_restarts(Some(0))
                .with_spawn_generation(Some(u64::from(enabled)))
                .with_restart_window_secs(Some(60))
                .with_drain_timeout_ms(Some(5000))
                .with_restart_backoff_ms(Some(1000))
                .with_restart_max_backoff_ms(Some(30000))
                .with_resources(Some(ChildResourceUsage::Unavailable {
                    reason: if enabled {
                        ChildResourceUnavailableReason::Unreadable
                    } else {
                        ChildResourceUnavailableReason::NotRunning
                    },
                }));
                entry.tree_unavailable = Some(
                    if enabled {
                        "not_yet_measured"
                    } else {
                        "not_running"
                    }
                    .to_owned(),
                );
                entry
            })
            .collect();
        for (op, request, response) in [
            (
                "health",
                ClientControlRequest::SupervisorHealth {},
                ClientControlResponse::SupervisorHealth {
                    generation: 7,
                    modules: health,
                },
            ),
            (
                "list",
                ClientControlRequest::SupervisorList {},
                ClientControlResponse::SupervisorList {
                    generation: 7,
                    modules: list,
                    daemon: None,
                    agent_commands: None,
                },
            ),
        ] {
            let request_bytes = serde_json::to_vec(&request).unwrap();
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                1,
                request_bytes.clone(),
            )
            .unwrap();
            let reply =
                control_response_body_frame(&frame, &response, "supervisor vector").unwrap();
            let mut sample = request_bytes;
            sample.push(b'\n');
            sample.extend_from_slice(&reply.body);
            sample.push(b'\n');
            let path = root.join(format!("{op}-{name}.jsonl"));
            if std::env::var_os("UPDATE_SUPERVISOR_HEALTH_VECTORS").is_some() {
                std::fs::write(&path, &sample).unwrap();
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), sample, "{}", path.display());
            }
        }
    }
}
