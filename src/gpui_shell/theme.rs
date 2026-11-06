//! The panel's look: near-black surfaces, the green accent and DM Sans, matching
//! the panel of the earlier Bridge betas.

use std::borrow::Cow;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, px};

const FONT: &[u8] = include_bytes!("../../assets/fonts/DMSans-Variable.ttf");

/// The title bar's strip.
pub const CHROME: u32 = 0x08090a;
pub const SURFACE: u32 = 0x141618;
pub const DIVIDER: u32 = 0x262a2d;
/// The top of a card's gradient.
pub const CARD_TOP: u32 = 0x1c1e20;
/// The top of the status card's gradient, with a green tint.
pub const LIT_CARD_TOP: u32 = 0x1c2520;

const BACKGROUND: (u8, u8, u8) = (12, 13, 15);
const LINE: (u8, u8, u8) = (38, 42, 45);
const TEXT: (u8, u8, u8) = (238, 240, 242);
const MUTED: (u8, u8, u8) = (170, 176, 183);
const ACCENT: (u8, u8, u8) = (93, 222, 137);
const ACCENT_INK: (u8, u8, u8) = (7, 22, 14);
const DANGER: (u8, u8, u8) = (255, 107, 107);

fn to_hsla(rgb: (u8, u8, u8)) -> Hsla {
    let (r, g, b) = (
        rgb.0 as f32 / 255.0,
        rgb.1 as f32 / 255.0,
        rgb.2 as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        };
    }
    let d = max - min;
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / d) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    Hsla {
        h: ((h + 360.0) % 360.0) / 360.0,
        s: s.clamp(0.0, 1.0),
        l: l.clamp(0.0, 1.0),
        a: 1.0,
    }
}

/// Paints the dark shell onto the live theme.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    if let Err(error) = cx.text_system().add_fonts(vec![Cow::Borrowed(FONT)]) {
        tracing::warn!(%error, "Could not load the panel's font");
    }
    let background = to_hsla(BACKGROUND);
    let border = to_hsla(LINE);
    let muted = to_hsla(MUTED);
    let muted_foreground = to_hsla(MUTED);
    let foreground = to_hsla(TEXT);
    let accent = to_hsla(ACCENT);
    let accent_ink = to_hsla(ACCENT_INK);
    let danger = to_hsla(DANGER);
    Theme::update(cx, |theme| {
        theme.font_family = "DM Sans".into();
        theme.font_size = px(16.);
        let c = &mut theme.colors;
        c.danger = danger;
        c.background = background;
        c.foreground = foreground;
        c.muted = muted;
        c.muted_foreground = muted_foreground;
        c.sidebar = background;
        c.border = border;
        c.accent = accent;
        c.accent_foreground = accent_ink;
        c.primary = accent;
        c.primary_foreground = accent_ink;
        c.title_bar = background;
        c.title_bar_border = border;
        c.scrollbar = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.0,
        };
        c.scrollbar_thumb = border;
        c.scrollbar_thumb_hover = muted;
    });
}
