use super::*;
use crate::config_gate::runtime::RouteSettingsRig;
use crate::control::claims::tests::rig_with_module;
use crate::control::policy_root::VAULT;
use subc_protocol::scope::{ScopeKind, ScopeRecord, ScopeSelector, SCOPE_WORKDIR_CAPABILITY};

const MODULE: &str = RouteSettingsRig::MODULE;

fn configure_credentials(schema: &mut Value) {
    let credential = json!({"type":"object", "description":"Vault key",
        "x-ck-kind":"credential_ref", "x-ck-required":false});
    schema["properties"]["selected"] = credential.clone();
    schema["properties"]["list"] = json!({
        "type":"array", "description":"Other keys", "default":[],
        "items":{"type":"object", "unevaluatedProperties":false, "properties":{"key":credential}}
    });
}

fn approved_values() -> Value {
    json!({
        "selected":{"vault_ref":"z-user"},
        "list":[{"key":{"vault_ref":"a-nested"}},{"key":{"vault_ref":"z-user"}}]
    })
}

async fn setup(config: &RouteSettingsRig, capabilities: &[&str]) -> (Rig, Net, Net) {
    let mut rig = rig_with_module(MODULE, |handler| {
        handler
            .with_scope_authority_owners(vec![MODULE.into()])
            .with_config_runtime(config.owner().clone())
    })
    .await;
    let target = unsupervised_target(&rig, VAULT, capabilities).await;
    let opener = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    (rig, target, opener)
}

async fn bind(opener: &mut Net, target: &mut Net, target_id: &str) -> Value {
    let (bind, reply) = open(opener, target, target_id, false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    bind
}

fn credentials(bind: &Value) -> Option<&Value> {
    bind["caller"].get("settings_credentials")
}

fn assert_credentials(bind: &Value, generation: u64, refs: &[&str]) {
    assert_eq!(
        bind["principal"],
        json!({"kind":"reserved","module_id":MODULE})
    );
    assert_eq!(
        credentials(bind),
        Some(&json!({"generation":generation,"refs":refs})),
        "received bind: {bind}"
    );
}

#[tokio::test]
async fn settings_credentials_vault_bind_uses_approved_sorted_unique_refs() {
    let config = RouteSettingsRig::new(false, configure_credentials);
    config.write_user(approved_values());
    let (_rig, mut vault, mut opener) = setup(&config, &[CALLER_ATTESTATION_CAPABILITY]).await;
    let unstamped = bind(&mut opener, &mut vault, VAULT).await;
    assert_eq!(unstamped["caller"]["claim"], "own");
    assert!(credentials(&unstamped).is_none(), "{unstamped}");

    let generation = config.stamp();
    config.write_user(json!({"selected":{"vault_ref":"not-approved"}}));
    assert_credentials(
        &bind(&mut opener, &mut vault, VAULT).await,
        generation,
        &["a-nested", "z-user"],
    );
}

#[tokio::test]
async fn settings_credentials_stamped_schema_without_credentials_sends_empty_refs() {
    let config = RouteSettingsRig::new(false, |_| {});
    let (_rig, mut vault, mut opener) = setup(&config, &[CALLER_ATTESTATION_CAPABILITY]).await;
    let generation = config.stamp();
    assert_credentials(&bind(&mut opener, &mut vault, VAULT).await, generation, &[]);
}

#[tokio::test]
async fn settings_credentials_require_the_vault_opt_in_and_a_module_opener() {
    let config = RouteSettingsRig::new(false, configure_credentials);
    config.write_user(approved_values());
    let (rig, mut vault, mut opener) = setup(&config, &[CALLER_ATTESTATION_CAPABILITY]).await;
    let mut other =
        unsupervised_target(&rig, "other-target", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let mut direct = Net::new(rig.router.clone()).await;
    let (plain_rig, mut plain_vault, mut plain_opener) = setup(&config, &[]).await;
    let generation = config.stamp();

    let other = bind(&mut opener, &mut other, "other-target").await;
    assert_eq!(other["caller"]["claim"], "own");
    assert!(credentials(&other).is_none(), "{other}");
    let direct = bind(&mut direct, &mut vault, VAULT).await;
    assert_eq!(direct["principal"], json!({"kind":"direct"}));
    assert!(credentials(&direct).is_none(), "{direct}");
    let unopted = bind(&mut plain_opener, &mut plain_vault, VAULT).await;
    assert!(unopted.get("caller").is_none(), "{unopted}");
    drop(plain_rig);
    assert_credentials(
        &bind(&mut opener, &mut vault, VAULT).await,
        generation,
        &["a-nested", "z-user"],
    );
}

#[tokio::test]
async fn settings_credentials_project_bind_uses_the_approved_project_or_user_fallback() {
    let config = RouteSettingsRig::new(true, configure_credentials);
    config.write_user(approved_values());
    let (mut rig, mut vault, mut opener) = setup(
        &config,
        &[CALLER_ATTESTATION_CAPABILITY, SCOPE_WORKDIR_CAPABILITY],
    )
    .await;
    let path = config.write_project("project:approved", json!({"limit":2}));
    let scopes = ["project:approved", "project:fallback"]
        .into_iter()
        .map(|name| {
            ScopeRecord::new(name, 1, ScopeKind::Head)
                .with_workdir(Some(path.to_str().unwrap().into()))
                .with_project_id(Some(name.into()))
        })
        .collect();
    let synced = rig
        .primary
        .call(
            &rig.router,
            request(ModuleControlRequestFromModule::ScopeSync {
                generation: 1,
                scopes,
            }),
        )
        .await;
    assert_eq!(synced.header.ty, FrameType::Response, "{}", text(&synced));
    config.stamp();
    let project_generation = config.approve_projects().await;
    // Project delivery retains the user settings it was reviewed with, rather
    // than substituting a newer user approval into the old project view.
    config.write_user(json!({"selected":{"vault_ref":"new-user"}}));
    let user_generation =
        RouteSettingsRig::finish_user_approval(config.owner(), config.begin_user_approval());
    assert!(user_generation > project_generation);

    for (name, generation, refs) in [
        (
            "project:approved",
            project_generation,
            &["a-nested", "z-user"][..],
        ),
        ("project:fallback", user_generation, &["new-user"][..]),
    ] {
        let selector = ScopeSelector::new(
            Principal::Reserved {
                module_id: MODULE.into(),
            },
            name,
            Some(1),
        );
        opener
            .send(open_frame_scoped(VAULT, false, None, Some(selector)))
            .await;
        let frame = next_bind(&mut vault).await;
        ack(&mut vault, &frame).await;
        let reply = open_reply(&mut opener).await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
        let bind = body(&frame);
        assert_eq!(bind["scope"]["project_id"], name, "{bind}");
        assert_credentials(&bind, generation, refs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_credentials_approval_replaces_generation_and_refs_atomically() {
    let config = RouteSettingsRig::new(false, configure_credentials);
    config.write_user(approved_values());
    let (_rig, mut vault, mut opener) = setup(&config, &[CALLER_ATTESTATION_CAPABILITY]).await;
    let old = config.stamp();
    config.write_user(json!({"selected":{"vault_ref":"new-key"}}));
    let attempt = config.begin_user_approval();

    let (reached, captured) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let owner = config.owner().clone();
    owner.set_polled_capture_hook(move || {
        reached.send(()).unwrap();
        let _ = resumed.recv();
    });
    opener.send(open_frame(VAULT, false, None)).await;
    tokio::time::timeout(Duration::from_secs(5), captured)
        .await
        .expect("the bind must capture the published settings")
        .unwrap();
    let approving = tokio::task::spawn_blocking(move || {
        RouteSettingsRig::finish_user_approval(&owner, attempt)
    });
    let result = tokio::time::timeout(Duration::from_secs(5), approving).await;
    resume.send(()).unwrap();
    let new = result
        .expect("the bind's capture must not lock out approval")
        .unwrap();
    assert!(new > old);

    // Approval committed after capture, but before the vault received the bind.
    let pending = next_bind(&mut vault).await;
    assert_credentials(&body(&pending), old, &["a-nested", "z-user"]);
    ack(&mut vault, &pending).await;
    let reply = open_reply(&mut opener).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    // An approval committed before capture supplies both fields to the next bind.
    assert_credentials(
        &bind(&mut opener, &mut vault, VAULT).await,
        new,
        &["new-key"],
    );
}
