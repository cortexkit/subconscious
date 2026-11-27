use super::*;
use serde_json::{json, Value};
use std::collections::VecDeque;

const LOCAL: &str = "fed-computer:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PEER: &str = "fed-computer:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OTHER: &str = "fed-computer:cccccccccccccccccccccccccccccccc";
const KEY: &str = "opaque:broca-deployment:20";
const INCOMING_KEY: &str = "opaque:broca-deployment:3";
const FRESH: &[&str] = &["transfer", "send", "--id", KEY, "--to", PEER];
const RECEIVE: &[&str] = &["transfer", "receive"];

fn peer_row(name: &str, member_id: &str, trust: &str, class: &str) -> Value {
    json!({"name":name,"fingerprint":member_id.strip_prefix(MEMBER_PREFIX).unwrap(),"trust":trust,"device_class":class})
}

fn roster(rows: &[Value]) -> Value {
    json!({"devices": rows, "self": {"fingerprint": LOCAL.strip_prefix(MEMBER_PREFIX).unwrap()}})
}

fn default_roster() -> Value {
    roster(&[peer_row("Other PC", PEER, "verified", "computer")])
}

struct Fake {
    devices: Option<Value>,
    replies: VecDeque<(Vec<String>, CommandResult)>,
    report: Vec<String>,
}

fn result(code: i32, stdout: impl Into<String>) -> CommandResult {
    CommandResult {
        success: code == 0,
        exit_code: Some(code),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

impl Fake {
    fn new(devices: Value) -> Self {
        Self {
            devices: Some(devices),
            replies: VecDeque::new(),
            report: Vec::new(),
        }
    }
    fn queue(&mut self, args: &[&str], code: i32, stdout: impl Into<String>) {
        self.replies.push_back((
            args.iter().map(|&arg| arg.into()).collect(),
            result(code, stdout),
        ));
    }
    fn list(&mut self, records: &[(&str, &str)]) {
        let credentials = records
            .iter()
            .map(|(id, state)| json!({"id": id, "state": state}))
            .collect::<Vec<_>>();
        self.queue(
            &["list", "--json"],
            0,
            json!({"v":1,"credentials":credentials}).to_string(),
        );
    }
    fn grants(&mut self, rows: &str) {
        self.queue(&["member", "list"], 0, rows);
    }
    fn ready(&mut self, with_key: bool) {
        let mut records = vec![(TRANSFER_KEY, "active")];
        if with_key {
            records.push((KEY, "active"));
        }
        self.list(&records);
        self.grants(&format!("{PEER}  transfer_deliver  more fields\n"));
    }
    fn run(&mut self, with: &str) -> Result<(), String> {
        let mut lines = Vec::new();
        let outcome = share_keys(with, Path::new("ck-auth"), self, |line| lines.push(line));
        assert!(self.devices.is_none(), "Callosum was not read");
        assert!(
            self.replies.is_empty(),
            "missed auth calls: {:?}",
            self.replies
        );
        self.report = lines;
        outcome
    }
}

impl CommandRunner for Fake {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        assert_eq!(program, "ck-auth");
        let (expected, reply) = self
            .replies
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected auth call: {args:?}"));
        assert_eq!(args, expected);
        Ok(reply)
    }
    fn callosum_devices(&mut self) -> Result<CommandResult, String> {
        Ok(result(
            0,
            self.devices
                .take()
                .expect("only one Callosum read")
                .to_string(),
        ))
    }
}

#[test]
fn resolves_unique_name_fingerprint_and_member_id() {
    for selector in ["Other PC", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", PEER] {
        let mut fake = Fake::new(roster(&[
            peer_row("Other PC", PEER, "verified", "computer"),
            peer_row("Other PC", OTHER, "verified", "phone"),
            peer_row("Other PC", LOCAL, "verified", "computer"),
        ]));
        fake.ready(false);
        fake.queue(RECEIVE, 0, "no pending transfers\n");
        fake.run(selector).unwrap();
        assert!(fake
            .report
            .iter()
            .any(|line| line.contains(&format!("--with {LOCAL}"))));
    }
}

#[test]
fn ambiguity_lists_fingerprints_without_touching_the_vault() {
    let mut fake = Fake::new(roster(&[
        peer_row("Other PC", PEER, "verified", "computer"),
        peer_row("Other PC", OTHER, "verified", "computer"),
    ]));
    let error = fake.run("Other PC").unwrap_err();
    assert!(error.contains("More than one"));
    for member in [PEER, OTHER] {
        assert!(error.contains(member.strip_prefix(MEMBER_PREFIX).unwrap()));
    }
}

#[test]
fn refuses_unknown_and_unverified_computers() {
    for selector in ["other pc", "Fox", "Unverified", OTHER] {
        let mut fake = Fake::new(roster(&[
            peer_row("Other PC", PEER, "verified", "computer"),
            peer_row("Unverified", OTHER, "pending", "computer"),
        ]));
        assert!(fake
            .run(selector)
            .unwrap_err()
            .contains("No other verified paired computer"));
    }
}

#[test]
fn missing_transfer_key_and_delivery_grant_name_fixes_and_stop() {
    for (active, deliver) in [(true, false), (false, true), (false, false)] {
        let mut fake = Fake::new(default_roster());
        let state = if active { "active" } else { "retired" };
        fake.list(&[(TRANSFER_KEY, state), (KEY, "active")]);
        let grantee = if deliver { PEER } else { OTHER };
        fake.grants(&format!("{grantee}  transfer_deliver\n"));
        let error = fake.run("Other PC").unwrap_err();
        assert_eq!(
            error.contains(&format!("ck auth transfer init --self {LOCAL}")),
            !active
        );
        assert_eq!(
            error.contains(&format!(
                "ck auth member grant --member {PEER} --operation transfer_deliver"
            )),
            !deliver
        );
    }
}

fn receipt(transfer: &str, credential: &str, reply: &str) -> String {
    format!("{transfer}  {credential}  to {PEER}  reply {reply}\n")
}

#[test]
fn send_reports_pending_accepted_and_rejected_from_reply_not_exit_code() {
    let mut fake = Fake::new(default_roster());
    const TWO: &str = "opaque:broca-deployment:31";
    const THREE: &str = "opaque:broca-deployment:42";
    fake.list(&[
        (TRANSFER_KEY, "active"),
        (THREE, "active"),
        (KEY, "active"),
        (TWO, "active"),
        ("opaque:other:5", "active"),
    ]);
    fake.grants(&format!("{PEER}  transfer_deliver\n"));
    for (credential, transfer, code) in [
        (KEY, "out1", "pending"),
        (TWO, "out2", "accepted"),
        (THREE, "out3", "rejected"),
    ] {
        fake.queue(
            &["transfer", "send", "--id", credential, "--to", PEER],
            0,
            receipt(transfer, credential, code),
        );
    }
    fake.queue(RECEIVE, 0, "no pending transfers");
    fake.run("Other PC").unwrap();
    assert!(fake.report[0].contains("still pending on the other computer"));
    assert!(fake.report[1].contains("accepted"));
    assert!(fake.report[2].contains("rejected"));
}

#[test]
fn expired_starts_a_fresh_send() {
    let mut fake = Fake::new(default_roster());
    fake.ready(true);
    fake.queue(FRESH, 0, receipt("old", KEY, "expired"));
    fake.queue(FRESH, 0, receipt("new", KEY, "pending"));
    fake.queue(RECEIVE, 0, "no pending transfers");
    fake.run("Other PC").unwrap();
    assert!(fake.report[0].contains("(new)"));
}

#[test]
fn refusal_with_a_receipt_resumes_once_and_exit_one_is_final() {
    for resume_code in [0, 1] {
        let mut fake = Fake::new(default_roster());
        fake.ready(true);
        fake.queue(FRESH, 0, receipt("out1", KEY, "peer_unavailable"));
        fake.queue(
            &["transfer", "send", "--resume", "out1"],
            resume_code,
            if resume_code == 0 {
                receipt("out1", KEY, "accepted")
            } else {
                String::new()
            },
        );
        fake.queue(RECEIVE, 0, "no pending transfers");
        let outcome = fake.run("Other PC");
        if resume_code == 0 {
            assert!(fake.report[0].contains("accepted"));
            assert!(outcome.is_ok());
        } else {
            assert!(outcome.unwrap_err().contains("--resume out1"));
        }
    }
}

#[test]
fn exit_five_has_no_id_and_neither_resumes_nor_sends_again() {
    let mut fake = Fake::new(default_roster());
    fake.ready(true);
    fake.queue(FRESH, 5, "");
    fake.queue(RECEIVE, 0, "no pending transfers");
    let error = fake.run("Other PC").unwrap_err();
    assert!(error.contains(&format!("outcome unknown for {KEY}")));
    assert!(error.contains("Run `ck setup share-keys` on the other computer"));
}

#[test]
fn receives_only_the_selected_peers_deployment_keys_and_never_replaces_one() {
    let mut fake = Fake::new(default_roster());
    fake.ready(true);
    fake.queue(FRESH, 0, receipt("out1", KEY, "pending"));
    fake.queue(RECEIVE, 0, format!("in-new  from {PEER}  {INCOMING_KEY}  opaque\nin-old  from {PEER}  {KEY}  opaque\nin-other  from {PEER}  opaque:unrelated:1  opaque\nin-stranger  from {OTHER}  opaque:broca-deployment:1  opaque\nin-copy  from {PEER}  {INCOMING_KEY}  opaque\n"));
    fake.queue(
        &[
            "grant",
            "--principal",
            "reserved:broca",
            "--selector-kind",
            "exact",
            "--selector",
            INCOMING_KEY,
            "--operation",
            "read",
        ],
        0,
        "granted",
    );
    fake.queue(
        &["transfer", "accept", "in-new", "--from", PEER],
        0,
        format!("accepted {INCOMING_KEY} at version 1"),
    );
    fake.queue(
        &["transfer", "reject", "in-old", "--from", PEER],
        0,
        "in-old  rejected",
    );
    fake.queue(
        &["transfer", "reject", "in-copy", "--from", PEER],
        0,
        "in-copy  rejected",
    );
    fake.run("Other PC").unwrap();
    assert!(fake.report[1].contains("Accepted"));
    assert!(fake.report[2].contains("Rejected"));
    assert!(fake.report[3].contains("Rejected"));
    assert!(fake.report[4].contains(&format!("lowest active deployment key is {INCOMING_KEY}")));
}

#[test]
fn grant_refusal_leaves_the_incoming_key_pending() {
    let mut fake = Fake::new(default_roster());
    fake.ready(false);
    fake.queue(
        RECEIVE,
        0,
        format!("in-new  from {PEER}  {INCOMING_KEY}  opaque"),
    );
    fake.queue(
        &[
            "grant",
            "--principal",
            "reserved:broca",
            "--selector-kind",
            "exact",
            "--selector",
            INCOMING_KEY,
            "--operation",
            "read",
        ],
        5,
        "",
    );
    let error = fake.run("Other PC").unwrap_err();
    assert!(error.contains("left in-new pending"));
}
