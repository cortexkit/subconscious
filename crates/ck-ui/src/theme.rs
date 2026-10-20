//! Colors by role, the light and dark palettes, and picking one for the
//! terminal in use.
//!
//! Screens never name a color. They name a role (dim text, the accent, a
//! selected row's background), and the theme turns the role into a color. Body
//! text stays `Color::Reset`, the terminal's own ink, so ck looks at home in any
//! terminal; color marks status, focus and selection only.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};

/// Foreground roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fg {
    Dim,
    Faint,
    Acc,
    Ok,
    Warn,
    Err,
    /// The quiet, unfilled part of a gauge.
    Track,
    /// Text on an accent-colored button.
    OnAcc,
    QrDark,
}

/// Background roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bg {
    /// A selected row.
    Sel,
    /// A notice band, such as a pending restart.
    AccBg,
    /// A key cap in the footer, or a secondary button.
    Key,
    Track,
    /// A search hit.
    Hl,
    Add,
    Del,
    /// A primary button.
    Acc,
    QrLight,
}

impl Fg {
    pub const ALL: [Fg; 9] = [
        Fg::Dim,
        Fg::Faint,
        Fg::Acc,
        Fg::Ok,
        Fg::Warn,
        Fg::Err,
        Fg::Track,
        Fg::OnAcc,
        Fg::QrDark,
    ];

    /// The role's name in golden style files.
    pub fn name(self) -> &'static str {
        match self {
            Fg::Dim => "dim",
            Fg::Faint => "faint",
            Fg::Acc => "acc",
            Fg::Ok => "ok",
            Fg::Warn => "warn",
            Fg::Err => "err",
            Fg::Track => "track",
            Fg::OnAcc => "onacc",
            Fg::QrDark => "qrdark",
        }
    }
}

impl Bg {
    pub const ALL: [Bg; 9] = [
        Bg::Sel,
        Bg::AccBg,
        Bg::Key,
        Bg::Track,
        Bg::Hl,
        Bg::Add,
        Bg::Del,
        Bg::Acc,
        Bg::QrLight,
    ];

    /// The role's name in golden style files.
    pub fn name(self) -> &'static str {
        match self {
            Bg::Sel => "sel",
            Bg::AccBg => "accbg",
            Bg::Key => "key",
            Bg::Track => "track",
            Bg::Hl => "hl",
            Bg::Add => "add",
            Bg::Del => "del",
            Bg::Acc => "acc",
            Bg::QrLight => "qrlight",
        }
    }
}

pub const BOLD: u8 = 1;
pub const UNDERLINE: u8 = 2;
pub const DIM: u8 = 4;

/// A style by role: optional foreground and background roles plus modifiers
/// (`BOLD`, `UNDERLINE`, `DIM`). `St::NONE` is plain body text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct St {
    pub fg: Option<Fg>,
    pub bg: Option<Bg>,
    pub mods: u8,
}

impl St {
    pub const NONE: St = St {
        fg: None,
        bg: None,
        mods: 0,
    };
    pub const B: St = St::NONE.bold();
    pub const D: St = St::fg(Fg::Dim);
    pub const F: St = St::fg(Fg::Faint);
    pub const A: St = St::fg(Fg::Acc);
    pub const O: St = St::fg(Fg::Ok);
    pub const W: St = St::fg(Fg::Warn);
    pub const E: St = St::fg(Fg::Err);

    pub const fn fg(fg: Fg) -> St {
        St {
            fg: Some(fg),
            bg: None,
            mods: 0,
        }
    }

    pub const fn bg(bg: Bg) -> St {
        St {
            fg: None,
            bg: Some(bg),
            mods: 0,
        }
    }

    pub const fn on(self, bg: Bg) -> St {
        St {
            bg: Some(bg),
            ..self
        }
    }

    pub const fn bold(self) -> St {
        St {
            mods: self.mods | BOLD,
            ..self
        }
    }

    pub const fn underline(self) -> St {
        St {
            mods: self.mods | UNDERLINE,
            ..self
        }
    }

    pub const fn dim(self) -> St {
        St {
            mods: self.mods | DIM,
            ..self
        }
    }
}

/// Light or dark terminal background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

/// The concrete colors for every role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub dim: Color,
    pub faint: Color,
    pub track: Color,
    pub acc: Color,
    pub accbg: Color,
    pub sel: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub key: Color,
    pub hl: Color,
    pub add: Color,
    pub del: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        dim: rgb(0x6E6E73),
        faint: rgb(0xB5B5BB),
        track: rgb(0xEBEBEF),
        acc: rgb(0x0A62CF),
        accbg: rgb(0xEAF2FF),
        sel: rgb(0xEEF3FB),
        ok: rgb(0x1E8A3F),
        warn: rgb(0xB4600A),
        err: rgb(0xC8372B),
        key: rgb(0xEFEFF2),
        hl: rgb(0xFFE58A),
        add: rgb(0xE6F5E9),
        del: rgb(0xFDECEC),
    };

    pub const DARK: Palette = Palette {
        dim: rgb(0x9C9CA3),
        faint: rgb(0x5B5B61),
        track: rgb(0x2F2F34),
        acc: rgb(0x5AA2FF),
        accbg: rgb(0x17304D),
        sel: rgb(0x253141),
        ok: rgb(0x42D36A),
        warn: rgb(0xF2A43A),
        err: rgb(0xFF6B5E),
        key: rgb(0x303035),
        hl: rgb(0x6B5A12),
        add: rgb(0x183A22),
        del: rgb(0x4A1E1B),
    };

    pub fn for_mode(mode: Mode) -> Palette {
        match mode {
            Mode::Light => Palette::LIGHT,
            Mode::Dark => Palette::DARK,
        }
    }

    fn fg(&self, fg: Fg) -> Color {
        match fg {
            Fg::Dim => self.dim,
            Fg::Faint => self.faint,
            Fg::Acc => self.acc,
            Fg::Ok => self.ok,
            Fg::Warn => self.warn,
            Fg::Err => self.err,
            Fg::Track => self.track,
            Fg::OnAcc => Color::Rgb(255, 255, 255),
            Fg::QrDark => Color::Rgb(0, 0, 0),
        }
    }

    fn bg(&self, bg: Bg) -> Color {
        match bg {
            Bg::Sel => self.sel,
            Bg::AccBg => self.accbg,
            Bg::Key => self.key,
            Bg::Track => self.track,
            Bg::Hl => self.hl,
            Bg::Add => self.add,
            Bg::Del => self.del,
            Bg::Acc => self.acc,
            Bg::QrLight => Color::Rgb(255, 255, 255),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Colors {
    Palette,
    /// `NO_COLOR`: glyphs, words and modifiers only.
    None,
    /// Every role gets its own indexed color so a rendered cell can be read
    /// back as roles.
    Test,
}

/// Turns roles into ratatui styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    mode: Mode,
    colors: Colors,
}

// Indexed colors of the test theme. Foreground and background roles use
// separate ranges so the inverse never confuses the two.
const TEST_FG_BASE: u8 = 1;
const TEST_BG_BASE: u8 = 101;

impl Theme {
    pub fn new(mode: Mode) -> Theme {
        Theme {
            mode,
            colors: Colors::Palette,
        }
    }

    pub fn light() -> Theme {
        Theme::new(Mode::Light)
    }

    pub fn dark() -> Theme {
        Theme::new(Mode::Dark)
    }

    /// No colors at all, for `NO_COLOR`.
    pub fn no_color(mode: Mode) -> Theme {
        Theme {
            mode,
            colors: Colors::None,
        }
    }

    /// The theme the golden tests render with: see [`Theme::test_roles`].
    pub fn test() -> Theme {
        Theme {
            mode: Mode::Light,
            colors: Colors::Test,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn is_no_color(&self) -> bool {
        self.colors == Colors::None
    }

    /// Switches between the light and dark palettes. Without colors, or in
    /// the test theme, only the remembered mode changes.
    pub fn toggled(self) -> Theme {
        Theme {
            mode: match self.mode {
                Mode::Light => Mode::Dark,
                Mode::Dark => Mode::Light,
            },
            ..self
        }
    }

    pub fn fg(&self, fg: Fg) -> Color {
        match self.colors {
            Colors::Palette => Palette::for_mode(self.mode).fg(fg),
            Colors::None => Color::Reset,
            Colors::Test => {
                let i = Fg::ALL.iter().position(|r| *r == fg).unwrap_or(0) as u8;
                Color::Indexed(TEST_FG_BASE + i)
            }
        }
    }

    pub fn bg(&self, bg: Bg) -> Color {
        match self.colors {
            Colors::Palette => Palette::for_mode(self.mode).bg(bg),
            Colors::None => Color::Reset,
            Colors::Test => {
                let i = Bg::ALL.iter().position(|r| *r == bg).unwrap_or(0) as u8;
                Color::Indexed(TEST_BG_BASE + i)
            }
        }
    }

    fn modifier(mods: u8) -> Modifier {
        let mut m = Modifier::empty();
        if mods & BOLD != 0 {
            m |= Modifier::BOLD;
        }
        if mods & UNDERLINE != 0 {
            m |= Modifier::UNDERLINED;
        }
        if mods & DIM != 0 {
            m |= Modifier::DIM;
        }
        m
    }

    /// The complete style for a cell: unset roles become `Color::Reset`, the
    /// terminal's own colors.
    pub fn style(&self, st: St) -> Style {
        Style::default()
            .fg(st.fg.map_or(Color::Reset, |f| self.fg(f)))
            .bg(st.bg.map_or(Color::Reset, |b| self.bg(b)))
            .add_modifier(Self::modifier(st.mods))
    }

    /// A style that only adds what `st` names, for painting over cells that
    /// already have a style, such as a selected row's background.
    pub fn patch(&self, st: St) -> Style {
        let mut s = Style::default().add_modifier(Self::modifier(st.mods));
        if let Some(f) = st.fg {
            s = s.fg(self.fg(f));
        }
        if let Some(b) = st.bg {
            s = s.bg(self.bg(b));
        }
        s
    }

    /// Reads a cell drawn with [`Theme::test`] back as the golden triple
    /// `(fg, bg, mods)`: role names, `-` for none, and mods as any of `b`,
    /// `u`, `d` in that order. A color the test theme never produces reads as
    /// `?`, so it can't match a golden by accident.
    pub fn test_roles(fg: Color, bg: Color, modifier: Modifier) -> (String, String, String) {
        let fg = match fg {
            Color::Reset => "-".to_string(),
            Color::Indexed(i) if (TEST_FG_BASE..TEST_FG_BASE + 9).contains(&i) => {
                Fg::ALL[usize::from(i - TEST_FG_BASE)].name().to_string()
            }
            _ => "?".to_string(),
        };
        let bg = match bg {
            Color::Reset => "-".to_string(),
            Color::Indexed(i) if (TEST_BG_BASE..TEST_BG_BASE + 9).contains(&i) => {
                Bg::ALL[usize::from(i - TEST_BG_BASE)].name().to_string()
            }
            _ => "?".to_string(),
        };
        let mut mods = String::new();
        if modifier.contains(Modifier::BOLD) {
            mods.push('b');
        }
        if modifier.contains(Modifier::UNDERLINED) {
            mods.push('u');
        }
        if modifier.contains(Modifier::DIM) {
            mods.push('d');
        }
        (fg, bg, mods)
    }
}

// ---- choosing light or dark -------------------------------------------------

/// How long to wait for the terminal to report its background color.
pub const OSC11_TIMEOUT: Duration = Duration::from_millis(150);

/// The query that asks a terminal for its background color.
pub const OSC11_QUERY: &str = "\x1b]11;?\x07";

/// Parses a terminal's answer to [`OSC11_QUERY`] into red, green and blue in
/// 0.0..=1.0. Accepts `rgb:RRRR/GGGG/BBBB` with 1 to 4 hex digits per
/// component (2 and 4 are what terminals send), with or without the
/// surrounding `ESC ] 11 ;` and a BEL or `ESC \` terminator.
pub fn parse_osc11(reply: &str) -> Option<(f64, f64, f64)> {
    let start = reply.find("rgb:")?;
    let body = &reply[start + 4..];
    let end = body
        .find(|c: char| !(c.is_ascii_hexdigit() || c == '/'))
        .unwrap_or(body.len());
    let mut parts = body[..end].split('/');
    let mut channel = || -> Option<f64> {
        let hex = parts.next()?;
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len() as u32)) - 1;
        Some(f64::from(v) / f64::from(max))
    };
    let r = channel()?;
    let g = channel()?;
    let b = channel()?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// WCAG relative luminance of an sRGB color with channels in 0.0..=1.0.
pub fn relative_luminance((r, g, b): (f64, f64, f64)) -> f64 {
    let lin = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// Picks the palette: `CK_THEME=light|dark` wins; otherwise the terminal's
/// answer to the background query, light when its luminance is at least 0.5;
/// with no usable answer, light.
pub fn choose_mode(ck_theme: Option<&str>, osc11_reply: Option<&str>) -> Mode {
    match ck_theme.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("light") => return Mode::Light,
        Some(v) if v.eq_ignore_ascii_case("dark") => return Mode::Dark,
        _ => {}
    }
    match osc11_reply.and_then(parse_osc11) {
        Some(rgb) if relative_luminance(rgb) < 0.5 => Mode::Dark,
        _ => Mode::Light,
    }
}

/// Whether `CK_THEME` alone settles the palette, so the terminal need not be
/// asked.
pub fn ck_theme_decides(ck_theme: Option<&str>) -> bool {
    matches!(
        ck_theme.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("light" | "dark")
    )
}

/// The theme for the given environment values: `NO_COLOR` (any value) turns
/// colors off but keeps the light/dark choice for `t`.
pub fn theme_for(
    no_color: Option<&str>,
    ck_theme: Option<&str>,
    osc11_reply: Option<&str>,
) -> Theme {
    let mode = choose_mode(ck_theme, osc11_reply);
    if no_color.is_some() {
        Theme::no_color(mode)
    } else {
        Theme::new(mode)
    }
}

/// Collects the terminal's background reply from key events.
///
/// crossterm has no event for an OSC reply. Its parser reads `ESC ]` as
/// Alt+`]`, the payload as plain characters, and the BEL terminator as
/// Ctrl+G (or `ESC \` as Alt+`\`), so the reply can be rebuilt from those.
#[derive(Debug, Default)]
pub struct Osc11Collector {
    started: bool,
    text: String,
    done: bool,
}

impl Osc11Collector {
    /// Feeds one event. Returns true once the reply is complete.
    pub fn feed(&mut self, event: &crossterm::event::Event) -> bool {
        use crossterm::event::{Event, KeyCode, KeyModifiers};
        if self.done {
            return true;
        }
        let Event::Key(key) = event else {
            return false;
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(']') if alt => {
                self.started = true;
                self.text.clear();
            }
            KeyCode::Char('g') if ctrl && self.started => self.done = true,
            KeyCode::Char('\\') if alt && self.started => self.done = true,
            KeyCode::Char(c) if self.started => self.text.push(c),
            _ => {}
        }
        self.done
    }

    /// The reply body without its introducer and terminator, such as
    /// `11;rgb:ffff/ffff/ffff`, once complete.
    pub fn reply(&self) -> Option<&str> {
        self.done.then_some(self.text.as_str())
    }
}

/// Asks the terminal for its background color and waits up to
/// [`OSC11_TIMEOUT`] for the answer. The terminal must already be in raw mode,
/// and nothing else may be reading terminal events yet.
pub fn query_terminal_background() -> Option<String> {
    use std::io::Write;
    use std::time::Instant;
    let mut out = std::io::stdout();
    out.write_all(OSC11_QUERY.as_bytes()).ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + OSC11_TIMEOUT;
    let mut collector = Osc11Collector::default();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !crossterm::event::poll(left).ok()? {
            return None;
        }
        let event = crossterm::event::read().ok()?;
        if collector.feed(&event) {
            return collector.reply().map(str::to_string);
        }
    }
}

/// The theme for this process: reads `NO_COLOR` and `CK_THEME`, and asks the
/// terminal only when `CK_THEME` doesn't decide. Raw mode must be on.
pub fn detect_theme() -> Theme {
    let no_color = std::env::var("NO_COLOR").ok();
    let ck_theme = std::env::var("CK_THEME").ok();
    let reply = if ck_theme_decides(ck_theme.as_deref()) {
        None
    } else {
        query_terminal_background()
    };
    theme_for(no_color.as_deref(), ck_theme.as_deref(), reply.as_deref())
}
