//! Named-computer pairing and device actions. Requests carry stable identities,
//! so a refreshed list cannot redirect a person's choice to another computer.

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use crate::model::Devices;
use crate::pairing::flow::Refusal;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub fingerprint: String,
    pub name: String,
    pub platform: String,
    pub state: String,
}

pub fn platform(value: &str) -> &str {
    match value {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => value,
    }
}

pub fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub fingerprint: Option<String>,
    pub account_name: Option<String>,
    pub computers: std::collections::BTreeMap<String, Details>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub platform: Option<String>,
    pub pairing: Option<String>,
    pub since: Option<i64>,
    pub shares: Option<Vec<Share>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Share {
    pub module: String,
    pub share: String,
    pub label: String,
    pub enabled: bool,
    pub needs_review: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareCall {
    pub fingerprint: String,
    pub module: String,
    pub share: String,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Candidates { ticket: u64 },
    Forget { ticket: u64, fingerprint: String },
    ReadDevices,
    Open { ticket: u64, fingerprint: String },
    Watch { window_id: u64 },
    Unwatch { window_id: u64 },
    Confirm { window_id: u64, matches: bool },
    Cancel { window_id: u64 },
    Sharing { ticket: u64, call: ShareCall },
}

impl Command {
    pub fn request(&self) -> Option<(&'static str, Value)> {
        match self {
            Self::Candidates { .. } => Some(("callosum.pairing_candidates_read", json!({}))),
            Self::Forget { fingerprint, .. } => {
                Some(("callosum.unpair", json!({ "fingerprint": fingerprint })))
            }
            Self::ReadDevices => None,
            Self::Open { fingerprint, .. } => Some((
                "callosum.pairing_computer_open",
                json!({"fingerprint": fingerprint}),
            )),
            Self::Confirm { window_id, matches } => Some((
                "callosum.pairing_computer_confirm",
                json!({"window_id":window_id,"match":matches}),
            )),
            Self::Cancel { window_id } => {
                Some(("callosum.pairing_cancel", json!({"window_id":window_id})))
            }
            Self::Watch { .. } | Self::Unwatch { .. } => None,
            Self::Sharing { call, .. } => Some((
                "callosum.sharing_set",
                json!({"fingerprint":call.fingerprint,"module":call.module,"share":call.share,"enabled":call.enabled}),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Candidates {
        ticket: u64,
        result: Result<(bool, Vec<Candidate>), Refusal>,
    },
    Forgotten {
        ticket: u64,
        result: Result<(), Refusal>,
    },
    Opened {
        ticket: u64,
        result: Result<Opened, Refusal>,
    },
    Watch {
        window_id: u64,
        state: Watch,
    },
    WatchLost {
        window_id: u64,
    },
    Confirmed {
        window_id: u64,
        matches: bool,
        result: Result<(), Refusal>,
    },
    Shared {
        ticket: u64,
        result: Result<(), Refusal>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub window_id: u64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    Waiting,
    Connecting,
    Code { code: String, confirm_by_ms: i64 },
    PeerConfirmed { code: String, confirm_by_ms: i64 },
    Paired { name: String, typed: bool },
    Declined { peer: bool },
    Expired { peer: bool },
    Failed { reason: String },
}

impl Watch {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Paired { .. }
                | Self::Declined { .. }
                | Self::Expired { .. }
                | Self::Failed { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub target: Candidate,
    pub opened: Opened,
    pub state: Watch,
    pub confirming: bool,
    pub answered: bool,
    pub retry_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Flow {
    pub session: Option<Session>,
    pub opening: Option<Candidate>,
    pub picker: bool,
    pub waiting: bool,
    pub candidates: Vec<Candidate>,
    pub selected: Option<String>,
    pub error: Option<String>,
    pub list_failed: bool,
    pub detail: Option<String>,
    pub remembered: Option<(crate::model::Device, Details)>,
    pub forget: bool,
    pub partial: bool,
    pub busy: bool,
    pub notice: Option<String>,
    pub outbox: Vec<Command>,
    ticket: u64,
    poll_at: i64,
    now: i64,
    pub refreshed: Option<String>,
    pub share_selected: Option<(String, String)>,
    pub share_pending: Option<(u64, ShareCall)>,
    pub share_error: Option<(ShareCall, Refusal, i64)>,
}

impl Flow {
    pub fn close(&mut self) {
        if self.session.is_some() {
            self.session_key(KeyCode::Esc);
        }
        self.picker = false;
        self.detail = None;
        self.forget = false;
        self.busy = false;
        self.ticket += 1;
    }

    pub fn open(&mut self) {
        self.picker = true;
        self.waiting = true;
        self.error = None;
        self.list_failed = false;
        self.candidates.clear();
        self.read_candidates();
    }

    fn read_candidates(&mut self) {
        if !self.busy {
            self.ticket += 1;
            self.busy = true;
            self.outbox.push(Command::Candidates {
                ticket: self.ticket,
            });
        }
    }

    pub fn tick(&mut self, now: i64) {
        self.now = now;
        if let Some(session) = &mut self.session {
            if session.retry_at.is_some_and(|at| now >= at) && !session.state.terminal() {
                session.retry_at = None;
                self.outbox.push(Command::Watch {
                    window_id: session.opened.window_id,
                });
            }
        }
        if self.picker && now >= self.poll_at {
            self.poll_at = now + 3_000;
            self.read_candidates();
        }
    }

    pub fn picked(&self) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|d| Some(&d.fingerprint) == self.selected.as_ref())
    }

    pub fn key(&mut self, key: KeyCode, devices: &Devices) -> bool {
        self.remember(devices);
        if self.session.is_some() {
            self.session_key(key);
            return true;
        }
        if self.picker {
            match key {
                KeyCode::Esc => {
                    self.picker = false;
                    self.busy = false;
                    self.ticket += 1;
                }
                KeyCode::Enter if !self.busy && !self.list_failed && !self.waiting => {
                    if let Some(target) = self.picked().filter(|d| d.state == "available").cloned()
                    {
                        self.start(target);
                    }
                }
                KeyCode::Up | KeyCode::Down => {
                    let available: Vec<_> = self
                        .candidates
                        .iter()
                        .filter(|d| d.state == "available")
                        .collect();
                    let at = available
                        .iter()
                        .position(|d| Some(&d.fingerprint) == self.selected.as_ref())
                        .unwrap_or(0);
                    if !available.is_empty() {
                        let next = if key == KeyCode::Up {
                            at.saturating_sub(1)
                        } else {
                            (at + 1).min(available.len() - 1)
                        };
                        self.selected = Some(available[next].fingerprint.clone());
                    }
                }
                _ => {}
            }
            return true;
        }
        if self.forget {
            match key {
                KeyCode::Esc if !self.busy => {
                    self.forget = false;
                    self.error = None;
                }
                KeyCode::Char('x') if !self.busy => {
                    if let Some(fingerprint) = &self.detail {
                        self.ticket += 1;
                        self.busy = true;
                        self.outbox.push(Command::Forget {
                            ticket: self.ticket,
                            fingerprint: fingerprint.clone(),
                        });
                    }
                }
                _ => {}
            }
            return true;
        }
        if self.detail.is_some() {
            match key {
                KeyCode::Esc => self.detail = None,
                KeyCode::Char('x') if self.share_pending.is_none() => {
                    self.forget = true;
                    self.partial = false;
                    self.error = None;
                }
                KeyCode::Up | KeyCode::Down if self.share_pending.is_none() => {
                    let list = self.shares(devices);
                    let at = self.share_index(devices);
                    if !list.is_empty() {
                        let next = if key == KeyCode::Up {
                            at.saturating_sub(1)
                        } else {
                            (at + 1).min(list.len() - 1)
                        };
                        self.share_selected =
                            Some((list[next].module.clone(), list[next].share.clone()));
                    }
                }
                KeyCode::Enter if self.share_pending.is_none() => {
                    if let Some(share) = self.shares(devices).get(self.share_index(devices)) {
                        let call = ShareCall {
                            fingerprint: self.detail.clone().unwrap_or_default(),
                            module: share.module.clone(),
                            share: share.share.clone(),
                            label: share.label.clone(),
                            enabled: !share.enabled || share.needs_review,
                        };
                        let cooling = self.share_error.as_ref().is_some_and(|(old, e, until)| {
                            old.fingerprint == call.fingerprint
                                && old.module == call.module
                                && old.share == call.share
                                && e.code == "fed_operator_confirmation_backoff"
                                && self.now < *until
                        });
                        if !cooling {
                            self.ticket += 1;
                            self.share_pending = Some((self.ticket, call.clone()));
                            self.share_error = None;
                            self.outbox.push(Command::Sharing {
                                ticket: self.ticket,
                                call,
                            });
                        }
                    }
                }
                _ => {}
            }
            return true;
        }
        false
    }

    pub fn absorb(&mut self, event: Event, devices: &Devices) {
        match event {
            Event::Shared { ticket, result }
                if self
                    .share_pending
                    .as_ref()
                    .is_some_and(|(t, _)| *t == ticket) =>
            {
                let (_, call) = self.share_pending.take().expect("matching pending switch");
                match result {
                    Ok(()) => {
                        self.share_error = None;
                        self.outbox.push(Command::ReadDevices);
                    }
                    Err(e) => {
                        let until = self.now.saturating_add(
                            e.retry_after_ms.unwrap_or(30_000).min(i64::MAX as u64) as i64,
                        );
                        self.share_error = Some((call, e, until));
                    }
                }
            }
            Event::Opened { ticket, result } => {
                if ticket != self.ticket || !self.picker {
                    if let Ok(opened) = result {
                        self.outbox.push(Command::Cancel {
                            window_id: opened.window_id,
                        });
                    }
                    return;
                }
                self.busy = false;
                let Some(target) = self.opening.take() else {
                    return;
                };
                match result {
                    Ok(opened) => {
                        self.outbox.push(Command::Watch {
                            window_id: opened.window_id,
                        });
                        self.session = Some(Session {
                            target,
                            opened,
                            state: Watch::Waiting,
                            confirming: false,
                            answered: false,
                            retry_at: None,
                        });
                        self.picker = false;
                        self.error = None;
                    }
                    Err(e) if e.code == "fed_bad_request" => {
                        self.refreshed = Some(format!(
                            "{} on {} can't be paired right now, so the list was read again.",
                            target.name,
                            platform(&target.platform)
                        ));
                        self.read_candidates();
                    }
                    Err(e) => self.error = Some(message(&e)),
                }
            }
            Event::Watch { window_id, state } => {
                if let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| s.opened.window_id == window_id && !s.state.terminal())
                {
                    // The peer's answer must not influence the local comparison.
                    session.state = match state {
                        Watch::PeerConfirmed {
                            code,
                            confirm_by_ms,
                        } => Watch::Code {
                            code,
                            confirm_by_ms,
                        },
                        other => other,
                    };
                    session.retry_at = None;
                    self.error = None;
                    if session.state.terminal() {
                        session.confirming = false;
                        self.outbox.push(Command::Unwatch { window_id });
                        self.outbox.push(Command::ReadDevices);
                    }
                }
            }
            Event::WatchLost { window_id } => {
                if let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| s.opened.window_id == window_id && !s.state.terminal())
                {
                    session.retry_at = Some(self.now + 3_000);
                    self.error = Some("callosum unreachable · retrying".into());
                }
            }
            Event::Confirmed {
                window_id,
                matches,
                result,
            } => {
                if let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| s.opened.window_id == window_id && !s.state.terminal())
                {
                    session.confirming = false;
                    match result {
                        Ok(()) => session.answered = matches,
                        Err(e)
                            if e.code == "fed_pairing_not_pending"
                                || e.code == "fed_operator_confirmation_refused" => {}
                        Err(e) => self.error = Some(message(&e)),
                    }
                }
            }
            Event::Candidates { ticket, result } if self.picker && ticket == self.ticket => {
                self.busy = false;
                match result {
                    Ok((waiting, list)) => {
                        self.list_failed = false;
                        self.waiting = waiting;
                        self.candidates = list;
                        if !self.picked().is_some_and(|d| d.state == "available") {
                            self.selected = self
                                .candidates
                                .iter()
                                .find(|d| d.state == "available")
                                .map(|d| d.fingerprint.clone());
                        }
                        self.error = None;
                    }
                    Err(e) => {
                        self.list_failed = true;
                        self.waiting = false;
                        self.error = Some(message(&e));
                    }
                }
            }
            Event::Forgotten { ticket, result } if self.forget && ticket == self.ticket => {
                self.busy = false;
                match result {
                    Ok(()) => self.forgotten(devices, false),
                    Err(e) if self.partial && e.code == "fed_not_paired" => {
                        self.forgotten(devices, true)
                    }
                    Err(e) if e.code == "fed_target_unavailable" => self.partial = true,
                    Err(e) => self.error = Some(message(&e)),
                }
            }
            _ => {}
        }
    }

    pub fn shares<'a>(&self, devices: &'a Devices) -> &'a [Share] {
        self.detail
            .as_ref()
            .and_then(|id| devices.inventory.computers.get(id))
            .and_then(|i| i.shares.as_deref())
            .unwrap_or(&[])
    }

    /// A partial forget can remove the list entry before the retry. Keep the
    /// identity being forgotten so the dialog still names the same computer.
    pub fn remember(&mut self, devices: &Devices) {
        if let Some(device) = devices
            .list
            .iter()
            .find(|d| d.fingerprint == self.detail && self.detail.is_some())
        {
            let details = self
                .detail
                .as_ref()
                .and_then(|id| devices.inventory.computers.get(id))
                .cloned()
                .unwrap_or_default();
            self.remembered = Some((device.clone(), details));
        }
    }

    pub fn share_index(&self, devices: &Devices) -> usize {
        self.shares(devices)
            .iter()
            .position(|s| {
                self.share_selected
                    .as_ref()
                    .is_some_and(|(module, share)| module == &s.module && share == &s.share)
            })
            .unwrap_or(0)
    }

    pub fn share_problem(&self, id: &str, share: &Share) -> Option<String> {
        let (_, error, until) = self.share_error.as_ref().filter(|(call, _, _)| {
            call.fingerprint == id && call.module == share.module && call.share == share.share
        })?;
        Some(match error.code.as_str() {
            "fed_operator_confirmation_refused" => "Confirmation declined · ⏎ try again".into(),
            "fed_operator_confirmation_backoff" => match error.retry_after_ms {
                Some(_) => format!(
                    "Confirmation is cooling down · try again in {} s",
                    ((until - self.now).max(0) + 999) / 1000
                ),
                None => "Confirmation is cooling down · try again shortly".into(),
            },
            _ => message(error),
        })
    }

    fn start(&mut self, target: Candidate) {
        self.ticket += 1;
        self.busy = true;
        self.error = None;
        self.refreshed = None;
        self.opening = Some(target.clone());
        self.outbox.push(Command::Open {
            ticket: self.ticket,
            fingerprint: target.fingerprint,
        });
    }

    fn session_key(&mut self, key: KeyCode) {
        let Some(session) = &mut self.session else {
            return;
        };
        let window_id = session.opened.window_id;
        match key {
            KeyCode::Esc => {
                if !session.state.terminal() {
                    self.outbox.push(Command::Cancel { window_id });
                }
                self.outbox.push(Command::Unwatch { window_id });
                self.session = None;
                self.error = None;
            }
            KeyCode::Char('y' | 'n')
                if matches!(session.state, Watch::Code { .. })
                    && !session.confirming
                    && !session.answered =>
            {
                session.confirming = true;
                self.outbox.push(Command::Confirm {
                    window_id,
                    matches: key == KeyCode::Char('y'),
                });
            }
            KeyCode::Enter if session.state.terminal() => {
                if matches!(session.state, Watch::Paired { .. }) {
                    self.detail = Some(session.target.fingerprint.clone());
                    self.session = None;
                    self.outbox.push(Command::ReadDevices);
                } else {
                    let target = session.target.clone();
                    self.session = None;
                    self.picker = true;
                    self.start(target);
                }
            }
            _ => {}
        }
    }

    fn forgotten(&mut self, devices: &Devices, already: bool) {
        let name = devices
            .list
            .iter()
            .find(|d| d.fingerprint == self.detail)
            .or_else(|| {
                self.remembered
                    .as_ref()
                    .filter(|(d, _)| d.fingerprint == self.detail)
                    .map(|(d, _)| d)
            })
            .map_or("The computer", |d| d.name.as_str());
        self.notice = Some(format!(
            "✓ {}. {name} still lists this computer until you forget it there too.",
            if already {
                "Already forgotten".into()
            } else {
                format!("{name} forgotten")
            }
        ));
        self.forget = false;
        self.detail = None;
        self.error = None;
        self.outbox.push(Command::ReadDevices);
    }
}

pub fn message(error: &Refusal) -> String {
    if error.message.is_empty() {
        error.code.clone()
    } else {
        error.message.clone()
    }
}

pub fn candidates(value: &Value) -> Result<(bool, Vec<Candidate>), Refusal> {
    let bad = || Refusal::code("unreadable computer list");
    let waiting = match value.get("registry").and_then(Value::as_str) {
        Some("waiting") => true,
        Some("synced") => false,
        _ => return Err(bad()),
    };
    let list = value
        .get("devices")
        .and_then(Value::as_array)
        .ok_or_else(bad)?;
    let mut out = Vec::new();
    for d in list {
        let text = |key| {
            d.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(bad)
        };
        let candidate = Candidate {
            fingerprint: text("fingerprint")?,
            name: text("name")?,
            platform: text("platform")?,
            state: text("state")?,
        };
        if !matches!(
            candidate.state.as_str(),
            "available" | "paired" | "unavailable"
        ) || !fingerprint(&candidate.fingerprint)
        {
            return Err(bad());
        }
        out.push(candidate);
    }
    if waiting && !out.is_empty() {
        return Err(bad());
    }
    Ok((waiting, out))
}

pub fn fingerprint(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn empty_reply(value: &Value) -> Result<(), Refusal> {
    if value.as_object().is_some_and(|v| v.is_empty()) {
        Ok(())
    } else {
        Err(Refusal::code("unreadable reply"))
    }
}

pub fn opened(value: &Value) -> Result<Opened, Refusal> {
    match (
        value.get("window_id").and_then(Value::as_u64),
        value.get("expires_at_ms").and_then(Value::as_i64),
    ) {
        (Some(window_id), Some(expires_at_ms)) if window_id > 0 && expires_at_ms > 0 => {
            Ok(Opened {
                window_id,
                expires_at_ms,
            })
        }
        _ => Err(Refusal::code("unreadable pairing reply")),
    }
}

pub fn watch(value: &Value) -> Result<Watch, Refusal> {
    let bad = || Refusal::code("unreadable pairing update");
    let text = |key| value.get(key).and_then(Value::as_str).ok_or_else(bad);
    let peer = || match text("by")? {
        "here" => Ok(false),
        "peer" => Ok(true),
        _ => Err(bad()),
    };
    Ok(match text("state")? {
        "waiting_for_device" => Watch::Waiting,
        "connecting" => Watch::Connecting,
        "code" | "peer_confirmed" => {
            let code = text("code")?;
            if code.len() != 7
                || code.as_bytes()[3] != b' '
                || !code
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| i == 3 || b.is_ascii_digit())
            {
                return Err(bad());
            }
            let code = code.replace(' ', "");
            let confirm_by_ms = value
                .get("confirm_by_ms")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or_else(bad)?;
            if text("state")? == "code" {
                Watch::Code {
                    code,
                    confirm_by_ms,
                }
            } else {
                Watch::PeerConfirmed {
                    code,
                    confirm_by_ms,
                }
            }
        }
        "paired" => Watch::Paired {
            name: text("name")?.into(),
            typed: match text("confirmed_with")? {
                "typed" => true,
                "prompt" => false,
                _ => return Err(bad()),
            },
        },
        "declined" => Watch::Declined { peer: peer()? },
        "expired" => Watch::Expired { peer: peer()? },
        "failed" => Watch::Failed {
            reason: text("reason")?.into(),
        },
        _ => return Err(bad()),
    })
}
