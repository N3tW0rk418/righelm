//! RigHelm — the Linux alternative to Win4Yaesu for the Yaesu FTDX3000.

// No console window behind the app on Windows (debug builds keep it, for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod console;
mod cw_key;
mod demo;
mod logger_connect;
mod memories;
mod menu_tab;
mod operate;
mod remote_tab;
mod scope;
mod settings_tab;
mod sharing;
mod waterfall;
mod widgets;

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use parking_lot::Mutex;
use ftdx3000_rig::detect::{self, DetectedRadio};
use ftdx3000_rig::{Config, LinkState, Rig, Services};

use widgets::{AMBER, GREEN, RED, ink};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Operate,
    Menu,
    Memories,
    Sharing,
    Remote,
    Console,
    Settings,
}

/// The app icon (a ship's wheel around a VFO knob), embedded.
pub const ICON_PNG: &[u8] = include_bytes!("../../../assets/righelm-256.png");

/// Something that needs a second click to confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    PowerOff,
    RestoreMenu,
}

pub struct App {
    services: Services,
    tab: Tab,
    pub radios: Vec<DetectedRadio>,
    /// Radio and band scope USB devices with no working driver (Windows).
    pub driver_problems: Vec<detect::DriverProblem>,
    last_detect: Instant,
    pub status: Option<(String, Instant)>,
    pub confirm: Option<Confirm>,
    pub operate: operate::OperateState,
    pub menu: menu_tab::MenuState,
    pub memories: memories::MemoriesState,
    pub sharing: sharing::SharingState,
    pub console: console::ConsoleState,
    pub remote: remote_tab::RemoteState,
    pub cw_key: cw_key::CwKeyState,
    pub logo: egui::TextureHandle,
    pub waterfall: waterfall::Waterfall,
    pub scope: scope::BandScope,
    /// Self-test: render each tab to `<dir>/<tab>.ppm` and exit
    /// (`FTDX3000_SCREENSHOTS=dir`).
    screenshots: Option<(std::path::PathBuf, Instant, usize)>,
    /// The space bar is holding the transmitter keyed (push-to-talk).
    space_keyed: bool,
    /// The keyboard as the main window last saw it, and when a window last
    /// saw the space bar held (push-to-talk lets go if that goes stale).
    root_keys: Option<Keys>,
    space_seen: Instant,
    /// Scripted demo being recorded (`RIGHELM_DEMO`).
    demo: Option<demo::Demo>,
}

const SCREENSHOT_TABS: [(Tab, &str); 7] = [
    (Tab::Operate, "operate"),
    (Tab::Menu, "menu"),
    (Tab::Memories, "memories"),
    (Tab::Sharing, "sharing"),
    (Tab::Remote, "remote"),
    (Tab::Console, "console"),
    (Tab::Settings, "settings"),
];

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let mut config = Config::load();
        let detected = config.autodetect_port();
        let services = Services::start(config);
        apply_theme(&cc.egui_ctx, services.config.ui.theme);
        let ctx = cc.egui_ctx.clone();
        services.rig.set_repaint(move || ctx.request_repaint());
        if let Some(r) = &detected {
            services.rig.note(format!("auto-detected {}: CAT {} (USB {})", r.title(), r.cat.stable_path(), r.usb_path));
            if let Some(a) = &r.audio {
                services.rig.note(format!("radio audio: ALSA {} — {}", a.alsa_plughw(), a.name));
            }
            if !r.cat.users.is_empty() {
                let names: Vec<String> = r.cat.users.iter().map(|u| format!("{} (pid {})", u.name, u.pid)).collect();
                services.rig.note(format!("CAT port is in use by {}", names.join(", ")));
            }
        }
        let sharing = sharing::SharingState::new(&services.config);
        let logo = {
            let icon = eframe::icon_data::from_png_bytes(ICON_PNG).expect("embedded icon");
            let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            cc.egui_ctx.load_texture("logo", img, egui::TextureOptions::LINEAR)
        };
        App {
            services,
            tab: Tab::Operate,
            radios: detect::detect_radios(),
            driver_problems: detect::driver_problems(),
            last_detect: Instant::now(),
            status: None,
            confirm: None,
            operate: Default::default(),
            menu: Default::default(),
            memories: Default::default(),
            sharing,
            console: Default::default(),
            remote: Default::default(),
            cw_key: Default::default(),
            logo,
            waterfall: Default::default(),
            scope: Default::default(),
            screenshots: std::env::var_os("FTDX3000_SCREENSHOTS").map(|d| (d.into(), Instant::now(), 0)),
            space_keyed: false,
            root_keys: None,
            space_seen: Instant::now(),
            demo: demo::Demo::from_env(),
        }
    }

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let detached = scope::detached(self);
        let Some((dir, at, idx)) = &mut self.screenshots else { return };
        // Layout check: shoot the Operate page with the computer key open.
        if *idx == 0 && std::env::var_os("RIGHELM_CW_TEST").is_some() && self.services.cw().is_none() && at.elapsed() > Duration::from_secs(1) {
            self.services.cw_arm(true);
            return;
        }
        // Give the radio time to report its state before the first shot.
        let wait = if *idx == 0 { Duration::from_secs(4) } else { Duration::from_millis(900) };
        if *idx > 0 && *idx <= SCREENSHOT_TABS.len() {
            self.tab = SCREENSHOT_TABS[*idx - 1].0;
            if self.tab == Tab::Remote && std::env::var_os("RIGHELM_FIREWALL_TEST").is_some() {
                remote_tab::show_firewall_help(&mut self.remote);
            }
        }
        if at.elapsed() < wait {
            ctx.request_repaint();
            return;
        }
        let shoot = |path: std::path::PathBuf| {
            egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let mut out = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
                for p in &img.pixels {
                    out.extend_from_slice(&[p.r(), p.g(), p.b()]);
                }
                let _ = std::fs::write(&path, out);
            }))
        };
        if *idx >= 1 && *idx <= SCREENSHOT_TABS.len() {
            ctx.send_viewport_cmd(shoot(dir.join(format!("{}.ppm", SCREENSHOT_TABS[*idx - 1].1))));
        }
        if *idx == 1 && detached {
            ctx.send_viewport_cmd_to(scope::window_id(), shoot(dir.join("scope_window.ppm")));
        }
        if *idx > SCREENSHOT_TABS.len() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        *idx += 1;
        *at = Instant::now();
        ctx.request_repaint();
    }

    /// Push-to-talk: hold the space bar to transmit, let go to stop. Not while
    /// typing in a text field or with the CW key armed, and it lets go when
    /// the window loses focus (the key-up would never arrive). Called by each
    /// window (this one and the band scope's own) with its own context; the
    /// window that has the keyboard decides.
    pub fn space_ptt(&mut self, ctx: &egui::Context) {
        let enabled = self.services.config.ui.space_ptt && self.services.cw().is_none() && self.rig().link().is_open();
        let (space_held, focused) = ctx.input(|i| (i.key_down(egui::Key::Space), i.focused));
        let keys = Keys { space_held, focused, typing: ctx.egui_wants_keyboard_input(), at: Instant::now() };
        if ctx.viewport_id() == egui::ViewportId::ROOT {
            self.root_keys = Some(keys);
        } else {
            self.scope.window_keys = Some(keys);
        }
        // A window that hasn't drawn lately (hidden, or closed) doesn't count.
        let k = [self.scope.window_keys, self.root_keys].into_iter().flatten().find(|k| k.focused && k.at.elapsed() < PTT_STALE);
        let (held, focused, typing) = k.map_or((false, false, false), |k| (k.space_held, true, k.typing));
        if held && focused {
            self.space_seen = Instant::now();
        }
        let transmitting = self.rig().state().transmitting();
        if let Some(key) = ptt_step(enabled, typing, held, focused, self.space_keyed, transmitting) {
            self.rig().send(if key { "TX1;" } else { "TX0;" });
            self.space_keyed = key;
        }
        // The space bar must not also press whatever button has focus.
        if enabled && !keys.typing {
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        }
        if self.space_keyed {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    /// Runs before every frame, and also while every window is hidden (when
    /// no frame is drawn at all).
    fn logic(&mut self, ctx: &egui::Context) {
        self.services.tick();
        if !self.rig().link().is_open() {
            self.memories.link_down();
        }
        // Push-to-talk held but no window has seen the space bar lately (they
        // stopped drawing — covered or minimized): let go of the transmitter.
        if self.space_keyed && self.space_seen.elapsed() > PTT_STALE {
            self.rig().send("TX0;");
            self.space_keyed = false;
        }
        if self.space_keyed {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    pub fn rig(&self) -> &Rig {
        &self.services.rig
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// One row: the link light and name, the tabs, then the connection and
    /// power on the right (the link's description fills what's left).
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let link = self.rig().link();
        ui.horizontal(|ui| {
            let (dot, text) = match &link {
                LinkState::Connected { .. } if demo::active() => (GREEN, "USB · CAT 38400 bps".to_string()),
                LinkState::Connected { description } => (GREEN, description.clone()),
                LinkState::NoResponse { description } => (AMBER, format!("{description} — radio not answering")),
                LinkState::Connecting(s) => (AMBER, s.clone()),
                LinkState::Error(e) => (RED, e.clone()),
                LinkState::Disconnected => (Color32::GRAY, "disconnected".into()),
            };
            ui.label(RichText::new("●").color(ink(dot)).size(18.0)).on_hover_text(&text);
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(26.0, 26.0)));
            ui.label(RichText::new("RigHelm").strong().size(18.0).color(ink(AMBER))).on_hover_text("Rig control");
            if link.is_open() {
                ui.label(RichText::new(self.rig().model().name).strong().size(14.0)).on_hover_text(self.rig().model().full_name());
            }
            ui.add_space(8.0);
            for (tab, label) in [
                (Tab::Operate, "Operate"),
                (Tab::Menu, "Radio menu"),
                (Tab::Memories, "Memories"),
                (Tab::Sharing, "Ports & sharing"),
                (Tab::Remote, "Remote (LAN)"),
                (Tab::Console, "CAT console"),
                (Tab::Settings, "Settings"),
            ] {
                let r = ui.selectable_label(self.tab == tab, RichText::new(label).size(15.0));
                demo::mark(format!("tab:{label}"), r.rect);
                if r.clicked() {
                    self.tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.services.cw().is_some()
                    && ui
                        .add(egui::Button::new(RichText::new("● COMPUTER KEY").strong().color(Color32::WHITE)).fill(RED))
                        .on_hover_text("CW is keyed from this computer. Click to go back to the radio's key.")
                        .clicked()
                {
                    self.services.cw_arm(false);
                }
                if link.is_open() || matches!(link, LinkState::Error(_) | LinkState::Connecting(_)) {
                    if ui.button("Disconnect").clicked() {
                        self.rig().disconnect();
                    }
                } else if ui.button(RichText::new("Connect").strong()).clicked() {
                    if self.services.config.engine.serial.path.is_empty() {
                        self.services.config.autodetect_port();
                    }
                    self.services.connect();
                }
                if link.is_open() {
                    let on = self.rig().state().power_on != Some(false);
                    if on {
                        if self.confirm == Some(Confirm::PowerOff) {
                            if ui.button(RichText::new("Really power off?").color(ink(RED))).clicked() {
                                self.rig().send("PS0;");
                                self.confirm = None;
                            }
                        } else if ui.button("Power off").clicked() {
                            self.confirm = Some(Confirm::PowerOff);
                        }
                    } else if ui.button("Power on").clicked() {
                        self.rig().send("PS1;");
                    }
                } else if matches!(link, LinkState::NoResponse { .. }) && ui.button("Power on").clicked() {
                    self.rig().send("PS1;");
                }
                // A passing message takes the place of the link's description.
                let flash = self.status.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(6)).map(|(m, _)| m.clone());
                let label = match flash {
                    Some(msg) => egui::Label::new(RichText::new(msg).color(ink(AMBER))),
                    None if dot == GREEN => egui::Label::new(RichText::new(&text).weak()),
                    None => egui::Label::new(RichText::new(&text).color(ink(dot))),
                };
                ui.add(label.truncate()).on_hover_text(&text);
            });
        });
    }
}

impl App {
    fn ui(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let ctx = ui.ctx().clone();
        self.space_ptt(&ctx);
        if self.last_detect.elapsed() > Duration::from_secs(3) {
            self.radios = detect::detect_radios();
            self.driver_problems = detect::driver_problems();
            self.last_detect = Instant::now();
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Operate => operate::show(self, ui),
            Tab::Menu => menu_tab::show(self, ui),
            Tab::Memories => memories::show(self, ui),
            Tab::Sharing => sharing::show(self, ui),
            Tab::Remote => remote_tab::show(self, ui),
            Tab::Console => console::show(self, ui),
            Tab::Settings => settings_tab::show(self, ui),
        });
        // The band scope's own window, if it's popped out.
        scope::window(self, &ctx, shared);
        // Only watch the dongle while the scope is on screen.
        if self.tab != Tab::Operate && !scope::detached(self) {
            self.scope.pause();
        }
        if let Some(d) = &mut self.demo {
            d.frame(&ctx, &self.services.rig, &self.logo);
        }
        let ctx = ui.ctx().clone();
        self.screenshot_step(&ctx);
        // Meters keep moving even when nothing else changes.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }

    fn raw_input_hook(&mut self, raw_input: &mut egui::RawInput) {
        if let Some(d) = &mut self.demo {
            d.input(raw_input);
        }
    }

    fn on_exit(&mut self) {
        if let Err(e) = self.services.config.save() {
            log::warn!("could not save config: {e}");
        }
        self.services.shutdown();
    }
}

/// The app behind a lock. The band scope's own window is drawn by eframe on
/// its own schedule (so it keeps going while the main window is covered or
/// minimized), and reaches the app through this.
pub type Shared = Arc<Mutex<App>>;

struct Root(Shared);

impl eframe::App for Root {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        widgets::set_light(ctx.theme() == egui::Theme::Light);
        self.0.lock().logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let shared = self.0.clone();
        self.0.lock().ui(ui, &shared);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.0.lock().raw_input_hook(raw_input);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.0.lock().on_exit();
    }
}

/// One window's keyboard, for push-to-talk.
#[derive(Debug, Clone, Copy)]
pub struct Keys {
    pub space_held: bool,
    pub focused: bool,
    pub typing: bool,
    pub at: Instant,
}

/// How long a window's view of the keyboard counts for push-to-talk.
const PTT_STALE: Duration = Duration::from_millis(500);

/// Push-to-talk decision: `Some(true)` = key, `Some(false)` = un-key.
/// Starts only from a focused window with no text field being typed in, and
/// never takes over a transmission started otherwise (MOX).
fn ptt_step(enabled: bool, typing: bool, held: bool, focused: bool, keyed: bool, transmitting: bool) -> Option<bool> {
    let want = enabled && held && focused && (!typing || keyed);
    match (want, keyed) {
        (true, false) if !transmitting => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Dark, light, or whichever the system uses. Setting only the dark visuals
/// isn't enough: where the system reports light (Windows) egui switches to
/// its light style.
pub fn apply_theme(ctx: &egui::Context, theme: ftdx3000_rig::services::Theme) {
    use ftdx3000_rig::services::Theme;
    ctx.set_theme(match theme {
        Theme::Dark => egui::ThemePreference::Dark,
        Theme::Light => egui::ThemePreference::Light,
        Theme::System => egui::ThemePreference::System,
    });
}

fn setup_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.slider_width = 170.0;
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
}

/// A starting window size for layout checks: `FTDX3000_WINDOW=1366x700`.
fn window_size() -> Option<[f32; 2]> {
    let v = std::env::var("FTDX3000_WINDOW").ok()?;
    let (w, h) = v.split_once('x')?;
    Some([w.trim().parse().ok()?, h.trim().parse().ok()?])
}

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,righelm=info,ftdx3000_rig=info")).init();
    // A recorded demo draws as fast as it can at 1080p, without waiting for
    // the display (it may be a headless one).
    let demo = std::env::var_os("RIGHELM_DEMO").is_some();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RigHelm — Rig Control")
            .with_app_id("righelm")
            .with_icon(eframe::icon_data::from_png_bytes(ICON_PNG).expect("embedded icon"))
            .with_inner_size(if demo { [1920.0, 1080.0] } else { window_size().unwrap_or([1280.0, 860.0]) })
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    let mut options = options;
    if demo {
        options.wgpu_options.surface.present_mode = eframe::egui_wgpu::wgpu::PresentMode::AutoNoVsync;
    }
    // `RIGHELM_RENDERER=glow` skips wgpu outright.
    if std::env::var("RIGHELM_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("glow")) {
        options.renderer = eframe::Renderer::Glow;
    }
    let app_created = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let creator = |created: Arc<std::sync::atomic::AtomicBool>| -> eframe::AppCreator<'static> {
        Box::new(move |cc| {
            created.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(Root(Arc::new(Mutex::new(App::new(cc))))))
        })
    };
    let glow_options = eframe::NativeOptions { renderer: eframe::Renderer::Glow, ..options.clone() };
    match eframe::run_native("RigHelm", options, creator(app_created.clone())) {
        // Macs running a newer macOS than their GPU driver supports (OpenCore
        // Legacy Patcher) fail wgpu's Metal shader compile before the app is
        // created; OpenGL still works there.
        Err(eframe::Error::Wgpu(err)) if !app_created.load(std::sync::atomic::Ordering::Relaxed) => {
            log::warn!("wgpu renderer failed ({err}); falling back to OpenGL");
            eframe::run_native("RigHelm", glow_options, creator(app_created))
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_to_talk_decisions() {
        // enabled, typing, held, focused, keyed, transmitting
        assert_eq!(ptt_step(true, false, true, true, false, false), Some(true), "press keys");
        assert_eq!(ptt_step(true, false, true, true, true, true), None, "holding stays keyed");
        assert_eq!(ptt_step(true, false, false, true, true, true), Some(false), "release un-keys");
        assert_eq!(ptt_step(true, false, true, false, true, true), Some(false), "focus lost un-keys");
        assert_eq!(ptt_step(false, false, true, true, true, true), Some(false), "switched off un-keys");
        assert_eq!(ptt_step(true, true, true, true, false, false), None, "typing a space never keys");
        assert_eq!(ptt_step(true, true, true, true, true, true), None, "a text field taking focus keeps the key held");
        assert_eq!(ptt_step(true, false, true, true, false, true), None, "MOX already on: leave it");
        assert_eq!(ptt_step(true, false, false, true, false, true), None, "releasing never un-keys MOX");
        assert_eq!(ptt_step(false, false, true, true, false, false), None, "off");
    }

    fn space(pressed: bool) -> egui::RawInput {
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::Key { key: egui::Key::Space, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE });
        raw
    }

    /// With egui's real input handling: the held state survives consuming
    /// the key, and a focused button is not pressed by the space bar.
    #[test]
    fn space_is_held_and_does_not_click_buttons() {
        let ctx = egui::Context::default();
        let mut clicks = 0;
        let mut frame = |raw: egui::RawInput, consume: bool, clicks: &mut i32| -> bool {
            let mut held = false;
            let mut out = ctx.run_ui(raw, |ui| {
                if consume {
                    ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
                }
                held = ui.ctx().input(|i| i.key_down(egui::Key::Space));
                let r = ui.button("MOX");
                r.request_focus();
                *clicks += r.clicked() as i32;
            });
            // No painter here to take the font atlas.
            out.textures_delta.clear();
            held
        };
        frame(egui::RawInput::default(), true, &mut clicks); // focus the button
        assert!(frame(space(true), true, &mut clicks), "held after press");
        assert!(frame(egui::RawInput::default(), true, &mut clicks), "still held");
        assert!(!frame(space(false), true, &mut clicks), "released");
        assert_eq!(clicks, 0, "space must not press the focused button");
        // Control: without consuming, the space bar would press it.
        frame(space(true), false, &mut clicks);
        frame(space(false), false, &mut clicks);
        assert!(clicks > 0, "control case should click");
    }
}
