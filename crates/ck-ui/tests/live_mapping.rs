//! Each live source's reply-to-snapshot mapping, against recorded replies in
//! the shapes the daemon and the modules send: refusals, fields older builds
//! leave out, CPU across a process change, engram's upload state, agents
//! whose state can't be read, and usage rows that carry an error.

use std::collections::HashMap;

use ck_ui::live::map::{self, CpuTracker};
use ck_ui::live::{action_progress, assemble, LiveProcess, State};
use ck_ui::model::{ActionKind, ActionPhase, LogLevel, Status};
use ck_ui::time::Timestamp;
use serde_json::{json, Value};
use subc_control::{SupervisorHealthEntry, SupervisorHealthStatus};

const NOW: i64 = 1_800_000_000_000;

fn now() -> Timestamp {
    Timestamp::from_unix_millis(NOW)
}

// ---- CPU -------------------------------------------------------------------------

#[test]
fn cpu_percent_is_cpu_time_over_wall_time_for_one_process() {
    let mut t = CpuTracker::default();
    t.record("broca", Some(10), 0, 1_000, 100 << 20);
    // One reading: memory is known, CPU needs a second reading.
    assert_eq!(t.current("broca"), (None, Some(100.0)));
    t.record("broca", Some(10), 2_000, 1_500, 120 << 20);
    assert_eq!(t.current("broca"), (Some(25.0), Some(120.0)));
}

#[test]
fn cpu_percent_is_never_computed_across_a_pid_change() {
    let mut t = CpuTracker::default();
    t.record("broca", Some(10), 0, 50_000, 100 << 20);
    t.record("broca", Some(10), 2_000, 51_000, 100 << 20);
    assert_eq!(t.current("broca").0, Some(50.0));
    // A new process: its counter starts near zero. Differencing it against
    // the old process's would give a negative or wild figure; instead the
    // series starts over, with no CPU reading and no old history.
    t.record("broca", Some(11), 4_000, 200, 30 << 20);
    assert_eq!(t.current("broca"), (None, Some(30.0)));
    assert_eq!(t.history("broca"), (vec![], vec![30.0]));
    t.record("broca", Some(11), 6_000, 400, 30 << 20);
    assert_eq!(t.current("broca").0, Some(10.0));
}

#[test]
fn cpu_history_keeps_ten_minutes() {
    let mut t = CpuTracker::default();
    for i in 0..=330 {
        t.record("m", Some(1), i * 2_000, (i * 100) as u64, 1 << 20);
    }
    let (cpu, mem) = t.history("m");
    // 660 s of readings every 2 s: only the last 600 s (301 readings) stay.
    assert_eq!(mem.len(), 301);
    assert_eq!(cpu.len(), 301);
    assert!(cpu.iter().all(|c| (*c - 5.0).abs() < 1e-9));
}

#[test]
fn a_counter_that_goes_backwards_gives_no_reading() {
    assert_eq!(map::cpu_percent((0, 500), (1_000, 400)), None);
    assert_eq!(map::cpu_percent((1_000, 500), (1_000, 600)), None);
}

// ---- usage -----------------------------------------------------------------------

fn usage_reply() -> Value {
    json!({ "result": [
        {
            "provider": "claude",
            "account": "acct-1",
            "accountInfo": { "email": "alex@example.com" },
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300, "resetsAt": "2027-01-15T10:00:00Z" },
                "secondary": { "usedPercent": 40.0, "rawUsedPercent": 60.0, "windowMinutes": 10080, "resetsAt": "2027-01-16T08:00:00.123+02:00" }
            },
            "fetchedAt": "2027-01-15T07:59:00Z"
        },
        {
            "provider": "claude",
            "accountInfo": { "email": "sam@example.org" },
            "error": "token expired",
            "errorClass": "credential_expired"
        },
        {
            "provider_id": "codex",
            "account": "0f1e2d3c-aaaa-bbbb-cccc-000000000000",
            "error": "upstream returned 500",
            "errorClass": "upstream_failed"
        },
        { "id": "gemini", "errorClass": "credential_absent" }
    ]})
}

#[test]
fn usage_sums_readable_accounts_and_lists_failing_ones_without_a_share() {
    let providers = map::usage(&usage_reply(), now()).unwrap();
    let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
    // gemini has nothing configured: not a failure, so not listed at all.
    assert_eq!(names, ["Claude", "Codex"]);

    let claude = &providers[0];
    // The main window is the longest one, read from its raw percentage.
    assert_eq!(claude.window, "week");
    assert_eq!(claude.used, Some(0.6));
    assert_eq!(claude.of, 1);
    assert_eq!(claude.unit, "account");
    assert_eq!(claude.error, None);
    assert_eq!(
        claude.resets_at,
        Some(map::parse_rfc3339("2027-01-16T06:00:00.123Z").unwrap())
    );
    assert_eq!(claude.accounts.len(), 2);
    assert_eq!(claude.accounts[1].who, "sam@example.org");
    assert_eq!(
        claude.accounts[1].error.as_deref(),
        Some("credential_expired")
    );
    assert!(claude.accounts[1].windows.is_empty());

    // Every account failing: an error, never a share of 0.
    let codex = &providers[1];
    assert_eq!(codex.used, None);
    assert_eq!(codex.error.as_deref(), Some("upstream_failed"));
    assert_eq!(codex.accounts[0].who, "0f1e2d3c");
}

#[test]
fn a_usage_refusal_is_its_code() {
    let reply = json!({ "error": { "code": "usage_unavailable", "message": "starting" } });
    assert_eq!(map::usage(&reply, now()).unwrap_err(), "usage_unavailable");
    assert!(map::usage(&json!({ "result": {} }), now()).is_err());
}

// ---- build server, backups, models -------------------------------------------------

#[test]
fn motor_metrics_nested_or_dotted_with_history_gaps() {
    let nested = json!({
        "build_server": { "permits_held": 80, "permits_cap": 80, "queue_depth": 21 },
        "queue_wait_1h": { "p90_ms": 180_000 },
        "sample_age_ms": 900,
        "history": [
            { "minute_ms": 60_000, "permits_held": 40, "permits_cap": 80, "queue_depth": 0 },
            // Two minutes missing: gaps, not zeros.
            { "minute_ms": 240_000, "permits_held": 80, "permits_cap": 80, "queue_depth": 3 }
        ]
    });
    let b = map::build_server(Some(&nested), &[1.0]);
    assert_eq!((b.busy, b.slots, b.queued), (Some(80), Some(80), Some(21)));
    assert_eq!(b.wait_secs, Some(180.0));
    assert_eq!(b.load_history.len(), 4);
    assert_eq!(b.load_history[0], 50.0);
    assert!(b.load_history[1].is_nan() && b.load_history[2].is_nan());
    assert_eq!(b.load_history[3], 100.0);

    let dotted = json!({ "build_server.permits_held": 3, "build_server.permits_cap": 8 });
    let b = map::build_server(Some(&dotted), &[37.5, 12.5]);
    assert_eq!((b.busy, b.slots, b.queued), (Some(3), Some(8), None));
    // No history from an older motor: ck's own samples.
    assert_eq!(b.load_history, [37.5, 12.5]);
    assert_eq!(b.wait_secs, None);
}

#[test]
fn engram_uploading_only_in_flight_with_no_failure_on_record() {
    let detail =
        "Cloud backup paused: account storage limit reached (102.7 of 103.1 GB); 3 days behind";
    let failing = json!({
        "publishStaleness": "behind",
        "lastPublishUnix": 1_799_740_800,
        "publishing": { "state": "in_flight" },
        "lastPublishFailure": { "code": "quota_exceeded", "at": 1_799_990_000, "usedBytes": 102_700_000_000u64 }
    });
    let b = map::backups(Some(&failing), Some(detail));
    assert!(
        !b.uploading,
        "a failure on record means nothing is uploading"
    );
    assert_eq!(b.detail.as_deref(), Some(detail));
    assert_eq!(b.status, Status::Degraded);
    assert_eq!(
        b.last_upload_at,
        Some(Timestamp::from_unix_millis(1_799_740_800_000))
    );

    // The same in-flight publish once the failure has been cleared.
    let mut cleared = failing.clone();
    cleared["lastPublishFailure"] = Value::Null;
    assert!(map::backups(Some(&cleared), Some(detail)).uploading);

    // An older engram doesn't send the field: in flight is uploading.
    let older = json!({ "publishStaleness": "fresh", "publishing": { "state": "in_flight" } });
    let b = map::backups(Some(&older), None);
    assert!(b.uploading);
    assert_eq!(b.status, Status::Ok);
    assert_eq!(b.last_upload_at, None);

    let idle = json!({ "publishStaleness": "stale", "publishing": { "state": "idle" } });
    assert!(!map::backups(Some(&idle), None).uploading);
}

#[test]
fn fusiform_counts_are_unknown_until_counted() {
    let probed = Some(now());
    let counted =
        json!({ "models_priced": 10_258, "model_count": 10_400, "observation_age_ms": 240_000 });
    assert_eq!(
        map::models(Some(&counted), probed),
        (Some(10_258), Some(now().add_millis(-240_000)))
    );
    let not_yet = json!({ "models_priced": null, "observation_age_ms": 5 });
    assert_eq!(map::models(Some(&not_yet), probed).0, None);
    let older = json!({ "observation_age_ms": 5 });
    assert_eq!(map::models(Some(&older), probed).0, None);
    assert_eq!(map::models(None, probed), (None, None));
}

// ---- devices -----------------------------------------------------------------------

#[test]
fn devices_online_means_an_established_tunnel_and_refusals_keep_their_code() {
    let reply = json!({
        "devices": [
            { "name": "CortexKit Mac 2", "fingerprint": "f1", "device_class": "mac", "trust": "verified",
              "tunnel": "established", "carrier": "direct", "since_unix_ms": NOW - 9_000,
              "last_seen_unix_ms": NOW - 2_000, "subscriptions_live": 0 },
            { "name": "iPhone", "fingerprint": "f2", "device_class": "phone", "trust": "verified",
              "tunnel": "closed:idle", "since_unix_ms": NOW - 9_000,
              "last_seen_unix_ms": null, "subscriptions_live": 3 },
            { "name": "Old", "fingerprint": "f3", "device_class": "sim", "trust": "tombstoned",
              "tunnel": "closed:revoked", "since_unix_ms": 0, "subscriptions_live": 0 }
        ],
        "self": { "listen_addr": "0.0.0.0:1", "lan_addrs": [], "rendezvous": null,
                  "wan_reachability": "unknown", "wan_port_mapping": null, "watch_ops_offered": [] }
    });
    let d = map::devices(&reply, now()).unwrap();
    assert_eq!(d.online, 1);
    assert_eq!(d.list.len(), 2);
    assert_eq!(d.list[0].short_name, "Mac 2");
    assert_eq!(d.list[0].status, Status::Ok);
    assert_eq!(d.list[0].seen, "now");
    assert_eq!(d.list[1].status, Status::Stopped);
    assert_eq!(d.list[1].seen, "unknown");
    assert_eq!(d.list[1].feeds, "3 live feeds");
    for code in [
        "fed_not_authorized",
        "fed_bad_request",
        "fed_not_ready",
        "fed_not_exposed",
    ] {
        let refusal = json!({ "error": { "code": code } });
        assert_eq!(map::devices(&refusal, now()).unwrap_err(), code);
    }
}

// ---- agents ------------------------------------------------------------------------

fn identity_reply() -> Value {
    json!({ "result": {
        "agents": [
            { "agentId": "a1", "displayName": "ARGUS", "role": "w", "tag": "", "labels": [],
              "project": { "id": "p1", "name": "web-app" }, "workspace": { "id": "w", "name": "w" } },
            { "agentId": "a2", "displayName": "IRIS", "role": "w", "tag": "", "labels": [],
              "project": { "id": "p2", "name": "api-server" }, "workspace": { "id": "w", "name": "w" } },
            { "agentId": "a3", "displayName": "NOX", "role": "w", "tag": "", "labels": [],
              "project": { "id": "p2", "name": "api-server" }, "workspace": { "id": "w", "name": "w" } }
        ],
        "token": "t", "generation": 1, "incarnation": "i", "logPosition": 4, "unchanged": false
    }})
}

#[test]
fn agents_join_state_and_place_on_exact_ids() {
    let ids = map::agent_identities(&identity_reply()).unwrap();
    let residences = map::agent_residences(&json!({
        "token": "t", "unchanged": false,
        "agents": [
            { "agentId": "a1", "residence": { "harness": "h", "state": "s" }, "activity": "busy",
              "board": { "statusText": "fixing the build" } },
            { "agentId": "a2", "residence": null, "activity": "idle", "activityAtMs": NOW - 240_000 },
            { "agentId": "a3", "residence": null, "activity": "unknown" },
            { "agentId": "A1", "residence": null, "activity": "busy" }
        ]
    }))
    .unwrap();
    let places = map::agent_places(
        &json!({ "result": { "agents": [
            { "agentId": "a1", "status": "held_here", "known": true, "stale": false },
            { "agentId": "a2", "status": "held_elsewhere", "holder_name": "Mac 2", "known": true, "stale": false },
            { "agentId": "a3", "status": "unclaimed", "known": true, "stale": false }
        ]}}),
        "This Mac",
    )
    .unwrap();
    let counts = map::fleet_counts(&json!({ "result": {
        "workspaces": [{}, {}], "projects": 35, "generation": 1, "incarnation": "i"
    }}))
    .unwrap();
    let a = map::agents(&ids, Some(&residences), Some(&places), counts, now());
    let rows: Vec<(&str, &str, &str)> = a
        .list
        .iter()
        .map(|x| (x.name.as_str(), x.state.as_str(), x.machine.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("ARGUS", "working", "This Mac"),
            ("IRIS", "idle 4m", "Mac 2"),
            ("NOX", "unknown", "unclaimed"),
        ]
    );
    assert_eq!(a.list[0].doing.as_deref(), Some("fixing the build"));
    assert_eq!(a.working, 1);
    assert_eq!(a.machines, Some(2));
    assert_eq!((a.workspaces, a.projects), (Some(2), Some(35)));
}

#[test]
fn without_state_every_agent_is_unknown_never_idle() {
    let ids = map::agent_identities(&identity_reply()).unwrap();
    let refused = json!({ "error": { "code": "principal_not_permitted" } });
    assert_eq!(
        map::agent_residences(&refused).unwrap_err(),
        "principal_not_permitted"
    );
    let a = map::agents(&ids, None, None, (None, None), now());
    assert!(a
        .list
        .iter()
        .all(|x| x.state == "unknown" && x.machine == "unknown"));
    assert_eq!(a.working, 0);
    assert_eq!(a.machines, None);
}

#[test]
fn a_refused_residence_source_shows_its_code_on_the_agents_panel() {
    let mut s = State::empty("This Mac");
    s.identities
        .set(map::agent_identities(&identity_reply()).unwrap(), now());
    s.residences.fail("principal_not_permitted".into());
    let snap = assemble(&s, now());
    assert!(snap.agents.loaded);
    assert_eq!(
        snap.agents.error.as_deref(),
        Some("state principal_not_permitted")
    );
    assert!(snap.agents.data.list.iter().all(|a| a.state == "unknown"));
}

#[test]
fn a_module_that_isnt_installed_says_so_instead_of_zeros() {
    let mut s = State::empty("This Mac");
    s.record_roster(Vec::new(), now());
    s.health.set(Vec::new(), now());
    let snap = assemble(&s, now());
    assert_eq!(
        snap.build_server.error.as_deref(),
        Some("motor isn't installed")
    );
    assert!(!snap.build_server.loaded);
    assert_eq!(snap.build_server.data.busy, None);
    assert_eq!(snap.usage.error.as_deref(), Some("insula isn't installed"));
    assert_eq!(snap.approvals.data.waiting, None);
}

// ---- activity, logs, health --------------------------------------------------------

#[test]
fn activity_names_its_subject_and_actor() {
    let entry: subc_control::ActivityEntry = serde_json::from_value(json!({
        "seq": 7, "at_ms": NOW, "daemon_incarnation": "i", "module": "subc",
        "kind": "subc.module_restarted", "summary": "module restarted",
        "subject": { "type": "module", "id": "engram" }, "details": {},
        "actor": { "principal": { "kind": "direct" } }
    }))
    .unwrap_or_else(|e| panic!("activity entry shape: {e}"));
    let names = HashMap::new();
    let a = map::activity_entry(&entry, &names);
    // A direct connection names nobody.
    assert_eq!((a.subject.as_str(), a.who.as_str()), ("engram", ""));
    // A module subject's summary loses its leading "module ".
    assert_eq!(a.what, "restarted");
    assert_eq!(a.kind, "module");
}

#[test]
fn log_lines_keep_their_level_and_time() {
    let (at, level, msg) =
        map::parse_log_line("2026-10-20T18:43:15.441Z  WARN broca::run: provider returned 429");
    assert_eq!(at, Timestamp::parse_utc("2026-10-20T18:43:15.441Z"));
    assert_eq!(level, LogLevel::Warn);
    assert_eq!(msg, "broca::run: provider returned 429");
    assert_eq!(map::parse_log_text("level=error boom").0, LogLevel::Error);
    assert_eq!(
        map::parse_log_text("plain words"),
        (LogLevel::Info, "plain words".into())
    );
}

#[test]
fn health_metrics_keep_the_modules_own_keys() {
    let h: SupervisorHealthEntry = serde_json::from_value(json!({
        "module_id": "broca", "status": "ok", "detail": "3 runs in flight",
        "metrics": { "headline": ["runs_today", "catalog_age_ms"], "runs_today": 1284,
                     "catalog_age_ms": 120_000, "other": 1 },
        "consecutive_failures": 0
    }))
    .unwrap();
    let (summary, metrics) = map::health_view(Some(&h), ck_ui::time::UtcOffset::UTC);
    assert_eq!(summary, "3 runs in flight");
    let rows: Vec<(&str, &str)> = metrics
        .iter()
        .map(|m| (m.label.as_str(), m.value.as_str()))
        .collect();
    // The module's own keys; numbers as numbers, no unit guessed from a
    // key's name.
    assert_eq!(
        rows,
        [("runs_today", "1,284"), ("catalog_age_ms", "120,000")]
    );
    assert_eq!(
        map::health_view(None, ck_ui::time::UtcOffset::UTC),
        (String::new(), vec![])
    );
}

#[test]
fn module_status_reads_state_then_health() {
    use SupervisorHealthStatus as H;
    assert_eq!(map::module_status("running", H::Ok), Status::Ok);
    assert_eq!(map::module_status("running", H::Degraded), Status::Degraded);
    assert_eq!(map::module_status("running", H::Failing), Status::Failed);
    assert_eq!(map::module_status("running", H::Unknown), Status::Unknown);
    assert_eq!(map::module_status("restarting", H::Ok), Status::Starting);
    assert_eq!(map::module_status("disabled", H::Ok), Status::Stopped);
    assert_eq!(map::module_status("failed", H::Ok), Status::Failed);
}

// ---- actions -------------------------------------------------------------------------

#[test]
fn a_restart_reports_the_new_pid_and_its_time_to_healthy() {
    let started = now();
    let before = LiveProcess {
        pid: 100,
        generation: 1,
        spawned_at: started.add_millis(-60_000),
    };
    let mut s = State::empty("This Mac");
    s.processes.insert("broca".into(), before);
    let (phase, done) = action_progress(
        &s,
        ActionKind::Restart,
        "broca",
        Some(before),
        started,
        now(),
    );
    assert_eq!((phase, done), (ActionPhase::Draining, None));

    s.exits.insert("broca".into(), started.add_millis(400));
    s.processes.remove("broca");
    let (phase, _) = action_progress(
        &s,
        ActionKind::Restart,
        "broca",
        Some(before),
        started,
        now(),
    );
    assert_eq!(phase, ActionPhase::Stopped);

    let spawned = started.add_millis(500);
    s.processes.insert(
        "broca".into(),
        LiveProcess {
            pid: 222,
            generation: 2,
            spawned_at: spawned,
        },
    );
    let (phase, done) = action_progress(
        &s,
        ActionKind::Restart,
        "broca",
        Some(before),
        started,
        now(),
    );
    assert_eq!(phase, ActionPhase::Starting { pid: 222 });
    assert_eq!(done, None);

    let probe = spawned.unix_millis() + 2_100;
    s.health.set(
        vec![serde_json::from_value(json!({
            "module_id": "broca", "status": "ok", "consecutive_failures": 0, "last_probe_ms": probe
        }))
        .unwrap()],
        now(),
    );
    let (_, done) = action_progress(
        &s,
        ActionKind::Restart,
        "broca",
        Some(before),
        started,
        now(),
    );
    assert_eq!(
        done,
        Some((
            true,
            "broca restarted · pid 222 · healthy after 2.1 s".into()
        ))
    );
}

// ---- activity rows ----------------------------------------------------------------

fn activity(seq: u64, at_ms: u64, v: Value) -> subc_control::ActivityEntry {
    let mut v = v;
    v["seq"] = json!(seq);
    v["at_ms"] = json!(at_ms);
    v["daemon_incarnation"] = json!("i");
    serde_json::from_value(v).unwrap()
}

/// The two entries the daemon writes for one restart of plexus, as recorded.
fn plexus_restart(at_ms: u64, new_inode: u64) -> [subc_control::ActivityEntry; 2] {
    [
        activity(
            11,
            at_ms + 2_000,
            json!({"kind":"subc.module_started","summary":"module started",
                   "subject":{"id":"plexus","type":"module"},
                   "details":{"new_evidence":{"device":16777233,"inode":new_inode,"method":"macos_spawn_inode"},
                              "old_evidence":{"device":16777233,"inode":3293687992u64,"method":"macos_spawn_inode"}},
                   "actor":{"principal":{"kind":"direct"}},"module":"subc"}),
        ),
        activity(
            10,
            at_ms,
            json!({"kind":"subc.module_restarted","summary":"module restarted",
                   "subject":{"id":"plexus","type":"module"},"details":{},
                   "actor":{"principal":{"kind":"direct"}},"module":"subc"}),
        ),
    ]
}

#[test]
fn a_restart_and_its_start_read_as_one_row() {
    let names = HashMap::new();
    let entries = plexus_restart(NOW as u64, 3_296_734_041);
    let rows = map::activity_rows(&entries, &names);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(
        (
            r.subject.as_str(),
            r.what.as_str(),
            r.kind.as_str(),
            r.who.as_str()
        ),
        ("plexus", "restarted · new binary", "module", "")
    );
    assert_eq!(r.at, Timestamp::from_unix_millis(NOW));

    // The same executable: just "restarted".
    let same = plexus_restart(NOW as u64, 3_293_687_992);
    assert_eq!(map::activity_rows(&same, &names)[0].what, "restarted");

    // A start with no restart before it, and a restart whose start hasn't
    // been recorded yet, each read alone.
    let [started, restarted] = plexus_restart(NOW as u64, 1);
    assert_eq!(map::activity_rows(&[started], &names)[0].what, "started");
    assert_eq!(
        map::activity_rows(&[restarted], &names)[0].what,
        "restarted"
    );
}

#[test]
fn kind_labels_are_short() {
    assert_eq!(map::kind_label("subc.module_restarted"), "module");
    assert_eq!(map::kind_label("subc.module_directed"), "module");
    assert_eq!(map::kind_label("cingulate.consent_answered"), "cingulate");
    assert_eq!(map::kind_label("broca_run"), "broca");
    assert_eq!(map::kind_label("config"), "config");
}

#[test]
fn starts_and_exits_merge_the_feed_the_ledger_and_retained_exits() {
    use ck_ui::live::starts_and_exits;
    use ck_ui::model::{ActivityEntry, ProcessEvent, ProcessEventKind};
    let at = |s: i64| Timestamp::from_unix_millis(NOW + s * 1000);
    let ev = |s, m: &str, what: &str, kind| ProcessEvent {
        at: at(s),
        module: m.into(),
        what: what.into(),
        kind,
    };
    let spawn = [
        ev(0, "broca", "started · pid 7", ProcessEventKind::Started),
        ev(
            -1,
            "broca",
            "exited cleanly · pid 6",
            ProcessEventKind::Exited,
        ),
    ];
    let ledger = [
        // The same restart the spawn feed saw: listed once, with its pid.
        ActivityEntry {
            at: at(1),
            who: "direct".into(),
            subject: "broca".into(),
            what: "restarted".into(),
            kind: "module".into(),
        },
        // One from before ck connected: only the ledger knows it.
        ActivityEntry {
            at: at(-600),
            who: "direct".into(),
            subject: "plexus".into(),
            what: "restarted · new binary".into(),
            kind: "module".into(),
        },
        ActivityEntry {
            at: at(-5),
            who: "direct".into(),
            subject: "broca".into(),
            what: "settings saved".into(),
            kind: "config".into(),
        },
    ];
    let retained = map::terminal_events(
        "broca",
        &serde_json::from_value(json!({
            "daemon_started_at_ms": 0,
            "entries": [{"exit_code": 0, "at_ms": NOW - 1_500, "disposition": "restarting"}]
        }))
        .unwrap(),
    );
    let rows = starts_and_exits(&spawn, &ledger, &retained);
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|e| (e.module.as_str(), e.what.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("broca", "started · pid 7"),
            ("broca", "exited cleanly · restarting"),
            ("plexus", "restarted · new binary"),
        ]
    );
}

// ---- agents' places, devices, usage pools ------------------------------------------

#[test]
fn where_many_maps_the_wrapped_reply_and_counts_machines() {
    let ids = map::agent_identities(&identity_reply()).unwrap();
    let reply = json!({ "result": { "agents": [
        { "agent_id": "a1", "status": "held_here", "holder": "m-here", "holder_name": "ufuk-mac", "epoch": 3, "known": true, "stale": false },
        { "agent_id": "a2", "status": "held_elsewhere", "holder": "m-2", "holder_name": "Mac 2", "epoch": 9, "known": true, "stale": false },
        { "agent_id": "a3", "status": "unclaimed", "holder": null, "holder_name": null, "epoch": 0, "known": true, "stale": false }
    ]}});
    let places = map::agent_places(&reply, "This Mac").unwrap();
    assert_eq!(places.get("a1").map(String::as_str), Some("This Mac"));
    assert_eq!(places.get("a2").map(String::as_str), Some("Mac 2"));
    assert_eq!(places.get("a3").map(String::as_str), Some("unclaimed"));
    let a = map::agents(&ids, None, Some(&places), (None, None), now());
    // This Mac and Mac 2; the unclaimed agent is on no machine.
    assert_eq!(a.machines, Some(2));
}

#[test]
fn devices_list_online_first() {
    let reply = json!({ "devices": [
        { "name": "ufuk-iphone-sim", "device_class": "sim", "trust": "verified", "tunnel": "closed:idle", "subscriptions_live": 0 },
        { "name": "CortexKit Mac 2", "device_class": "mac", "trust": "verified", "tunnel": "established", "subscriptions_live": 0 },
        { "name": "iPhone", "device_class": "phone", "trust": "verified", "tunnel": "established", "subscriptions_live": 1 }
    ]});
    let d = map::devices(&reply, now()).unwrap();
    let names: Vec<&str> = d.list.iter().map(|x| x.short_name.as_str()).collect();
    assert_eq!(names, ["Mac 2", "iPhone", "ufuk-iphone-sim"]);
    assert_eq!(d.online, 2);
}

#[test]
fn an_account_with_only_a_credit_pool_shows_what_is_left() {
    let reply = json!({ "result": [
        { "provider": "claude-console", "accountInfo": { "email": "ops@example.com" }, "usage": {},
          "spend": [{ "label": "Credit", "remaining": { "minor": 19663, "exponent": 2, "unit": "USD" },
                      "total": { "minor": 20000, "exponent": 2, "unit": "USD" } }] }
    ]});
    let p = &map::usage(&reply, now()).unwrap()[0];
    assert_eq!(p.used, None);
    assert_eq!(p.error, None);
    assert_eq!(p.balance.as_deref(), Some("$196.63 of $200.00 left"));
    assert_eq!(p.accounts[0].note, "$196.63 of $200.00 left");
}

// ---- health metrics and log lines ------------------------------------------------------

#[test]
fn the_caches_truncation_marker_is_never_shown_as_metrics() {
    let marker = json!({ "truncated": true, "original_bytes": 75_264 });
    assert!(map::is_truncation_marker(&marker));
    let h: SupervisorHealthEntry = serde_json::from_value(json!({
        "module_id": "aft", "status": "ok", "metrics": marker, "consecutive_failures": 0
    }))
    .unwrap();
    assert_eq!(
        map::health_view(Some(&h), ck_ui::time::UtcOffset::UTC).1,
        vec![]
    );
    // A module's own metric named "truncated" is still its own.
    assert!(!map::is_truncation_marker(
        &json!({ "truncated": true, "original_bytes": 1, "files": 3 })
    ));

    // With the module selected, its pane shows the fresh probe's metrics.
    let mut s = State::empty("This Mac");
    let roster: Vec<subc_control::SupervisorEntry> = serde_json::from_value(json!([
        { "module_id": "aft", "state": "running", "enabled": true, "live": true, "health": "ok" }
    ]))
    .unwrap();
    s.record_roster(roster, now());
    s.health.set(vec![h], now());
    s.focus = Some("aft".into());
    s.probes.insert(
        "aft".into(),
        ck_ui::live::Probe {
            at: now(),
            detail: Some("indexing".into()),
            metrics: Some(json!({ "headline": ["files"], "files": 12 })),
        },
    );
    let snap = assemble(&s, now());
    let aft = &snap.modules.data[0];
    assert_eq!(aft.summary, "indexing");
    let rows: Vec<(&str, &str)> = aft
        .metrics
        .iter()
        .map(|m| (m.label.as_str(), m.value.as_str()))
        .collect();
    assert_eq!(rows, [("files", "12")]);
}

#[test]
fn the_capture_prefix_and_level_word_are_stripped() {
    assert_eq!(
        map::parse_log_text("[aft] INFO tool_call reply sent"),
        (LogLevel::Info, "tool_call reply sent".into())
    );
    assert_eq!(
        map::parse_log_text("[aft] WARN slow index"),
        (LogLevel::Warn, "slow index".into())
    );
    let (at, level, msg) = map::parse_log_line("[aft] 2026-10-20T18:43:15.441Z ERROR boom");
    assert_eq!(at, Timestamp::parse_utc("2026-10-20T18:43:15.441Z"));
    assert_eq!((level, msg.as_str()), (LogLevel::Error, "boom"));
    // A bracketed level is a level, not a prefix.
    assert_eq!(map::parse_log_text("[WARN] disk low").0, LogLevel::Warn);
}

#[tokio::test]
async fn where_many_waits_for_identity_then_asks_in_batches_of_64() {
    use std::sync::{Arc, Mutex};
    let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
    let call = |sent: Arc<Mutex<Vec<Value>>>| {
        move |params: Value| {
            let sent = Arc::clone(&sent);
            async move {
                let ids: Vec<String> = serde_json::from_value(params["agentIds"].clone()).unwrap();
                sent.lock().unwrap().push(params);
                let rows: Vec<Value> = ids
                    .iter()
                    .map(|id| json!({ "agent_id": id, "status": "held_elsewhere", "holder_name": "Mac 2" }))
                    .collect();
                Ok::<Value, String>(json!({ "result": { "agents": rows } }))
            }
        }
    };
    // No agent ids yet: no call at all.
    let none = ck_ui::live::where_many_round(&[], "This Mac", call(Arc::clone(&sent))).await;
    assert!(none.is_none());
    assert!(sent.lock().unwrap().is_empty());

    // 70 agents: two calls, 64 and 6 ids, merged.
    let ids: Vec<String> = (0..70).map(|i| format!("agent-{i}")).collect();
    let places = ck_ui::live::where_many_round(&ids, "This Mac", call(Arc::clone(&sent)))
        .await
        .unwrap()
        .unwrap();
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["agentIds"].as_array().unwrap().len(), 64);
    assert_eq!(sent[1]["agentIds"].as_array().unwrap().len(), 6);
    assert_eq!(places.len(), 70);
    assert_eq!(places["agent-69"], "Mac 2");
}

// ---- usage rows shared with ck quota ----------------------------------------------------

#[test]
fn antigravity_reads_its_extra_rate_windows() {
    // The real reply's row, under an invented account.
    let reply = json!({ "result": [{
        "provider": "antigravity",
        "account": "dev@example.test",
        "accountInfo": { "email": "dev@example.test" },
        "usage": { "extraRateWindows": [
            {"id":"gemini-weekly","title":"Gemini Models weekly","window":{"resetsAt":"2026-10-23T22:00:53Z","usedPercent":100.0,"windowKind":"weekly","windowMinutes":10080}},
            {"id":"3p-weekly","title":"Claude and GPT models weekly","window":{"resetsAt":"2026-10-21T21:23:30Z","usedPercent":100.0,"windowKind":"weekly","windowMinutes":10080}}
        ]}
    }]});
    let now = map::parse_rfc3339("2026-10-20T18:48:00Z").unwrap();
    let p = &map::usage(&reply, now).unwrap()[0];
    assert_eq!(p.name, "Antigravity");
    assert_eq!(p.error, None, "{p:?}");
    assert_eq!(p.used, Some(1.0));
    assert_eq!(p.of, 1);
    assert_eq!(p.window, "Gemini Models weekly");
    let names: Vec<&str> = p.accounts[0]
        .windows
        .iter()
        .map(|w| w.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["Gemini Models weekly", "Claude and GPT models weekly"]
    );
    // The reset shown is the shown window's own.
    assert_eq!(p.resets_at, map::parse_rfc3339("2026-10-23T22:00:53Z"));
}

#[test]
fn the_provider_bar_is_the_window_closest_to_its_limit() {
    let reply = json!({ "result": [
        { "provider": "claude", "account": "a", "usage": {
            "primary": { "usedPercent": 90.0, "windowMinutes": 300 },
            "secondary": { "usedPercent": 40.0, "windowMinutes": 10080 } } },
        { "provider": "claude", "account": "b", "usage": {
            "primary": { "usedPercent": 70.0, "windowMinutes": 300 },
            "secondary": { "usedPercent": 60.0, "windowMinutes": 10080 } } }
    ]});
    let p = &map::usage(&reply, now()).unwrap()[0];
    assert_eq!(p.window, "5h");
    assert_eq!((p.used, p.of), (Some(1.6), 2));
}

fn pool(remaining: i64, total: i64, unit: &str, expires: Option<&str>) -> Value {
    let mut p = json!({
        "basis": "reported", "funding": "subscription", "id": "pool", "label": "API credit (Max 20x)",
        "remaining": { "exponent": 2, "minor": remaining, "unit": unit },
        "total": { "exponent": 2, "minor": total, "unit": unit }
    });
    if let Some(e) = expires {
        p["expiresAt"] = json!(e);
    }
    p
}

#[test]
fn credit_pools_add_up_across_a_providers_accounts() {
    let row = |account: &str, pools: Vec<Value>| {
        json!({
            "provider": "claude-console", "account": account, "usage": {}, "spend": pools
        })
    };
    let reply = json!({ "result": [
        row("console-1", vec![pool(19663, 20000, "USD", Some("2026-10-25T00:00:00Z")), pool(20000, 20000, "USD", None)]),
        row("console-2", vec![pool(19663, 20000, "USD", Some("2026-11-02T00:00:00Z"))]),
        row("console-3", vec![pool(19000, 20000, "USD", None)]),
        row("console-4", vec![pool(20000, 20000, "USD", None)]),
    ]});
    let p = &map::usage(&reply, now()).unwrap()[0];
    assert_eq!(p.name, "Claude Console");
    assert_eq!(p.used, None);
    assert_eq!(p.error, None);
    assert_eq!(
        p.balance.as_deref(),
        Some("$983.26 of $1,000.00 left · 5 pools")
    );
    assert_eq!(
        p.balance_expires,
        map::parse_rfc3339("2026-10-25T00:00:00Z")
    );

    // Pools in different units are only counted.
    let mixed = json!({ "result": [
        row("console-1", vec![pool(100, 200, "USD", None)]),
        row("console-2", vec![pool(100, 200, "CNY", None)]),
    ]});
    let p = &map::usage(&mixed, now()).unwrap()[0];
    assert_eq!(p.balance.as_deref(), Some("2 credit pools"));

    // One pool: no count.
    let one = json!({ "result": [ row("console-1", vec![pool(19663, 20000, "USD", None)]) ] });
    assert_eq!(
        map::usage(&one, now()).unwrap()[0].balance.as_deref(),
        Some("$196.63 of $200.00 left")
    );
}

#[test]
fn activity_names_who_asked_when_the_record_does() {
    let mut names = HashMap::new();
    names.insert("a1".to_string(), "ARGUS".to_string());
    let by_agent = activity(
        1,
        NOW as u64,
        json!({
            "kind": "subc.module_restarted", "summary": "module restarted",
            "subject": {"type": "module", "id": "broca"}, "details": {},
            "actor": {"principal": {"kind": "direct"}, "agent_id": "a1"}, "module": "subc"
        }),
    );
    assert_eq!(map::activity_entry(&by_agent, &names).who, "ARGUS");
    let by_module = activity(
        2,
        NOW as u64,
        json!({
            "kind": "broca.run_ended", "summary": "run ended", "details": {},
            "actor": {"principal": {"kind": "reserved", "module_id": "broca"}}, "module": "broca"
        }),
    );
    assert_eq!(map::activity_entry(&by_module, &names).who, "broca");
}

// ---- motor history, truncated metrics, merged logs -------------------------------------

#[test]
fn motor_compact_history_reads_with_gaps_and_a_varying_cap() {
    let m = json!({
        "build_server": { "permits_held": 40, "permits_cap": 80, "queue_depth": 0 },
        "history": {
            "start_minute_ms": 1_800_000_000_000u64, "step_ms": 60000,
            "permits_held": [40, null, 80, 20],
            "permits_cap": 80,
            "queue_depth": [0, null, 3, 0],
            "queue_p90_ms": [0, null, 1000, 0]
        }
    });
    let b = map::build_server(Some(&m), &[1.0]);
    assert_eq!(b.load_history.len(), 4);
    assert_eq!(b.load_history[0], 50.0);
    assert!(
        b.load_history[1].is_nan(),
        "a minute without a sample is a gap"
    );
    assert_eq!(b.load_history[2], 100.0);
    assert_eq!(b.load_history[3], 25.0);

    let varying = json!({ "history": {
        "start_minute_ms": 0, "step_ms": 60000,
        "permits_held": [10, 10], "permits_cap": [20, 40]
    }});
    assert_eq!(
        map::build_server(Some(&varying), &[]).load_history,
        [50.0, 25.0]
    );
}

fn motor_roster_and_truncated_cache(s: &mut State) {
    let roster: Vec<subc_control::SupervisorEntry> = serde_json::from_value(json!([
        { "module_id": "motor", "state": "running", "enabled": true, "live": true, "health": "ok" }
    ]))
    .unwrap();
    s.record_roster(roster, now());
    s.health.set(
        vec![serde_json::from_value(json!({
            "module_id": "motor", "status": "ok", "consecutive_failures": 0,
            "metrics": { "truncated": true, "original_bytes": 18_600 }
        }))
        .unwrap()],
        now(),
    );
}

#[test]
fn a_truncated_cache_entry_is_read_from_the_modules_direct_probe() {
    let mut s = State::empty("This Mac");
    motor_roster_and_truncated_cache(&mut s);
    // Due for a probe, and nothing to show until it arrives.
    assert_eq!(s.truncated_due(now()), ["motor"]);
    let snap = assemble(&s, now());
    assert_eq!(snap.build_server.data.busy, None);

    s.probes.insert(
        "motor".into(),
        ck_ui::live::Probe {
            at: now(),
            detail: Some("80 of 80 permits busy".into()),
            metrics: Some(json!({
                "headline": ["build_server", "queue_wait_1h"],
                "build_server": { "permits_held": 80, "permits_cap": 80, "queue_depth": 21 },
                "queue_wait_1h": { "p90_ms": 180000 }
            })),
        },
    );
    let snap = assemble(&s, now());
    let b = &snap.build_server.data;
    assert_eq!(
        (b.busy, b.slots, b.queued, b.wait_secs),
        (Some(80), Some(80), Some(21), Some(180.0))
    );
    let motor = &snap.modules.data[0];
    assert_eq!(motor.summary, "80 of 80 permits busy");
    let labels: Vec<&str> = motor.metrics.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(labels, ["build_server", "queue_wait_1h"]);
    assert!(motor
        .all_metrics
        .iter()
        .any(|m| m.label == "build_server.permits_held" && m.value == "80"));
    // Probed just now: not due again for 30 s.
    assert!(s.truncated_due(now()).is_empty());
    assert_eq!(s.truncated_due(now().add_secs(31.0)), ["motor"]);
}

#[test]
fn merged_log_lines_keep_their_time_level_and_text() {
    use ck_ui::live::ModuleLogLine;
    let lines = map::merged_log_lines(&[
        ModuleLogLine {
            at_ms: Some(NOW),
            level: Some(LogLevel::Warn),
            text: "2027-01-15T08:00:00.000Z WARN broca.run: provider returned 429".into(),
        },
        ModuleLogLine {
            at_ms: None,
            level: None,
            text: "[broca] panicked at src/run.rs:12".into(),
        },
    ]);
    assert_eq!(lines[0].at, Some(now()));
    assert_eq!(lines[0].level, LogLevel::Warn);
    assert_eq!(lines[0].message, "broca.run: provider returned 429");
    assert_eq!(lines[1].at, None);
    assert_eq!(lines[1].message, "panicked at src/run.rs:12");
}

// ---- Home from live replies: lifecycle words, usage errors, the spacer ------------------

fn home_rows(snap: &ck_ui::Snapshot, w: u16, h: u16) -> Vec<String> {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    ck_ui::App::new(ck_ui::Theme::test()).render(&mut buf, snap, now());
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn roster(entries: Value) -> Vec<subc_control::SupervisorEntry> {
    serde_json::from_value(entries).unwrap()
}

fn running(id: &str) -> Value {
    json!({ "module_id": id, "state": "running", "enabled": true, "live": true, "health": "ok" })
}

#[test]
fn a_module_starting_up_reads_as_starting_not_unknown() {
    let mut s = State::empty("This Mac");
    // insula's new process started 5 s ago and has had no health check.
    s.processes.insert(
        "insula".into(),
        LiveProcess {
            pid: 4242,
            generation: 7,
            spawned_at: now().add_secs(-5.0),
        },
    );
    let mut entries = vec![running("aft"), running("broca")];
    entries.push(
        json!({ "module_id": "insula", "state": "running", "enabled": true,
                         "live": true, "health": "unknown" }),
    );
    entries.push(
        json!({ "module_id": "motor", "state": "restarting", "enabled": true,
                         "live": false, "health": "unknown" }),
    );
    s.record_roster(roster(Value::Array(entries)), now());
    let snap = assemble(&s, now());
    let insula = snap.modules.data.iter().find(|m| m.id == "insula").unwrap();
    assert_eq!(insula.status, Status::Starting);
    assert_eq!(insula.lifecycle.as_deref(), Some("Starting"));
    let motor = snap.modules.data.iter().find(|m| m.id == "motor").unwrap();
    assert_eq!(motor.lifecycle.as_deref(), Some("Restarting"));

    let rows = home_rows(&snap, 120, 36);
    let problems = rows[5].clone() + &rows[6];
    assert!(
        problems.contains("insula") && problems.contains("Starting"),
        "{problems}"
    );
    assert!(rows[8].contains("2 ok · 2 starting"), "{}", rows[8]);
    assert!(!rows[8].contains("unknown"), "{}", rows[8]);

    // A module that has run for long with no health check anyone can read
    // is unknown: the app genuinely can't say.
    s.processes.get_mut("insula").unwrap().spawned_at = now().add_secs(-3600.0);
    let snap = assemble(&s, now());
    let insula = snap.modules.data.iter().find(|m| m.id == "insula").unwrap();
    assert_eq!(insula.status, Status::Unknown);
}

#[test]
fn a_usage_code_never_shows_as_the_value() {
    let mut s = State::empty("This Mac");
    s.record_roster(roster(json!([running("insula")])), now());
    let reply = json!({ "result": [
        { "provider": "claude", "account": "acct-1", "error": "upstream returned 500",
          "errorClass": "upstream_failed" }
    ]});
    s.usage.set(map::usage(&reply, now()).unwrap(), now());
    let snap = assemble(&s, now());
    // The code stays in the model, though Home shows a sentence.
    assert_eq!(
        snap.usage.data.providers[0].error.as_deref(),
        Some("upstream_failed")
    );
    let rows = home_rows(&snap, 120, 36);
    let usage: String = rows[10].chars().skip(45).take(37).collect();
    assert!(usage.contains("couldn't read usage"), "{usage}");
    assert!(!usage.contains("upstream_failed"), "{usage}");
}

#[test]
fn three_problems_keep_the_spacer_and_give_up_a_row() {
    let mut s = State::empty("This Mac");
    let entries = ["aft", "broca", "engram", "motor"]
        .iter()
        .map(|id| {
            let state = if *id == "motor" { "running" } else { "failed" };
            json!({ "module_id": id, "state": state, "enabled": true, "live": true, "health": "ok" })
        })
        .collect::<Vec<_>>();
    s.record_roster(roster(Value::Array(entries)), now());
    let snap = assemble(&s, now());
    assert_eq!(snap.attention().len(), 3);
    for (w, h) in [(120u16, 36u16), (160, 48)] {
        let rows = home_rows(&snap, w, h);
        assert!(rows[5].contains("aft"), "{w}×{h}");
        assert!(rows[6].contains("+2 more"), "{w}×{h}: {}", rows[6]);
        assert!(
            rows[7].trim().is_empty(),
            "{w}×{h}: the spacer stays: {:?}",
            rows[7]
        );
        assert!(rows[8].contains("Modules"), "{w}×{h}");
    }
}
