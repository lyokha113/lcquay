//! Style tokens over one of four palettes in 24-bit color, picked by `config.theme` and switched
//! with `set` (startup and config reload). The whole frame is painted with `base()`; no terminal
//! attributes other than BOLD/UNDERLINED are used for meaning.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};

use crate::config::Theme;

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// One color per role; tokens below combine them with modifiers.
pub struct Palette {
    pub bg: Color,
    pub bar_bg: Color,
    pub bar_fg: Color,
    pub fg: Color,
    pub muted: Color,
    pub header: Color,
    pub label: Color,
    pub number: Color,
    pub time: Color,
    pub accent: Color,
    pub ok: Color,
    pub bad: Color,
    pub warn: Color,
    pub title: Color,
    pub title_idle: Color,
    pub border: Color,
    pub focus: Color,
    pub sel: Color,
    pub sel_idle: Color,
    pub key: Color,
}

/// Tokyo Night ("night").
const TOKYO_NIGHT: Palette = Palette {
    bg: rgb(0x1a1b26),
    bar_bg: rgb(0x16161e),
    bar_fg: rgb(0xa9b1d6),
    fg: rgb(0xc0caf5),
    muted: rgb(0x565f89),
    header: rgb(0xbb9af7),
    label: rgb(0x7aa2f7),
    number: rgb(0xff9e64),
    time: rgb(0x73daca),
    accent: rgb(0x7dcfff),
    ok: rgb(0x9ece6a),
    bad: rgb(0xf7768e),
    warn: rgb(0xe0af68),
    title: rgb(0x7aa2f7),
    title_idle: rgb(0xa9b1d6),
    border: rgb(0x3b4261),
    focus: rgb(0x7aa2f7),
    sel: rgb(0x283457),
    sel_idle: rgb(0x292e42),
    key: rgb(0x7aa2f7),
};

/// Tokyo Night accents on the terminal's own background (as portr does), quieter selection.
const TOKYO_NIGHT_BLACK: Palette = Palette {
    bg: Color::Reset,
    bar_bg: Color::Reset,
    bar_fg: rgb(0xa9b1d6),
    fg: rgb(0xd5d6db),
    muted: rgb(0x565f89),
    header: rgb(0xbb9af7),
    label: rgb(0x7aa2f7),
    number: rgb(0xe0af68),
    time: rgb(0xa9b1d6),
    accent: rgb(0x7aa2f7),
    ok: rgb(0x9ece6a),
    bad: rgb(0xf7768e),
    warn: rgb(0xe0af68),
    title: rgb(0x7aa2f7),
    title_idle: rgb(0xa9b1d6),
    border: rgb(0x2b2f45),
    focus: rgb(0x7aa2f7),
    sel: rgb(0x2d3250),
    sel_idle: rgb(0x1d1f2b),
    key: rgb(0x7aa2f7),
};

/// Maple Dark (subframe7536/vscode-theme-maple): neutral charcoal, slate text, pastel accents.
const MAPLE_DARK: Palette = Palette {
    bg: rgb(0x1e1e1f),
    bar_bg: rgb(0x171718),
    bar_fg: rgb(0xcbd5e1),
    fg: rgb(0xcbd5e1),
    muted: rgb(0x999999),
    header: rgb(0xd2ccff),
    label: rgb(0x8fc7ff),
    number: rgb(0xd5f288),
    time: rgb(0xa1e8e5),
    accent: rgb(0xb8d7f9),
    ok: rgb(0xa4dfae),
    bad: rgb(0xedabab),
    warn: rgb(0xeecfa0),
    title: rgb(0x8fc7ff),
    title_idle: rgb(0xcbd5e1),
    border: rgb(0x44444b),
    focus: rgb(0x6285a7),
    sel: rgb(0x334155),
    sel_idle: rgb(0x283242),
    key: rgb(0x8fc7ff),
};

/// Maple Dark accents on the terminal's own background.
const MAPLE_DARK_BLACK: Palette = Palette {
    bg: Color::Reset,
    bar_bg: Color::Reset,
    ..MAPLE_DARK
};

pub fn palette(t: Theme) -> &'static Palette {
    match t {
        Theme::TokyoNight => &TOKYO_NIGHT,
        Theme::TokyoNightBlack => &TOKYO_NIGHT_BLACK,
        Theme::MapleDark => &MAPLE_DARK,
        Theme::MapleDarkBlack => &MAPLE_DARK_BLACK,
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Theme::MapleDark as u8);

/// Switches the palette for every later frame.
pub fn set(t: Theme) {
    CURRENT.store(t as u8, Ordering::Relaxed);
}

fn p() -> &'static Palette {
    palette(match CURRENT.load(Ordering::Relaxed) {
        0 => Theme::TokyoNight,
        1 => Theme::TokyoNightBlack,
        3 => Theme::MapleDarkBlack,
        _ => Theme::MapleDark,
    })
}

/// Default text on the app background.
pub fn base() -> Style {
    Style::new().fg(p().fg).bg(p().bg)
}
/// The app background.
pub fn bg() -> Color {
    p().bg
}
/// Hint bar and status line rows.
pub fn bar() -> Style {
    Style::new().fg(p().bar_fg).bg(p().bar_bg)
}
/// Chrome and secondary text.
pub fn muted() -> Style {
    Style::new().fg(p().muted)
}
/// Table / history header cells.
pub fn header() -> Style {
    Style::new().fg(p().header)
}
/// Detail key column.
pub fn label() -> Style {
    Style::new().fg(p().label)
}
/// Sizes and counts.
pub fn number() -> Style {
    Style::new().fg(p().number)
}
/// Pushed / pulled / created times.
pub fn time() -> Style {
    Style::new().fg(p().time)
}
/// Data marks.
pub fn accent() -> Style {
    Style::new().fg(p().accent)
}
pub fn ok() -> Style {
    Style::new().fg(p().ok)
}
pub fn bad() -> Style {
    Style::new().fg(p().bad)
}
pub fn warn() -> Style {
    Style::new().fg(p().warn)
}
/// Focused pane title, modal titles, active tab.
pub fn title() -> Style {
    Style::new().fg(p().title).add_modifier(Modifier::BOLD)
}
/// Unfocused pane title.
pub fn title_idle() -> Style {
    Style::new().fg(p().title_idle)
}
pub fn border() -> Style {
    Style::new().fg(p().border)
}
pub fn border_focus() -> Style {
    Style::new().fg(p().focus)
}
/// Selection in the focused pane.
pub fn selected() -> Style {
    Style::new().bg(p().sel).add_modifier(Modifier::BOLD)
}
/// Selection in an unfocused pane.
pub fn selected_idle() -> Style {
    Style::new().bg(p().sel_idle)
}
/// Key names in hints and help.
pub fn key() -> Style {
    Style::new().fg(p().key).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_by_name() {
        assert_eq!(palette(Theme::TokyoNight).bg, rgb(0x1a1b26));
        assert_eq!(palette(Theme::TokyoNightBlack).bg, Color::Reset);
        assert_eq!(palette(Theme::MapleDark).bg, rgb(0x1e1e1f));
        assert_eq!(palette(Theme::MapleDark).fg, rgb(0xcbd5e1));
    }
}
