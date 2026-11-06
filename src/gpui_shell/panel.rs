//! The tray panel's views: Home, Settings and the Bridge info page.

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    slider::{Slider, SliderEvent, SliderState},
    switch::Switch,
};
use gpui_kit::*;
use openmouse_bridge::{
    BRIDGE_PORT, config,
    drivers::NATIVE_HID_OWNED,
    platform::{self, TrayBatteryStyle},
    service::{BridgeSnapshot, KnownDeviceStatus, Notice},
    updater::{DownloadProgress, UPDATES_ENABLED, UpdateInfo, download_progress},
};
use tokio::sync::mpsc as tokio_mpsc;

use super::{ExitFlags, theme};
use crate::overlay::Overlay;
use crate::runtime::{
    BridgeCommand, UpdateEvent, WINDOW_HEIGHT, WINDOW_WIDTH, notify_natively, open_logs,
};

const OPENMOUSE_URL: &str = "https://control.openmouse.app";

const LOGO: &[u8] = include_bytes!("../../assets/openmouse-logo.png");
const TITLE_BAR_HEIGHT: f32 = 46.0;
/// The title bar's title.
const TEXT_TITLE: f32 = 16.0;
/// Rows, labels and the status card's detail.
const TEXT_BODY: f32 = 15.0;
/// Section headings, descriptions and notes.
const TEXT_SMALL: f32 = 13.5;

/// The low-battery alert levels on offer.
const BATTERY_THRESHOLDS: [u8; 7] = [10, 15, 20, 25, 30, 40, 50];

/// How long after a settings change the panel trusts its own value over the
/// core's. A snapshot taken before the change can arrive after it, and would
/// otherwise put the old value back until the next one. Snapshots come every
/// second and wait for a poll, so this outlasts any that were on their way.
const SETTINGS_SETTLE: Duration = Duration::from_secs(2);

/// Whether a snapshot may overwrite the panel's settings, given how long ago
/// one was last changed here.
fn settings_settled(since_change: Option<Duration>) -> bool {
    since_change.is_none_or(|elapsed| elapsed >= SETTINGS_SETTLE)
}

/// How many remembered mice Home lists before summarising the rest.
const HOME_DEVICE_ROWS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Settings,
    Info,
}

impl View {
    /// The panel's base height in logical points.
    fn base_height(self) -> f32 {
        match self {
            View::Home => WINDOW_HEIGHT,
            View::Settings if UPDATES_ENABLED => 720.0,
            // Without the updates card.
            View::Settings => 596.0,
            View::Info => 500.0,
        }
    }
}

/// Extra height for one more line of content, in logical points.
const ROW_HEIGHT: f32 = 28.0;
const NOTE_HEIGHT: f32 = 24.0;
/// A card's padding and the gap above it.
const CARD_PADDING: f32 = 28.0;
/// The update card on Home.
const NOTICE_HEIGHT: f32 = 58.0;
/// The progress bar under the update card's headline, with its gap.
const BAR_HEIGHT: f32 = 14.0;

pub struct PanelView {
    snapshots: mpsc::Receiver<BridgeSnapshot>,
    commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    updates: mpsc::Receiver<UpdateEvent>,
    notices: mpsc::Receiver<Notice>,
    panel_requests: mpsc::Receiver<()>,
    exit: ExitFlags,
    snapshot: Option<BridgeSnapshot>,
    view: View,
    autostart: bool,
    automatic_updates: bool,
    hybrid_mode: bool,
    tray_style: TrayBatteryStyle,
    battery_threshold: u8,
    notification_sound: bool,
    notification_volume: u8,
    update: UpdateState,
    /// Bytes received of the archive being downloaded, while one is.
    progress: Option<DownloadProgress>,
    /// Set when the updater has staged a new version and handed off to its
    /// helper, which waits for this process to exit before swapping files in.
    quit_for_update: bool,
    /// Shown on Home until the panel is first closed: the first launch opens
    /// the panel, so the app's home in the tray needs saying.
    first_run_hint: bool,
    error: Option<String>,
    /// Lets the panel take key presses, so Escape can dismiss it.
    focus: FocusHandle,
    logo: Arc<Image>,
    volume: Entity<SliderState>,
    /// When a setting last changed here; see [`SETTINGS_SETTLE`].
    settings_changed_at: Option<Instant>,
    /// The volume slider is being dragged: the snapshot must not move it.
    adjusting_volume: bool,
    was_visible: bool,
    /// The on-screen banner for low-battery and game-profile notices. Without
    /// it (no display for the native window), notices fall back to system
    /// notifications.
    overlay: Option<Overlay>,
}

pub struct Drained {
    pub changed: bool,
    pub panel_requested: bool,
    /// The updater has handed off: the app must exit now so its helper can
    /// install the new version and relaunch it.
    pub quit_for_update: bool,
    /// How soon the overlay needs the next tick while a banner is fading.
    pub overlay_delay: Option<Duration>,
}

/// Where the update flow stands.
#[derive(Clone)]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading(String),
    Restarting(String),
    Failed(String),
}

impl UpdateState {
    fn from_event(event: UpdateEvent) -> Self {
        match event {
            UpdateEvent::Checking => Self::Checking,
            UpdateEvent::UpToDate => Self::UpToDate,
            UpdateEvent::Available(update) => Self::Available(update),
            UpdateEvent::Downloading(version) => Self::Downloading(version),
            UpdateEvent::Restarting(version) => Self::Restarting(version),
            UpdateEvent::Failed(error) => Self::Failed(error),
        }
    }

    /// Something is in progress, so another check or install must wait.
    fn busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading(_) | Self::Restarting(_)
        )
    }

    /// Worth a card on Home, not only a line in Settings.
    fn is_notice(&self) -> bool {
        matches!(
            self,
            Self::Available(_) | Self::Downloading(_) | Self::Restarting(_) | Self::Failed(_)
        )
    }

    /// A progress bar goes under the headline.
    fn shows_bar(&self) -> bool {
        matches!(self, Self::Downloading(_) | Self::Restarting(_))
    }

    /// The one line Settings shows for any state.
    fn text(&self, progress: Option<DownloadProgress>) -> Option<String> {
        Some(match self {
            Self::Idle => return None,
            Self::Checking => "Checking for updates…".into(),
            Self::UpToDate => "Bridge is up to date".into(),
            Self::Available(update) => format!("Version {} is available", update.version),
            Self::Downloading(version) => downloading_text(version, progress),
            Self::Restarting(version) => format!("Restarting into {version}…"),
            Self::Failed(error) => format!("Update failed: {error}"),
        })
    }

    /// The short headline of the card on Home.
    fn headline(&self, progress: Option<DownloadProgress>) -> Option<String> {
        Some(match self {
            Self::Failed(_) => "Update failed".into(),
            Self::Available(_) | Self::Downloading(_) | Self::Restarting(_) => {
                self.text(progress)?
            }
            Self::Idle | Self::Checking | Self::UpToDate => return None,
        })
    }
}

/// "Downloading 2.0.0…" with the percentage once the size is known, and
/// "Preparing" for the checks and unpacking after the last byte.
fn downloading_text(version: &str, progress: Option<DownloadProgress>) -> String {
    match progress {
        Some(progress) if progress.is_complete() => format!("Preparing {version}…"),
        Some(progress) => match progress.fraction() {
            Some(fraction) => format!("Downloading {version} · {}%", (fraction * 100.0) as u32),
            None => format!("Downloading {version}…"),
        },
        None => format!("Downloading {version}…"),
    }
}

/// A thin progress bar. With a fraction it fills that far; without one (the
/// size is unknown, or the download is being checked and unpacked) a segment
/// sweeps across it.
fn progress_bar(fraction: Option<f32>, fill: Hsla, track: Hsla) -> AnyElement {
    let bar = div()
        .relative()
        .h(px(4.))
        .w_full()
        .flex_none()
        .rounded_full()
        .overflow_hidden()
        .bg(track);
    match fraction {
        Some(fraction) => bar
            .child(
                div()
                    .h_full()
                    .w(relative(fraction.clamp(0.0, 1.0)))
                    .rounded_full()
                    .bg(fill),
            )
            .into_any_element(),
        None => bar
            .child(
                div()
                    .absolute()
                    .top(px(0.))
                    .h_full()
                    .w(relative(0.35))
                    .rounded_full()
                    .bg(fill)
                    .with_animation(
                        "update-sweep",
                        Animation::new(Duration::from_millis(1100)).repeat(),
                        |segment, delta| segment.left(relative(delta * 1.35 - 0.35)),
                    ),
            )
            .into_any_element(),
    }
}

/// The channels between the panel and the core.
pub struct CoreLinks {
    pub snapshots: mpsc::Receiver<BridgeSnapshot>,
    pub commands: tokio_mpsc::UnboundedSender<BridgeCommand>,
    pub updates: mpsc::Receiver<UpdateEvent>,
    pub notices: mpsc::Receiver<Notice>,
    pub panel_requests: mpsc::Receiver<()>,
}

impl PanelView {
    pub fn new(
        links: CoreLinks,
        exit: ExitFlags,
        first_run: bool,
        focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(60.0)
        });
        cx.subscribe(&volume, |this, _, event: &SliderEvent, cx| {
            this.volume_changed(event, cx);
        })
        .detach();
        let CoreLinks {
            snapshots,
            commands,
            updates,
            notices,
            panel_requests,
        } = links;
        if first_run {
            // Tell the core the tray hint was shown, so later launches start
            // hidden in the tray instead of opening the panel again.
            let _ = commands.send(BridgeCommand::AcknowledgeTrayHint);
        }
        Self {
            snapshots,
            commands,
            updates,
            notices,
            panel_requests,
            exit,
            snapshot: None,
            // A development aid: OPENMOUSE_BRIDGE_START_VIEW=settings|info opens
            // straight onto that view.
            view: match std::env::var("OPENMOUSE_BRIDGE_START_VIEW").as_deref() {
                Ok("settings") => View::Settings,
                Ok("info") => View::Info,
                _ => View::Home,
            },
            autostart: platform::autostart_enabled(),
            automatic_updates: false,
            hybrid_mode: true,
            tray_style: TrayBatteryStyle::Glyph,
            battery_threshold: 20,
            notification_sound: true,
            notification_volume: 60,
            update: UpdateState::Idle,
            progress: None,
            quit_for_update: false,
            first_run_hint: first_run,
            error: None,
            focus,
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, LOGO.to_vec())),
            volume,
            settings_changed_at: None,
            adjusting_volume: false,
            was_visible: true,
            overlay: Overlay::new()
                .inspect_err(|error| tracing::warn!(%error, "Could not create the overlay"))
                .ok(),
        }
    }

    /// What the tray icon should show for the latest snapshot.
    pub fn tray_battery(&self) -> Option<crate::tray_battery::TrayBattery> {
        crate::tray_battery::from_snapshot(self.snapshot.as_ref())
    }

    /// Takes everything the core has queued since the last poll. Notices become
    /// system notifications for now; the GPUI overlay replaces that later.
    pub fn drain(&mut self) -> Drained {
        let mut changed = false;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            if settings_settled(self.settings_changed_at.map(|at| at.elapsed())) {
                self.autostart = snapshot.autostart_enabled;
                self.automatic_updates = snapshot.automatic_updates;
                self.hybrid_mode = snapshot.hybrid_mode;
                self.tray_style = TrayBatteryStyle::from_config_str(&snapshot.tray_battery_style);
                self.battery_threshold = snapshot.battery_threshold_percent;
                self.notification_sound = snapshot.notification_sound;
                if !self.adjusting_volume {
                    self.notification_volume = snapshot.notification_volume;
                }
            }
            self.snapshot = Some(snapshot);
            changed = true;
        }
        while let Ok(event) = self.updates.try_recv() {
            self.quit_for_update |= matches!(event, UpdateEvent::Restarting(_));
            self.update = UpdateState::from_event(event);
            changed = true;
        }
        // The counter is a shared atomic, not an event, so sample it while a
        // download runs and repaint when it moved.
        if matches!(self.update, UpdateState::Downloading(_)) {
            let progress = download_progress();
            if self.progress != Some(progress) {
                self.progress = Some(progress);
                changed = true;
            }
        } else if self.progress.take().is_some() {
            changed = true;
        }
        while let Ok(notice) = self.notices.try_recv() {
            self.present(&notice);
        }
        let mut panel_requested = false;
        while self.panel_requests.try_recv().is_ok() {
            panel_requested = true;
        }
        // Nothing repaints a panel that is hidden in the tray; the state above
        // keeps up regardless, and one repaint on showing it catches up.
        let visible = super::tray::is_visible();
        let became_visible = visible && !self.was_visible;
        self.was_visible = visible;
        Drained {
            changed: (changed && visible) || became_visible,
            panel_requested,
            quit_for_update: self.quit_for_update,
            overlay_delay: self.overlay.as_mut().and_then(Overlay::tick),
        }
    }

    /// Shows `notice` on the overlay with its chime, or as a system
    /// notification when there is no overlay. Low-battery alerts and the
    /// profile switch for a detected game both arrive here.
    fn present(&mut self, notice: &Notice) {
        let (sound, volume) = self.snapshot.as_ref().map_or((true, 60), |snapshot| {
            (snapshot.notification_sound, snapshot.notification_volume)
        });
        let Some(overlay) = self.overlay.as_mut() else {
            notify_natively(notice);
            return;
        };
        if let Err(error) = overlay.show(&notice.title, &notice.detail) {
            tracing::warn!(%error, "Could not show the overlay");
            notify_natively(notice);
            return;
        }
        tracing::info!(title = %notice.title, sound, volume, "Showed a notice on the overlay");
        if sound && let Err(error) = overlay.chime(volume) {
            tracing::warn!(%error, "Could not play the notification sound");
        }
    }

    fn show_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.view = view;
        self.error = None;
        cx.notify();
    }

    /// The height the current view needs: the base height plus a line for each
    /// extra remembered mouse, the "more" note, and the update status text.
    fn desired_height(&self) -> f32 {
        let mut height = self.view.base_height();
        match self.view {
            View::Home => {
                let known = self
                    .snapshot
                    .as_ref()
                    .map_or(&[][..], |snapshot| snapshot.known_devices.as_slice());
                // The card lists every mouse but the one in the status card.
                let others = known.len() - usize::from(hero_device(known).is_some());
                if others > 0 {
                    height += CARD_PADDING + ROW_HEIGHT * others.min(HOME_DEVICE_ROWS) as f32;
                }
                if others > HOME_DEVICE_ROWS {
                    height += NOTE_HEIGHT;
                }
                if self.update.is_notice() {
                    height += NOTICE_HEIGHT;
                    if self.update.shows_bar() {
                        height += BAR_HEIGHT;
                    }
                    if matches!(self.update, UpdateState::Failed(_)) {
                        height += NOTE_HEIGHT * 2.0;
                    }
                }
                if native_problem(self.snapshot.as_ref()).is_some() {
                    height += NOTE_HEIGHT * 2.0;
                }
                if self.first_run_hint {
                    height += NOTE_HEIGHT * 2.0;
                }
            }
            View::Settings => {}
            View::Info => {}
        }
        height
    }

    fn set_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        match platform::set_autostart(enabled) {
            Ok(()) => {
                self.settings_changed_at = Some(Instant::now());
                self.autostart = enabled;
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn set_automatic_updates(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings_changed_at = Some(Instant::now());
        self.automatic_updates = enabled;
        let _ = self
            .commands
            .send(BridgeCommand::SetAutomaticUpdates(enabled));
        cx.notify();
    }

    /// Forgets a remembered mouse that is no longer plugged in.
    fn forget_device(&mut self, id: String, cx: &mut Context<Self>) {
        let _ = self.commands.send(BridgeCommand::ForgetBattery(id));
        cx.notify();
    }

    fn set_hybrid_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings_changed_at = Some(Instant::now());
        self.hybrid_mode = enabled;
        let _ = self.commands.send(BridgeCommand::SetHybridMode(enabled));
        cx.notify();
    }

    fn set_tray_style(&mut self, style: TrayBatteryStyle, cx: &mut Context<Self>) {
        self.settings_changed_at = Some(Instant::now());
        self.tray_style = style;
        let _ = self
            .commands
            .send(BridgeCommand::SetTrayBatteryStyle(style));
        cx.notify();
    }

    fn set_battery_threshold(&mut self, level: u8, cx: &mut Context<Self>) {
        tracing::debug!(level, "low battery alert changed");
        self.settings_changed_at = Some(Instant::now());
        self.battery_threshold = level;
        let _ = self
            .commands
            .send(BridgeCommand::SetBatteryThreshold(level));
        cx.notify();
    }

    fn set_notification_sound(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings_changed_at = Some(Instant::now());
        self.notification_sound = enabled;
        let _ = self
            .commands
            .send(BridgeCommand::SetNotificationSound(enabled));
        cx.notify();
    }

    /// Follows the slider while it moves, and saves where it stops, so a drag
    /// is one change to the config rather than one for each step.
    fn volume_changed(&mut self, event: &SliderEvent, cx: &mut Context<Self>) {
        match event {
            SliderEvent::Change(value) => {
                self.adjusting_volume = true;
                self.notification_volume = value.end().round() as u8;
            }
            SliderEvent::Release(value) => {
                self.adjusting_volume = false;
                self.settings_changed_at = Some(Instant::now());
                self.notification_volume = value.end().round() as u8;
                let _ = self.commands.send(BridgeCommand::SetNotificationVolume(
                    self.notification_volume,
                ));
            }
        }
        cx.notify();
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.update = UpdateState::Checking;
        let _ = self.commands.send(BridgeCommand::CheckForUpdates);
        cx.notify();
    }

    fn install_update(&mut self, update: UpdateInfo, cx: &mut Context<Self>) {
        self.update = UpdateState::Downloading(update.version.clone());
        let _ = self.commands.send(BridgeCommand::InstallUpdate(update));
        cx.notify();
    }

    /// Sends a notice down the same path a low-battery alert or a detected
    /// game's profile switch takes, so it shows the overlay and plays the chime.
    fn test_notification(&mut self, cx: &mut Context<Self>) {
        self.present(&Notice {
            title: "Test notification".into(),
            detail: "This is how OpenMouse Bridge alerts look.".into(),
        });
        cx.notify();
    }

    fn open_logs(&mut self, cx: &mut Context<Self>) {
        self.error = open_logs().err().map(|error| error.to_string());
        cx.notify();
    }
}

fn row(muted: Hsla, label: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap_3()
        .text_size(px(TEXT_BODY))
        .child(div().text_color(muted).child(label))
        .child(div().truncate().child(value.into()))
}

/// A card, lit from the top edge like the rest of the panel.
fn card(line: Hsla, fill: Hsla) -> Div {
    framed(rgb(theme::CARD_TOP).into(), line, fill)
        .py_2()
        .px_3()
}

/// The status card: as [`card`], with a green tint.
fn lit_card(line: Hsla) -> Div {
    framed(
        rgb(theme::LIT_CARD_TOP).into(),
        line,
        rgb(theme::SURFACE).into(),
    )
    .p_3()
}

fn framed(top: Hsla, line: Hsla, bottom: Hsla) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap_2()
        .rounded(px(10.))
        .border_1()
        .border_color(line)
        .bg(linear_gradient(
            180.,
            linear_color_stop(top, 0.),
            linear_color_stop(bottom, 1.),
        ))
}

fn section_label(label: &'static str, muted: Hsla) -> Div {
    div()
        .text_size(px(TEXT_SMALL))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(muted)
        .child(label)
}

fn divider(line: Hsla) -> Div {
    div().h(px(1.)).flex_none().bg(line)
}

/// The mouse Home's status card shows, drawn to match the tray's.
fn mouse_glyph(color: Hsla) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(3.))
        .w(px(16.))
        .h(px(24.))
        .rounded(px(8.))
        .border_2()
        .border_color(color)
        .child(div().w(px(2.)).h(px(5.)).rounded(px(1.)).bg(color))
}

/// The mouse the status card is about: the first one connected.
fn hero_device(known: &[KnownDeviceStatus]) -> Option<&KnownDeviceStatus> {
    known.iter().find(|device| device.connected)
}

/// The profile row: the game's profile while one is active, otherwise Default.
fn profile_name(snapshot: Option<&BridgeSnapshot>) -> String {
    snapshot
        .and_then(|snapshot| {
            snapshot
                .active_profile
                .as_ref()
                .filter(|_| !snapshot.active_profile_is_default)
        })
        .map_or_else(
            || "Default".into(),
            |profile| profile.application.name.clone(),
        )
}

/// A failed or pending hardware write, kept out of the profile row so it stays
/// readable; a selected profile is not proof the mouse took it.
fn native_problem(snapshot: Option<&BridgeSnapshot>) -> Option<String> {
    let snapshot = snapshot?;
    if snapshot.pending_native_restores > 0 {
        let mut text = format!(
            "Native restore pending ({})",
            snapshot.pending_native_restores
        );
        if let Some(error) = &snapshot.native_automation_error {
            text.push_str(": ");
            text.push_str(error);
        }
        return Some(text);
    }
    snapshot
        .native_automation_error
        .as_ref()
        // The web panel holding the mouse is the normal handover, not a problem.
        .filter(|error| error.as_str() != NATIVE_HID_OWNED)
        .map(|error| format!("Native settings not applied: {error}"))
}

fn switch_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .items_center()
        .text_size(px(TEXT_BODY))
        .child(label)
        .child(control)
}

fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".into(),
        60..=3_599 => format!("{} min ago", seconds / 60),
        3_600..=86_399 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn format_uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else {
        format!("{minutes} min")
    }
}

/// What Home shows for a remembered mouse: its live reading while connected,
/// otherwise when it was last seen and the reading it had then.
fn known_device_text(device: &KnownDeviceStatus) -> String {
    let mut parts = Vec::new();
    if device.connected {
        match device.percent {
            Some(percent) if device.charging == Some(true) => {
                parts.push(format!("{percent}% · Charging"));
            }
            Some(percent) => parts.push(format!("{percent}%")),
            None => parts.push("Connected".into()),
        }
        if let Some(dpi) = device.dpi {
            parts.push(format!("{dpi} DPI"));
        }
    } else {
        parts.push(format!(
            "Last seen {}",
            format_age(device.last_seen_seconds_ago)
        ));
        if let Some(percent) = device.percent {
            parts.push(format!("{percent}%"));
        }
    }
    parts.join(" · ")
}

impl PanelView {
    /// The panel's title bar on its darker strip: the logo and name on Home, a
    /// back button and title elsewhere (the empty space drags the borderless
    /// window), the settings button on Home, and a close button that hides the
    /// panel to the tray (or quits where there is no tray).
    fn title_bar(
        &self,
        title: &'static str,
        back: bool,
        settings: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut bar = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(TITLE_BAR_HEIGHT))
            .px_2()
            .bg(rgb(theme::CHROME))
            .border_b_1()
            .border_color(rgb(theme::DIVIDER));
        if back {
            bar = bar.child(
                Button::new("back")
                    .ghost()
                    .compact()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_view(View::Home, cx);
                    })),
            );
        }
        let heading = if back {
            div()
                .text_size(px(TEXT_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title)
        } else {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .pl_1()
                .child(img(self.logo.clone()).flex_none().w(px(12.)).h(px(18.)))
                .child(
                    div()
                        .text_size(px(TEXT_TITLE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
        };
        // Only this area starts a window move, so the buttons keep their own
        // clicks.
        bar = bar.child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .h_full()
                .items_center()
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                .child(heading),
        );
        if settings {
            bar = bar.child(
                Button::new("settings")
                    .ghost()
                    .compact()
                    .icon(IconName::Settings)
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_view(View::Settings, cx);
                    })),
            );
        }
        // A Linux window can go to the dock as well as to the tray.
        if cfg!(target_os = "linux") {
            bar = bar.child(
                Button::new("minimize")
                    .ghost()
                    .compact()
                    .icon(IconName::WindowMinimize)
                    .tooltip("Minimize")
                    .on_click(cx.listener(|_, _, window, _| window.minimize_window())),
            );
        }
        bar.child(
            Button::new("close")
                .ghost()
                .compact()
                .icon(IconName::Close)
                .tooltip(if super::tray::can_hide() {
                    "Hide to tray"
                } else {
                    "Quit"
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.first_run_hint = false;
                    if super::tray::can_hide() {
                        super::tray::hide(window);
                    } else {
                        this.exit.quit(cx);
                    }
                })),
        )
    }

    /// The status card at the top of Home: the connected mouse and its battery,
    /// or what to do while there is none.
    fn mouse_card(
        &self,
        hero: Option<&KnownDeviceStatus>,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
    ) -> Div {
        let starting = self.snapshot.is_none();
        let (name, detail): (String, String) = match hero {
            _ if starting => ("Starting Bridge…".into(), "Waiting for service".into()),
            Some(device) => (device.name.clone(), known_device_text(device)),
            None => (
                "Ready to connect".into(),
                "Connect a mouse in OpenMouse.".into(),
            ),
        };
        let glyph = if hero.is_some() { accent } else { muted };
        lit_card(line).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(44.))
                        .rounded(px(8.))
                        .bg(rgb(theme::CHROME))
                        .border_1()
                        .border_color(line)
                        .child(mouse_glyph(glyph)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(TEXT_BODY))
                                .text_color(muted)
                                .child(detail),
                        ),
                ),
        )
    }

    fn home(
        &self,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
        fill: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        let snapshot = self.snapshot.as_ref();
        let known = snapshot
            .map(|snapshot| snapshot.known_devices.as_slice())
            .unwrap_or(&[]);
        let hero = hero_device(known);
        let connected = snapshot.is_some_and(|snapshot| snapshot.client_connected);

        let status = card(line, fill).py(px(5.)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_size(px(TEXT_BODY))
                .text_color(muted)
                .child(
                    div()
                        .flex_none()
                        .size(px(5.))
                        .rounded_full()
                        .bg(if snapshot.is_some() { accent } else { muted }),
                )
                .child(if snapshot.is_some() {
                    "Bridge ready"
                } else {
                    "Starting"
                })
                .child(div().flex_1())
                .child(if connected {
                    "Panel connected"
                } else {
                    "Panel offline"
                })
                .child(
                    Button::new("connection-details")
                        .ghost()
                        .compact()
                        .icon(IconName::Info)
                        .tooltip("Connection details")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_view(View::Info, cx);
                        })),
                ),
        );
        let profile = card(line, fill).child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .items_center()
                .gap_3()
                .text_size(px(TEXT_BODY))
                .child(div().text_color(muted).child("Mouse profile"))
                .child(
                    div()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(profile_name(snapshot)),
                ),
        );

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_1()
            .child(self.mouse_card(hero, muted, accent, line))
            .child(status)
            .child(profile);

        // Mice that are not the one in the card above, such as one unplugged.
        let others = known
            .iter()
            .filter(|device| hero.is_none_or(|hero| hero.id != device.id))
            .collect::<Vec<_>>();
        if !others.is_empty() {
            let mut list = card(line, fill);
            for device in others.iter().take(HOME_DEVICE_ROWS) {
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .text_size(px(TEXT_BODY))
                        .child(
                            div()
                                .truncate()
                                .text_color(if device.connected { accent } else { muted })
                                .child(device.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_none()
                                .items_center()
                                .gap_1()
                                .text_color(muted)
                                .child(known_device_text(device))
                                .children((!device.connected).then(|| {
                                    let id = device.id.clone();
                                    Button::new(SharedString::from(format!("forget-{}", device.id)))
                                        .ghost()
                                        .compact()
                                        .icon(IconName::Close)
                                        .tooltip("Forget this mouse")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.forget_device(id.clone(), cx);
                                        }))
                                })),
                        ),
                );
            }
            if others.len() > HOME_DEVICE_ROWS {
                list = list.child(div().text_size(px(TEXT_SMALL)).text_color(muted).child(
                    format!("+{} more remembered", others.len() - HOME_DEVICE_ROWS),
                ));
            }
            body = body.child(list);
        }
        if self.first_run_hint {
            body =
                body.child(div().text_size(px(TEXT_SMALL)).text_color(muted).child(
                    "Bridge keeps running in the tray. Use its icon to open this panel again.",
                ));
        }
        if let Some(problem) = native_problem(snapshot) {
            body = body.child(
                div()
                    .text_size(px(TEXT_SMALL))
                    .text_color(cx.theme().danger)
                    .child(problem),
            );
        }
        if let Some(headline) = self.update.headline(self.progress) {
            let action = match &self.update {
                UpdateState::Available(update) => {
                    let update = update.clone();
                    Some(
                        Button::new("home-install-update")
                            .primary()
                            .compact()
                            .label("Install")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.install_update(update.clone(), cx);
                            })),
                    )
                }
                UpdateState::Failed(_) => Some(
                    Button::new("home-retry-update")
                        .compact()
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
                ),
                _ => None,
            };
            let percent = match (&self.update, self.progress) {
                (UpdateState::Downloading(_), Some(progress)) if !progress.is_complete() => {
                    progress
                        .fraction()
                        .map(|fraction| format!("{}%", (fraction * 100.0) as u32))
                }
                _ => None,
            };
            let mut notice = card(line, fill).child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .text_size(px(TEXT_BODY))
                    .child(div().truncate().child(headline))
                    .children(action)
                    .children(
                        percent.map(|percent| div().flex_none().text_color(muted).child(percent)),
                    ),
            );
            if self.update.shows_bar() {
                // Downloading shows how far; the checks after the last byte,
                // and the restart, have no length to show.
                let fraction = match (&self.update, self.progress) {
                    (UpdateState::Downloading(_), Some(progress)) if !progress.is_complete() => {
                        progress.fraction()
                    }
                    _ => None,
                };
                notice = notice.child(progress_bar(fraction, accent, line));
            }
            if let UpdateState::Failed(error) = &self.update {
                notice = notice.child(
                    div()
                        .text_size(px(TEXT_SMALL))
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            body = body.child(notice);
        }
        let body = body.child(div().flex_1()).child(
            Button::new("open-control-panel")
                .primary()
                .w_full()
                .icon(IconName::ExternalLink)
                .label("Open control panel")
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.open_url(OPENMOUSE_URL);
                })),
        );
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.title_bar("OpenMouse Bridge", false, true, cx))
            .child(body)
    }

    fn settings(
        &self,
        muted: Hsla,
        accent: Hsla,
        line: Hsla,
        fill: Hsla,
        danger: Hsla,
        cx: &mut Context<Self>,
    ) -> Div {
        // The title bar and footer stay put; everything between them scrolls, so
        // the panel fits a short screen.
        let mut body = div()
            .id("settings-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap(px(6.))
            .overflow_y_scroll()
            .child(
                card(line, fill).child(switch_row(
                    "Launch at login",
                    Switch::new("autostart")
                        .checked(self.autostart)
                        .on_change(cx.listener(|this, enabled: &bool, _, cx| {
                            this.set_autostart(*enabled, cx);
                        })),
                )),
            );

        let mut device_mode =
            card(line, fill)
                .child(section_label("Device mode", muted))
                .child(switch_row(
                    "Hybrid",
                    Switch::new("hybrid-mode")
                        .checked(self.hybrid_mode)
                        .on_change(cx.listener(|this, enabled: &bool, _, cx| {
                            this.set_hybrid_mode(*enabled, cx);
                        })),
                ))
                .child(div().text_size(px(TEXT_SMALL)).text_color(muted).child(
                    if self.hybrid_mode {
                        "While the web panel is closed, Bridge applies the mouse's stored \
                         settings and reads the battery itself"
                    } else {
                        "Only the web panel changes the mouse"
                    },
                ));
        // The Linux tray always draws its own battery, so this has no effect there.
        if !cfg!(target_os = "linux") {
            device_mode = device_mode
                .child(divider(line))
                .child(switch_row("Battery display", self.battery_style_menu(cx)));
        }

        let notifications = card(line, fill)
            .child(section_label("Notifications", muted))
            .child(switch_row(
                "Low battery alert",
                self.battery_threshold_menu(cx),
            ))
            .child(divider(line))
            .child(switch_row(
                "Notification sound",
                Switch::new("notification-sound")
                    .checked(self.notification_sound)
                    .on_change(cx.listener(|this, enabled: &bool, _, cx| {
                        this.set_notification_sound(*enabled, cx);
                    })),
            ))
            .child(divider(line))
            .child(switch_row(
                "Volume",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(130.))
                            .child(Slider::new(&self.volume).disabled(!self.notification_sound)),
                    )
                    .child(
                        div()
                            .w(px(38.))
                            .text_right()
                            .text_size(px(TEXT_BODY))
                            .text_color(muted)
                            .child(format!("{}%", self.notification_volume)),
                    ),
            ))
            .child(divider(line))
            .child(switch_row(
                "Test notification",
                Button::new("test-notification")
                    .compact()
                    .label("Test")
                    .on_click(cx.listener(|this, _, _, cx| this.test_notification(cx))),
            ));
        body = body.child(device_mode).child(notifications);

        if UPDATES_ENABLED {
            let mut updates = card(line, fill)
                .child(section_label("Updates", muted))
                .child(switch_row(
                    "Automatic updates",
                    Switch::new("automatic-updates")
                        .checked(self.automatic_updates)
                        .on_change(cx.listener(|this, enabled: &bool, _, cx| {
                            this.set_automatic_updates(*enabled, cx);
                        })),
                ))
                .child(
                    div().flex().flex_row().items_center().gap_2().child(
                        Button::new("check-updates")
                            .compact()
                            .label("Check for updates")
                            .loading(self.update.busy())
                            .disabled(self.update.busy())
                            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
                    ),
                );
            if let UpdateState::Available(update) = self.update.clone() {
                updates = updates.child(
                    Button::new("install-update")
                        .primary()
                        .compact()
                        .label(format!("Install {}", update.version))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.install_update(update.clone(), cx);
                        })),
                );
            }
            if self.update.shows_bar() {
                let fraction = match (&self.update, self.progress) {
                    (UpdateState::Downloading(_), Some(progress)) if !progress.is_complete() => {
                        progress.fraction()
                    }
                    _ => None,
                };
                updates = updates.child(progress_bar(fraction, accent, line));
            }
            if let Some(text) = self.update.text(self.progress) {
                updates = updates.child(
                    div()
                        .text_size(px(TEXT_SMALL))
                        .text_color(muted)
                        .child(text.clone()),
                );
            }
            body = body.child(updates);
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(TEXT_SMALL))
                    .text_color(danger)
                    .child(error.clone()),
            );
        }
        body = body.child(
            div()
                .flex_none()
                .pt_1()
                .text_center()
                .text_size(px(TEXT_SMALL))
                .text_color(muted)
                .child(format!("OpenMouse Bridge v{}", env!("CARGO_PKG_VERSION"))),
        );

        let footer = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(line)
            .child(
                Button::new("open-logs")
                    .compact()
                    .icon(IconName::FileText)
                    .label("Open logs")
                    .on_click(cx.listener(|this, _, _, cx| this.open_logs(cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("info")
                    .ghost()
                    .compact()
                    .icon(IconName::Info)
                    .tooltip("OpenMouse Bridge info")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_view(View::Info, cx);
                    })),
            )
            .child(
                Button::new("quit")
                    .danger()
                    .compact()
                    .label("Quit Bridge")
                    .on_click(cx.listener(|this, _, _, cx| this.exit.quit(cx))),
            );
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.title_bar("Settings", true, false, cx))
            .child(body)
            .child(footer)
    }

    /// A dropdown of the low-battery alert levels.
    fn battery_threshold_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let current = self.battery_threshold;
        let view = cx.entity();
        Button::new("battery-threshold")
            .outline()
            .compact()
            .label(format!("{current}%"))
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                for level in BATTERY_THRESHOLDS {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{level}%"))
                            .checked(level == current)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.set_battery_threshold(level, cx);
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// A dropdown of the tray icon's styles, where the tray draws them.
    fn battery_style_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let current = self.tray_style;
        let view = cx.entity();
        let name = |style| match style {
            TrayBatteryStyle::Glyph => "Battery glyph",
            TrayBatteryStyle::Percentage => "Percentage",
        };
        Button::new("battery-display")
            .outline()
            .compact()
            .label(name(current))
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                for style in [TrayBatteryStyle::Glyph, TrayBatteryStyle::Percentage] {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name(style))
                            .checked(style == current)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| this.set_tray_style(style, cx));
                            }),
                    );
                }
                menu
            })
    }

    fn info(&self, muted: Hsla, line: Hsla, fill: Hsla, cx: &mut Context<Self>) -> Div {
        let snapshot = self.snapshot.as_ref();
        let platform_text = match snapshot.and_then(|snapshot| snapshot.linux_distribution.clone())
        {
            Some(distribution) => format!("{} · {distribution}", platform::platform_name()),
            None => platform::platform_name().to_owned(),
        };
        let path_text = |path: anyhow::Result<std::path::PathBuf>| {
            path.map(|path| path.display().to_string())
                .unwrap_or_else(|_| "unavailable".into())
        };
        let mut details = card(line, fill)
            .child(row(muted, "Version", env!("CARGO_PKG_VERSION")))
            .child(row(muted, "Platform", platform_text))
            .child(row(muted, "Address", format!("127.0.0.1:{BRIDGE_PORT}")));
        if let Some(snapshot) = snapshot {
            details = details
                .child(row(
                    muted,
                    "Running for",
                    format_uptime(snapshot.uptime_seconds),
                ))
                .child(row(
                    muted,
                    "Control panel",
                    if snapshot.client_connected {
                        "Connected"
                    } else {
                        "Not connected"
                    },
                ))
                .child(row(muted, "Device mode", snapshot.device_mode.clone()))
                .child(row(muted, "Profiles", snapshot.profile_count.to_string()))
                .child(row(
                    muted,
                    "Remembered mice",
                    snapshot.known_devices.len().to_string(),
                ));
        }
        details = details
            .child(row(muted, "Config", path_text(config::config_path())))
            .child(row(muted, "Logs", path_text(config::log_dir())));
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.title_bar("Bridge info", true, false, cx))
            .child(div().p_3().child(details))
    }
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, accent, foreground, danger) = (
            theme.muted_foreground,
            theme.primary,
            theme.foreground,
            theme.danger,
        );
        let line = theme.border;
        let fill: Hsla = rgb(theme::SURFACE).into();
        // Keep the window as tall as its content so nothing is clipped.
        let desired = self.desired_height();
        if (window.viewport_size().height.as_f32() - desired).abs() > 1.0 {
            window.resize(size(px(WINDOW_WIDTH), px(desired)));
        }
        // The snapshot moved the volume, and the slider has not caught up.
        if !self.adjusting_volume {
            let volume = f32::from(self.notification_volume);
            if (self.volume.read(cx).value().end() - volume).abs() >= 0.5 {
                self.volume
                    .update(cx, |slider, cx| slider.set_value(volume, window, cx));
            }
        }
        let content = match self.view {
            View::Home => self.home(muted, accent, line, fill, cx),
            View::Settings => self.settings(muted, accent, line, fill, danger, cx),
            View::Info => self.info(muted, line, fill, cx),
        };
        // Escape hides the panel to the tray, where there is one. The panel
        // takes focus whenever it is drawn and does not have it.
        if !self.focus.is_focused(window) {
            window.focus(&self.focus, cx);
        }
        div()
            .size_full()
            .border_1()
            .border_color(line)
            .text_color(foreground)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, window, _| {
                if event.keystroke.key == "escape" && super::tray::can_hide() {
                    super::tray::hide(window);
                }
            }))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that pulls in GPUI's own `test` macro, which shadows
    // `#[test]`.
    use super::{
        NATIVE_HID_OWNED, SETTINGS_SETTLE, UpdateState, hero_device, native_problem,
        settings_settled,
    };
    use crate::runtime::UpdateEvent;
    use std::time::Duration;

    fn mouse(id: &str, connected: bool) -> openmouse_bridge::service::KnownDeviceStatus {
        openmouse_bridge::service::KnownDeviceStatus {
            id: id.into(),
            name: id.into(),
            connected,
            last_seen_seconds_ago: 0,
            percent: None,
            charging: None,
            dpi: None,
            polling_rate_hz: None,
            switch_mode: None,
        }
    }

    #[test]
    fn a_snapshot_cannot_undo_a_setting_changed_just_now() {
        assert!(settings_settled(None), "nothing was changed");
        assert!(!settings_settled(Some(Duration::ZERO)));
        assert!(!settings_settled(Some(
            SETTINGS_SETTLE - Duration::from_millis(1)
        )));
        assert!(settings_settled(Some(SETTINGS_SETTLE)));
    }

    #[test]
    fn the_card_shows_the_first_connected_mouse() {
        let known = [mouse("old", false), mouse("live", true)];
        assert_eq!(
            hero_device(&known).map(|device| device.id.as_str()),
            Some("live")
        );
        assert!(hero_device(&[mouse("old", false)]).is_none());
        assert!(hero_device(&[]).is_none());
    }

    #[test]
    fn the_web_panel_holding_the_mouse_is_not_shown_as_a_problem() {
        let mut snapshot = crate::gpui_shell::test_snapshot();
        snapshot.native_automation_error = Some(NATIVE_HID_OWNED.to_owned());
        assert_eq!(native_problem(Some(&snapshot)), None);

        snapshot.native_automation_error = Some("the write failed".to_owned());
        assert_eq!(
            native_problem(Some(&snapshot)).as_deref(),
            Some("Native settings not applied: the write failed")
        );

        // A restore still waiting is worth showing, whatever is holding it up.
        snapshot.native_automation_error = Some(NATIVE_HID_OWNED.to_owned());
        snapshot.pending_native_restores = 1;
        assert!(native_problem(Some(&snapshot)).is_some());
    }

    fn state(event: UpdateEvent) -> UpdateState {
        UpdateState::from_event(event)
    }

    #[test]
    fn only_work_in_progress_is_busy() {
        assert!(state(UpdateEvent::Checking).busy());
        assert!(state(UpdateEvent::Downloading("2.0.0".into())).busy());
        assert!(state(UpdateEvent::Restarting("2.0.0".into())).busy());
        assert!(!UpdateState::Idle.busy());
        assert!(!state(UpdateEvent::UpToDate).busy());
        assert!(!state(UpdateEvent::Failed("no network".into())).busy());
    }

    #[test]
    fn home_gets_a_card_for_updates_that_need_attention_only() {
        assert!(!UpdateState::Idle.is_notice());
        assert!(!state(UpdateEvent::Checking).is_notice());
        assert!(!state(UpdateEvent::UpToDate).is_notice());
        assert!(state(UpdateEvent::Downloading("2.0.0".into())).is_notice());
        assert!(state(UpdateEvent::Restarting("2.0.0".into())).is_notice());
        assert!(state(UpdateEvent::Failed("no network".into())).is_notice());
    }

    #[test]
    fn the_texts_name_the_version_and_the_failure() {
        assert_eq!(UpdateState::Idle.text(None), None);
        assert_eq!(
            state(UpdateEvent::UpToDate).text(None).as_deref(),
            Some("Bridge is up to date")
        );
        assert_eq!(
            state(UpdateEvent::Downloading("2.0.0".into()))
                .headline(None)
                .as_deref(),
            Some("Downloading 2.0.0…")
        );
        assert_eq!(
            state(UpdateEvent::Restarting("2.0.0".into()))
                .headline(None)
                .as_deref(),
            Some("Restarting into 2.0.0…")
        );
        let failed = state(UpdateEvent::Failed("no network".into()));
        assert_eq!(failed.headline(None).as_deref(), Some("Update failed"));
        assert_eq!(
            failed.text(None).as_deref(),
            Some("Update failed: no network")
        );
        assert_eq!(state(UpdateEvent::Checking).headline(None), None);
    }
}
