//! The app: state, keys and mouse, rendering, and the terminal run loop.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use tokio::sync::watch;

use crate::draw::{tab_spans, Canvas};
use crate::model::{ActionKind, Daemon, LogView, Module, PaletteCommand, Panel, Snapshot, Status};
use crate::screens::logs::{self, LevelFilter, LogState};
use crate::screens::{self, overlays, View, MIN_H, MIN_W};
use crate::theme::{detect_theme, Theme};
use crate::time::Timestamp;

pub use crate::model::Tab;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    Palette,
    Help,
}

/// A one-line text field that has the keyboard, such as a list filter, a
/// settings value or a word the person types. While one is open, printable
/// keys go to it ahead of every app-wide key, so typing "q" or "1" into it
/// neither quits nor switches tabs. The screen that opened it reads `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    pub text: String,
}

/// What the app asks of whoever feeds it data. The app never waits for an
/// answer: the effects show up in later snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The `ck module restart`, `stop` and `start` requests.
    Act { kind: ActionKind, module: String },
    /// The module whose log the app shows (the Modules pane or the log
    /// view), or `None` when no log is on screen.
    WatchLog(Option<String>),
    /// Start the installed daemon service.
    StartDaemon,
}

/// A question waiting for an answer before an action is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub kind: ActionKind,
    pub module: String,
    /// What the action will do, in a sentence or two.
    pub consequence: String,
}

/// Everything the app remembers between frames. The data itself lives in the
/// [`Snapshot`] it is handed.
#[derive(Clone, Debug)]
pub struct App {
    pub theme: Theme,
    pub tab: Tab,
    pub overlay: Option<Overlay>,
    /// Selected row of Home's attention list.
    pub home_sel: usize,
    /// Selected module in Modules.
    pub mod_sel: usize,
    pub palette_query: String,
    pub palette_sel: usize,
    /// Seconds since the app started.
    pub tick: u64,
    pub quit: bool,
    /// The window's columns and rows as of the last event. Moving through
    /// Home's attention list depends on how many rows the layout gives it.
    pub size: (u16, u16),
    /// Where `esc` returns to after `⏎` opened something.
    back: Option<Tab>,
    /// The log view's settings while it is open over Modules.
    pub log: Option<LogState>,
    /// A confirmation waiting for an answer.
    pub confirm: Option<Confirm>,
    /// The all-metrics list's scroll position while it is open over
    /// Modules.
    pub metrics: Option<usize>,
    /// Whether the daemon-down screen shows the daemon's log.
    pub down_log: bool,
    /// The text field that has the keyboard, if any.
    pub field: Option<TextField>,
    /// Requests made since the run loop last collected them.
    pub requests: Vec<Request>,
}

impl App {
    pub fn new(theme: Theme) -> App {
        App {
            theme,
            tab: Tab::Home,
            overlay: None,
            home_sel: 0,
            mod_sel: 0,
            palette_query: String::new(),
            palette_sel: 0,
            tick: 0,
            quit: false,
            size: (120, 36),
            back: None,
            log: None,
            confirm: None,
            metrics: None,
            down_log: false,
            field: None,
            requests: Vec::new(),
        }
    }

    /// The module whose log is on screen, if any: the selected module while
    /// Modules is showing.
    pub fn log_focus(&self, snap: &Snapshot) -> Option<String> {
        if matches!(snap.daemon.data, Daemon::Down(_)) || self.tab != Tab::Modules {
            return None;
        }
        self.selected(snap).map(|m| m.id.clone())
    }

    /// The module whose palette commands list first: the selected one while
    /// Modules is showing.
    fn preferred_module<'a>(&self, snap: &'a Snapshot) -> Option<&'a str> {
        (self.tab == Tab::Modules)
            .then(|| self.selected(snap).map(|m| m.id.as_str()))
            .flatten()
    }

    /// The tab on screen: the chosen one while it shows, else Home. A tab
    /// stops showing when the module behind it leaves the roster.
    fn shown_tab(&self, snap: &Snapshot) -> Tab {
        if snap.visible_tabs().contains(self.tab) {
            self.tab
        } else {
            Tab::Home
        }
    }

    /// Returns to Home when the chosen tab no longer shows, because the
    /// module behind it left the roster. Returns true when it moved.
    pub fn follow_roster(&mut self, snap: &Snapshot) -> bool {
        if self.shown_tab(snap) == self.tab {
            return false;
        }
        self.tab = Tab::Home;
        self.back = None;
        self.log = None;
        self.metrics = None;
        self.field = None;
        true
    }

    /// The palette's commands for the tabs that show.
    fn palette_commands(&self, snap: &Snapshot) -> Vec<PaletteCommand> {
        overlays::shown_commands(&snap.commands, snap.visible_tabs())
    }

    fn selected<'a>(&self, snap: &'a Snapshot) -> Option<&'a Module> {
        let mods = &snap.modules.data;
        mods.get(self.mod_sel.min(mods.len().saturating_sub(1)))
    }

    /// The log view's data for the selected module, or an empty panel while
    /// its first reading is on the way.
    fn log_panel(&self, snap: &Snapshot) -> Panel<LogView> {
        let id = self
            .selected(snap)
            .map(|m| m.id.clone())
            .unwrap_or_default();
        match &snap.log_view {
            Some(p) if p.data.module == id => p.clone(),
            _ => Panel::waiting(
                LogView {
                    module: id,
                    lines: Vec::new(),
                    source: String::new(),
                },
                None,
            ),
        }
    }

    /// Draws the whole frame into `buf`.
    pub fn render(&self, buf: &mut Buffer, snap: &Snapshot, now: Timestamp) {
        let mut c = Canvas::new(buf, self.theme);
        if c.w < MIN_W || c.h < MIN_H {
            screens::too_small(&mut c);
            return;
        }
        let v = View {
            snap,
            now,
            tick: self.tick,
            home_sel: self.home_sel,
            mod_sel: self.mod_sel,
        };
        if let Daemon::Down(d) = &snap.daemon.data {
            if self.down_log {
                let panel = Panel::new(
                    snap.daemon.received_at,
                    LogView {
                        module: "daemon".into(),
                        lines: d.log_tail.clone(),
                        source: "the daemon's own log".into(),
                    },
                );
                screens::logs::draw(&mut c, &v, &panel, &LogState::default());
            } else {
                screens::down::draw(&mut c, &v, d);
            }
            return;
        }
        match (
            self.shown_tab(snap),
            &self.log,
            self.metrics,
            self.selected(snap),
        ) {
            (Tab::Home, ..) => screens::home::draw(&mut c, &v),
            (Tab::Modules, Some(state), ..) => {
                screens::logs::draw(&mut c, &v, &self.log_panel(snap), state)
            }
            (Tab::Modules, None, Some(scroll), Some(m)) => {
                screens::metrics::draw(&mut c, &v, m, scroll)
            }
            (Tab::Modules, ..) => screens::modules::draw(&mut c, &v),
            (Tab::Activity, ..) => screens::activity::draw(&mut c, &v),
            (Tab::Usage, ..) => screens::usage::draw(&mut c, &v),
            (Tab::Agents, ..) => screens::agents::draw(&mut c, &v),
            (Tab::Devices, ..) => screens::devices::draw(&mut c, &v),
            (Tab::Settings, ..) => screens::settings::draw(&mut c, &v),
        }
        screens::result_toast(&mut c, &v);
        if let Some(q) = &self.confirm {
            let title = match q.kind {
                ActionKind::Restart => format!("Restart {}?", q.module),
                ActionKind::Stop => format!("Stop {}?", q.module),
                ActionKind::Start => format!("Start {}?", q.module),
            };
            let yes = match q.kind {
                ActionKind::Restart => "Restart",
                ActionKind::Stop => "Stop",
                ActionKind::Start => "Start",
            };
            overlays::confirm(&mut c, &title, std::slice::from_ref(&q.consequence), yes);
            return;
        }
        match self.overlay {
            Some(Overlay::Palette) => overlays::palette(
                &mut c,
                &self.palette_commands(snap),
                &self.palette_query,
                self.palette_sel,
                self.preferred_module(snap),
            ),
            Some(Overlay::Help) => overlays::help(&mut c, snap.serves, snap.visible_tabs()),
            None => {}
        }
    }

    /// Handles one terminal event for a window of `size` (columns, rows).
    /// Returns true when the frame needs redrawing.
    pub fn handle_event(&mut self, event: &Event, snap: &Snapshot, size: (u16, u16)) -> bool {
        self.size = size;
        match event {
            Event::Key(key) => self.handle_key(*key, snap),
            Event::Mouse(mouse) => self.handle_mouse(*mouse, snap, size),
            Event::Resize(..) => true,
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, snap: &Snapshot) -> bool {
        // Some terminals also report key releases; act once per press.
        if key.kind == KeyEventKind::Release {
            return false;
        }
        self.follow_roster(snap);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return true;
        }
        if matches!(snap.daemon.data, Daemon::Down(_)) {
            match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('t') => self.theme = self.theme.toggled(),
                KeyCode::Enter if !self.down_log => self.requests.push(Request::StartDaemon),
                KeyCode::Char('l') => self.down_log = !self.down_log,
                KeyCode::Esc if self.down_log => self.down_log = false,
                _ => return false,
            }
            return true;
        }
        if self.confirm.is_some() {
            return self.confirm_key(key);
        }
        if let Some(handled) = self.field_key(key) {
            return handled;
        }
        if self.overlay == Some(Overlay::Palette) {
            return self.palette_key(key, snap);
        }
        if self.overlay.is_none() && self.tab == Tab::Modules && self.metrics.is_some() {
            if let Some(handled) = self.metrics_key(key, snap) {
                return handled;
            }
        }
        if self.overlay.is_none() && self.tab == Tab::Modules && self.log.is_some() {
            if let Some(handled) = self.log_key(key, snap) {
                return handled;
            }
        }
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        match key.code {
            KeyCode::Char('k') if ctrl => self.open_palette(),
            KeyCode::Char(':') => self.open_palette(),
            KeyCode::Char('?') => {
                self.overlay = match self.overlay {
                    Some(Overlay::Help) => None,
                    _ => Some(Overlay::Help),
                };
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('t') => self.theme = self.theme.toggled(),
            // Each tab keeps its number while others are hidden, so a hidden
            // tab's key does nothing rather than reaching another tab.
            KeyCode::Char(ch) if Tab::from_key(ch).is_some() => {
                match Tab::from_key(ch).filter(|t| snap.visible_tabs().contains(*t)) {
                    Some(tab) => self.switch(tab),
                    None => return false,
                }
            }
            KeyCode::Esc => {
                if self.overlay.is_some() {
                    self.overlay = None;
                } else if let Some(tab) = self.back.take() {
                    self.tab = tab;
                } else {
                    return false;
                }
            }
            _ if self.overlay.is_some() => return false,
            KeyCode::Up | KeyCode::Char('k') => return self.move_by(-1, snap),
            // Top and bottom of the list.
            KeyCode::Char('g') => return self.move_by(-1_000_000, snap),
            KeyCode::Char('G') => return self.move_by(1_000_000, snap),
            KeyCode::Down | KeyCode::Char('j') => return self.move_by(1, snap),
            KeyCode::Char('r') if self.tab == Tab::Modules => return self.restart(snap),
            KeyCode::Char('s') if self.tab == Tab::Modules => return self.stop_or_start(snap),
            KeyCode::Char('m') if self.tab == Tab::Modules && snap.serves.all_metrics => {
                if snap.modules.data.is_empty() {
                    return false;
                }
                self.metrics = Some(0);
            }
            KeyCode::Char('l') if self.tab == Tab::Modules => {
                if snap.modules.data.is_empty() {
                    return false;
                }
                self.log = Some(LogState::default());
            }
            KeyCode::Enter if self.tab == Tab::Home => {
                let att = snap.attention();
                let Some(m) = att.get(self.home_sel) else {
                    return false;
                };
                if let Some(i) = snap.modules.data.iter().position(|x| x.id == m.id) {
                    self.mod_sel = i;
                }
                self.tab = Tab::Modules;
                self.back = Some(Tab::Home);
            }
            _ => return false,
        }
        true
    }

    /// `r` on Modules: always asks first, naming what the restart
    /// interrupts (see [`interruption`]). A restart cuts off whatever the
    /// module is doing once its drain limit runs out, so even a healthy
    /// module's restart can lose work once confirmed.
    fn restart(&mut self, snap: &Snapshot) -> bool {
        let Some(m) = self.selected(snap) else {
            return false;
        };
        if m.action.is_some() {
            return false;
        }
        let id = &m.id;
        let state = match m.status {
            Status::Ok => String::new(),
            Status::Stopped => format!("{id} is stopped; a restart starts it. "),
            Status::Degraded => format!("{id} is degraded; a restart stops it mid-recovery. "),
            Status::Failed => format!(
                "{id} has failed; a restart resets its crash budget, and if the cause is still there it fails again. "
            ),
            Status::Starting => {
                format!("{id} is still starting; a restart throws that start away. ")
            }
            Status::Unknown => format!("Nothing has confirmed {id}'s health yet. "),
        };
        let consequence = format!("{state}{}", interruption(m, ActionKind::Restart));
        self.confirm = Some(Confirm {
            kind: ActionKind::Restart,
            module: id.clone(),
            consequence,
        });
        true
    }

    /// `s` on Modules: a stopped module starts at once; any other is stopped
    /// after asking first, naming what stopping interrupts.
    fn stop_or_start(&mut self, snap: &Snapshot) -> bool {
        let Some(m) = self.selected(snap) else {
            return false;
        };
        if m.action.is_some() {
            return false;
        }
        let id = m.id.clone();
        if m.status == Status::Stopped {
            self.requests.push(Request::Act {
                kind: ActionKind::Start,
                module: id,
            });
            return true;
        }
        self.confirm = Some(Confirm {
            kind: ActionKind::Stop,
            module: id.clone(),
            consequence: format!(
                "{} {id} stays stopped until you start it again.",
                interruption(m, ActionKind::Stop)
            ),
        });
        true
    }

    /// Keys for the open text field, ahead of every app-wide key. Printable
    /// keys type and backspace deletes; esc closes the field. ⏎, tab,
    /// shift-tab and the arrows are left to the screen that opened the field
    /// (`None`), and every other key is swallowed so it can't act behind
    /// the field. Ctrl-c is handled before this and still quits.
    fn field_key(&mut self, key: KeyEvent) -> Option<bool> {
        let field = self.field.as_mut()?;
        match key.code {
            KeyCode::Char(ch)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                field.text.push(ch)
            }
            KeyCode::Backspace => {
                field.text.pop();
            }
            KeyCode::Esc => self.field = None,
            KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right => return None,
            _ => return Some(false),
        }
        Some(true)
    }

    fn confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                if let Some(q) = self.confirm.take() {
                    self.requests.push(Request::Act {
                        kind: q.kind,
                        module: q.module,
                    });
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => self.confirm = None,
            _ => return false,
        }
        true
    }

    /// Keys for the log view. `None` lets the key fall through to the
    /// app-wide keys (tabs, palette, help, quit).
    fn log_key(&mut self, key: KeyEvent, snap: &Snapshot) -> Option<bool> {
        let panel = self.log_panel(snap);
        let rows = logs::rows(i32::from(self.size.1));
        let state = self.log.as_mut()?;
        if state.typing {
            match key.code {
                KeyCode::Esc => {
                    state.typing = false;
                    state.query.clear();
                }
                KeyCode::Enter => state.typing = false,
                KeyCode::Backspace => {
                    state.query.pop();
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.query.push(ch)
                }
                _ => return Some(false),
            }
            return Some(true);
        }
        let lines = logs::visible(&panel.data, state);
        let len = lines.len();
        match key.code {
            KeyCode::Esc => {
                if state.query.is_empty() {
                    self.log = None;
                } else {
                    state.query.clear();
                }
            }
            KeyCode::Char('/') => {
                state.typing = true;
                state.query.clear();
            }
            KeyCode::Char('f') => {
                state.paused = !state.paused;
                if !state.paused {
                    state.scroll = 0;
                }
            }
            KeyCode::Tab => {
                state.levels = state.levels.next();
                state.scroll = 0;
            }
            KeyCode::Char('n') => {
                // The next match below the top row shown, wrapping to the
                // first; the view scrolls to put it on the top row.
                let hits = logs::matches(&lines, &state.query);
                let first = logs::first_shown(len, rows, state.scroll);
                let Some(&next) = hits.iter().find(|&&i| i > first).or(hits.first()) else {
                    return Some(false);
                };
                state.paused = true;
                state.scroll = len.saturating_sub(rows).saturating_sub(next);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.paused = true;
                state.scroll = (state.scroll + 1).min(len.saturating_sub(rows));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.scroll = state.scroll.saturating_sub(1);
            }
            // The oldest line kept, and back to following the newest.
            KeyCode::Char('g') => {
                state.paused = true;
                state.scroll = len.saturating_sub(rows);
            }
            KeyCode::Char('G') => {
                state.paused = false;
                state.scroll = 0;
            }
            KeyCode::Char('m') => {
                let n = snap.modules.data.len();
                if n > 0 {
                    self.mod_sel = (self.mod_sel + 1) % n;
                    *state = LogState {
                        levels: state.levels,
                        ..LogState::default()
                    };
                }
            }
            _ => return None,
        }
        Some(true)
    }

    /// Keys for the all-metrics list. `None` lets the key fall through to
    /// the app-wide keys.
    fn metrics_key(&mut self, key: KeyEvent, snap: &Snapshot) -> Option<bool> {
        let len = self.selected(snap).map_or(0, |m| m.all_metrics.len());
        let rows = screens::metrics::rows(i32::from(self.size.1));
        let bottom = len.saturating_sub(rows);
        let scroll = self.metrics.as_mut()?;
        match key.code {
            KeyCode::Esc => self.metrics = None,
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(bottom),
            KeyCode::Char('g') => *scroll = 0,
            KeyCode::Char('G') => *scroll = bottom,
            _ => return None,
        }
        Some(true)
    }

    fn open_palette(&mut self) {
        self.overlay = Some(Overlay::Palette);
        self.palette_query.clear();
        self.palette_sel = 0;
    }

    fn switch(&mut self, tab: Tab) {
        self.tab = tab;
        self.overlay = None;
        self.back = None;
        self.log = None;
        self.metrics = None;
        self.field = None;
    }

    fn palette_key(&mut self, key: KeyEvent, snap: &Snapshot) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
            KeyCode::Down => {
                let n = overlays::matches(
                    &self.palette_commands(snap),
                    &self.palette_query,
                    self.preferred_module(snap),
                )
                .len();
                self.palette_sel = (self.palette_sel + 1).min(n.saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.palette_query.pop();
                self.palette_sel = 0;
            }
            KeyCode::Enter => {
                let commands = self.palette_commands(snap);
                let list =
                    overlays::matches(&commands, &self.palette_query, self.preferred_module(snap));
                let Some(cmd) = list.get(self.palette_sel.min(list.len().saturating_sub(1))) else {
                    return false;
                };
                let (area, title) = (cmd.area.clone(), cmd.title.clone());
                self.overlay = None;
                // Commands for Modules open it on the module they name. The
                // other areas' commands have no action in this app yet, so
                // the palette just closes.
                if area == "Modules" {
                    let named = snap
                        .modules
                        .data
                        .iter()
                        .position(|m| title.split_whitespace().any(|w| w == m.id));
                    if let Some(i) = named {
                        self.mod_sel = i;
                    }
                    self.tab = Tab::Modules;
                    self.back = None;
                    // A command naming a module and a verb does what it
                    // says, through the same path as the key for it.
                    if named.is_some() {
                        if title.starts_with("Restart ") {
                            self.restart(snap);
                        } else if title.starts_with("Stop ") || title.starts_with("Start ") {
                            self.stop_or_start(snap);
                        } else if title.contains(" logs") {
                            self.log = Some(LogState::default());
                        } else if title.starts_with("All metrics") {
                            self.metrics = Some(0);
                        }
                    }
                } else if area == "App" {
                    // The app's own commands, each what its key does.
                    match title.as_str() {
                        "Keys" => self.overlay = Some(Overlay::Help),
                        "Light or dark" => self.theme = self.theme.toggled(),
                        "Quit" => self.quit = true,
                        // A tab's name switches to it; the list holds only
                        // the tabs that show.
                        name => {
                            if let Some(tab) = Tab::from_name(name) {
                                self.switch(tab);
                            }
                        }
                    }
                }
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.palette_query.push(ch);
                self.palette_sel = 0;
            }
            _ => return false,
        }
        true
    }

    fn move_by(&mut self, delta: i32, snap: &Snapshot) -> bool {
        let (sel, len) = match self.tab {
            Tab::Home => {
                let (w, h) = (i32::from(self.size.0), i32::from(self.size.1));
                let rows = screens::home::attention_rows(w, h, snap.attention().len());
                (&mut self.home_sel, rows)
            }
            Tab::Modules => (&mut self.mod_sel, snap.modules.data.len()),
            // The other tabs' screens keep no selection here yet.
            _ => return false,
        };
        if len == 0 {
            return false;
        }
        let next = (*sel as i64 + i64::from(delta)).clamp(0, len as i64 - 1) as usize;
        let changed = next != *sel;
        *sel = next;
        changed
    }

    /// Clicks on a tab or a Modules row, and scrolling to move the selection.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, snap: &Snapshot, size: (u16, u16)) -> bool {
        self.follow_roster(snap);
        if self.overlay.is_some()
            || self.confirm.is_some()
            || matches!(snap.daemon.data, Daemon::Down(_))
        {
            return false;
        }
        let (w, h) = (i32::from(size.0), i32::from(size.1));
        if w < MIN_W || h < MIN_H {
            return false;
        }
        let (x, y) = (i32::from(mouse.column), i32::from(mouse.row));
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if y == 0 {
                    for (sx, sw, tab) in tab_spans(w, self.tab, snap.visible_tabs()) {
                        if x >= sx && x < sx + sw {
                            self.switch(tab);
                            return true;
                        }
                    }
                    return false;
                }
                if let (Tab::Modules, Some(state)) = (self.tab, self.log.as_mut()) {
                    // The level chips on the log view's top row.
                    if y == 3 {
                        let chips = [
                            (22, 12, LevelFilter::All),
                            (35, 10, LevelFilter::Warnings),
                            (46, 8, LevelFilter::Errors),
                        ];
                        if let Some((_, _, f)) =
                            chips.iter().find(|(cx, cw, _)| x >= *cx && x < cx + cw)
                        {
                            state.levels = *f;
                            state.scroll = 0;
                            return true;
                        }
                    }
                    return false;
                }
                if self.tab == Tab::Modules {
                    let l = screens::modules::layout(w, h);
                    if x >= 1 && x < 1 + l.lw {
                        let len = snap.modules.data.len();
                        if let Some(i) = screens::modules::module_at(w, h, len, self.mod_sel, y) {
                            self.mod_sel = i;
                            return true;
                        }
                    }
                }
                false
            }
            MouseEventKind::ScrollUp => self.move_by(-1, snap),
            MouseEventKind::ScrollDown => self.move_by(1, snap),
            _ => false,
        }
    }
}

/// What a restart or stop of `m` interrupts, from what the snapshot knows:
/// the routes open to it (and who holds them), the work in flight it
/// reports (when it reports any), and how long it gets to finish that work.
/// Unknown routes and an unknown drain limit are said to be unknown.
pub fn interruption(m: &Module, kind: ActionKind) -> String {
    let close = if kind == ActionKind::Restart {
        "close and reopen"
    } else {
        "close"
    };
    let routes = match &m.serving {
        None => "Its open routes are unknown.".to_string(),
        Some(list) if list.is_empty() => "It has no open routes.".to_string(),
        Some(list) => {
            let mut names: Vec<&str> = list.iter().map(String::as_str).collect();
            names.sort_unstable();
            names.dedup();
            format!(
                "Its {} open {} {close} ({}).",
                list.len(),
                if list.len() == 1 { "route" } else { "routes" },
                names.join(", ")
            )
        }
    };
    // In-flight work as the module itself reports it: a metric that says so
    // (among its headline metrics, else among all it reports, a top-level
    // key before a nested one), else its own health sentence.
    let mut all: Vec<&crate::model::Metric> = m.all_metrics.iter().collect();
    all.sort_by_key(|x| x.label.contains('.') || x.label.contains('['));
    let in_flight = m
        .metrics
        .iter()
        .chain(all)
        .find(|x| {
            let l = x.label.to_ascii_lowercase();
            l.contains("in_flight") || l.contains("inflight") || l.contains("in flight")
        })
        .map(|x| format!(" It reports {} {}.", x.label, x.value))
        .or_else(|| {
            (!m.summary.is_empty()).then(|| format!(" It reports: \u{201c}{}\u{201d}.", m.summary))
        })
        .unwrap_or_default();
    let drain = match m.drain_limit_secs {
        Some(d) => format!(
            " Work in flight gets up to {} to finish; whatever is left then is cut off.",
            crate::time::ago(d)
        ),
        None => " How long work in flight gets to finish is unknown.".to_string(),
    };
    format!("{routes}{in_flight}{drain}")
}

// ---- terminal setup ----------------------------------------------------------

/// Turns the terminal's raw mode on and off. The real one is
/// [`CrosstermRawMode`]; tests use a fake so they never touch a terminal.
pub trait RawMode {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}

pub struct CrosstermRawMode;

impl RawMode for CrosstermRawMode {
    fn enable(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

/// Puts the terminal into app mode (raw input, alternate screen, mouse
/// reporting, hidden cursor) and puts it back when dropped, including while a
/// panic unwinds.
pub struct TerminalGuard<W: Write, R: RawMode> {
    out: W,
    raw: R,
    active: bool,
}

impl<W: Write, R: RawMode> TerminalGuard<W, R> {
    pub fn enter(out: W, mut raw: R) -> io::Result<Self> {
        raw.enable()?;
        // From here on, dropping the guard undoes whatever got switched on.
        let mut guard = TerminalGuard {
            out,
            raw,
            active: true,
        };
        crossterm::execute!(
            guard.out,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            crossterm::cursor::Hide
        )?;
        Ok(guard)
    }

    /// Restores the terminal now. Later calls, and the drop, do nothing.
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let shown = write_restore(&mut self.out);
        let raw = self.raw.disable();
        shown.and(raw)
    }
}

impl<W: Write, R: RawMode> Drop for TerminalGuard<W, R> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn write_restore(out: &mut impl Write) -> io::Result<()> {
    crossterm::execute!(
        out,
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Installs a panic hook that restores the terminal before the panic message
/// prints, so the message lands on the normal screen. Returns the previous
/// hook for [`restore_panic_hook`].
fn install_panic_hook() -> Arc<PanicHook> {
    let prev: Arc<PanicHook> = Arc::new(std::panic::take_hook());
    let chained = Arc::clone(&prev);
    std::panic::set_hook(Box::new(move |info| {
        let _ = write_restore(&mut io::stdout());
        let _ = crossterm::terminal::disable_raw_mode();
        chained(info);
    }));
    prev
}

fn restore_panic_hook(prev: Arc<PanicHook>) {
    let _ = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| prev(info)));
}

// ---- run loop ------------------------------------------------------------------

/// Where the app reads the time. The real clock unless a caller, such as the
/// demo, supplies its own.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub struct RunOptions {
    pub clock: Clock,
    /// Where the app's requests go (actions, the log it shows, starting the
    /// daemon). Without it the app only looks.
    pub requests: Option<tokio::sync::mpsc::UnboundedSender<Request>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            clock: Arc::new(Timestamp::system_now),
            requests: None,
        }
    }
}

/// Hands the app's queued requests to `out`, and tells it when the log on
/// screen changes. `watching` is the last log focus sent.
fn forward_requests(
    app: &mut App,
    snap: &Snapshot,
    out: Option<&tokio::sync::mpsc::UnboundedSender<Request>>,
    watching: &mut Option<Option<String>>,
) {
    let focus = app.log_focus(snap);
    if watching.as_ref() != Some(&focus) {
        app.requests.push(Request::WatchLog(focus.clone()));
        *watching = Some(focus);
    }
    for request in app.requests.drain(..) {
        if let Some(out) = out {
            let _ = out.send(request);
        }
    }
}

/// The shortest time between two frames: at most 60 a second.
const FRAME: Duration = Duration::from_micros(16_667);

/// Runs the app full screen until the user quits, showing whatever snapshot
/// `rx` holds.
pub async fn run(rx: watch::Receiver<Snapshot>) -> io::Result<()> {
    run_with(rx, RunOptions::default()).await
}

pub async fn run_with(rx: watch::Receiver<Snapshot>, options: RunOptions) -> io::Result<()> {
    let prev_hook = install_panic_hook();
    let result = async {
        let mut guard = TerminalGuard::enter(io::stdout(), CrosstermRawMode)?;
        let result = event_loop(rx, options).await;
        guard.restore()?;
        result
    }
    .await;
    restore_panic_hook(prev_hook);
    result
}

async fn event_loop(mut rx: watch::Receiver<Snapshot>, options: RunOptions) -> io::Result<()> {
    // Ask for the background color before anything else reads terminal
    // input, so the reply isn't taken for key presses.
    let theme = tokio::task::spawn_blocking(detect_theme)
        .await
        .unwrap_or_else(|_| Theme::light());
    let mut app = App::new(theme);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let size = terminal.size()?;
    app.size = (size.width, size.height);

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut dirty = true;
    let mut source_open = true;
    let mut last_draw = tokio::time::Instant::now() - FRAME;
    let mut watching = None;
    forward_requests(
        &mut app,
        &rx.borrow(),
        options.requests.as_ref(),
        &mut watching,
    );

    loop {
        if app.quit {
            return Ok(());
        }
        let frame_at = last_draw + FRAME;
        tokio::select! {
            _ = tokio::time::sleep_until(frame_at), if dirty => {
                let now = (options.clock)();
                let snap = rx.borrow_and_update();
                terminal.draw(|f| app.render(f.buffer_mut(), &snap, now))?;
                drop(snap);
                dirty = false;
                last_draw = tokio::time::Instant::now();
            }
            event = events.next() => match event {
                Some(Ok(event)) => {
                    let size = terminal.size()?;
                    let snap = rx.borrow().clone();
                    if app.handle_event(&event, &snap, (size.width, size.height)) {
                        dirty = true;
                    }
                    forward_requests(&mut app, &snap, options.requests.as_ref(), &mut watching);
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            _ = ticker.tick() => {
                app.tick += 1;
                dirty = true;
            }
            changed = rx.changed(), if source_open => {
                // A closed source keeps its last snapshot on screen.
                if changed.is_err() {
                    source_open = false;
                } else {
                    dirty = true;
                    let snap = rx.borrow().clone();
                    app.follow_roster(&snap);
                    forward_requests(&mut app, &snap, options.requests.as_ref(), &mut watching);
                }
            }
        }
    }
}
