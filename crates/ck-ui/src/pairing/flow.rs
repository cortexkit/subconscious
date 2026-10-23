//! The Add a phone exchange with callosum, as a state machine with no I/O.
//!
//! The app feeds it keys, the access groups the latest `callosum.devices_read`
//! lists ([`Reading`]), callosum's replies and watch events ([`Event`]) and the
//! clock; it answers with the calls to make ([`Command`]) and with what the
//! overlay shows. The live layer makes the calls and hands their results back
//! as events, so every rule here is tested by feeding scripted events.
//!
//! The rules it keeps:
//! - A group is chosen by name, never by position. A listed group named after
//!   the device type is pre-selected; otherwise the person picks one with tab.
//! - At most one `callosum.pairing_qr_open` is in flight. Opens asked for
//!   meanwhile collapse into one, carrying the latest group, sent when the
//!   reply lands. Each open has a local generation, and a reply whose
//!   generation is no longer the one the overlay waits for is never drawn.
//! - Every watch event and confirm reply names its window. A result for a
//!   window the overlay no longer shows is recorded on Devices as the last
//!   action and never touches the overlay.
//! - A window whose confirm is in flight keeps its watch after the overlay
//!   closes or moves on, until its terminal event or the confirm's timeout.

use crate::model::{AccessGroup, AccessGroups, Devices, Panel};
use crate::pairing::{Claim, PairingState};
use crate::time::Timestamp;

/// The device type the overlay pairs. Callosum also pairs simulators; this
/// overlay adds phones only.
pub const CLASS: &str = "phone";

/// How long `⏎` stays ignored after a backoff refusal that names no wait.
const BACKOFF_MS: i64 = 30_000;

/// How long a confirm call may run. Callosum asks for Touch ID and only then
/// re-checks the claim, whose own 120 s limit a pending prompt doesn't
/// extend, so the call can outlive both the claim and the window. Callosum
/// waits up to 290 s for Touch ID (`operator.confirm`'s deadline in
/// subc-client-rs, the module SDK it is built on); this leaves room past it
/// for callosum's answer to arrive.
pub const CONFIRM_TIMEOUT_MS: u64 = 300_000;

/// How ck shows a phone callosum's registry has no name for.
pub const NO_NAME: &str = "(no name)";

/// What the latest `callosum.devices_read` says about the access groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// No reply has arrived yet.
    Waiting,
    /// The latest read failed or was refused, with its code.
    Failed(String),
    /// The reply carried `self.expose_profiles` in a shape ck can't read.
    Malformed,
    /// The groups to offer, in callosum's order.
    Groups(Vec<AccessGroup>),
}

impl Reading {
    /// The reading the Devices panel holds now. A failed latest read wins
    /// over an older good one: the groups it listed may have changed.
    pub fn of(devices: &Panel<Devices>) -> Reading {
        if let Some(error) = &devices.error {
            return Reading::Failed(error.clone());
        }
        if !devices.loaded {
            return Reading::Waiting;
        }
        match &devices.data.access_groups {
            AccessGroups::Listed(list) => Reading::Groups(list.clone()),
            // A callosum from before the list offers these two, and says
            // nothing about how many operations each allows.
            AccessGroups::Unlisted => Reading::Groups(
                ["phone", "sim"]
                    .map(|name| AccessGroup {
                        name: name.into(),
                        operation_count: None,
                    })
                    .into(),
            ),
            AccessGroups::Malformed => Reading::Malformed,
        }
    }
}

/// A refusal from callosum: its code, its message, and the wait a backoff
/// refusal names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: String,
    pub message: String,
    pub retry_after_ms: Option<u64>,
}

impl Refusal {
    pub fn code(code: impl Into<String>) -> Refusal {
        Refusal {
            code: code.into(),
            message: String::new(),
            retry_after_ms: None,
        }
    }
}

/// A `callosum.pairing_qr_open` reply.
#[derive(Clone, PartialEq, Eq)]
pub struct Opened {
    pub window_id: String,
    /// The text the code encodes. It carries the window's one-time secret.
    pub qr_payload: String,
    pub expires_at_ms: i64,
    /// When the reply arrived, which the "works for N minutes" sentence
    /// counts from.
    pub received_at: Timestamp,
}

// Written by hand so the payload, which carries the pairing secret, never
// reaches a log.
impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("window_id", &self.window_id)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

/// One event on a window's `callosum.pairing_qr_watch` stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Open,
    Claimed(Claim),
    ForeignAttempt(String),
    Granted,
    Declined,
    Expired,
}

/// What a `callosum.pairing_qr_confirm` reply says when it isn't refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Granted,
    Mismatch,
}

/// A confirm that didn't come back with a verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmFailure {
    Refused(Refusal),
    /// The call timed out or lost its connection: whether callosum acted on
    /// it is unknown.
    Lost,
}

/// A result the live layer hands back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The reply to the open with this generation.
    Opened {
        gen: u64,
        result: Result<Opened, Refusal>,
    },
    Watch {
        window_id: String,
        event: WatchEvent,
    },
    /// The window's watch failed to open, or ended before a terminal event.
    WatchEnded { window_id: String },
    /// The window's watch sent a notice ck can't read; its watch is closed.
    WatchUnreadable { window_id: String },
    Confirmed {
        window_id: String,
        result: Result<Verdict, ConfirmFailure>,
    },
}

/// A call for the live layer to make.
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    /// Read `callosum.devices_read` now.
    ReadGroups,
    /// `callosum.pairing_qr_open {class: "phone", expose_profile: group}`.
    Open { gen: u64, group: String },
    /// Subscribe to the window's `callosum.pairing_qr_watch` on a route of
    /// its own.
    Watch { window_id: String },
    /// Close that subscription and its route.
    Unwatch { window_id: String },
    /// `callosum.pairing_qr_confirm {window_id, words}`, which may run for
    /// `timeout`.
    Confirm {
        window_id: String,
        words: [String; 4],
        timeout: std::time::Duration,
    },
    /// Show this on Devices as the last action: the result of a window the
    /// overlay no longer shows.
    Record { text: String, ok: bool },
}

// Written by hand so the typed words never reach a log.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::ReadGroups => f.write_str("ReadGroups"),
            Command::Open { gen, group } => f
                .debug_struct("Open")
                .field("gen", gen)
                .field("group", group)
                .finish(),
            Command::Watch { window_id } => f
                .debug_struct("Watch")
                .field("window_id", window_id)
                .finish(),
            Command::Unwatch { window_id } => f
                .debug_struct("Unwatch")
                .field("window_id", window_id)
                .finish(),
            Command::Confirm {
                window_id, timeout, ..
            } => f
                .debug_struct("Confirm")
                .field("window_id", window_id)
                .field("timeout", timeout)
                .finish_non_exhaustive(),
            Command::Record { text, ok } => f
                .debug_struct("Record")
                .field("text", text)
                .field("ok", ok)
                .finish(),
        }
    }
}

/// Where the overlay is, which decides its status line and what `⏎` and
/// tab do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    // Before a window is open.
    ReadingGroups,
    GroupsFailed(String),
    GroupsMalformed,
    NoGroups,
    Choose,
    Opening,
    OpenFailed(String),
    NoSuchGroup(String),
    // With a window.
    /// Open, or claimed and waiting for the words.
    Waiting,
    /// A confirm is in flight: callosum is asking for Touch ID.
    TouchId,
    TouchIdRefused,
    /// Too many tries: `⏎` is ignored until `until`. `named` says whether
    /// callosum gave the wait.
    Backoff {
        until: Timestamp,
        named: bool,
    },
    /// `None` when callosum's registry has no name for the phone.
    Granted {
        device_name: Option<String>,
    },
    Mismatch,
    NotPending,
    Failed(String),
    Declined,
    Expired,
    LostContact,
    Unknown,
    /// Callosum sent a watch notice ck can't read; the overlay stops there.
    Unreadable,
}

impl Stage {
    /// Whether the window is over as far as the overlay is concerned: its
    /// code is no longer drawn, and only a new code goes further.
    pub fn closed(&self) -> bool {
        matches!(
            self,
            Stage::Granted { .. }
                | Stage::Mismatch
                | Stage::NotPending
                | Stage::Failed(_)
                | Stage::Declined
                | Stage::Expired
                | Stage::LostContact
                | Stage::Unknown
                | Stage::Unreadable
        )
    }
}

/// How a window ended, for the overlay or for Devices.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Granted,
    Mismatch,
    Declined,
    Expired,
    NotPending,
    Failed(String),
    LostContact,
    Unknown,
    Unreadable,
    TouchIdRefused,
    Backoff,
}

/// A window ck watches: the one the overlay shows, or one held for its
/// confirm after the overlay moved on.
#[derive(Clone, Debug)]
struct Exchange {
    window_id: String,
    /// The claim's device name once the window is claimed; the inner `None`
    /// when callosum's registry has no name for the phone.
    device_name: Option<Option<String>>,
    /// Set while a confirm is in flight.
    confirm: Option<Timestamp>,
    /// The last confirm's deadline, which bounds a held watch.
    deadline: Option<Timestamp>,
    /// A `declined` that arrived while the confirm was in flight, kept until
    /// its reply decides.
    declined: bool,
    reply_lost: bool,
    watch_lost: bool,
    /// The overlay no longer shows this window.
    held: bool,
}

/// The whole exchange: what the overlay shows and every window ck still
/// watches.
#[derive(Clone, Debug)]
pub struct Flow {
    visible: bool,
    /// The window the overlay shows, with its code and the typed words.
    pub window: Option<PairingState>,
    /// The group chosen for the next open, by name.
    picked: Option<String>,
    pub stage: Stage,
    reading: Reading,
    /// The latest well-formed list, for the open window's operation count.
    last_list: Option<Vec<AccessGroup>>,
    next_gen: u64,
    /// The open in flight: its generation and group.
    in_flight: Option<(u64, String)>,
    /// The group of an open asked for while another was in flight.
    pending: Option<String>,
    /// The generation whose reply the overlay waits for.
    current: Option<u64>,
    exchanges: Vec<Exchange>,
    /// Calls to make, collected by the app.
    pub outbox: Vec<Command>,
    now: Timestamp,
}

impl Default for Flow {
    fn default() -> Flow {
        Flow {
            visible: false,
            window: None,
            picked: None,
            stage: Stage::Waiting,
            reading: Reading::Waiting,
            last_list: None,
            next_gen: 0,
            in_flight: None,
            pending: None,
            current: None,
            exchanges: Vec::new(),
            outbox: Vec::new(),
            now: Timestamp::from_unix_millis(0),
        }
    }
}

impl Flow {
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The group chosen for the next open, if any.
    pub fn picked(&self) -> Option<&str> {
        self.picked.as_deref()
    }

    /// The groups the latest well-formed reading lists, while no group is
    /// chosen: the overlay shows them for the person to pick from.
    pub fn offered(&self) -> &[AccessGroup] {
        match &self.reading {
            Reading::Groups(list) => list,
            _ => &[],
        }
    }

    /// The operation count the latest list gives a group, if it lists it.
    pub fn count(&self, group: &str) -> Option<u64> {
        self.last_list
            .as_ref()?
            .iter()
            .find(|g| g.name == group)?
            .operation_count
    }

    /// Whether the latest list leaves out the open window's group.
    pub fn group_gone(&self) -> bool {
        match (&self.window, &self.last_list) {
            (Some(w), Some(list)) => !list.iter().any(|g| g.name == w.group),
            _ => false,
        }
    }

    /// Whether the open window's code may still be scanned and so is drawn.
    pub fn code_shown(&self) -> bool {
        self.window.is_some() && (!self.stage.closed() || self.granted())
    }

    pub fn granted(&self) -> bool {
        matches!(self.stage, Stage::Granted { .. })
    }

    /// Windows whose watch ck keeps open, the shown one included.
    pub fn watched(&self) -> usize {
        self.exchanges.len()
    }

    /// The status line the overlay shows now.
    pub fn status(&self) -> Option<String> {
        let line: String = match &self.stage {
            Stage::ReadingGroups => "Reading this Mac's access groups…".into(),
            Stage::GroupsFailed(code) => {
                format!("Couldn't read this Mac's access groups: {code}")
            }
            Stage::GroupsMalformed => "Couldn't read this Mac's access groups".into(),
            Stage::NoGroups => "This Mac has no access groups".into(),
            Stage::Choose => "Choose this phone's access · tab".into(),
            Stage::Opening => "Opening a code…".into(),
            Stage::OpenFailed(code) => format!("Couldn't open a code: {code} · ⏎ try again"),
            Stage::NoSuchGroup(group) => {
                format!("This Mac has no access group named {group} · tab changes the group")
            }
            Stage::Waiting | Stage::Backoff { .. } | Stage::TouchIdRefused if self.group_gone() => {
                let group = self.window.as_ref().map_or("", |w| w.group.as_str());
                format!("{group} is no longer offered on this Mac · tab picks another")
            }
            Stage::Waiting => {
                let claimed = self.window.as_ref().is_some_and(|w| w.claim.is_some());
                if claimed {
                    return None;
                }
                "Waiting for your phone…".into()
            }
            Stage::TouchId => "Waiting for Touch ID".into(),
            Stage::TouchIdRefused => "Touch ID was declined · ⏎ try again".into(),
            Stage::Backoff { until, named } => {
                if *named {
                    let left = (until.unix_millis() - self.now.unix_millis()).max(0);
                    format!(
                        "Touch ID is cooling down · try again in {} s",
                        (left + 999) / 1000
                    )
                } else {
                    "Touch ID is cooling down · try again shortly".into()
                }
            }
            Stage::Granted { device_name } => {
                format!("{} is paired.", device_name.as_deref().unwrap_or(NO_NAME))
            }
            Stage::Mismatch => "The words didn't match · this code is closed · ⏎ new code".into(),
            Stage::NotPending => "The claim expired or was cancelled · ⏎ scan a new code".into(),
            Stage::Failed(code) => match refusal_sentence(code) {
                Some(sentence) => format!("Pairing failed: {sentence}"),
                None => format!("Pairing failed: {code}"),
            },
            Stage::Unreadable => "Callosum sent a pairing update ck can't read".into(),
            Stage::Declined => "Pairing was declined · ⏎ new code".into(),
            Stage::Expired => "The code expired · ⏎ new code".into(),
            Stage::LostContact => "Lost contact with callosum · ⏎ new code".into(),
            Stage::Unknown => {
                "Whether pairing finished is unknown · check Devices · ⏎ new code".into()
            }
        };
        Some(line)
    }

    // ---- the overlay opening and closing --------------------------------------

    /// The overlay opens: read the groups if none have arrived, then choose.
    pub fn open(&mut self) {
        self.visible = true;
        self.picked = None;
        self.stage = Stage::ReadingGroups;
        if self.reading == Reading::Waiting {
            self.outbox.push(Command::ReadGroups);
        }
        self.choose();
    }

    /// `esc`: the overlay closes. Nothing is cancelled on callosum, which has
    /// no cancel operation; a window whose confirm is in flight keeps its
    /// watch so its result still reaches Devices.
    pub fn close(&mut self) {
        self.retire_window();
        self.visible = false;
        self.picked = None;
        self.pending = None;
        self.current = None;
        self.stage = Stage::Waiting;
    }

    // ---- readings and events ---------------------------------------------------

    /// A new reading of the access groups.
    pub fn observe(&mut self, reading: Reading) {
        if let Reading::Groups(list) = &reading {
            self.last_list = Some(list.clone());
        }
        self.reading = reading;
        if let Some(w) = &self.window {
            let count = self.count(&w.group);
            if let Some(w) = &mut self.window {
                w.operation_count = count;
            }
        }
        self.choose();
    }

    /// The clock moved on: timeouts and the end of a backoff.
    pub fn tick(&mut self, now: Timestamp) {
        self.now = now;
        if let Stage::Backoff { until, .. } = self.stage {
            if now >= until {
                self.stage = Stage::Waiting;
            }
        }
        while let Some(i) = self
            .exchanges
            .iter()
            .position(|x| x.deadline.is_some_and(|d| now >= d) && (x.held || x.confirm.is_some()))
        {
            let shown = self.shows(i);
            let x = &mut self.exchanges[i];
            x.confirm = None;
            if x.declined {
                self.decide(i, Outcome::Declined);
            } else if !shown || x.watch_lost {
                self.decide(i, Outcome::Unknown);
            } else {
                // The overlay still shows the window: a terminal event may
                // still decide it, until the window itself expires.
                x.reply_lost = true;
                x.deadline = None;
                self.clear_words();
                self.stage = Stage::Unknown;
            }
        }
    }

    pub fn absorb(&mut self, event: Event) {
        match event {
            Event::Opened { gen, result } => self.opened(gen, result),
            Event::Watch { window_id, event } => self.watch_event(&window_id, event),
            Event::WatchEnded { window_id } => self.watch_ended(&window_id),
            Event::WatchUnreadable { window_id } => {
                if let Some(i) = self.exchange(&window_id) {
                    self.decide(i, Outcome::Unreadable);
                }
            }
            Event::Confirmed { window_id, result } => self.confirmed(&window_id, result),
        }
    }

    fn opened(&mut self, gen: u64, result: Result<Opened, Refusal>) {
        let group = match self.in_flight.take() {
            Some((g, group)) if g == gen => group,
            other => {
                self.in_flight = other;
                return;
            }
        };
        if let Some(next) = self.pending.take() {
            // Superseded before it landed: the next open cancels this window
            // on callosum, so it is neither drawn nor watched.
            if self.visible {
                self.send_open(next);
            }
            return;
        }
        if self.current != Some(gen) || !self.visible {
            return;
        }
        self.current = None;
        match result {
            Ok(o) => {
                let count = self.count(&group);
                let expires = Timestamp::from_unix_millis(o.expires_at_ms);
                match PairingState::new(&o.qr_payload, group, count, o.received_at, expires) {
                    Ok(mut w) => {
                        w.window_id = o.window_id.clone();
                        self.window = Some(w);
                        self.exchanges.push(Exchange {
                            window_id: o.window_id.clone(),
                            device_name: None,
                            confirm: None,
                            deadline: None,
                            declined: false,
                            reply_lost: false,
                            watch_lost: false,
                            held: false,
                        });
                        self.outbox.push(Command::Watch {
                            window_id: o.window_id,
                        });
                        self.stage = Stage::Waiting;
                    }
                    // fed-pairing-v1 payloads always fit a QR code; one that
                    // doesn't is a reply ck can't show.
                    Err(_) => self.stage = Stage::OpenFailed("payload too long".into()),
                }
            }
            Err(r) => {
                let unknown_group = matches!(
                    r.code.as_str(),
                    "fed_bad_request" | "fed_target_unavailable"
                ) && r.message.contains("unknown expose profile");
                self.stage = if unknown_group {
                    Stage::NoSuchGroup(group)
                } else {
                    Stage::OpenFailed(r.code)
                };
            }
        }
    }

    fn watch_event(&mut self, window_id: &str, event: WatchEvent) {
        let Some(i) = self.exchange(window_id) else {
            return;
        };
        let shown = self.shows(i);
        match event {
            WatchEvent::Open => {}
            WatchEvent::Claimed(claim) => {
                self.exchanges[i].device_name = Some(claim.device_name.clone());
                if shown {
                    if let Some(w) = &mut self.window {
                        w.claim = Some(claim);
                        w.word_sel = 0;
                    }
                }
            }
            WatchEvent::ForeignAttempt(addr) => {
                if shown {
                    if let Some(w) = &mut self.window {
                        w.foreign_attempt = Some(addr);
                    }
                }
            }
            WatchEvent::Granted => self.decide(i, Outcome::Granted),
            WatchEvent::Expired => self.decide(i, Outcome::Expired),
            WatchEvent::Declined => {
                if self.exchanges[i].confirm.is_some() {
                    // A mismatch also declines the window; the confirm's
                    // reply says which it was.
                    self.exchanges[i].declined = true;
                } else {
                    self.decide(i, Outcome::Declined);
                }
            }
        }
    }

    fn watch_ended(&mut self, window_id: &str) {
        let Some(i) = self.exchange(window_id) else {
            return;
        };
        let x = &mut self.exchanges[i];
        x.watch_lost = true;
        if x.confirm.is_some() {
            return;
        }
        let outcome = if x.reply_lost {
            Outcome::Unknown
        } else {
            Outcome::LostContact
        };
        self.decide(i, outcome);
    }

    fn confirmed(&mut self, window_id: &str, result: Result<Verdict, ConfirmFailure>) {
        let Some(i) = self.exchange(window_id) else {
            return;
        };
        let x = &mut self.exchanges[i];
        let in_flight = x.confirm.take().is_some();
        let declined = x.declined;
        match result {
            Ok(Verdict::Granted) => self.decide(i, Outcome::Granted),
            Ok(Verdict::Mismatch) => self.decide(i, Outcome::Mismatch),
            // A failure that arrives after the confirm already timed out
            // says nothing new.
            Err(_) if !in_flight => {}
            Err(_) if declined => self.decide(i, Outcome::Declined),
            Err(ConfirmFailure::Refused(r)) => {
                let outcome = match r.code.as_str() {
                    "fed_operator_confirmation_refused" => Outcome::TouchIdRefused,
                    "fed_operator_confirmation_backoff" => {
                        let named = r.retry_after_ms.is_some();
                        let wait = r.retry_after_ms.map_or(BACKOFF_MS, |ms| ms as i64);
                        if self.shows(i) {
                            self.stage = Stage::Backoff {
                                until: self.now.add_millis(wait),
                                named,
                            };
                            return;
                        }
                        Outcome::Backoff
                    }
                    "fed_pairing_not_pending" => Outcome::NotPending,
                    code => Outcome::Failed(code.to_string()),
                };
                if outcome == Outcome::TouchIdRefused && self.shows(i) {
                    // The window stays pending and the words stay typed.
                    self.stage = Stage::TouchIdRefused;
                    return;
                }
                self.decide(i, outcome);
            }
            Err(ConfirmFailure::Lost) => {
                let shown = self.shows(i);
                let x = &mut self.exchanges[i];
                if x.watch_lost {
                    self.decide(i, Outcome::Unknown);
                } else if !shown {
                    // Held until a terminal event or the confirm's deadline.
                    x.reply_lost = true;
                } else {
                    // The confirm is never sent again; a terminal event on
                    // the watch may still decide.
                    x.reply_lost = true;
                    x.deadline = None;
                    self.clear_words();
                    self.stage = Stage::Unknown;
                }
            }
        }
    }

    // ---- keys ------------------------------------------------------------------

    /// `⏎`. Returns whether anything changed.
    pub fn enter(&mut self) -> bool {
        match self.stage.clone() {
            Stage::GroupsFailed(_) => {
                self.outbox.push(Command::ReadGroups);
                self.stage = Stage::ReadingGroups;
                true
            }
            Stage::OpenFailed(_) => match self.picked.clone() {
                Some(group) => {
                    self.request_open(group);
                    true
                }
                None => false,
            },
            Stage::Mismatch
            | Stage::NotPending
            | Stage::Declined
            | Stage::Expired
            | Stage::LostContact
            | Stage::Unknown => {
                let group = self
                    .window
                    .as_ref()
                    .map(|w| w.group.clone())
                    .or_else(|| self.picked.clone());
                match group {
                    Some(group) => {
                        self.request_open(group);
                        true
                    }
                    None => false,
                }
            }
            Stage::Granted { .. } => {
                self.close();
                true
            }
            Stage::Waiting | Stage::TouchIdRefused => self.confirm(),
            Stage::Backoff { until, .. } => {
                if self.now < until {
                    return false;
                }
                self.confirm()
            }
            _ => false,
        }
    }

    /// Tab: the next group, or the next word field while the window is
    /// claimed and its group still offered.
    pub fn tab(&mut self) -> bool {
        if let Some(w) = &self.window {
            if w.claim.is_some() && !self.stage.closed() && !self.group_gone() {
                return self.move_word(1);
            }
            // After a pairing, or a notice ck couldn't read, the overlay
            // offers nothing more.
            if self.granted() || self.stage == Stage::Unreadable {
                return false;
            }
            let after = w.group.clone();
            return self.next_group(Some(&after));
        }
        match self.stage {
            Stage::Choose | Stage::Opening | Stage::OpenFailed(_) | Stage::NoSuchGroup(_) => {
                let after = match &self.stage {
                    Stage::NoSuchGroup(g) => Some(g.clone()),
                    _ => self.picked.clone(),
                };
                self.next_group(after.as_deref())
            }
            _ => false,
        }
    }

    /// A printable key on the word step. Space moves to the next field.
    pub fn type_char(&mut self, ch: char) -> bool {
        if !self.typing() {
            return false;
        }
        if ch == ' ' {
            return self.move_word(1);
        }
        let w = self.window.as_mut().expect("typing needs a window");
        let i = w.word_sel % 4;
        w.words[i].push(ch);
        true
    }

    /// Backspace: deletes, or moves back from an empty field.
    pub fn backspace(&mut self) -> bool {
        if !self.typing() {
            return false;
        }
        let w = self.window.as_mut().expect("typing needs a window");
        let i = w.word_sel % 4;
        if w.words[i].pop().is_none() {
            w.word_sel = (i + 3) % 4;
        }
        true
    }

    /// Moves between word fields by `delta`.
    pub fn move_word(&mut self, delta: i32) -> bool {
        if !self.typing() {
            return false;
        }
        let w = self.window.as_mut().expect("typing needs a window");
        w.word_sel = (w.word_sel as i32 + delta).rem_euclid(4) as usize;
        true
    }

    fn typing(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.claim.is_some()) && !self.stage.closed()
    }

    // ---- internals ---------------------------------------------------------------

    /// Before any open: pre-select the group named after the device type,
    /// or wait for the person to pick one, or say why there is none.
    fn choose(&mut self) {
        let undecided = matches!(
            self.stage,
            Stage::ReadingGroups
                | Stage::GroupsFailed(_)
                | Stage::GroupsMalformed
                | Stage::NoGroups
                | Stage::Choose
        );
        if !self.visible || self.window.is_some() || self.current.is_some() || !undecided {
            return;
        }
        let listed = |name: &str| match &self.reading {
            Reading::Groups(list) => list.iter().any(|g| g.name == name),
            _ => false,
        };
        if self.picked.is_none() && listed(CLASS) {
            self.request_open(CLASS.to_string());
            return;
        }
        self.stage = match &self.reading {
            Reading::Waiting => Stage::ReadingGroups,
            Reading::Failed(code) => Stage::GroupsFailed(code.clone()),
            Reading::Malformed => Stage::GroupsMalformed,
            Reading::Groups(list) if list.is_empty() => Stage::NoGroups,
            Reading::Groups(_) => Stage::Choose,
        };
    }

    /// The group after `after` in the latest well-formed list, or its first
    /// group when `after` isn't listed; then opens a window with it.
    fn next_group(&mut self, after: Option<&str>) -> bool {
        let Reading::Groups(list) = &self.reading else {
            return false;
        };
        if list.is_empty() {
            return false;
        }
        let at = after.and_then(|a| list.iter().position(|g| g.name == a));
        let next = match at {
            Some(i) => list[(i + 1) % list.len()].name.clone(),
            None => list[0].name.clone(),
        };
        self.request_open(next);
        true
    }

    /// Asks for a window with `group`. The window on screen goes; the open
    /// is sent now, or after the one in flight lands.
    fn request_open(&mut self, group: String) {
        self.retire_window();
        self.picked = Some(group.clone());
        self.stage = Stage::Opening;
        if self.in_flight.is_some() {
            self.pending = Some(group);
            self.current = None;
        } else {
            self.send_open(group);
        }
    }

    fn send_open(&mut self, group: String) {
        self.next_gen += 1;
        let gen = self.next_gen;
        self.in_flight = Some((gen, group.clone()));
        self.current = Some(gen);
        self.outbox.push(Command::Open { gen, group });
    }

    /// The overlay stops showing its window. Its watch closes, unless its
    /// confirm is in flight.
    fn retire_window(&mut self) {
        let Some(w) = self.window.take() else {
            return;
        };
        if let Some(i) = self.exchange(&w.window_id) {
            if self.exchanges[i].confirm.is_some() {
                self.exchanges[i].held = true;
            } else {
                let x = self.exchanges.remove(i);
                self.outbox.push(Command::Unwatch {
                    window_id: x.window_id,
                });
            }
        }
    }

    fn exchange(&self, window_id: &str) -> Option<usize> {
        self.exchanges.iter().position(|x| x.window_id == window_id)
    }

    /// Whether the overlay shows exchange `i`'s window.
    fn shows(&self, i: usize) -> bool {
        let x = &self.exchanges[i];
        !x.held
            && self.visible
            && self
                .window
                .as_ref()
                .is_some_and(|w| w.window_id == x.window_id)
    }

    /// Exchange `i` is over. The overlay shows the outcome if it shows the
    /// window; otherwise Devices records it.
    fn decide(&mut self, i: usize, outcome: Outcome) {
        let shown = self.shows(i);
        let x = self.exchanges.remove(i);
        self.outbox.push(Command::Unwatch {
            window_id: x.window_id.clone(),
        });
        if outcome == Outcome::Granted {
            // Devices lists the new device at its next reading; read now.
            self.outbox.push(Command::ReadGroups);
        }
        if shown {
            self.clear_words();
            self.stage = match outcome {
                Outcome::Granted => Stage::Granted {
                    device_name: x.device_name.clone().flatten(),
                },
                Outcome::Mismatch => Stage::Mismatch,
                Outcome::Declined => Stage::Declined,
                Outcome::Expired => Stage::Expired,
                Outcome::NotPending => Stage::NotPending,
                Outcome::Failed(code) => Stage::Failed(code),
                Outcome::LostContact => Stage::LostContact,
                Outcome::Unknown => Stage::Unknown,
                Outcome::Unreadable => Stage::Unreadable,
                Outcome::TouchIdRefused => Stage::TouchIdRefused,
                Outcome::Backoff => Stage::Backoff {
                    until: self.now.add_millis(BACKOFF_MS),
                    named: false,
                },
            };
            return;
        }
        // Only a claimed window's confirm can outlive the overlay, so a
        // window recorded here has always been claimed.
        let who = match &x.device_name {
            Some(Some(name)) => name.as_str(),
            Some(None) => NO_NAME,
            None => "the phone",
        };
        let (text, ok) = match outcome {
            Outcome::Granted => (format!("{who} is paired"), true),
            Outcome::Mismatch => (format!("Pairing {who}: the words didn't match"), false),
            Outcome::Declined => (format!("Pairing {who} was declined"), false),
            Outcome::Expired => (format!("Pairing {who}: the code expired"), false),
            Outcome::NotPending => (
                format!("Pairing {who}: the claim expired or was cancelled"),
                false,
            ),
            Outcome::Failed(code) => match refusal_sentence(&code) {
                Some(sentence) => (format!("Pairing {who} failed: {sentence}"), false),
                None => (format!("Pairing {who} failed: {code}"), false),
            },
            Outcome::LostContact => (format!("Pairing {who}: lost contact with callosum"), false),
            Outcome::Unknown => (
                format!("Whether pairing {who} finished is unknown · check Devices"),
                false,
            ),
            Outcome::Unreadable => (
                format!("Pairing {who}: callosum sent a pairing update ck can't read"),
                false,
            ),
            Outcome::TouchIdRefused => (format!("Pairing {who}: Touch ID was declined"), false),
            Outcome::Backoff => (format!("Pairing {who}: Touch ID is cooling down"), false),
        };
        self.outbox.push(Command::Record { text, ok });
    }

    /// `⏎` on the word step: with fewer than four words, focus the first
    /// empty field; otherwise send one confirm with the words trimmed.
    fn confirm(&mut self) -> bool {
        let Some(w) = &self.window else {
            return false;
        };
        if w.claim.is_none() {
            return false;
        }
        let Some(i) = self.exchange(&w.window_id) else {
            return false;
        };
        if self.exchanges[i].confirm.is_some() {
            return false;
        }
        let words = w.words.clone().map(|s| s.trim().to_string());
        if let Some(empty) = words.iter().position(String::is_empty) {
            let w = self.window.as_mut().expect("checked above");
            w.word_sel = empty;
            return true;
        }
        let deadline = self.now.add_millis(CONFIRM_TIMEOUT_MS as i64);
        let window_id = w.window_id.clone();
        let x = &mut self.exchanges[i];
        x.confirm = Some(deadline);
        x.deadline = Some(deadline);
        x.declined = false;
        self.stage = Stage::TouchId;
        self.outbox.push(Command::Confirm {
            window_id,
            words,
            timeout: std::time::Duration::from_millis(CONFIRM_TIMEOUT_MS),
        });
        true
    }

    fn clear_words(&mut self) {
        if let Some(w) = &mut self.window {
            w.words = std::array::from_fn(|_| String::new());
            w.word_sel = 0;
        }
    }
}

/// The plain sentence for a confirm refusal callosum documents, which reads
/// after "Pairing failed: ". Codes it doesn't document have none, and show as
/// they came.
pub fn refusal_sentence(code: &str) -> Option<&'static str> {
    Some(match code {
        "fed_not_authorized" => "callosum didn't authorize ck to confirm it",
        "fed_bad_request" => "callosum couldn't read the confirm ck sent",
        "fed_target_unavailable" => {
            "callosum is missing its rendezvous credential, LAN listener or store"
        }
        "fed_not_ready" => "callosum isn't ready yet",
        _ => return None,
    })
}
