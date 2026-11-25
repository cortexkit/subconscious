use super::*;

#[test]
fn root_io_blocking_read_releases_the_root_before_returning() {
    let pool = RootIoPool::new().unwrap();
    let root = std::env::current_dir().unwrap().join("reviewed-workspace");
    assert_eq!(pool.read_blocking(root.clone(), || 42), Ok(42));
    assert!(!pool.is_busy(&root));
    assert_eq!(pool.read_blocking(root, || 43), Ok(43));
}
use std::sync::{atomic, mpsc};

const TEST_LIMIT: Duration = Duration::from_secs(5);

async fn started<T>(signal: oneshot::Receiver<T>) -> T {
    tokio::time::timeout(TEST_LIMIT, signal)
        .await
        .expect("root worker did not dispatch")
        .expect("root worker dropped its start signal")
}

async fn park(pool: &RootIoPool, root: &str) -> (RootIoJob<()>, mpsc::Sender<()>) {
    let (release, blocked) = mpsc::channel();
    let (signal, entered) = oneshot::channel();
    let job = pool
        .submit(root.into(), move || {
            assert!(std::thread::current()
                .name()
                .unwrap()
                .starts_with("subc-root-io-"));
            assert!(tokio::runtime::Handle::try_current().is_err());
            signal.send(()).unwrap();
            let _ = blocked.recv();
        })
        .unwrap();
    started(entered).await;
    (job, release)
}

#[tokio::test]
async fn root_io_queue_is_fifo_and_bounded() {
    let pool = RootIoPool::new().unwrap();
    let mut held = Vec::new();
    for index in 0..ROOT_IO_WORKERS {
        held.push(park(&pool, &format!("/held-{index}")).await);
    }
    assert!(pool.health().is_none());
    let dispatch = Arc::new(atomic::AtomicUsize::new(0));
    let mut queued = Vec::new();
    for index in 0..16 {
        let dispatch = dispatch.clone();
        queued.push(
            pool.submit(format!("/queued-{index}").into(), move || {
                dispatch.fetch_add(1, atomic::Ordering::Relaxed)
            })
            .unwrap(),
        );
    }
    assert!(matches!(
        pool.submit("/full".into(), || panic!("refused work ran")),
        Err(RootIoError::QueueFull)
    ));
    assert!(matches!(
        pool.submit("/queued-0".into(), || panic!("duplicate work ran")),
        Err(RootIoError::RootBusy)
    ));
    let (job, release) = held.pop().unwrap();
    release.send(()).unwrap();
    job.wait().await.unwrap();
    // With exactly one worker free, the observed call order is the dequeue order.
    for (index, job) in queued.into_iter().enumerate() {
        assert_eq!(job.wait().await.unwrap(), index);
    }
}

#[tokio::test]
async fn root_io_busy_read_times_out_without_queuing_work() {
    let pool = RootIoPool::new().unwrap();
    let (holder, release) = park(&pool, "/busy").await;
    let deadline = Instant::now() + Duration::from_millis(300);
    let read = pool.start_read("/busy".into(), Some(deadline), || {
        panic!("a second read started while its root was busy");
    });
    let mut read = std::pin::pin!(read);
    std::future::poll_fn(|cx| {
        assert!(read.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;

    assert_eq!(pool.shared.state.lock().unwrap().queue.len(), 0);
    pool.submit("/healthy".into(), || ())
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(read.await, Err(RootIoError::DeadlineElapsed));
    assert!(pool.is_busy(Path::new("/busy")));
    assert_eq!(pool.shared.state.lock().unwrap().queue.len(), 0);
    release.send(()).unwrap();
    holder.wait().await.unwrap();
}

#[tokio::test]
async fn root_io_busy_read_keeps_original_deadline_after_release() {
    let pool = RootIoPool::new().unwrap();
    let (holder, release) = park(&pool, "/busy").await;
    let deadline = Instant::now() + TEST_LIMIT;
    let shared = pool.shared.clone();
    let read = pool.start_read("/busy".into(), Some(deadline), move || {
        shared.state.lock().unwrap().running[Path::new("/busy")].deadline
    });

    // A release before the reader first polls must not be missed either.
    release.send(()).unwrap();
    holder.wait().await.unwrap();
    assert_eq!(read.await, Ok(deadline));
}

#[tokio::test]
async fn root_io_expired_queue_never_dispatches_and_charged_workers_refuse() {
    let pool = RootIoPool::with_timeout(Duration::from_millis(300)).unwrap();
    let mut held = Vec::new();
    let roots = ["/d", "/a/b", "/c", "/a-b"];
    for root in roots {
        held.push(park(&pool, root).await);
    }
    let dispatch = Arc::new(atomic::AtomicUsize::new(0));
    let expired = {
        let dispatch = dispatch.clone();
        pool.submit("/expired".into(), move || {
            dispatch.fetch_add(1, atomic::Ordering::Relaxed);
        })
        .unwrap()
    };
    let (job, release) = held.pop().unwrap();
    assert_eq!(job.wait().await, Err(RootIoError::DeadlineElapsed));
    assert_eq!(expired.wait().await, Err(RootIoError::DeadlineElapsed));
    assert!(pool.shared.state.lock().unwrap().queue.is_empty());
    let health = pool.health().unwrap();
    assert_eq!(health.charged_workers, 4);
    assert_eq!(health.stuck_roots, ["/a-b", "/a/b", "/c", "/d"]);
    for _ in 0..20 {
        let duplicate = pool.submit("/d".into(), || panic!("duplicate work ran"));
        assert!(matches!(duplicate, Err(RootIoError::RootBusy)));
    }
    let Err(refused) = pool.submit("/new".into(), || panic!("saturated worker ran")) else {
        panic!("work was queued with all workers charged");
    };
    assert_eq!(refused, RootIoError::WorkersCharged);
    assert_eq!(
        super::super::GateError::from(refused).code,
        "config_unavailable"
    );
    release.send(()).unwrap();
    tokio::time::timeout(TEST_LIMIT, async {
        while pool.health().unwrap().charged_workers != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pool.submit("/healthy".into(), || ())
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(dispatch.load(atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn root_io_worker_skips_expired_jobs_when_no_caller_waits() {
    let pool = RootIoPool::with_timeout(Duration::from_millis(300)).unwrap();
    let mut held = Vec::new();
    for index in 0..ROOT_IO_WORKERS {
        held.push(park(&pool, &format!("/held-{index}")).await);
    }
    let (signal, started_if_dispatched) = oneshot::channel();
    let expired = pool
        .submit("/expired".into(), move || signal.send(()).unwrap())
        .unwrap();
    let deadline = expired.deadline;
    drop(expired);
    tokio::time::sleep_until(deadline.into()).await;
    assert_eq!(pool.shared.state.lock().unwrap().queue.len(), 1);
    let (_, release) = held.pop().unwrap();
    release.send(()).unwrap();
    assert!(tokio::time::timeout(TEST_LIMIT, started_if_dispatched)
        .await
        .unwrap()
        .is_err());
}

#[tokio::test]
async fn root_io_running_near_deadline_keeps_original_submission_deadline() {
    let pool = RootIoPool::with_timeout(Duration::from_secs(2)).unwrap();
    let mut held = Vec::new();
    for index in 0..ROOT_IO_WORKERS {
        held.push(park(&pool, &format!("/held-{index}")).await);
    }
    let (signal, entered) = oneshot::channel();
    let (finish, resume) = mpsc::channel();
    let queued = pool
        .submit("/near-deadline".into(), move || {
            signal.send(()).unwrap();
            let _ = resume.recv();
        })
        .unwrap();
    let deadline = queued.deadline;
    let (_, release) = held.pop().unwrap();
    tokio::time::sleep_until((deadline - Duration::from_millis(500)).into()).await;
    release.send(()).unwrap();
    started(entered).await;
    let result = tokio::time::timeout_at(
        (deadline + Duration::from_millis(500)).into(),
        queued.wait(),
    )
    .await
    .expect("dispatch must not reset the submission deadline");
    assert_eq!(result, Err(RootIoError::DeadlineElapsed));
    assert!(pool
        .health()
        .unwrap()
        .stuck_roots
        .contains(&"/near-deadline".into()));
    finish.send(()).unwrap();
    drop(held);
    tokio::time::timeout(TEST_LIMIT, async {
        while pool.health().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
