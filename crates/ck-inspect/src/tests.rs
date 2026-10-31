use super::*;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};
use subc_client_rs::RouteHandle;
use subc_control::SupervisorHealthStatus;
use subc_protocol::{scope::ScopeStamp, BindIdentity, RouteTarget};

struct FixtureRoster {
    entries: Vec<SupervisorEntry>,
    snapshots: Mutex<VecDeque<Vec<LiveSpawn>>>,
}
#[async_trait]
impl Roster for FixtureRoster {
    async fn list(&self) -> Result<Vec<SupervisorEntry>, Refusal> {
        Ok(self.entries.clone())
    }
    async fn snapshot(&self) -> Result<Vec<LiveSpawn>, Refusal> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
}
struct FakeSampler {
    supported: bool,
    failure: bool,
    calls: AtomicUsize,
}
#[async_trait]
impl Sampler for FakeSampler {
    fn supported(&self) -> bool {
        self.supported
    }
    async fn capture(
        &self,
        mut profile: Profile,
        _deadline: tokio::time::Instant,
    ) -> Result<Profile, &'static str> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failure {
            return Err("fixture sampler failure");
        }
        profile.threads.push(Thread::new("test-thread".into()));
        Ok(profile)
    }
}
fn spawn(pid: u32, generation: u64) -> LiveSpawn {
    LiveSpawn {
        module_id: "fixture".into(),
        pid,
        spawn_generation: generation,
        spawned_at_ms: 1,
    }
}
fn entry() -> SupervisorEntry {
    SupervisorEntry::new(
        "fixture",
        "running",
        true,
        true,
        SupervisorHealthStatus::Unknown,
    )
    .with_spawn_generation(Some(3))
}
fn service(
    entries: Vec<SupervisorEntry>,
    snapshots: Vec<Vec<LiveSpawn>>,
    supported: bool,
    failure: bool,
) -> (Inspect, Arc<FakeSampler>) {
    let sampler = Arc::new(FakeSampler {
        supported,
        failure,
        calls: AtomicUsize::new(0),
    });
    (
        Inspect {
            roster: Arc::new(FixtureRoster {
                entries,
                snapshots: Mutex::new(snapshots.into()),
            }),
            sampler: sampler.clone(),
            bounds: Bounds::new(),
        },
        sampler,
    )
}
fn request(seconds: u64) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"method":"module.profile", "params":{"module_id":"fixture", "seconds": seconds}}),
    )
    .unwrap()
}

#[tokio::test]
async fn resources_is_an_object_and_copies_only_observations_with_matching_pid() {
    let e = entry().with_resources(Some(subc_control::ChildResourceUsage::Unavailable {
        reason: subc_control::ChildResourceUnavailableReason::Unreadable,
    }));
    let (service, _) = service(vec![e.clone()], vec![vec![spawn(72, 3)]], true, false);
    let result = service
        .query(br#"{"method":"module.resources","params":{}}"#)
        .await
        .unwrap();
    assert!(result.is_object());
    assert_eq!(
        result.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["modules"]
    );
    assert_eq!(
        result["modules"],
        json!([{"module_id":"fixture", "state":"running", "spawn_generation":3,"pid":72,"resources":{"status":"unavailable","reason":"unreadable"}}])
    );
    assert!(result["modules"][0].get("health").is_none());
    let mut altered = e;
    altered.spawn_generation = Some(4);
    let (service, _) = self::service(vec![altered], vec![vec![spawn(72, 3)]], true, false);
    let result = service
        .query(br#"{"method":"module.resources","params":{}}"#)
        .await
        .unwrap();
    assert!(result["modules"][0].get("pid").is_none());
}

#[test]
fn management_envelope_with_whole_number_seconds_parses_and_each_refusal_names_its_field() {
    let profile = |seconds| Request::Profile {
        module_id: "fixture".into(),
        seconds,
    };
    for (body, expected) in [
        (
            &br#"{"method":"module.profile","params":{"module_id":"fixture","seconds":8}}"#[..],
            profile(8),
        ),
        (
            br#"{"method":"module.profile","params":{"module_id":"fixture","seconds":8.0}}"#,
            profile(8),
        ),
        (
            br#"{"method":"module.resources","params":{}}"#,
            Request::Resources,
        ),
        (br#"{"method":"module.resources"}"#, Request::Resources),
    ] {
        assert_eq!(parse_request(body).unwrap(), expected);
    }
    for (body, reason) in [
        (&b"[1]"[..], "request body must be a JSON object"),
        (
            br#"{"op":"module.resources"}"#,
            "request takes only method and params",
        ),
        (
            br#"{"params":{"module_id":"fixture","seconds":8}}"#,
            "request needs a method string",
        ),
        (
            br#"{"method":"module.resources","params":[]}"#,
            "params must be a JSON object",
        ),
        (
            br#"{"method":"module.top","params":{}}"#,
            "unknown method; expected module.resources or module.profile",
        ),
        (
            br#"{"method":"module.resources","params":{"x":1}}"#,
            "module.resources takes no params",
        ),
        (
            br#"{"method":"module.profile","params":{"seconds":8}}"#,
            "module_id must be a string",
        ),
        (
            br#"{"method":"module.profile","params":{"module_id":"f","seconds":"8"}}"#,
            "seconds must be a whole number from 1 to 10",
        ),
        (
            br#"{"method":"module.profile","params":{"module_id":"f","seconds":8.5}}"#,
            "seconds must be a whole number from 1 to 10",
        ),
        (
            br#"{"method":"module.profile","params":{"module_id":"f","seconds":1,"pid":1}}"#,
            "module.profile params take only module_id and seconds",
        ),
    ] {
        let refusal = parse_request(body).expect_err(std::str::from_utf8(body).unwrap());
        assert_eq!((refusal.code, refusal.reason), ("invalid_request", reason));
    }
}

#[tokio::test]
async fn invalid_seconds_and_caller_pid_are_refused_without_sampling() {
    let (service, sampler) = service(vec![entry()], vec![], true, false);
    for body in [
        request(0),
        request(11),
        br#"{"method":"module.profile","params":{"module_id":"fixture","seconds":1.5}}"#.to_vec(),
        br#"{"method":"module.profile","params":{"module_id":"fixture","seconds":-1}}"#.to_vec(),
        br#"{"method":"module.profile","params":{"module_id":"fixture","seconds":1,"pid":1}}"#
            .to_vec(),
    ] {
        assert_eq!(
            service.query(&body).await.unwrap_err().code,
            "invalid_request"
        );
    }
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_module_refusal() {
    let (service, sampler) = service(vec![], vec![], true, false);
    assert_eq!(
        service.query(&request(1)).await.unwrap_err().code,
        "unknown_module"
    );
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn not_running_refusal() {
    let (service, sampler) = service(vec![entry()], vec![vec![]], true, false);
    assert_eq!(
        service.query(&request(1)).await.unwrap_err().code,
        "not_running"
    );
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn unsupported_platform_refusal() {
    let (service, sampler) = service(vec![entry()], vec![vec![spawn(72, 3)]], false, false);
    assert_eq!(
        service.query(&request(1)).await.unwrap_err().code,
        "unsupported_platform"
    );
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn sampler_failed_refusal_has_no_raw_output() {
    let (service, _) = service(
        vec![entry()],
        vec![vec![spawn(72, 3)], vec![spawn(72, 3)]],
        true,
        true,
    );
    let error = service.query(&request(1)).await.unwrap_err();
    assert_eq!(error.code, "sampler_failed");
    assert_eq!(error.reason, "fixture sampler failure");
}

#[tokio::test]
async fn unavailable_daemon_refusal() {
    struct MissingRoster;
    #[async_trait]
    impl Roster for MissingRoster {
        async fn list(&self) -> Result<Vec<SupervisorEntry>, Refusal> {
            Err(Refusal::new(
                "roster_unavailable",
                "daemon does not provide the supervised roster",
            ))
        }
        async fn snapshot(&self) -> Result<Vec<LiveSpawn>, Refusal> {
            Err(Refusal::new(
                "roster_unavailable",
                "daemon does not provide the supervised spawn snapshot",
            ))
        }
    }
    let (mut service, sampler) = service(vec![], vec![], true, false);
    service.roster = Arc::new(MissingRoster);
    assert_eq!(
        service
            .query(br#"{"method":"module.resources","params":{}}"#)
            .await
            .unwrap_err()
            .code,
        "roster_unavailable"
    );
    assert_eq!(
        service.query(&request(1)).await.unwrap_err().code,
        "roster_unavailable"
    );
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn module_restarted_discards_pid_generation_and_missing_entry_results() {
    for after in [vec![spawn(73, 3)], vec![spawn(72, 4)], vec![]] {
        let (service, sampler) =
            service(vec![entry()], vec![vec![spawn(72, 3)], after], true, false);
        assert_eq!(
            service.query(&request(1)).await.unwrap_err().code,
            "module_restarted"
        );
        assert_eq!(sampler.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test(start_paused = true)]
async fn busy_is_not_queued_and_cooldown_runs_from_completion() {
    let (service, sampler) = service(vec![entry()], vec![vec![spawn(72, 3)]], true, false);
    let active = service.bounds.enter("other").unwrap();
    assert_eq!(service.query(&request(1)).await.unwrap_err().code, "busy");
    assert_eq!(sampler.calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(active);
    let refusal = service.bounds.enter("other").err().unwrap();
    assert_eq!(refusal.code, "rate_limited");
    assert_eq!(refusal.retry_after_ms, Some(60_000));
    let HandlerOutcome::ErrorWithDetail { detail, .. } = refusal.outcome() else {
        panic!("missing detail")
    };
    assert_eq!(detail["retry_after_ms"], 60_000);
    tokio::time::advance(Duration::from_millis(59_999)).await;
    assert_eq!(
        service.bounds.enter("other").err().unwrap().retry_after_ms,
        Some(1)
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(service.bounds.enter("other").is_ok());
    assert!(service.bounds.enter("fixture").is_ok());
}

#[test]
fn cap_measures_serialized_json_and_marks_truncation() {
    let mut profile = Profile::new("fixture".into(), 72, 1, "sample".into());
    for _ in 0..8 {
        let mut thread = Thread::new("thread".into());
        for _ in 0..3 {
            thread
                .stacks
                .push(Stack::new(1, vec!["\\\"界".repeat(300); 12]));
        }
        profile.threads.push(thread);
    }
    assert!(serde_json::to_vec(&profile).unwrap().len() > 16 * 1024);
    profile.cap().unwrap();
    let bytes = serde_json::to_vec(&profile).unwrap();
    assert!(bytes.len() <= 16 * 1024);
    assert!(profile.truncated);
    assert_eq!(serde_json::from_slice::<Profile>(&bytes).unwrap(), profile);
    let mut oversized = Profile::new("x".repeat(16 * 1024), 1, 1, "sample".into());
    assert_eq!(
        oversized.cap(),
        Err("profile metadata exceeds reply budget")
    );
}

fn bind(principal: Option<Principal>, flow: Option<(bool, bool)>) -> RouteBindRequest {
    let mut req = RouteBindRequest::new(
        RouteHandle::detached(1, 1),
        RouteTarget::ManagementSurface {
            module_id: "inspect".into(),
        },
        BindIdentity::new("test", "inspect"),
    );
    req.principal = principal;
    if let Some((authorized, has_flow)) = flow {
        let stamp: ScopeStamp = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"owner"}, "ref":"scope", "scope_epoch":1,"kind":"ephemeral", "owner_authorized":authorized, "attributes":if has_flow {json!({"flow_id":"watch"})} else {json!({"agent_id":"agent"})}})).unwrap();
        req = req.with_scope(stamp);
    }
    req
}
#[test]
fn admission_accepts_direct_operator() {
    assert!(admitted(&bind(Some(Principal::Direct), None)));
}
#[test]
fn admission_accepts_owner_authorized_flow() {
    assert!(admitted(&bind(
        Some(Principal::Reserved {
            module_id: "carrier".into()
        }),
        Some((true, true))
    )));
}
#[test]
fn admission_refuses_unauthorized_flow() {
    assert!(!admitted(&bind(None, Some((false, true)))));
}
#[test]
fn admission_refuses_authorized_agent_scope_without_flow() {
    assert!(!admitted(&bind(None, Some((true, false)))));
}
#[test]
fn admission_refuses_unscoped_reserved_caller() {
    assert!(!admitted(&bind(
        Some(Principal::Reserved {
            module_id: "module".into()
        }),
        None
    )));
}
#[test]
fn admission_refuses_missing_or_unverified_principal() {
    assert!(!admitted(&bind(None, None)));
    assert!(!admitted(&bind(Some(Principal::Unverified), None)));
}
#[test]
fn manifest_is_query_only_and_flow_capable_without_tools() {
    let value = serde_json::to_value(manifest()).unwrap();
    assert_eq!(
        value["flow_operations"],
        json!(["module.resources", "module.profile"])
    );
    assert_eq!(value["capabilities"]["provides"], json!(["flow-scopes/v1"]));
    assert_eq!(value["provides"].as_array().unwrap().len(), 1);
    let ops = value["provides"][0]["operations"].as_array().unwrap();
    assert!(ops.iter().all(|op| op["kind"] == "query"));
}
#[test]
fn every_refusal_is_terminal_with_detail() {
    for code in [
        "invalid_request",
        "not_permitted",
        "unknown_module",
        "not_running",
        "busy",
        "rate_limited",
        "module_restarted",
        "unsupported_platform",
        "sampler_failed",
        "roster_unavailable",
    ] {
        assert!(!subc_protocol::error_codes::is_retryable_route_open(code));
        let HandlerOutcome::ErrorWithDetail { detail, .. } = Refusal::new(code, "reason").outcome()
        else {
            panic!("missing detail")
        };
        assert_eq!(detail["class"], "terminal");
    }
}

#[test]
fn successful_replies_use_the_management_result_envelope_within_16_kib() {
    let reply: Value = serde_json::from_slice(&result_envelope(&json!({"modules": []}))).unwrap();
    assert_eq!(reply, json!({"result": {"modules": []}}));
    let mut profile = Profile::new("fixture".into(), 1, 1, "sample".into());
    let mut thread = Thread::new("x".repeat(64));
    thread.stacks = (0..2000)
        .map(|count| Stack::new(count, vec!["frame".repeat(8)]))
        .collect();
    profile.threads.push(thread);
    profile.cap().unwrap();
    let whole = result_envelope(&serde_json::to_value(&profile).unwrap());
    assert!(whole.len() <= 16 * 1024, "{}", whole.len());
}
