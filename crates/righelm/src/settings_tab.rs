//! Transmit audio, display preferences and about.

use eframe::egui::{self, Color32, RichText, Slider, Ui};
use ftdx3000_rig::audio::{self, DeviceEntry};
use ftdx3000_rig::txaudio;

use crate::App;
use crate::widgets::{self, AMBER, DIM, GREEN, RED, ink};

pub fn show(app: &mut App, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        tx_audio_panel(app, ui);
        ui.add_space(12.0);
        crate::cw_key::settings_panel(app, ui);
        ui.add_space(12.0);
        scope_panel(app, ui);
        ui.add_space(12.0);
        display(app, ui);
        ui.add_space(12.0);
        about(app, ui);
    });
}

pub const REFUSES_MIC: &str = "The remote radio's computer is not accepting microphone audio, so your voice is not transmitted. \
     Turn on \"Accept microphone audio for transmit\" on that computer (Remote (LAN) tab).";

/// One-click switch between the radio's own microphone and this computer's,
/// with the computer mic's device, gain and level.
pub fn tx_audio_panel(app: &mut App, ui: &mut Ui) {
    crate::remote_tab::refresh_nodes(app);
    let remote = app.services.config.engine.remote.clone();
    let on = app.services.config.tx_audio.computer_mic;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Transmit audio").strong().size(16.0).color(ink(AMBER)));
        ui.horizontal(|ui| {
            let choice = |text: &str, selected: bool| {
                let color = if selected { Color32::BLACK } else { Color32::LIGHT_GRAY };
                egui::Button::new(RichText::new(text).size(16.0).strong().color(color))
                    .fill(if selected { AMBER } else { Color32::from_rgb(40, 34, 20) })
                    .min_size(egui::vec2(240.0, 40.0))
            };
            if ui.add(choice("Radio's microphone", !on)).on_hover_text("Talk into the mic plugged into the radio (MIC SEL = FRONT)").clicked() && on {
                app.services.set_computer_mic(false);
            }
            if ui
                .add(choice("Computer microphone", on))
                .on_hover_text("Talk into this computer's microphone; it is sent to the radio over USB (MIC SEL = USB)")
                .clicked()
                && !on
            {
                app.services.set_computer_mic(true);
            }
        });
        let on = app.services.config.tx_audio.computer_mic;
        mic_sel_line(app, ui, on);
        if ui
            .checkbox(&mut app.services.config.ui.space_ptt, "Hold the space bar to transmit (push-to-talk)")
            .on_hover_text("Transmits while the space bar is held in RigHelm's window, except while typing in a text box or with the CW key armed")
            .changed()
        {
            let _ = app.services.config.save();
        }
        if on {
            mic_controls(app, ui, remote.is_some());
        }
        ui.label(
            RichText::new(
                "The computer mic goes to the radio while you transmit in SSB, AM or FM with MOX here, the radio's PTT or a footswitch — \
                 never when WSJT-X or another program keys the radio. Switching back restores the radio's mic settings.",
            )
            .weak(),
        );
    });
}

/// What the radio's MIC SEL menus say now, and whether that matches.
fn mic_sel_line(app: &App, ui: &mut Ui, computer: bool) {
    let rig = app.rig();
    if !rig.link().is_open() {
        ui.label(RichText::new("The radio is not connected: its mic setting will be changed when it connects.").color(DIM));
        return;
    }
    let model = rig.model();
    let roles = model.roles;
    if roles.mic_sel.is_empty() {
        ui.label(RichText::new(format!("RigHelm doesn't know which {} setting picks the USB audio input: set it on the radio.", model.name)).color(ink(AMBER)));
        return;
    }
    let mut parts = Vec::new();
    let mut matches = true;
    for &menu in roles.mic_sel {
        let v = txaudio::mic_sel(rig, menu);
        let item = model.menu_item(menu);
        let label = match (v, item) {
            (Some(v), Some(i)) => i.options().into_iter().find(|o| o.0 == v).map_or(v.to_string(), |o| o.1),
            _ => "…".to_string(),
        };
        let mode = item.map_or(String::new(), |i| i.group.trim_start_matches("MODE-").to_string());
        matches &= v.is_some_and(|v| (v == roles.mic_usb) == computer);
        parts.push(format!("{mode} {label}"));
    }
    let text = format!("Radio MIC SEL: {}", parts.join(" · "));
    ui.label(RichText::new(text).color(ink(if matches { GREEN } else { AMBER })));
}

fn mic_controls(app: &mut App, ui: &mut Ui, remote: bool) {
    // Never offer the radio's own codec as "the computer mic".
    let nodes: Vec<DeviceEntry> = app
        .remote
        .nodes
        .iter()
        .filter(|n| n.input && !audio::is_radio_codec(&n.device) && !audio::is_radio_codec(&audio::AudioDevice::Named(n.label.clone())))
        .cloned()
        .collect();
    let (level, sending, error, mic_name) = if remote {
        let st = app.services.audio_client_status();
        let s = st.unwrap_or_default();
        (s.mic_level_db, s.sending_mic, s.error, None)
    } else {
        let s = app.services.mic_status().unwrap_or_default();
        (s.level_db, s.sending, s.error, s.mic)
    };
    let refuses = remote && app.services.remote_refuses_mic();
    let mut changed = false;
    let tx = &mut app.services.config.tx_audio;
    egui::Grid::new("txmic").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Microphone");
        ui.horizontal(|ui| {
            changed |= crate::remote_tab::device_combo(ui, "txmic", &mut tx.mic, &nodes, true);
            if let Some(n) = &mic_name {
                ui.label(RichText::new(format!("using {n}")).weak());
            }
        });
        ui.end_row();
        ui.label("Mic gain");
        changed |= ui.add(Slider::new(&mut tx.mic_gain, 0.0..=4.0).step_by(0.05)).changed();
        ui.end_row();
        ui.label("Level");
        ui.horizontal(|ui| {
            let frac = ((level + 60.0) / 60.0).clamp(0.0, 1.0);
            let color = if refuses { AMBER } else { GREEN };
            widgets::Meter { label: "MIC", fraction: frac, text: format!("{level:.0} dB"), color, ticks: &[], red_from: Some(0.9), peak: None }
                .show(ui, 260.0);
            if sending && refuses {
                ui.label(RichText::new("✖ NOT SENT").color(ink(AMBER)).strong());
            } else if sending {
                ui.label(RichText::new("● ON AIR").color(ink(RED)).strong());
            }
        });
        ui.end_row();
    });
    if let Some(e) = error {
        ui.label(RichText::new(e).color(ink(AMBER)));
    }
    if refuses {
        ui.label(
            RichText::new(format!("⚠ {REFUSES_MIC}")).color(ink(AMBER)).strong(),
        );
    }
    if changed {
        app.services.apply_tx_audio();
        let _ = app.services.config.save();
    }
}

/// The RTL-SDR band scope: which jack the dongle is on, and calibration.
fn scope_panel(app: &mut App, ui: &mut Ui) {
    use ftdx3000_rig::scope::{DEFAULT_IF_HZ, Feed};
    let status = app.services.scope_status();
    let remote = app.services.config.engine.remote.is_some();
    let before = app.services.config.scope.clone();
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Band scope (RTL-SDR)").strong().size(16.0).color(ink(AMBER)));
        let color = if status.running { GREEN } else if status.present { AMBER } else { DIM };
        ui.label(RichText::new(if status.text.is_empty() { "starting…" } else { &status.text }).color(ink(color)));
        if remote {
            ui.label(RichText::new("The band scope comes from the dongle on the radio's computer; it is set up there.").color(DIM));
            return;
        }
        let cfg = &mut app.services.config.scope;
        ui.checkbox(&mut cfg.enabled, "Use an RTL-SDR dongle when one is plugged in");
        if status.dongles.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Dongle");
                let shown = status.dongles.iter().find(|(serial, _)| *serial == cfg.serial).map_or("first one found", |(_, d)| d.as_str()).to_string();
                egui::ComboBox::from_id_salt("scope-dongle").selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut cfg.serial, String::new(), "first one found");
                    for (serial, desc) in &status.dongles {
                        ui.selectable_value(&mut cfg.serial, serial.clone(), desc);
                    }
                });
            });
        }
        ui.horizontal(|ui| {
            ui.label("Connected to the radio's");
            ui.selectable_value(&mut cfg.feed, Feed::IfOut, "IF OUT (9 MHz)")
                .on_hover_text("The radio's first IF. The picture follows the VFO and the radio's own front end filters it.");
            ui.selectable_value(&mut cfg.feed, Feed::RxOut, "RX OUT (antenna)")
                .on_hover_text("The receive antenna signal. The dongle tunes itself to show the whole band (or around the VFO).");
        });
        if cfg.feed == Feed::IfOut {
            ui.horizontal(|ui| {
                ui.label("IF frequency");
                ui.add(egui::DragValue::new(&mut cfg.if_hz).range(DEFAULT_IF_HZ - 50_000..=DEFAULT_IF_HZ + 50_000).speed(10).suffix(" Hz"));
                if ui.small_button("reset").clicked() {
                    cfg.if_hz = DEFAULT_IF_HZ;
                }
                ui.checkbox(&mut cfg.if_inverted, "Flipped")
                    .on_hover_text("Higher frequencies come out lower in the IF. Untick if signals move the wrong way when you tune.");
            });
            ui.label(
                RichText::new(
                    "If signals move the wrong way when you tune, change Flipped. If they sit a little off the dial line, use Calibrate now, or nudge the IF frequency.",
                )
                .color(DIM),
            );
        }

        // Calibration: automatic from broadcast and time-signal carriers, or by hand.
        ui.add_space(4.0);
        ui.label(RichText::new("Frequency calibration").strong());
        ui.horizontal(|ui| {
            let now = if cfg.feed == Feed::IfOut {
                format!("IF frequency {} Hz", cfg.if_hz)
            } else {
                format!("dongle correction {:+.2} ppm", cfg.ppm as f64 + cfg.ppm_fine)
            };
            let state = if cfg.calibrated { "calibrated" } else { "not calibrated yet" };
            ui.label(format!("{now} · {state}"));
            let calibrate = ui.add_enabled(status.present, egui::Button::new("Calibrate now"));
            crate::demo::mark("calibrate", calibrate.rect);
            if calibrate
                .on_hover_text("Measures shortwave broadcast stations and time signals (WWV, CHU) in view, whose frequencies are exact, and corrects the scope to match. Takes a few seconds.")
                .on_disabled_hover_text("Plug in the RTL-SDR dongle first.")
                .clicked()
            {
                app.services.scope_tap.request_calibration();
            }
            if ui.button("Reset").on_hover_text("Forget the calibration and start over (it calibrates itself again).").clicked() {
                (cfg.ppm, cfg.ppm_fine, cfg.if_hz, cfg.calibrated) = (0, 0.0, DEFAULT_IF_HZ, false);
            }
        });
        if let Some(msg) = &app.services.scope_calibration {
            ui.label(RichText::new(format!("Last calibration: {msg}")).color(DIM));
        }
        ui.horizontal(|ui| {
            ui.label("Dongle correction by hand");
            let r = ui.add(egui::DragValue::new(&mut cfg.ppm).range(-200..=200).suffix(" ppm"));
            if r.changed() {
                // Taken over by hand: no fine part, no automatic correction.
                cfg.ppm_fine = 0.0;
                cfg.calibrated = true;
            }
        });
        ui.label(
            RichText::new(
                "It calibrates itself the first time broadcast stations are in view (e.g. 40 m at night next to the 41 m broadcast band, or 20 m by day). \
                 To check it: hover over a broadcast station's carrier, which sits exactly on a 5 kHz step (7.335.000), or WWV on 10.000.000.",
            )
            .color(DIM),
        );
    });
    if app.services.config.scope != before {
        if app.services.config.scope.feed != before.feed {
            app.services.config.scope.feed_confirmed = true;
        }
        app.services.apply_scope();
        let _ = app.services.config.save();
    }
}

fn display(app: &mut App, ui: &mut Ui) {
    let before = app.services.config.ui.clone();
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Display").strong().size(16.0).color(ink(AMBER)));
        let prefs = &mut app.services.config.ui;
        ui.horizontal(|ui| {
            use ftdx3000_rig::services::Theme;
            ui.label("Theme");
            ui.selectable_value(&mut prefs.theme, Theme::System, "Follow the system");
            ui.selectable_value(&mut prefs.theme, Theme::Dark, "Dark");
            ui.selectable_value(&mut prefs.theme, Theme::Light, "Light");
        });
        ui.checkbox(&mut prefs.show_waterfall, "Show the audio waterfall on the Operate page");
        ui.horizontal(|ui| {
            ui.label("Audio waterfall height under the band scope");
            ui.add(egui::Slider::new(&mut prefs.wf_height, 80..=600).suffix(" px"));
        });
        ui.checkbox(&mut prefs.show_scope, "Show the band scope on the Operate page (when an RTL-SDR dongle is found)");
        ui.checkbox(&mut prefs.scope_window, "Band scope in a window of its own (to make it big, or put it on another monitor)");
        ui.checkbox(&mut prefs.scope_band_plan, "Band scope: show the US band plan (CW/data and phone) and FT8/FT4 calling frequencies");
        ui.checkbox(&mut prefs.scope_band_span, "Band scope: add a Band span that shows the whole band")
            .on_hover_text("Fits the band the dial is in, whichever part you are tuned to. Outside the amateur bands, and on the other spans, the scope follows the dial as usual.");
        ui.label(
            RichText::new(
                "The band scope and the audio waterfall fill the middle of the Operate page; drag the line between them to share it out. \
                 The control columns on either side can be made wider or narrower by dragging their edges.",
            )
            .weak(),
        );
        ui.checkbox(&mut prefs.show_macros, "Show the macro buttons on the Operate page");
        ui.checkbox(&mut prefs.show_rotator, "Show rotator control (Yaesu G-800DXA/1000DXA/2800DXA on the radio's ROT jack)");
    });
    if app.services.config.ui != before {
        if app.services.config.ui.theme != before.theme {
            crate::apply_theme(ui.ctx(), app.services.config.ui.theme);
        }
        let _ = app.services.config.save();
    }
}

fn about(app: &App, ui: &mut Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&app.logo).fit_to_exact_size(egui::vec2(96.0, 96.0)));
            ui.vertical(|ui| {
                ui.label(RichText::new("RigHelm").strong().size(28.0).color(ink(AMBER)));
                ui.label(RichText::new(format!("The Linux alternative to Win4Yaesu for the Yaesu FTDX3000 — and other Yaesu, Icom, Kenwood and Elecraft radios · version {}", env!("CARGO_PKG_VERSION"))).size(14.0));
                ui.label(RichText::new("At the helm of your rig: control, sharing and remote operation.").color(DIM));
                ui.label(RichText::new(format!("Settings file: {}", ftdx3000_rig::Config::path().display())).color(DIM));
            });
        });
    });
}
