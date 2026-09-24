use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use subc_protocol::session::{HealthReport, HealthStatus};
use tokio::time::Instant;

use super::{
    cause, is_own_violation, Answer, Class, Monitor, Probe, RoundOutcome, Timing, Verdict,
};
use crate::{
    bootstrap,
    runtime::{SeamResult, SentinelHealth},
};

fn timing(period_ms: u64, timeout_ms: u64) -> Timing {
    Timing {
        period: Duration::from_millis(period_ms),
        timeout: Duration::from_millis(timeout_ms),
    }
}

fn answered() -> RoundOutcome {
    RoundOutcome::Answered {
        reply_subject: "_INBOX.UBOX.reply".to_string(),
    }
}

fn no_reply() -> RoundOutcome {
    RoundOutcome::Failed {
        class: Class::Unavailable,
        cause: cause::NO_REPLY,
        message: "no reply".to_string(),
    }
}

fn class(report: &HealthReport) -> Option<&str> {
    report.metrics.as_ref().and_then(|m| m["class"].as_str())
}

fn cause_of(report: &HealthReport) -> Option<&str> {
    report.metrics.as_ref().and_then(|m| m["cause"].as_str())
}

#[test]
fn a_new_process_answers_down_unavailable_until_its_first_round() {
    let monitor = Monitor::new("1-1".to_string(), timing(1000, 200));
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Failing);
    assert_eq!(report.detail.as_deref(), Some("bus.health.down"));
    assert_eq!(class(&report), Some("Unavailable"));
    assert_eq!(cause_of(&report), Some(cause::NO_ANSWER_YET));
    assert_eq!(report.metrics.as_ref().unwrap()["incarnation"], "1-1");
}

#[test]
fn up_takes_a_success_and_down_takes_three_consecutive_failures() {
    let monitor = Monitor::new("1-1".to_string(), timing(1000, 200));
    assert!(matches!(
        monitor.record(answered()),
        Some(Verdict::Up { .. })
    ));
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Ok);
    assert_eq!(report.detail.as_deref(), Some("bus.health.up"));
    assert_eq!(class(&report), None, "no class while up");
    assert_eq!(
        report.metrics.as_ref().unwrap()["reply_subject"],
        "_INBOX.UBOX.reply"
    );

    assert_eq!(monitor.record(no_reply()), None, "first failure tolerated");
    assert_eq!(monitor.record(no_reply()), None, "second failure tolerated");
    assert_eq!(monitor.report().status, HealthStatus::Ok);
    assert!(matches!(
        monitor.record(no_reply()),
        Some(Verdict::Down {
            class: Class::Unavailable,
            ..
        })
    ));
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Failing);
    assert_eq!(class(&report), Some("Unavailable"));
    assert_eq!(cause_of(&report), Some(cause::NO_REPLY));

    // A success in between resets the count.
    monitor.record(answered());
    monitor.record(no_reply());
    monitor.record(no_reply());
    monitor.record(answered());
    monitor.record(no_reply());
    monitor.record(no_reply());
    assert_eq!(monitor.report().status, HealthStatus::Ok);
}

#[test]
fn a_denied_round_while_down_reports_class_denied() {
    let monitor = Monitor::new("1-1".to_string(), timing(1000, 200));
    monitor.record(RoundOutcome::Failed {
        class: Class::Denied,
        cause: cause::DENIED,
        message: "Permissions Violation for Publish to \"_INBOX.UBOX.x\"".to_string(),
    });
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Failing);
    assert_eq!(class(&report), Some("Denied"));
    assert_eq!(cause_of(&report), Some(cause::DENIED));
}

#[test]
fn a_stale_success_is_not_up() {
    // Bound: 3 * 10 + 5 = 35 ms.
    let monitor = Monitor::new("1-1".to_string(), timing(10, 5));
    monitor.record(answered());
    assert_eq!(monitor.report().status, HealthStatus::Ok);
    std::thread::sleep(Duration::from_millis(60));
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Failing);
    assert_eq!(class(&report), Some("Unavailable"));
    assert_eq!(cause_of(&report), Some(cause::STALE));
}

#[test]
fn only_violations_on_the_sentinels_own_subjects_count() {
    let subject = "ck.box_x.sentinel.ping";
    assert!(is_own_violation(
        "Permissions Violation for Publish to \"_INBOX.UBOX.abc\"",
        subject,
        "UBOX"
    ));
    assert!(is_own_violation(
        "Permissions Violation for Subscription to \"ck.box_x.sentinel.ping\"",
        subject,
        "UBOX"
    ));
    assert!(!is_own_violation(
        "Permissions Violation for Publish to \"ck.box_x.effect.x\"",
        subject,
        "UBOX"
    ));
    assert!(!is_own_violation(
        "Permissions Violation for Publish to \"_INBOX.UOTHER.abc\"",
        subject,
        "UBOX"
    ));
    assert!(!is_own_violation(
        "maximum payload exceeded",
        subject,
        "UBOX"
    ));
}

/// Answers its scripted outcomes in order; once they run out, every round hangs
/// forever, as a round over a bus that stopped responding would without a timeout.
struct ScriptedProbe {
    script: Mutex<VecDeque<RoundOutcome>>,
}

#[async_trait]
impl Probe for ScriptedProbe {
    async fn round(&self, _round: u64, _timeout: Duration) -> RoundOutcome {
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(outcome) => outcome,
            None => std::future::pending().await,
        }
    }
}

struct FixedBootstrap(serde_json::Value);

#[async_trait]
impl SentinelHealth for FixedBootstrap {
    async fn report_health(&self) -> SeamResult<HealthReport> {
        Ok(HealthReport {
            status: HealthStatus::Failing,
            detail: Some("bus.health.down".to_string()),
            metrics: Some(self.0.clone()),
        })
    }
}

#[tokio::test]
async fn the_answer_is_bootstraps_until_bootstrap_is_ready() {
    let (_ready_tx, ready) = tokio::sync::watch::channel(None);
    let monitor = Arc::new(Monitor::new("1-1".to_string(), timing(1000, 200)));
    let still_booting = Answer::new(
        Arc::new(FixedBootstrap(
            json!({"class": "Unavailable", "cause": bootstrap::cause::ROOT_KEY_UNREACHABLE}),
        )),
        ready.clone(),
        monitor.clone(),
    );
    let report = still_booting.report_health().await.unwrap();
    assert_eq!(
        cause_of(&report),
        Some(bootstrap::cause::ROOT_KEY_UNREACHABLE)
    );

    let finished = Answer::new(
        Arc::new(FixedBootstrap(
            json!({"class": "Unavailable", "cause": bootstrap::cause::SENTINEL_NOT_LANDED}),
        )),
        ready,
        monitor.clone(),
    );
    let report = finished.report_health().await.unwrap();
    assert_eq!(cause_of(&report), Some(cause::NO_ANSWER_YET));
    monitor.record(answered());
    assert_eq!(
        finished.report_health().await.unwrap().status,
        HealthStatus::Ok
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_bus_cannot_slow_the_health_answer() {
    // One answered round, then every round hangs. Bound: 3 * 40 + 20 = 140 ms.
    let timing = timing(40, 20);
    let monitor = Arc::new(Monitor::new("1-1".to_string(), timing));
    let probe = Arc::new(ScriptedProbe {
        script: Mutex::new(VecDeque::from([answered()])),
    });
    let (_ready_tx, ready) = tokio::sync::watch::channel(None);
    let answer = Answer::new(
        Arc::new(FixedBootstrap(
            json!({"cause": bootstrap::cause::SENTINEL_NOT_LANDED}),
        )),
        ready,
        monitor.clone(),
    );
    let driving = {
        let monitor = monitor.clone();
        tokio::spawn(async move {
            monitor.drive(probe.as_ref(), &|_verdict| {}).await;
        })
    };

    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    let mut seen_up = false;
    let mut down_after_up = None;
    while started.elapsed() < Duration::from_millis(800) {
        let asked = Instant::now();
        let report = answer.report_health().await.unwrap();
        slowest = slowest.max(asked.elapsed());
        if report.status == HealthStatus::Ok {
            seen_up = true;
        } else if seen_up && down_after_up.is_none() {
            down_after_up = Some((cause_of(&report).unwrap().to_string(), started.elapsed()));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    driving.abort();
    assert!(seen_up, "the answered round reported up");
    let (cause, _) = down_after_up.expect("the hung bus is reported down");
    assert!(
        cause == cause::NO_REPLY || cause == cause::STALE,
        "down names the hung round or the stale success: {cause}"
    );
    assert!(
        slowest < Duration::from_millis(20),
        "every health answer returns without waiting on the hung round; slowest {slowest:?}"
    );
}
