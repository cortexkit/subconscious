use super::*;

#[tokio::test]
async fn config_phone_process_exit_withdraws_the_pending_card_without_recording() {
    let mut fixture = Fixture::build(true).await;
    let apply = fixture.apply();
    let (correlation, _) = fixture.request().await;
    let pid = fixture.module.status().unwrap().pid.unwrap();
    assert!(std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.module.status().unwrap().last_exit.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the real supervisor must reap the killed consent process");
    let withdrawn = observe_withdrawal_before_cleanup(&mut fixture, apply, correlation).await;
    fixture.stop().await;
    assert!(
        withdrawn,
        "the actual process exit must withdraw the pending card before cleanup"
    );
}

#[tokio::test]
async fn config_phone_actual_cutover_withdraws_the_pending_card_without_recording() {
    let mut fixture = Fixture::build(true).await;
    let apply = fixture.apply();
    let (correlation, _) = fixture.request().await;
    let module = fixture.module.clone();
    let swap =
        tokio::spawn(async move { module.swap(Some(std::time::Duration::from_secs(5))).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !fixture.handler.supervisor.swap_open("callosum") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let candidate = ConnectionId::new(91);
    let mut manifest =
        subc_protocol::manifest::ModuleManifest::builder("callosum", "1.0.0").build();
    manifest.ready = Some(true);
    fixture
        .handler
        .registry
        .register_candidate_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            candidate,
            // Consent is independent of config.review; lifecycle invalidation may
            // not depend on the module participating in configuration delivery.
            vec![subc_protocol::consent::CONSENT_V1.into()],
        )
        .unwrap();
    let (sender, _candidate_frames) = mpsc::channel(8);
    fixture
        .handler
        .forwarding
        .register_candidate_module_connection(
            candidate,
            "callosum".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), swap)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .handler
            .registry
            .get_module("callosum")
            .unwrap()
            .unwrap()
            .connection_id,
        candidate
    );
    assert_eq!(
        fixture
            .handler
            .forwarding
            .active_module_endpoint("callosum")
            .unwrap()
            .unwrap()
            .connection_id,
        candidate
    );
    let withdrawn = observe_withdrawal_before_cleanup(&mut fixture, apply, correlation).await;
    fixture
        .handler
        .registry
        .deregister_connection(candidate)
        .unwrap();
    fixture.stop().await;
    assert!(
        withdrawn,
        "actual forwarding/registry cutover must withdraw the pending card before cleanup"
    );
}

async fn observe_withdrawal_before_cleanup(
    fixture: &mut Fixture,
    apply: tokio::task::JoinHandle<std::result::Result<ConfigControlResponse, GateError>>,
    correlation: u64,
) -> bool {
    // Observe the original endpoint before aborting or tearing down the fixture:
    // either cleanup would itself withdraw and could hide a missing callback.
    let withdrawn =
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.frames.recv()).await;
    let withdrawn = match withdrawn {
        Ok(Some(mut outbound)) => {
            let matches = outbound.frame.header.ty == FrameType::Cancel
                && outbound.frame.header.channel == 0
                && outbound.frame.header.corr == correlation;
            if let Some(flushed) = outbound.flushed.take() {
                let _ = flushed.send(());
            }
            matches
        }
        _ => false,
    };
    if !withdrawn {
        apply.abort();
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), apply)
        .await
        .unwrap();
    if withdrawn {
        assert_eq!(
            result.unwrap().unwrap_err().code,
            "approval_surface_unavailable"
        );
    }
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    withdrawn
}
