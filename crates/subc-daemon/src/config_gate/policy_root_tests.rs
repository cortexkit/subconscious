use std::sync::atomic::Ordering;

use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use subc_control::policy_root::RecoverPolicyRoot;
use subc_protocol::policy_root::{fingerprint, key_id, RotatePolicyRoot};

use super::*;
use crate::policy_root::{Key, Move};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn key(seed: u8) -> Key {
    let raw = SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes();
    Key {
        key_id: key_id(&raw),
        public_key_hex: hex(&raw),
    }
}

#[test]
fn policy_root_review_rejects_alternate_point_encoding() {
    let rig = scratch();
    let daemon = owner(&rig, "aliased-point");
    // y = 3 + (2^255 - 19): this decodes to the non-weak point at y = 3,
    // but hashes to a different key id from its canonical encoding.
    let mut alias = [0xff; 32];
    alias[0] = 0xf0;
    alias[31] = 0x7f;
    let pin = Key {
        key_id: key_id(&alias),
        public_key_hex: hex(&alias),
    };
    assert_eq!(
        daemon.plan_policy_root(Move::Set(pin)).unwrap_err().code,
        "policy_root_not_canonical"
    );
}

#[test]
fn policy_root_review_refuses_a_deleted_ledger_even_with_no_section() {
    let rig = scratch();
    std::fs::create_dir(rig.path.join("policy-root")).unwrap();
    let daemon = owner(&rig, "prior-ledger");
    assert!(pin(&daemon).is_none());
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(1))).unwrap_err().code,
        "policy_root_ledger_unavailable"
    );
}

#[test]
fn policy_root_review_replaces_a_malformed_or_null_section() {
    for (index, malformed) in [json!(null), json!(["not a key"])].into_iter().enumerate() {
        let rig = scratch();
        let config = json!({"version":1,"port":47612,"agent_policy_root":malformed});
        let path = rig.path.join("subc.jsonc");
        std::fs::write(&path, config.to_string()).unwrap();
        let parsed = crate::daemon_config::load(&path).unwrap().unwrap();
        assert_eq!(parsed.port, Some(47612));
        assert_eq!(parsed.agent_policy_root, Err("policy_root_not_canonical"));
        let daemon = owner(&rig, &format!("malformed-{index}"));
        assert!(pin(&daemon).is_none());
        let pending = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
        daemon
            .apply_unstamped_edits_after_presence(&pending.digest, true, 12, || Ok(()))
            .unwrap();
        assert_eq!(pin(&daemon).unwrap().key_id, key(1).key_id);
        assert_eq!(
            crate::daemon_config::load(&path).unwrap().unwrap().port,
            Some(47612)
        );
    }
}

#[test]
fn policy_root_review_refuses_relative_data_home() {
    let rig = scratch();
    let daemon = rig
        .owner("relative-home")
        .with_policy_root_data("target/scratch/relative-ledger-home".into());
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(1))).unwrap_err().code,
        "policy_root_ledger_unavailable"
    );
}

#[test]
fn policy_root_review_stamped_checkpoint_refuses_shorter_and_rewritten_ledger() {
    let rig = scratch();
    let daemon = owner(&rig, "ledger-anchor");
    rig.stamp(&daemon);
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&daemon, &first.digest).unwrap();
    let movement = daemon.plan_policy_root(recover(&key(1), &key(2))).unwrap();
    apply(&daemon, &movement.digest).unwrap();
    let (generations, _) = daemon.history().unwrap();
    assert_eq!(
        generations.last().unwrap().changeset.apply_plan["policy_root_ledger"]["row_count"],
        1
    );
    let path = rig.path.join("policy-root/ledger.json");
    let original = std::fs::read(&path).unwrap();
    let mut rewritten: serde_json::Value = serde_json::from_slice(&original).unwrap();
    rewritten[0]["unix_secs"] = json!(999);
    for invalid in [b"[]".to_vec(), serde_json::to_vec(&rewritten).unwrap()] {
        std::fs::write(&path, invalid).unwrap();
        let (view, problem) = daemon.read_policy_root().unwrap();
        assert!(view.pin.is_none());
        assert!(problem.unwrap().contains("policy-root/ledger.json"));
        assert_eq!(
            daemon
                .plan_policy_root(recover(&key(2), &key(3)))
                .unwrap_err()
                .code,
            "policy_root_ledger_unavailable"
        );
    }
    std::fs::write(&path, &original).unwrap();
    assert_eq!(pin(&daemon).unwrap().key_id, key(2).key_id);
}

#[test]
fn policy_root_review_stamp_retires_the_resolved_prior_key() {
    let rig = scratch();
    let daemon = owner(&rig, "stamp-after-drift");
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    daemon
        .apply_unstamped_edits_after_presence(&first.digest, true, 12, || Ok(()))
        .unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"agent_policy_root":key(2)}).to_string(),
    )
    .unwrap();
    rig.stamp(&daemon);
    assert_eq!(pin(&daemon).unwrap().key_id, key(2).key_id);
    assert_eq!(pin(&daemon).unwrap().retired_key_ids, [key(1).key_id]);
}

#[test]
fn policy_root_review_warm_recovery_stamp_retires_authenticated_prior_key() {
    let rig = scratch();
    let daemon = owner(&rig, "warm-recovery");
    rig.stamp(&daemon);
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&daemon, &first.digest).unwrap();
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"unverified",
    )
    .unwrap();
    std::fs::write(rig.path.join("subc.jsonc"), br#"{"version":1}"#).unwrap();
    assert!(daemon.check_recovery().unwrap());
    let generation = rig.stamp(&daemon);
    daemon.complete_authority_exit_for_test(generation).unwrap();
    assert!(pin(&daemon).is_none());
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(1))).unwrap_err().code,
        "policy_root_key_reused"
    );
}

#[test]
fn policy_root_review_restamp_retires_previous_approved_key() {
    let rig = scratch();
    let daemon = owner(&rig, "restamp");
    rig.stamp(&daemon);
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&daemon, &first.digest).unwrap();
    let unstamp = daemon
        .finish_plan(daemon.prepare(PlanKind::Unstamp).unwrap(), daemon_review)
        .unwrap();
    apply(&daemon, &unstamp.digest).unwrap();
    std::fs::write(rig.path.join("subc.jsonc"), br#"{"version":1}"#).unwrap();
    daemon.rescan_policy_root(Ok(None)).unwrap();
    rig.stamp(&daemon);
    assert!(pin(&daemon).is_none());
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(1))).unwrap_err().code,
        "policy_root_key_reused"
    );
    std::fs::remove_dir_all(rig.path.join("policy-root")).unwrap();
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(2))).unwrap_err().code,
        "policy_root_ledger_unavailable"
    );
}

#[test]
fn policy_root_review_stamped_section_is_history_even_without_a_checkpoint() {
    let rig = scratch();
    let daemon = owner(&rig, "section-history");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        br#"{"version":1,"agent_policy_root":null}"#,
    )
    .unwrap();
    rig.stamp(&daemon);
    assert!(!rig.path.join("policy-root").exists());
    assert_eq!(
        daemon.plan_policy_root(Move::Set(key(1))).unwrap_err().code,
        "policy_root_ledger_unavailable"
    );
}

#[test]
fn policy_root_review_cold_recovery_anchors_the_readable_ledger() {
    let rig = scratch();
    let daemon = owner(&rig, "before-cold-recovery");
    rig.stamp(&daemon);
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&daemon, &first.digest).unwrap();
    let second = daemon.plan_policy_root(recover(&key(1), &key(2))).unwrap();
    apply(&daemon, &second.digest).unwrap();
    drop(daemon);

    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"unverified",
    )
    .unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"agent_policy_root":key(3)}).to_string(),
    )
    .unwrap();
    let daemon = owner(&rig, "cold-recovery");
    let generation = rig.stamp(&daemon);
    daemon.complete_authority_exit_for_test(generation).unwrap();
    assert_eq!(pin(&daemon).unwrap().retired_key_ids, [key(1).key_id]);
    assert_eq!(pin(&daemon).unwrap().key_id, key(3).key_id);
    std::fs::write(rig.path.join("policy-root/ledger.json"), b"[]").unwrap();
    assert!(pin(&daemon).is_none());
}

#[test]
fn policy_root_review_refuses_a_small_order_pin_or_destination_and_noncanonical_s() {
    let rig = scratch();
    let daemon = owner(&rig, "strict-ed25519");
    let mut raw = [0; 32];
    raw[0] = 1;
    let weak = Key {
        key_id: key_id(&raw),
        public_key_hex: hex(&raw),
    };
    let path = rig.path.join("non-primary.jsonc");
    std::fs::write(
        &path,
        json!({"version":1,"agent_policy_root":weak}).to_string(),
    )
    .unwrap();
    assert_eq!(
        crate::daemon_config::load(&path)
            .unwrap()
            .unwrap()
            .agent_policy_root,
        Err("policy_root_not_canonical")
    );
    rig.stamp(&daemon);
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&daemon, &first.digest).unwrap();
    let a = key(1);
    assert_eq!(
        daemon
            .plan_policy_root(rotate(&a.key_id, &weak, 1, ""))
            .unwrap_err()
            .code,
        "policy_root_not_canonical"
    );
    let Move::Rotate(mut forged) = rotate(&a.key_id, &key(2), 1, "") else {
        unreachable!()
    };
    // Adding the group order to a valid scalar preserves the scalar modulo
    // the order but is not a canonical signature encoding.
    let order = "edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010";
    let mut carry = 0u16;
    let mut noncanonical = [0; 32];
    for (i, byte) in noncanonical.iter_mut().enumerate() {
        let old = u16::from_str_radix(&forged.signature_hex[64 + 2 * i..66 + 2 * i], 16).unwrap();
        let add = u16::from_str_radix(&order[2 * i..2 * i + 2], 16).unwrap();
        let sum = old + add + carry;
        *byte = sum as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0);
    forged
        .signature_hex
        .replace_range(64.., &hex(&noncanonical));
    assert_eq!(
        daemon
            .plan_policy_root(Move::Rotate(forged))
            .unwrap_err()
            .code,
        "policy_root_signature_invalid"
    );
    assert_eq!(pin(&daemon).unwrap().key_id, a.key_id);
}

#[cfg(unix)]
#[test]
fn policy_root_review_refuses_unsafe_or_malformed_ledger_files() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let rig = scratch();
    let daemon = owner(&rig, "ledger-file-rules");
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    daemon
        .apply_unstamped_edits_after_presence(&first.digest, true, 12, || Ok(()))
        .unwrap();
    let path = rig.path.join("policy-root/ledger.json");
    let backup = path.with_file_name("preserved-ledger.json");
    let unavailable = || {
        let (view, problem) = daemon.read_policy_root().unwrap();
        assert!(view.pin.is_none());
        assert!(problem.unwrap().contains("policy-root/ledger.json"));
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    unavailable();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(pin(&daemon).is_some());

    std::fs::rename(&path, &backup).unwrap();
    symlink(&backup, &path).unwrap();
    unavailable();
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&backup, &path).unwrap();
    unavailable();
    std::fs::remove_file(&backup).unwrap();
    assert!(pin(&daemon).is_some());

    std::fs::write(
        &path,
        br#"[{"key_id":"bad-id","departure":"retired","unix_secs":1}]"#,
    )
    .unwrap();
    unavailable();
}

#[test]
fn policy_root_review_unstamped_rescan_cannot_restore_a_ledgered_key() {
    let rig = scratch();
    let daemon = owner(&rig, "unstamped-override");
    let first = daemon.plan_policy_root(Move::Set(key(1))).unwrap();
    daemon
        .apply_unstamped_edits_after_presence(&first.digest, true, 12, || Ok(()))
        .unwrap();
    let next = daemon.plan_policy_root(recover(&key(1), &key(2))).unwrap();
    daemon
        .apply_unstamped_edits_after_presence(&next.digest, true, 13, || Ok(()))
        .unwrap();
    let ledger_path = rig.path.join("policy-root/ledger.json");
    let ledger = std::fs::read(&ledger_path).unwrap();
    let config_path = rig.path.join("subc.jsonc");
    std::fs::write(
        &config_path,
        json!({"version":1,"agent_policy_root":key(1)}).to_string(),
    )
    .unwrap();
    let config = crate::daemon_config::load(&config_path).unwrap().unwrap();
    daemon.rescan_policy_root(config.agent_policy_root).unwrap();
    assert!(pin(&daemon).is_none());
    assert_eq!(std::fs::read(&ledger_path).unwrap(), ledger);
}

fn scratch() -> Rig {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        b"{\n  // Keep this comment.\n  \"version\": 1,\n  \"modules\": {},\n}\n",
    )
    .unwrap();
    rig
}

fn owner(rig: &Rig, incarnation: &str) -> RuntimeOwner {
    rig.owner(incarnation)
        .with_policy_root_data(std::fs::canonicalize(&rig.path).unwrap())
}

fn rotate(from: &str, to: &Key, signer: u8, leading_space: &str) -> Move {
    let body = format!(
        r#"{leading_space}{{"from_key_id":"{from}","issued_at_unix_secs":7,"to_key_id":"{}","to_public_key_hex":"{}","v":"ck-policy-root-rotation/v1"}}"#,
        to.key_id, to.public_key_hex
    );
    let message = format!("ck-policy-root-rotation/v1\0{body}");
    let sig = SigningKey::from_bytes(&[signer; 32]).sign(message.as_bytes());
    Move::Rotate(RotatePolicyRoot {
        statement_b64: STANDARD.encode(body),
        signature_hex: hex(&sig.to_bytes()),
    })
}

fn recover(from: &Key, to: &Key) -> Move {
    let body = format!(
        r#"{{"issued_at_unix_secs":7,"retired_key_id":"{}","to_key_id":"{}","to_public_key_hex":"{}","v":"ck-policy-root-recovery/v1"}}"#,
        from.key_id, to.key_id, to.public_key_hex
    );
    Move::Recover(RecoverPolicyRoot {
        body_b64: STANDARD.encode(body),
    })
}

fn apply(owner: &RuntimeOwner, digest: &Digest) -> Result<u64> {
    let view = owner.plan_get(digest)?;
    owner.desktop_view_delivered(ConnectionId::new(7), &view)?;
    let attempt = owner.begin_approval(digest, &Rig::facts())?.attempt();
    owner.finish_approval(
        attempt,
        &Rig::facts(),
        ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
        12,
    )
}

fn pin(owner: &RuntimeOwner) -> Option<subc_protocol::policy_root::PolicyRootPin> {
    owner.read_policy_root().unwrap().0.pin
}

#[test]
fn policy_root_config_refuses_noncanonical_and_mismatched_sections() {
    let rig = scratch();
    let path = rig.path.join("subc.jsonc");
    for (key_id, public, error) in [
        (
            key(1).key_id,
            key(1).public_key_hex.to_uppercase(),
            "not canonical",
        ),
        (key(2).key_id, key(1).public_key_hex, "does not derive"),
        (
            key(1).key_id.to_uppercase(),
            key(1).public_key_hex,
            "not canonical",
        ),
    ] {
        let config =
            json!({"version":1,"agent_policy_root":{"key_id":key_id,"public_key_hex":public}});
        std::fs::write(&path, config.to_string()).unwrap();
        let (view, problem) = owner(&rig, "invalid").read_policy_root().unwrap();
        assert!(view.pin.is_none());
        assert!(problem.unwrap().contains(error));
    }
}

#[test]
fn policy_root_get_without_section_and_unstamped_apply_requires_presence() {
    let rig = scratch();
    let owner = owner(&rig, "unstamped");
    let (view, problem) = owner.read_policy_root().unwrap();
    assert_eq!(json!(view), json!({"pin":null,"generation":0}));
    assert!(problem.is_none());
    let pending = owner.plan_policy_root(Move::Set(key(1))).unwrap();
    assert_eq!(
        owner
            .apply_unstamped_edits(&pending.digest)
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    assert!(pin(&owner).is_none());
    assert!(!rig.path.join("policy-root/ledger.json").exists());
    assert_eq!(
        owner
            .apply_unstamped_edits_after_presence(&pending.digest, true, 12, || Ok(()))
            .unwrap(),
        Some(0)
    );
    assert_eq!(pin(&owner).unwrap().key_id, key(1).key_id);
    assert_eq!(
        owner.plan_policy_root(Move::Set(key(2))).unwrap_err().code,
        "policy_root_pin_exists"
    );
    assert_eq!(
        std::fs::read(rig.path.join("policy-root/ledger.json")).unwrap(),
        b"[]"
    );
}

#[test]
fn policy_root_rotation_and_recovery_preserve_ledger_and_refuse_replays() {
    let rig = scratch();
    let owner = owner(&rig, "moves");
    rig.stamp(&owner);
    let first = owner.plan_policy_root(Move::Set(key(1))).unwrap();
    assert!(pin(&owner).is_none());
    assert_eq!(apply(&owner, &first.digest).unwrap(), 2);
    let a = key(1);
    let b = key(2);
    let c = key(3);
    for (movement, code) in [
        (
            rotate(&a.key_id, &b, 2, ""),
            "policy_root_signature_invalid",
        ),
        (rotate(&c.key_id, &b, 1, ""), "policy_root_wrong_from_key"),
        (rotate(&a.key_id, &b, 1, " "), "policy_root_not_canonical"),
        (rotate(&a.key_id, &a, 1, ""), "policy_root_key_reused"),
    ] {
        assert_eq!(owner.plan_policy_root(movement).unwrap_err().code, code);
    }
    let pending = owner
        .plan_policy_root(rotate(&a.key_id, &b, 1, ""))
        .unwrap();
    assert_eq!(pin(&owner).unwrap().key_id, a.key_id);
    assert_eq!(
        owner.plan_policy_root(recover(&a, &c)).unwrap_err().code,
        "policy_root_change_pending"
    );
    let view = owner.plan_get(&pending.digest).unwrap();
    let row = &view.plan.changeset.policy_changes[0];
    let old = fingerprint(&SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
    let new = fingerprint(&SigningKey::from_bytes(&[2; 32]).verifying_key().to_bytes());
    assert_eq!(
        row["summary"],
        format!("Replace the agent policy trust root. Old: {old}. New: {new}.")
    );
    assert_eq!(apply(&owner, &pending.digest).unwrap(), 3);
    assert!(pin(&owner).unwrap().retired_key_ids.is_empty());
    assert_eq!(
        owner
            .plan_policy_root(rotate(&b.key_id, &a, 2, ""))
            .unwrap_err()
            .code,
        "policy_root_key_reused"
    );
    let recovery = owner.plan_policy_root(recover(&b, &c)).unwrap();
    assert_eq!(apply(&owner, &recovery.digest).unwrap(), 4);
    assert_eq!(
        pin(&owner).unwrap().retired_key_ids,
        std::slice::from_ref(&b.key_id)
    );
    assert_eq!(
        owner
            .plan_policy_root(rotate(&c.key_id, &b, 3, ""))
            .unwrap_err()
            .code,
        "policy_root_key_reused"
    );
    assert_eq!(
        owner
            .plan_policy_root(rotate(&a.key_id, &b, 1, ""))
            .unwrap_err()
            .code,
        "policy_root_wrong_from_key"
    );
    let text = std::fs::read_to_string(rig.path.join("subc.jsonc")).unwrap();
    assert!(text.contains("// Keep this comment."));
    assert!(text.contains(&c.key_id));
    assert!(!text.contains("retired_key_ids"));
}

#[test]
fn policy_root_rollback_preserves_key_and_missing_ledger_fails_closed() {
    let rig = scratch();
    let owner = owner(&rig, "rollback");
    rig.stamp(&owner);
    let first = owner.plan_policy_root(Move::Set(key(1))).unwrap();
    apply(&owner, &first.digest).unwrap();
    let recovery = owner.plan_policy_root(recover(&key(1), &key(2))).unwrap();
    apply(&owner, &recovery.digest).unwrap();
    let rollback = owner
        .finish_plan(owner.prepare(PlanKind::Rollback(2)).unwrap(), daemon_review)
        .unwrap();
    apply(&owner, &rollback.digest).unwrap();
    assert_eq!(pin(&owner).unwrap().key_id, key(2).key_id);
    assert_eq!(pin(&owner).unwrap().retired_key_ids, [key(1).key_id]);
    std::fs::remove_file(rig.path.join("policy-root/ledger.json")).unwrap();
    let (view, problem) = owner.read_policy_root().unwrap();
    assert!(view.pin.is_none());
    assert!(problem.unwrap().contains("policy-root/ledger.json"));
    assert_eq!(
        owner.plan_policy_root(Move::Set(key(3))).unwrap_err().code,
        "policy_root_ledger_unavailable"
    );
}

#[test]
fn policy_root_stamped_refuses_raw_config_plan_changes() {
    let rig = scratch();
    let owner = owner(&rig, "raw-edits");
    rig.stamp(&owner);
    let edit: subc_control::config::ConfigEdit = serde_json::from_value(json!({
        "module_id":"subc", "layer":"user", "path":"/agent_policy_root", "set":key(1),
    }))
    .unwrap();
    let edits = owner
        .prepare_edits(BTreeMap::from([("subc".into(), 0)]), vec![edit])
        .unwrap();
    let refused = owner.finish_plan(edits, daemon_review).unwrap();
    assert_eq!(refused.refusals[0].code, "value_invalid");
    assert_eq!(refused.refusals[0].path, "/agent_policy_root");

    let config = json!({"version":1,"modules":{},"agent_policy_root":key(1)});
    std::fs::write(rig.path.join("subc.jsonc"), config.to_string()).unwrap();
    let capture = owner.prepare(PlanKind::Change).unwrap();
    assert_eq!(
        owner.finish_plan(capture, daemon_review).unwrap_err().code,
        "value_invalid"
    );
    assert!(pin(&owner).is_none());
}

#[test]
fn policy_root_retry_after_ledger_append_and_interrupted_restart() {
    let rig = scratch();
    let daemon = owner(&rig, "before-restart");
    rig.stamp(&daemon);
    let a = key(1);
    let b = key(2);
    let c = key(3);
    let d = key(4);
    let first = daemon.plan_policy_root(Move::Set(a.clone())).unwrap();
    apply(&daemon, &first.digest).unwrap();
    let second = daemon
        .plan_policy_root(rotate(&a.key_id, &b, 1, ""))
        .unwrap();
    daemon
        .policy_root_fail_after_append
        .store(true, Ordering::Release);
    assert_eq!(
        apply(&daemon, &second.digest).unwrap_err().code,
        "publication_failed"
    );
    assert!(pin(&daemon).is_none());
    assert!(daemon
        .read_policy_root()
        .unwrap()
        .1
        .unwrap()
        .contains("interrupted"));
    let ledger = rig.path.join("policy-root/ledger.json");
    let after_append = std::fs::read(&ledger).unwrap();
    apply(&daemon, &second.digest).unwrap();
    assert_eq!(pin(&daemon).unwrap().key_id, b.key_id);
    assert_eq!(std::fs::read(&ledger).unwrap(), after_append);

    let third = daemon.plan_policy_root(recover(&b, &c)).unwrap();
    daemon
        .policy_root_fail_after_append
        .store(true, Ordering::Release);
    assert_eq!(
        apply(&daemon, &third.digest).unwrap_err().code,
        "publication_failed"
    );
    drop(daemon);
    let daemon = owner(&rig, "after-restart");
    assert!(pin(&daemon).is_none());
    assert!(daemon
        .read_policy_root()
        .unwrap()
        .1
        .unwrap()
        .contains("interrupted"));
    assert_eq!(
        daemon.plan_policy_root(recover(&b, &d)).unwrap_err().code,
        "policy_root_key_reused"
    );
    let resumed = daemon.plan_policy_root(recover(&b, &c)).unwrap();
    assert_ne!(resumed.digest, third.digest);
    apply(&daemon, &resumed.digest).unwrap();
    assert_eq!(pin(&daemon).unwrap().key_id, c.key_id);
    assert_eq!(pin(&daemon).unwrap().retired_key_ids, [b.key_id]);
}
