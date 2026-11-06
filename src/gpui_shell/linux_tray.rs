//! The Linux tray icon: a StatusNotifierItem on D-Bus, which AppIndicator hosts
//! (KDE, and GNOME with an AppIndicator extension such as Ubuntu's) show in the
//! panel. The host decides what a click does: Ubuntu's opens the menu on a left
//! click and activates on a double-click, KDE activates on a left click.

use std::{sync::mpsc, thread, time::Duration};

use ksni::{
    Category, Icon, MenuItem, ToolTip,
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
};

use super::tray::Command;
use crate::tray_battery::{self, TrayBattery};

/// A host can still be starting when Bridge starts at login, so a missing one
/// is tried again this many times before Bridge settles for having no tray.
const ATTEMPTS: u32 = 5;
const RETRY_DELAY: Duration = Duration::from_secs(1);

const NAME: &str = "OpenMouse Bridge";

/// The logo is tall and hosts scale an icon to fill the panel's height, so it
/// is drawn into a square with a margin to sit level with its neighbours.
const LOGO_CANVAS: u32 = 64;
const LOGO_HEIGHT: u32 = 56;

/// The battery icon is drawn large and scaled down by the host, like the logo,
/// which keeps it sharp on a high-density display.
const BATTERY_CANVAS: u32 = 64;
/// Samples per pixel along each axis, to smooth the edges.
const SUBSAMPLES: u32 = 4;
/// The units the mouse is designed in: a square this many units wide.
const DESIGN_SIZE: f32 = 22.0;
/// The charging bolt's fill.
const BOLT: (u8, u8, u8) = (236, 239, 244);
/// The bolt's corners in design units.
const BOLT_POINTS: [(f32, f32); 6] = [
    (11.68, 8.0),
    (7.6, 14.12),
    (10.32, 14.12),
    (9.64, 18.88),
    (14.4, 12.08),
    (11.68, 12.08),
];

pub struct Indicator {
    commands: mpsc::Sender<Command>,
    logo: Vec<Icon>,
    battery: Option<TrayBattery>,
}

impl Indicator {
    pub fn set_battery(&mut self, battery: Option<TrayBattery>) {
        self.battery = battery;
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl ksni::Tray for Indicator {
    fn id(&self) -> String {
        "io.openmouse.bridge".into()
    }

    fn title(&self) -> String {
        NAME.into()
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let Some(battery) = &self.battery else {
            return self.logo.clone();
        };
        let rgba = mouse_battery_rgba(
            battery.percent,
            battery.charging,
            battery.ink,
            BATTERY_CANVAS,
        );
        vec![icon_from_rgba(rgba, BATTERY_CANVAS, BATTERY_CANVAS)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self
                .battery
                .as_ref()
                .map_or_else(|| NAME.to_owned(), tray_battery::tooltip),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Show(None));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open OpenMouse Bridge".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Show(None))),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Restart".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Restart)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit Bridge".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Registers the icon, or returns `None` when no host will show it, in which
/// case closing the panel has to quit: there would be no way back to it.
pub fn spawn(commands: mpsc::Sender<Command>) -> Option<Handle<Indicator>> {
    let logo = match crate::runtime::openmouse_icon_rgba() {
        Ok((rgba, width, height)) => {
            let square = fit_in_square(&rgba, width, height, LOGO_CANVAS, LOGO_HEIGHT);
            vec![icon_from_rgba(square, LOGO_CANVAS, LOGO_CANVAS)]
        }
        Err(error) => {
            tracing::warn!(%error, "Could not load the tray logo");
            Vec::new()
        }
    };
    for attempt in 1..=ATTEMPTS {
        let indicator = Indicator {
            commands: commands.clone(),
            logo: logo.clone(),
            battery: None,
        };
        match indicator.spawn() {
            Ok(handle) => return Some(handle),
            Err(error) if attempt == ATTEMPTS => {
                tracing::warn!(%error, "No tray host; closing the panel will quit Bridge");
            }
            Err(error) => {
                tracing::debug!(%error, attempt, "No tray host yet");
                thread::sleep(RETRY_DELAY);
            }
        }
    }
    None
}

/// Shrinks an RGBA image to `inner` pixels on its longer side, centred on a
/// transparent `canvas`-sized square. Each pixel averages the source pixels it
/// covers, weighted by alpha so transparent ones do not darken the edges.
fn fit_in_square(rgba: &[u8], width: u32, height: u32, canvas: u32, inner: u32) -> Vec<u8> {
    let longest = width.max(height).max(1);
    let scaled = |side: u32| ((side * inner + longest / 2) / longest).max(1);
    let (out_width, out_height) = (scaled(width), scaled(height));
    let (left, top) = ((canvas - out_width) / 2, (canvas - out_height) / 2);
    let mut out = vec![0u8; (canvas * canvas * 4) as usize];
    for y in 0..out_height {
        let rows = y * height / out_height
            ..((y + 1) * height / out_height).max(y * height / out_height + 1);
        for x in 0..out_width {
            let start = x * width / out_width;
            let columns = start..((x + 1) * width / out_width).max(start + 1);
            let (mut red, mut green, mut blue, mut alpha, mut count) =
                (0u64, 0u64, 0u64, 0u64, 0u64);
            for source_y in rows.clone() {
                for source_x in columns.clone() {
                    let index = ((source_y * width + source_x) * 4) as usize;
                    let a = u64::from(rgba[index + 3]);
                    red += u64::from(rgba[index]) * a;
                    green += u64::from(rgba[index + 1]) * a;
                    blue += u64::from(rgba[index + 2]) * a;
                    alpha += a;
                    count += 1;
                }
            }
            if alpha == 0 {
                continue;
            }
            let index = (((top + y) * canvas + left + x) * 4) as usize;
            out[index] = (red / alpha) as u8;
            out[index + 1] = (green / alpha) as u8;
            out[index + 2] = (blue / alpha) as u8;
            out[index + 3] = (alpha / count) as u8;
        }
    }
    out
}

/// How far `(x, y)` is from a rounded rectangle's edge; negative inside.
fn rounded_rect_distance(
    (x, y): (f32, f32),
    (center_x, center_y): (f32, f32),
    (half_width, half_height): (f32, f32),
    radius: f32,
) -> f32 {
    let dx = (x - center_x).abs() - half_width + radius;
    let dy = (y - center_y).abs() - half_height + radius;
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius
}

/// How far `(x, y)` is from the charging bolt's outline; negative inside.
fn bolt_distance((x, y): (f32, f32)) -> f32 {
    let mut nearest = f32::MAX;
    let mut inside = false;
    let mut previous = BOLT_POINTS[BOLT_POINTS.len() - 1];
    for point in BOLT_POINTS {
        let (ex, ey) = (previous.0 - point.0, previous.1 - point.1);
        let (px, py) = (x - point.0, y - point.1);
        let along = ((px * ex + py * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        nearest = nearest.min((px - ex * along).hypot(py - ey * along));
        if (point.1 > y) != (previous.1 > y) && px < ex * (y - point.1) / ey {
            inside = !inside;
        }
        previous = point;
    }
    if inside { -nearest } else { nearest }
}

/// A mouse's outline that fills from the bottom with the charge. Where a detail
/// sits on the fill it is cut out instead of drawn dark, so it reads on any
/// panel colour. A sample's colour, or `None` where it is transparent.
fn mouse_sample(
    point: (f32, f32),
    percent: u8,
    charging: bool,
    ink: (u8, u8, u8),
) -> Option<(u8, u8, u8)> {
    if charging {
        let bolt = bolt_distance(point);
        if bolt <= 0.0 {
            return Some(BOLT);
        }
        // A gap around the bolt, so it stands apart from the fill behind it.
        if bolt <= 0.6 {
            return None;
        }
    }
    let body = rounded_rect_distance(point, (11.0, 11.0), (5.4, 9.4), 5.4);
    if body.abs() <= 0.85 {
        return Some(ink);
    }
    let filled = body < 0.0 && point.1 >= 20.0 - f32::from(percent.min(100)) * 18.0 / 100.0;
    let wheel = rounded_rect_distance(point, (11.0, 6.6), (0.9, 2.0), 0.9) <= 0.0;
    (filled != wheel).then_some(ink)
}

/// Draws the battery as a mouse, with `SUBSAMPLES`² samples for each pixel.
fn mouse_battery_rgba(percent: u8, charging: bool, ink: (u8, u8, u8), canvas: u32) -> Vec<u8> {
    let mut out = vec![0u8; (canvas * canvas * 4) as usize];
    let samples = SUBSAMPLES * SUBSAMPLES;
    let per_pixel = canvas * SUBSAMPLES;
    for y in 0..canvas {
        for x in 0..canvas {
            let (mut red, mut green, mut blue, mut hits) = (0u32, 0u32, 0u32, 0u32);
            for sub_y in 0..SUBSAMPLES {
                for sub_x in 0..SUBSAMPLES {
                    let at = |pixel: u32, sub: u32| {
                        ((pixel * SUBSAMPLES + sub) as f32 + 0.5) * DESIGN_SIZE / per_pixel as f32
                    };
                    let point = (at(x, sub_x), at(y, sub_y));
                    if let Some(color) = mouse_sample(point, percent, charging, ink) {
                        red += u32::from(color.0);
                        green += u32::from(color.1);
                        blue += u32::from(color.2);
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let index = ((y * canvas + x) * 4) as usize;
            out[index] = (red / hits) as u8;
            out[index + 1] = (green / hits) as u8;
            out[index + 2] = (blue / hits) as u8;
            out[index + 3] = (hits * 255 / samples) as u8;
        }
    }
    out
}

/// StatusNotifierItem pixmaps are ARGB32 in network byte order.
fn icon_from_rgba(mut data: Vec<u8>, width: u32, height: u32) -> Icon {
    let (pixels, _) = data.as_chunks_mut::<4>();
    for pixel in pixels {
        pixel.rotate_right(1);
    }
    Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::{fit_in_square, icon_from_rgba, mouse_battery_rgba, mouse_sample};

    const INK: (u8, u8, u8) = (242, 184, 75);

    /// Inside the body, below the wheel and clear of the bolt's area.
    const LOW_IN_BODY: (f32, f32) = (11.0, 19.0);
    const HIGH_IN_BODY: (f32, f32) = (8.0, 6.0);
    const WHEEL: (f32, f32) = (11.0, 6.6);

    #[test]
    fn the_mouse_fills_from_the_bottom_with_the_charge() {
        let at = |point, percent| mouse_sample(point, percent, false, INK);
        assert_eq!(at(LOW_IN_BODY, 0), None, "empty");
        assert_eq!(at(LOW_IN_BODY, 30), Some(INK), "the bottom fills first");
        assert_eq!(at(HIGH_IN_BODY, 30), None, "the top is still empty");
        assert_eq!(at(HIGH_IN_BODY, 100), Some(INK), "full");
    }

    #[test]
    fn the_wheel_is_cut_out_of_the_fill() {
        let at = |percent| mouse_sample(WHEEL, percent, false, INK);
        assert_eq!(at(30), Some(INK), "drawn on the empty body");
        assert_eq!(at(100), None, "a gap in the fill");
    }

    #[test]
    fn charging_adds_a_bolt_with_a_gap_around_it() {
        let middle_of_bolt = (10.5, 13.5);
        let next_to_bolt = (7.4, 14.4);
        assert_eq!(
            mouse_sample(middle_of_bolt, 100, true, INK),
            Some(super::BOLT)
        );
        assert_eq!(mouse_sample(next_to_bolt, 100, true, INK), None);
        assert_eq!(mouse_sample(next_to_bolt, 100, false, INK), Some(INK));
    }

    #[test]
    fn the_corners_stay_transparent_and_the_size_is_right() {
        let out = mouse_battery_rgba(100, false, INK, 16);
        assert_eq!(out.len(), 16 * 16 * 4);
        assert_eq!(out[3], 0, "the top left corner");
        assert_eq!(out[16 * 16 * 4 - 1], 0, "the bottom right corner");
    }

    #[test]
    fn a_tall_image_is_centred_with_a_margin() {
        // A solid white 10×20 image: 8×16 on a 32 pixel square.
        let source = [255u8; 10 * 20 * 4];
        let out = fit_in_square(&source, 10, 20, 32, 16);
        assert_eq!(out.len(), 32 * 32 * 4);
        let alpha = |x: usize, y: usize| out[(y * 32 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0, "the corner is transparent");
        assert_eq!(alpha(16, 8), 255, "the top edge of the image");
        assert_eq!(alpha(16, 7), 0, "just above it");
        assert_eq!(alpha(12, 16), 255, "the left edge of the image");
        assert_eq!(alpha(11, 16), 0, "just left of it");
    }

    #[test]
    fn transparent_pixels_do_not_darken_the_average() {
        // One white opaque pixel beside one black transparent one, into one.
        let source = [255, 255, 255, 255, 0, 0, 0, 0];
        let out = fit_in_square(&source, 2, 1, 1, 1);
        assert_eq!(out, [255, 255, 255, 127]);
    }

    #[test]
    fn pixels_become_argb_in_network_order() {
        let icon = icon_from_rgba(vec![1, 2, 3, 4, 5, 6, 7, 8], 2, 1);
        assert_eq!(icon.data, [4, 1, 2, 3, 8, 5, 6, 7]);
        assert_eq!((icon.width, icon.height), (2, 1));
    }
}
