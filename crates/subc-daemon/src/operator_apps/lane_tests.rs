use super::*;
use std::{
    future::{poll_fn, Future},
    pin::pin,
    task::Poll,
};

struct Blocked {
    released: Mutex<bool>,
    wake: Condvar,
    entered: tokio::sync::Semaphore,
}

impl Blocked {
    fn wait(&self) {
        let guard = self.released.lock().unwrap();
        self.entered.add_permits(1);
        drop(self.wake.wait_while(guard, |released| !*released).unwrap());
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct Release(Arc<Blocked>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn two_stuck_threads_refuse_immediately_and_serve_after_release() {
    let lane = PeerIdentityLane::start();
    let blocked = Arc::new(Blocked {
        released: Mutex::new(false),
        wake: Condvar::new(),
        entered: tokio::sync::Semaphore::new(0),
    });
    let _release = Release(blocked.clone());
    let block = || {
        let blocked = blocked.clone();
        move || {
            blocked.wait();
            7
        }
    };
    let entered = async {
        tokio::time::timeout(BUDGET, blocked.entered.acquire_many(2))
            .await
            .expect("both OS threads started")
            .unwrap()
            .forget();
    };
    let (first, second, ()) = tokio::join!(lane.run(block()), lane.run(block()), entered);
    assert_eq!((first, second), (None, None));
    assert_eq!(lane.stuck_slots(), 2);

    let mut refused = pin!(lane.run(|| panic!("a refused job cannot run")));
    poll_fn(|cx| {
        assert_eq!(refused.as_mut().poll(cx), Poll::Ready(None::<()>));
        Poll::Ready(())
    })
    .await;

    blocked.release();
    tokio::time::timeout(BUDGET, async {
        while lane.stuck_slots() == 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(lane.run(|| 42).await, Some(42));
    })
    .await
    .expect("lane returns to service after native call returns");
}
