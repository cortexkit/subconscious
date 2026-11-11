use super::*;
use crate::control::claims::tests::{mint, request, rig, Net, Wire};
use crate::forwarding::RouteBindRelayOutcome;
use subc_control::{ClaimPresentRequest, ClaimPurpose};
use subc_protocol::FrameType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct PreExecSibling {
    release: std::io::PipeWriter,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PreExecSibling {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.release.write_all(&[1]);
    }
}

async fn pre_exec_sibling(endpoint: &std::os::unix::net::UnixStream) -> PreExecSibling {
    use std::os::fd::AsRawFd;
    let mut command = std::process::Command::new("/bin/true");
    let (mut ready, release) =
        subc_os::fork_exec_test::pause_before_exec(&mut command, endpoint.as_raw_fd()).unwrap();
    let task = tokio::task::spawn_blocking(move || {
        command.spawn().unwrap().wait().unwrap();
    });
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        ready.read_exact(&mut [0; 8]).unwrap();
    })
    .await
    .unwrap();
    PreExecSibling { release, task }
}

async fn first_observation(mode: &'static str, caller: bool) {
    let (endpoint, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let sibling = pre_exec_sibling(&endpoint).await;
    let mut rig = rig().await;
    let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let pending_claim = mint(&mut rig, ClaimPurpose::Helper).await.claim;
    let parked_token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    // This connection has no server loop, so teardown cannot remove the pending
    // bind before its liveness check is exercised.
    let mut parked = Wire::new(&rig.router);
    assert_eq!(
        parked
            .call(&rig.router, request(ClaimPresentRequest::new(parked_token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let mut bound = Net::new(rig.router.clone()).await;
    assert_eq!(
        bound
            .call(request(ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let bound_id = bound.connection_id(&rig.handler);
    let forwarding = rig.handler.forwarding();
    let (tx, mut replies) = tokio::sync::mpsc::channel(8);
    let sink = crate::FrameSink::new(tx);
    let existing = forwarding
        .begin_route_bind_relay_for_test(bound_id, sink.clone(), 899, "claims-module")
        .unwrap();
    forwarding
        .complete_pending_relay(
            rig.primary.ctx.connection_id,
            existing.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(
        replies.try_recv().unwrap().frame.header.ty,
        FrameType::Response
    );
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    let client = if caller {
        parked.connection.id()
    } else {
        crate::ConnectionId::new(80_000)
    };
    let mut pending = forwarding
        .begin_route_bind_relay_for_test(client, sink.clone(), 900, "claims-module")
        .unwrap();
    let primary = rig.primary.ctx.connection_id;

    let stream = endpoint.try_clone().unwrap();
    *rig.spawn.endpoint.lock().unwrap() = Some(endpoint);
    peer.set_nonblocking(true).unwrap();
    let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
    let (reached, observation) = tokio::sync::oneshot::channel();
    let (release, paused) = std::sync::mpsc::channel();
    *rig.spawn.retirement_pause.lock().unwrap() = Some(RetirementPause {
        reached,
        release: paused,
    });

    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        module_id: "retirement-probe".into(),
        program: "/bin/sleep".into(),
        args: vec!["300".into()],
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: true,
    };
    let mut child = spawn_and_mark_running(
        &spec,
        &runtime,
        &Arc::new(Mutex::new(SupervisorSnapshot::starting())),
    )
    .unwrap();
    let pid = child.pid;
    assert!(
        child.child.container().is_some(),
        "fixture must retain its group pin"
    );
    child.registration_spawn = Some(rig.spawn.clone());
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("retirement-probe", "retirement-nonce".into());
    let binding = handle
        .begin_program_binding("retirement-probe", SpawnRole::Plain)
        .unwrap();
    binding.attach_registration(&rig.spawn);
    child._program_binding = Some(binding.clone());
    child.child.start_kill().unwrap();
    while subc_os::child_exit::observe(pid).unwrap().is_none() {
        tokio::task::yield_now().await;
    }
    let spawn = rig.spawn.clone();
    // Retirement's test hook blocks synchronously. Keep it off the I/O workers
    // so a wake parked in a worker's local queue cannot delay the assertion task.
    let executor = tokio::runtime::Handle::current();
    let observer = tokio::task::spawn_blocking(move || {
        executor.block_on(async move {
            match mode {
                "wait" => {
                    child.wait().await.unwrap();
                }
                "try_wait" => {
                    assert!(child.try_wait().unwrap().is_some());
                }
                "finish_exit" => {
                    child.finish_exit(true).await.unwrap();
                }
                "hook" => {
                    binding.retirement_hook().upgrade().unwrap().retire();
                }
                "eof" => {
                    stream.set_nonblocking(true).unwrap();
                    let stream = tokio::net::UnixStream::from_std(stream).unwrap();
                    let mut stream = RegistrationStream { stream, spawn };
                    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
                }
                _ => unreachable!(),
            }
            child
        })
    });
    if mode == "eof" {
        peer.shutdown().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), observation)
        .await
        .expect("the exit observation must synchronously retire before cleanup")
        .unwrap();
    // Retirement has disabled launch readiness and voided pending claims, but
    // has not notified connection loops yet. Neither the unreaped child nor a
    // pre-exec descriptor copy can keep the channel or a claim alive here.
    assert!(!rig.spawn.launch_ready());
    assert_eq!(rig.spawn.claims.get().unwrap().pending_count_for_test(), 0);
    assert!(subc_os::child_exit::observe(pid).unwrap().is_some());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let completion = forwarding
        .complete_pending_relay(primary, pending.corr, RouteBindRelayOutcome::Accepted)
        .unwrap();
    assert!(completion.settled);
    assert!(completion.abandoned.is_some());
    assert!(
        !matches!(
            pending.receiver.try_recv().unwrap(),
            RouteBindRelayOutcome::Accepted
        ),
        "bind commit must recheck spawn liveness before close notifications run"
    );
    assert!(
        replies.try_recv().is_err(),
        "a retired spawn must not publish route.open"
    );
    assert_eq!(forwarding.reserved_route_count().unwrap(), (0, 0));
    release.send(()).unwrap();
    let mut child = observer.await.unwrap();
    assert!(bound.recv().await.is_none(), "bound connection must close");
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        0,
        "bound routes must close with their connection"
    );
    let mut refused = Net::new(rig.router.clone()).await;
    let reply = refused
        .call(request(ClaimPresentRequest::new(pending_claim)))
        .await;
    crate::control::claims::tests::error(&reply, "claim_invalid");
    assert!(refused.recv().await.is_none());
    child.finish_exit(false).await.unwrap();
    use std::io::Write;
    let mut sibling = sibling;
    sibling.release.write_all(&[1]).unwrap();
    (&mut sibling.task).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_wait_fences_claims_and_bind_with_unreaped_pin() {
    first_observation("wait", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_try_wait_fences_claims_and_bind() {
    first_observation("try_wait", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_finish_exit_fences_claims_and_bind() {
    first_observation("finish_exit", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_carried_hook_fences_claims_and_bind() {
    first_observation("hook", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_eof_fences_claims_and_bind_before_cleanup() {
    first_observation("eof", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_target_exit_refuses_pending_bind() {
    first_observation("eof", false).await;
}
