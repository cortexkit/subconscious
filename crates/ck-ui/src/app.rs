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
use crate::model::{Daemon, Snapshot};
use crate::screens::{self, overlays, View, MIN_H, MIN_W};
use crate::theme::{detect_theme, Theme};
use crate::time::Timestamp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Home,
    Modules,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    Palette,
    Help,
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
            screens::down::draw(&mut c, &v, d);
            return;
        }
        match self.tab {
            Tab::Home => screens::home::draw(&mut c, &v),
            Tab::Modules => screens::modules::draw(&mut c, &v),
        }
        match self.overlay {
            Some(Overlay::Palette) => overlays::palette(
                &mut c,
                &snap.commands,
                &self.palette_query,
                self.palette_sel,
            ),
            Some(Overlay::Help) => overlays::help(&mut c),
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return true;
        }
        if matches!(snap.daemon.data, Daemon::Down(_)) {
            match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('t') => self.theme = self.theme.toggled(),
                _ => return false,
            }
            return true;
        }
        if self.overlay == Some(Overlay::Palette) {
            return self.palette_key(key, snap);
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
            KeyCode::Char('1') => self.switch(Tab::Home),
            KeyCode::Char('2') => self.switch(Tab::Modules),
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
            KeyCode::Down | KeyCode::Char('j') => return self.move_by(1, snap),
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

    fn open_palette(&mut self) {
        self.overlay = Some(Overlay::Palette);
        self.palette_query.clear();
        self.palette_sel = 0;
    }

    fn switch(&mut self, tab: Tab) {
        self.tab = tab;
        self.overlay = None;
        self.back = None;
    }

    fn palette_key(&mut self, key: KeyEvent, snap: &Snapshot) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
            KeyCode::Down => {
                let n = overlays::matches(&snap.commands, &self.palette_query).len();
                self.palette_sel = (self.palette_sel + 1).min(n.saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.palette_query.pop();
                self.palette_sel = 0;
            }
            KeyCode::Enter => {
                let list = overlays::matches(&snap.commands, &self.palette_query);
                let Some(cmd) = list.get(self.palette_sel.min(list.len().saturating_sub(1))) else {
                    return false;
                };
                let (area, title) = (cmd.area.clone(), cmd.title.clone());
                self.overlay = None;
                // Commands for Modules open it on the module they name. The
                // other areas have no screen in this app yet, so the palette
                // just closes.
                if area == "Modules" {
                    if let Some(i) = snap
                        .modules
                        .data
                        .iter()
                        .position(|m| title.split_whitespace().any(|w| w == m.id))
                    {
                        self.mod_sel = i;
                    }
                    self.tab = Tab::Modules;
                    self.back = None;
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
                let rows = screens::home::attention_rows(w, h);
                (&mut self.home_sel, snap.attention().len().min(rows))
            }
            Tab::Modules => (&mut self.mod_sel, snap.modules.data.len()),
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
        if self.overlay.is_some() || matches!(snap.daemon.data, Daemon::Down(_)) {
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
                    let active = match self.tab {
                        Tab::Home => 0,
                        Tab::Modules => 1,
                    };
                    for (sx, sw, i) in tab_spans(w, active) {
                        if x >= sx && x < sx + sw {
                            let tab = match i {
                                0 => Tab::Home,
                                1 => Tab::Modules,
                                _ => return false,
                            };
                            self.switch(tab);
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
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            clock: Arc::new(Timestamp::system_now),
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
                    let snap = rx.borrow();
                    if app.handle_event(&event, &snap, (size.width, size.height)) {
                        dirty = true;
                    }
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
                }
            }
        }
    }
}
