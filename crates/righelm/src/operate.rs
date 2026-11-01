//! The main operating panel.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ComboBox, RichText, Slider, Ui};
use ftdx3000_cat::types::{self, Meter, Mode, WidthClass, interpolate};
use ftdx3000_cat::{RadioState, set_freq_cmd_for, set_mode_cmd};
use ftdx3000_rig::Rig;

use crate::App;
use crate::widgets::{self, AMBER, CYAN, DIM, FreqAction, GREEN, RED, button, ink, led_button};

pub struct OperateState {
    /// Frequency being typed: (vfo_b, text).
    freq_edit: Option<(bool, String)>,
    peaks: std::collections::HashMap<Meter, (f32, Instant)>,
    /// Where each bar is drawn, chasing the reading: (fraction, drawn at).
    needles: std::collections::HashMap<Meter, (f32, Instant)>,
    keyer_text: [String; 5],
    keyer_loaded: [bool; 5],
    /// The tuner's power meter shows 1 kW full scale until then (an
    /// amplifier is in line).
    tuner_kw_until: Option<Instant>,
    /// The tuner's meters, held like the radio's: the power bar (watts,
    /// drawn at), its peak (watts, since), and the last SWR (since).
    tuner_needle: (f32, Instant),
    tuner_peak: (f32, Instant),
    tuner_swr: (f32, Instant),
}

impl Default for OperateState {
    fn default() -> Self {
        Self {
            freq_edit: None,
            peaks: Default::default(),
            needles: Default::default(),
            keyer_text: Default::default(),
            keyer_loaded: [false; 5],
            tuner_kw_until: None,
            tuner_needle: (0.0, Instant::now()),
            tuner_peak: (0.0, Instant::now()),
            tuner_swr: (0.0, Instant::now()),
        }
    }
}

/// The transmit-audio switch, where the operator looks: one click between
/// the radio's own microphone and this computer's.
fn mic_button(app: &mut App, ui: &mut Ui, size: egui::Vec2) {
    let computer = app.services.config.tx_audio.computer_mic;
    let text = if computer { "COMPUTER MIC" } else { "RADIO MIC" };
    let button = egui::Button::new(RichText::new(text).strong().size(15.0).color(if computer { Color32::BLACK } else { Color32::LIGHT_GRAY }))
        .min_size(size)
        .fill(if computer { AMBER } else { Color32::from_rgb(40, 34, 20) });
    let hover = if computer {
        "Talking into this computer's microphone. Click to go back to the radio's own mic."
    } else {
        "Talking into the radio's own mic. Click to use this computer's microphone."
    };
    if ui.add(button).on_hover_text(hover).clicked() {
        app.services.set_computer_mic(!computer);
    }
}

/// One line under the MIC and KEY switches, always there (so nothing moves
/// when it fills in): the computer mic's level and whether it's on the air,
/// or why the computer key let go.
fn tx_status_line(app: &mut App, ui: &mut Ui) {
    let computer = app.services.config.tx_audio.computer_mic;
    let (sending, level, error) = if app.services.config.engine.remote.is_some() {
        let st = app.services.audio_client_status().unwrap_or_default();
        (st.sending_mic, st.mic_level_db, st.error)
    } else {
        let st = app.services.mic_status().unwrap_or_default();
        (st.sending, st.level_db, st.error)
    };
    let refuses = app.services.remote_refuses_mic();
    let key_note = crate::cw_key::disarm_note(app);
    ui.horizontal(|ui| {
        ui.set_height(16.0);
        if computer {
            ui.label(RichText::new("mic").weak().size(11.0));
            let frac = ((level + 60.0) / 60.0).clamp(0.0, 1.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(80.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(30, 32, 38));
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height()));
            ui.painter().rect_filled(fill, 2.0, if refuses { AMBER } else if frac > 0.9 { RED } else { GREEN });
            if refuses {
                let text = if sending { "✖ NOT SENT: station refuses mic" } else { "⚠ station refuses mic" };
                ui.add(egui::Label::new(RichText::new(text).color(ink(AMBER)).strong()).truncate()).on_hover_text(crate::settings_tab::REFUSES_MIC);
            } else if sending {
                ui.label(RichText::new("● ON AIR").color(ink(RED)).strong());
            }
            if let Some(e) = error {
                ui.label(RichText::new("⚠").color(ink(AMBER))).on_hover_text(e);
            }
        }
        if let Some(note) = key_note {
            ui.add(egui::Label::new(RichText::new(&note).color(ink(AMBER))).truncate()).on_hover_text(note);
        }
    });
}

const STEPS: [(i64, &str); 6] = [(1, "1 Hz"), (10, "10 Hz"), (100, "100 Hz"), (1000, "1 kHz"), (5000, "5 kHz"), (10_000, "10 kHz")];

/// The groups of knobs and switches, placed wherever the window has room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Filter,
    Receiver,
    Dsp,
    Transmitter,
    Tuner,
    Clarifier,
    Cw,
    Macros,
    Rotator,
}

/// Receive-side groups (left of the scope), then transmit-side (right).
const LEFT: [Group; 3] = [Group::Filter, Group::Receiver, Group::Dsp];
const RIGHT: [Group; 6] = [Group::Tuner, Group::Transmitter, Group::Clarifier, Group::Cw, Group::Macros, Group::Rotator];

/// Narrowest a column of groups gets.
const COLUMN_MIN: f32 = 270.0;

fn draw_group(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState, g: Group) {
    match g {
        Group::Filter => filter_group(ui, rig, s),
        Group::Receiver => receiver_group(ui, rig, s),
        Group::Dsp => dsp_group(ui, rig, s),
        Group::Transmitter => transmitter_group(ui, rig, s),
        Group::Tuner => tuner_group(ui, app, s),
        Group::Clarifier => clarifier_group(ui, rig, s),
        Group::Cw => cw_group(ui, &mut app.operate, rig, s),
        Group::Macros => macro_group(ui, app, rig),
        Group::Rotator => rotator_group(ui, rig, s),
    }
}

/// Groups in a column that scrolls on its own; side by side when it is
/// wide enough for more than one.
fn stack(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState, id: &str, groups: &[Group]) {
    let prefs = &app.services.config.ui;
    let tuner = app.services.tuner.is_some();
    let groups: Vec<Group> = groups
        .iter()
        .copied()
        .filter(|g| match g {
            Group::Macros => prefs.show_macros,
            Group::Rotator => prefs.show_rotator && s.model.has("RO"),
            Group::Tuner => tuner,
            Group::Clarifier => s.model.has("RT") || s.model.has("XT"),
            Group::Cw => s.model.has("KS") || s.model.has("KR"),
            _ => true,
        })
        .collect();
    egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let cols = (((ui.available_width() + 6.0) / (COLUMN_MIN + 6.0)) as usize).clamp(1, groups.len().max(1));
        if cols == 1 {
            for g in groups {
                draw_group(app, ui, rig, s, g);
                ui.add_space(4.0);
            }
        } else {
            ui.columns(cols, |c| {
                for (i, g) in groups.into_iter().enumerate() {
                    draw_group(app, &mut c[i % cols], rig, s, g);
                    c[i % cols].add_space(4.0);
                }
            });
        }
    });
}

/// The Operate page, laid out to fit a laptop screen without scrolling the
/// page: the radio across the top (VFOs, meters, transmit, bands and
/// modes), the band scope and audio waterfall in the middle taking all the
/// height that's left, and the knobs in a column either side — receive on
/// the left, transmit on the right — each scrolling by itself if it must.
pub fn show(app: &mut App, ui: &mut Ui) {
    let rig = app.rig().clone();
    let s = rig.state().clone();
    let live = rig.link().is_open() && s.power_on != Some(false);
    if live && !ui.ctx().egui_wants_keyboard_input() {
        keyboard(ui, &rig, &s, app.services.config.ui.tune_step);
    }
    if !live {
        ui.label(RichText::new("Not connected to the radio. Use Connect (top right) or check Ports & sharing.").color(ink(AMBER)));
        for p in app.driver_problems.iter().filter(|p| p.what == ftdx3000_rig::detect::DriverFor::Radio) {
            ui.label(RichText::new(p.text()).color(ink(RED)));
        }
    }
    ui.add_enabled_ui(live, |ui| {
        egui::Panel::top("op_radio")
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { bottom: 6, ..Default::default() }))
            .show(ui, |ui| radio_strip(app, ui, &rig, &s, live));
        let detached = crate::scope::detached(app);
        let middle = (crate::scope::visible(app) && !detached) || app.services.config.ui.show_waterfall || app.services.cw().is_some();
        if !middle {
            if detached {
                scope_away_note(app, ui);
            }
            // Nothing to show in the middle: the groups take the whole width.
            let all: Vec<Group> = LEFT.iter().chain(RIGHT.iter()).copied().collect();
            stack(app, ui, &rig, &s, "op_all", &all);
            crate::scope::show(app, ui, &rig, &s);
            crate::waterfall::show(app, ui, &rig, &s);
            return;
        }
        let full = ui.available_width();
        let max_side = (full * 0.4).max(COLUMN_MIN);
        // Room for a column either side and a scope at least as wide as both.
        let two_sides = full >= 4.0 * COLUMN_MIN;
        let side = |ui: &mut Ui, app: &mut App, left: bool, groups: &[Group]| {
            let prefs = &app.services.config.ui;
            let (panel, w) = if left {
                (egui::Panel::left("op_left"), prefs.operate_left_w)
            } else {
                (egui::Panel::right("op_right"), prefs.operate_right_w)
            };
            let r = panel
                .resizable(true)
                .default_size(w as f32)
                .size_range(COLUMN_MIN..=max_side)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: if left { 0 } else { 6 }, right: if left { 6 } else { 0 }, ..Default::default() }))
                .show(ui, |ui| stack(app, ui, &rig, &s, if left { "op_left" } else { "op_right" }, groups));
            let w = r.response.rect.width().round() as u32;
            let prefs = &mut app.services.config.ui;
            if left { prefs.operate_left_w = w } else { prefs.operate_right_w = w }
        };
        if two_sides {
            side(ui, app, true, &LEFT);
            side(ui, app, false, &RIGHT);
        } else {
            let all: Vec<Group> = LEFT.iter().chain(RIGHT.iter()).copied().collect();
            side(ui, app, false, &all);
        }
        middle_column(app, ui, &rig, &s);
    });
}

/// A line saying where the band scope went, with a way back.
fn scope_away_note(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("BAND SCOPE").strong().color(ink(AMBER)));
        ui.label(RichText::new("is in its own window").weak());
        if ui.small_button("⤒ Put back").on_hover_text("Put the band scope back here").clicked() {
            app.services.config.ui.scope_window = false;
        }
    });
}

/// The band scope over the audio waterfall, sharing the height between them
/// at a line that can be dragged; the computer key's pad under them.
fn middle_column(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState) {
    if crate::scope::detached(app) {
        scope_away_note(app, ui);
    }
    if app.services.cw().is_some() {
        egui::Panel::bottom("op_cw_pad")
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { top: 4, ..Default::default() }))
            .show(ui, |ui| crate::cw_key::panel(app, ui));
    }
    let scope = crate::scope::visible(app) && !crate::scope::detached(app);
    let audio = app.services.config.ui.show_waterfall;
    let w = ui.available_width();
    let fill = |ui: &mut Ui, h: f32, f: &mut dyn FnMut(&mut Ui)| {
        ui.allocate_ui(egui::vec2(w, h.max(0.0)), |ui| {
            ui.set_min_height(h.max(0.0));
            f(ui)
        });
    };
    if scope && audio {
        const GRIP: f32 = 8.0;
        let avail = ui.available_height();
        let wf_h = (app.services.config.ui.wf_height as f32).clamp(80.0, (avail - 200.0).max(80.0));
        fill(ui, avail - wf_h - GRIP, &mut |ui| crate::scope::show(app, ui, rig, s));
        let (rect, grip) = ui.allocate_exact_size(egui::vec2(w, GRIP), egui::Sense::drag());
        let color = if grip.hovered() || grip.dragged() { Color32::GRAY } else { Color32::from_gray(60) };
        ui.painter().line_segment([rect.center() - egui::vec2(20.0, 0.0), rect.center() + egui::vec2(20.0, 0.0)], egui::Stroke::new(3.0, color));
        let grip = grip.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to share the height between the band scope and the audio waterfall");
        if grip.dragged() {
            let h = (wf_h - grip.drag_delta().y).clamp(80.0, 600.0);
            app.services.config.ui.wf_height = h.round() as u32;
        }
        fill(ui, ui.available_height(), &mut |ui| crate::waterfall::show(app, ui, rig, s));
    } else {
        fill(ui, ui.available_height(), &mut |ui| {
            crate::scope::show(app, ui, rig, s);
            crate::waterfall::show(app, ui, rig, s);
        });
    }
}

/// Across the top: the VFOs, the meters, the transmit switches, and the
/// band and mode keys. All in one row when the window is wide enough; on a
/// narrower one the bands, then the transmit switches, go to a second row.
fn radio_strip(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState, live: bool) {
    const GAP: f32 = 14.0;
    const TX_W: f32 = 300.0;
    const BANDS_W: f32 = 330.0;
    const METERS_MIN: f32 = 280.0;
    const METERS_MAX: f32 = 440.0;
    // The VFO block's width from the last frame (it depends on the fonts).
    let vfo_id = ui.scope_id().with("vfo_w");
    let vfo_w: f32 = ui.data(|d| d.get_temp(vfo_id)).unwrap_or(350.0);
    let full = ui.available_width();
    // A gap as laid out: the space plus the item spacing either side of it.
    let gap = GAP + 2.0 * ui.spacing().item_spacing.x;
    let fixed = vfo_w + TX_W + 2.0 * gap;
    let (tx_beside, bands_beside) = if full - fixed - BANDS_W - gap >= METERS_MIN {
        (true, true)
    } else if full - fixed >= METERS_MIN {
        (true, false)
    } else {
        (false, false)
    };
    let meters_w = (full - vfo_w - gap - if tx_beside { TX_W + gap } else { 0.0 } - if bands_beside { BANDS_W + gap } else { 0.0 }).clamp(METERS_MIN, METERS_MAX);
    ui.horizontal_top(|ui| {
        let w = ui.vertical(|ui| vfo_block(app, ui, rig, s, live)).response.rect.width();
        ui.data_mut(|d| d.insert_temp(vfo_id, w));
        ui.add_space(GAP);
        ui.vertical(|ui| {
            ui.set_width(meters_w);
            meters_block(app, ui, s, meters_w);
        });
        if tx_beside {
            ui.add_space(GAP);
            ui.vertical(|ui| {
                ui.set_width(TX_W);
                tx_block(app, ui, rig, s);
            });
        }
        if bands_beside {
            ui.add_space(GAP);
            ui.vertical(|ui| bands_and_modes(ui, rig, s));
        }
    });
    if !bands_beside {
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            if !tx_beside {
                ui.vertical(|ui| {
                    ui.set_width(TX_W);
                    tx_block(app, ui, rig, s);
                });
                ui.add_space(GAP);
            }
            ui.vertical(|ui| bands_and_modes(ui, rig, s));
        });
    }
}

/// Arrow keys tune VFO-A by the step; PageUp/PageDown change band.
pub(crate) fn keyboard(ui: &mut Ui, rig: &Rig, s: &RadioState, step: i64) {
    let (up, down, pgup, pgdn) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight),
            i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft),
            i.key_pressed(egui::Key::PageUp),
            i.key_pressed(egui::Key::PageDown),
        )
    });
    let tune = |d: i64| {
        let hz = (s.freq_a as i64 + d).max(0) as u64;
        rig.send(set_freq_cmd_for(s.model, false, hz));
    };
    if up {
        tune(step);
    }
    if down {
        tune(-step);
    }
    if pgup && s.model.has("BU") {
        rig.send(format!("{}BU0;", vfo_first(s)));
    }
    if pgdn && s.model.has("BD") {
        rig.send(format!("{}BD0;", vfo_first(s)));
    }
}

fn badge(ui: &mut Ui, text: &str, on: bool, color: Color32) {
    let t = RichText::new(text).strong().size(12.0);
    if on {
        ui.label(t.color(Color32::BLACK).background_color(color));
    } else {
        ui.label(t.color(DIM));
    }
}

fn vfo_line(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState, b: bool, live: bool) {
    let (freq, mode) = if b { (s.freq_b, s.mode_b) } else { (s.freq_a, s.mode_a) };
    let rx = s.rx_on_b() == b;
    let tx = (s.tx_vfo == 1) == b;
    let heading = |ui: &mut Ui| {
        ui.label(RichText::new(if b { "VFO-B" } else { "VFO-A" }).strong().color(ink(if b { CYAN } else { AMBER })));
        badge(ui, "RX", rx, GREEN);
        badge(ui, "TX", tx, if s.transmitting() && tx { RED } else { AMBER });
        if let Some(m) = mode {
            ui.label(RichText::new(m.label()).strong().size(15.0));
        }
        if let Some(band) = s.model.band_of(freq) {
            // "20" is metres; "70cm" says its own unit.
            let unit = if band.label.bytes().all(|c| c.is_ascii_digit()) { " m" } else { "" };
            ui.label(RichText::new(format!("{}{unit}", band.label)).weak());
        }
        if !b {
            if s.info_a.mem_mode != 0 {
                ui.label(RichText::new(format!("MEM {:03}", s.info_a.channel)).color(ink(CYAN)));
                // A 60 m memory shows the channel centre even in USB.
                if let Some((c, cw)) = sixty_m_memory(s) {
                    let text = if cw { format!("{} · CW on centre", c.name.trim()) } else { format!("{} · centre; USB dial {:.1}", c.name.trim(), c.dial as f64 / 1000.0) };
                    ui.label(RichText::new(text).color(ink(CYAN))).on_hover_text("60 m memories show the channel centre; a USB carrier is 1.5 kHz below it");
                }
            }
            if s.lock & 1 == 1 {
                ui.label(RichText::new("LOCK").color(ink(RED)));
            }
        }
    };
    let step = app.services.config.ui.tune_step;
    let op = &mut app.operate;
    let size = if b { 24.0 } else { 46.0 };
    // VFO-B on one line: heading, then the frequency.
    ui.horizontal(|ui| {
        heading(ui);
        if !b {
            return;
        }
        ui.add_space(4.0);
        freq_or_edit(ui, rig, op, s, b, freq, size, step, live);
    });
    if !b {
        freq_or_edit(ui, rig, op, s, b, freq, size, step, live);
    }
}

/// The frequency readout, or the box for typing one.
#[allow(clippy::too_many_arguments)]
fn freq_or_edit(ui: &mut Ui, rig: &Rig, op: &mut OperateState, s: &RadioState, b: bool, freq: u64, size: f32, step: i64, live: bool) {
    if let Some((eb, text)) = &mut op.freq_edit {
        if *eb == b {
            let mut done = None;
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(if b { 110.0 } else { 200.0 }).font(egui::FontId::monospace(if b { 18.0 } else { 30.0 })));
                r.request_focus();
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) || ui.button("Cancel").clicked() {
                    done = Some(false);
                }
                if !b {
                    ui.label(RichText::new("e.g. 14.074, 7074.5, 3573k").weak());
                }
            });
            match done {
                Some(true) => {
                    if let Some(hz) = widgets::parse_freq(text) {
                        if s.model.in_range(hz) {
                            rig.send(set_freq_cmd_for(s.model, b, hz));
                        }
                    }
                    op.freq_edit = None;
                }
                Some(false) => op.freq_edit = None,
                None => {}
            }
            return;
        }
    }
    match widgets::freq_display(ui, freq, size, if b { CYAN } else { AMBER }, step, live && !(s.lock & if b { 2 } else { 1 } != 0)) {
        FreqAction::Step(d) => {
            let hz = (freq as i64 + d).max(0) as u64;
            rig.send(set_freq_cmd_for(s.model, b, hz));
        }
        FreqAction::Edit => op.freq_edit = Some((b, String::new())),
        FreqAction::None => {}
    }
}

fn vfo_block(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState, live: bool) {
    ui.spacing_mut().item_spacing.y = 4.0;
    let m = s.model;
    vfo_line(app, ui, rig, s, false, live);
    if m.has("FB") {
        vfo_line(app, ui, rig, s, true, live);
    }
    ui.horizontal(|ui| {
        if m.has("AB") && button(ui, "A ▸ B") {
            rig.send("AB;");
        }
        if m.has("BA") && button(ui, "B ▸ A") {
            rig.send("BA;");
        }
        if m.has("SV") && button(ui, "A ⇄ B") {
            rig.send("SV;");
        }
        let split = s.split();
        if m.has("FT") && led_button(ui, split, "SPLIT", AMBER) {
            // Transmit on the other VFO than the receiver (or back on the same one).
            let tx_b = if split { s.rx_on_b() } else { !s.rx_on_b() };
            rig.send(if tx_b { "FT3;" } else { "FT2;" });
        }
        if m.has("QS") && button(ui, "Q.SPLIT") {
            rig.send("QS;");
        }
    });
    ui.horizontal(|ui| {
        if m.has("FR") {
            ui.label("RX on");
            if ui.selectable_label(!s.rx_on_b(), "A").clicked() {
                rig.send("FR0;");
            }
            if ui.selectable_label(s.rx_on_b(), "B").clicked() {
                rig.send("FR4;");
            }
            ui.separator();
        }
        if m.has("LK") && led_button(ui, s.lock & 1 == 1, "LOCK", RED) {
            rig.send(if s.lock & 1 == 1 { "LK0;" } else { "LK1;" });
        }
        if m.has("FS") && led_button(ui, s.fast & 1 == 1, "FAST", AMBER) {
            rig.send(if s.fast & 1 == 1 { "FS0;" } else { "FS1;" });
        }
        if m.has("LK") || m.has("FS") {
            ui.separator();
        }
        ui.label("Step");
        let step = &mut app.services.config.ui.tune_step;
        ComboBox::from_id_salt("step")
            .width(64.0)
            .selected_text(STEPS.iter().find(|x| x.0 == *step).map(|x| x.1).unwrap_or("?"))
            .show_ui(ui, |ui| {
                for (v, l) in STEPS {
                    ui.selectable_value(step, v, l);
                }
            });
    });
}

/// Every meter, always shown (blank when it doesn't apply), so nothing near
/// them moves when the radio goes between receive and transmit.
fn meters_block(app: &mut App, ui: &mut Ui, s: &RadioState, w: f32) {
    ui.spacing_mut().item_spacing.y = 3.0;
    let tx = s.transmitting();
    let now = Instant::now();
    let mut peak = |m: Meter, f: f32| -> f32 {
        let e = app.operate.peaks.entry(m).or_insert((0.0, now));
        if f >= e.0 || now.duration_since(e.1) > Duration::from_millis(1500) {
            *e = (f, now);
        }
        e.0
    };
    // Bars rise quickly and fall back slowly, like the radio's own meter,
    // instead of jumping from one reading to the next.
    let mut moving = false;
    let mut needle = |m: Meter, f: f32| -> f32 {
        let e = app.operate.needles.entry(m).or_insert((f, now));
        let dt = now.duration_since(e.1).as_secs_f32();
        let tau = if f > e.0 { 0.03 } else { 0.3 };
        e.0 += (f - e.0) * (1.0 - (-dt / tau).exp());
        e.1 = now;
        moving |= (f - e.0).abs() > 0.002;
        e.0
    };
    const CAL: &str = "Meter calibration is approximate";
    {
        let raw = if tx { 0 } else { s.s_meter };
        let f = raw as f32 / 255.0;
        let pk = peak(Meter::S, f);
        widgets::Meter {
            label: "S",
            fraction: needle(Meter::S, f),
            text: if tx { "—".into() } else { types::s_meter_text(raw) },
            color: GREEN,
            ticks: &[(0.0, "0"), (40.0 / 255.0, "3"), (80.0 / 255.0, "6"), (130.0 / 255.0, "9"), (172.0 / 255.0, "+20"), (220.0 / 255.0, "+40"), (1.0, "+60")],
            red_from: Some(130.0 / 255.0),
            peak: (!tx).then_some(pk),
        }
        .show_sized(ui, w, 32.0)
        .on_hover_text(CAL);
    }
    let get = |m: Meter| s.meter(m).unwrap_or(0);
    let po_w = interpolate(&types::PO_CAL, get(Meter::Po));
    // The meter's full scale: 100 W, or the radio's own top power.
    let low_power = s.model.caps.power.1 <= 20;
    let full = if low_power { s.model.caps.power.1 as f32 } else { 100.0 };
    let pf = if tx { po_w / full } else { 0.0 };
    let pk = peak(Meter::Po, pf);
    const TICKS_100: &[(f32, &str)] = &[(0.0, "0"), (0.25, "25"), (0.5, "50"), (0.75, "75"), (1.0, "100")];
    const TICKS_10: &[(f32, &str)] = &[(0.0, "0"), (0.25, "2.5"), (0.5, "5"), (0.75, "7.5"), (1.0, "10")];
    widgets::Meter {
        label: "PO",
        fraction: needle(Meter::Po, pf),
        text: if !tx { "—".into() } else if low_power { format!("{po_w:.1} W") } else { format!("{po_w:.0} W") },
        color: AMBER,
        ticks: if low_power && s.model.caps.power.1 == 10 { TICKS_10 } else if low_power { &[] } else { TICKS_100 },
        red_from: None,
        peak: tx.then_some(pk),
    }
    .show_sized(ui, w, 26.0)
    .on_hover_text(CAL);
    let swr = interpolate(&types::SWR_CAL, get(Meter::Swr));
    widgets::Meter {
        label: "SWR",
        fraction: if tx { get(Meter::Swr) as f32 / 255.0 } else { 0.0 },
        text: if tx { format!("{swr:.1}:1") } else { "—".into() },
        color: CYAN,
        ticks: &[(0.0, "1"), (52.0 / 255.0, "1.5"), (89.0 / 255.0, "2"), (126.0 / 255.0, "3"), (1.0, "∞")],
        red_from: Some(89.0 / 255.0),
        peak: None,
    }
    .show_sized(ui, w, 26.0)
    .on_hover_text(CAL);
    let txt = |t: String| if tx { t } else { "—".into() };
    let alc = if tx { get(Meter::Alc) as f32 / 255.0 } else { 0.0 };
    widgets::Meter { label: "ALC", fraction: needle(Meter::Alc, alc), text: txt(format!("{:.0}%", alc * 100.0)), color: CYAN, ticks: &[], red_from: Some(0.5), peak: None }
        .show_sized(ui, w, 20.0);
    let comp = if tx { get(Meter::Comp) as f32 / 255.0 } else { 0.0 };
    widgets::Meter { label: "COMP", fraction: needle(Meter::Comp, comp), text: txt(format!("{:.0} dB", comp * 30.0)), color: CYAN, ticks: &[], red_from: None, peak: None }
        .show_sized(ui, w, 20.0);
    let id = if tx { interpolate(&types::ID_CAL, get(Meter::Id)) } else { 0.0 };
    widgets::Meter { label: "ID", fraction: id / 25.0, text: txt(format!("{id:.1} A")), color: CYAN, ticks: &[], red_from: None, peak: None }
        .show_sized(ui, w, 20.0)
        .on_hover_text(CAL);
    let vdd = interpolate(&types::VDD_CAL, get(Meter::Vdd));
    let vdd = if s.meter(Meter::Vdd).is_some() { format!("{vdd:.1} V") } else { "—".into() };
    ui.label(RichText::new(format!("VDD {vdd}")).weak().size(11.0)).on_hover_text(CAL);
    if moving {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

/// Transmit: MOX, the tuner, which mic and which key, antenna and the
/// radio's own meter.
fn tx_block(app: &mut App, ui: &mut Ui, rig: &Rig, s: &RadioState) {
    ui.spacing_mut().item_spacing.y = 5.0;
    ui.horizontal(|ui| {
        let txing = s.transmitting();
        let mox = egui::Button::new(RichText::new(if txing { "TX  (click to stop)" } else { "MOX / PTT" }).strong().size(16.0).color(if txing { Color32::WHITE } else { RED }))
            .fill(if txing { RED } else { Color32::from_rgb(40, 20, 20) });
        if ui.add_sized([170.0, 34.0], mox).on_hover_text("Transmits! Uses CAT TX (TX1;/TX0;). Or hold the space bar to talk (Settings).").clicked() {
            rig.send(if txing { "TX0;" } else { "TX1;" });
        }
        if s.model.has("AC") {
            let tuning = s.tuner == 2;
            if ui
                .add(egui::Button::new(RichText::new("TUNE").strong().color(if tuning { Color32::BLACK } else { Color32::LIGHT_GRAY })).min_size(egui::vec2(62.0, 34.0)).fill(if tuning { AMBER } else { Color32::from_rgb(40, 34, 20) }))
                .on_hover_text("Starts the antenna tuner — transmits a carrier")
                .clicked()
            {
                rig.send("AC002;");
            }
            if led_button(ui, s.tuner != 0, "ATU", AMBER) {
                rig.send(if s.tuner != 0 { "AC000;" } else { "AC001;" });
            }
        }
    });
    let half = egui::vec2((ui.available_width() - ui.spacing().item_spacing.x) / 2.0, 30.0);
    ui.horizontal(|ui| {
        mic_button(app, ui, half);
        crate::cw_key::arm_button(app, ui, half);
    });
    tx_status_line(app, ui);
    let caps = &s.model.caps;
    ui.horizontal(|ui| {
        if s.model.has("AN") && caps.antennas > 1 {
            ui.label("Antenna");
            for a in 1..=caps.antennas {
                if ui.selectable_label(s.antenna == a, format!("ANT {a}")).clicked() {
                    rig.send(format!("AN0{a};"));
                }
            }
        }
        // Where the radio warns, beside the antenna it's about.
        if s.radio_info.get(&0) == Some(&true) {
            ui.label(RichText::new("HI-SWR").strong().color(ink(RED)));
        }
    });
    if s.model.has("MS") && !caps.meter_select.is_empty() {
        ui.horizontal(|ui| {
            ui.label("Radio meter");
            let names = caps.meter_select;
            ComboBox::from_id_salt("ms")
                .width(70.0)
                .selected_text(names.iter().find(|n| n.0 == s.meter_sw).map(|n| n.1).unwrap_or("—"))
                .show_ui(ui, |ui| {
                    for &(c, n) in names {
                        if ui.selectable_label(s.meter_sw == c, n).clicked() {
                            rig.send(format!("MS{c};"));
                        }
                    }
                });
        });
    }
}

/// Band and mode keys in a compact block.
fn bands_and_modes(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let model = s.model;
    let cur = model.band_of(s.freq_a).map(|b| b.code);
    ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
    ui.horizontal_wrapped(|ui| {
        if model.has("BS") {
            for b in model.bands {
                if b.label == "60" {
                    continue; // channelized: its key comes after 80, below
                }
                let on = cur == Some(b.code);
                if ui.add(egui::Button::new(RichText::new(b.label).strong()).min_size(egui::vec2(38.0, 24.0)).selected(on)).clicked() {
                    rig.send(format!("{}BS{:02};", vfo_first(s), b.code));
                }
                if b.label == "80" {
                    sixty_metres(ui, rig, s);
                }
            }
        }
        if model.has("BD") && ui.add(egui::Button::new("◀").min_size(egui::vec2(38.0, 24.0))).on_hover_text("Band down (PageDown)").clicked() {
            rig.send(format!("{}BD0;", vfo_first(s)));
        }
        if model.has("BU") && ui.add(egui::Button::new("▶").min_size(egui::vec2(38.0, 24.0))).on_hover_text("Band up (PageUp)").clicked() {
            rig.send(format!("{}BU0;", vfo_first(s)));
        }
    });
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for &m in model.modes {
            let on = s.mode_a == Some(m);
            let r = ui.add(egui::Button::new(RichText::new(m.label()).strong()).min_size(egui::vec2(56.0, 24.0)).selected(on));
            crate::demo::mark(format!("mode:{}", m.label()), r.rect);
            if r.clicked() {
                rig.send(set_mode_cmd(m));
            }
        }
    });
}

/// What to send before a band change: in memory mode the radio ignores
/// band keys, so first `VM;` (the V/M key, a toggle) to get back to VFO.
fn vfo_first(s: &RadioState) -> &'static str {
    if s.info_a.mem_mode != 0 && s.model.has("VM") { "VM;" } else { "" }
}

/// A place to operate on 60 m.
struct SixtyM {
    name: &'static str,
    /// USB dial (carrier) frequency and centre, in Hz.
    dial: u64,
    centre: u64,
    /// The most transmitter power, in watts, that keeps under the ERP limit
    /// with a dipole (FCC presumes 0 dBd).
    watts: u8,
}

/// US 60 m since 13 February 2026 (Part 97.303(h), 97.313(i)): four
/// channels at 100 W ERP, and the 5351.5-5366.5 kHz segment at 9.15 W ERP,
/// shown at its usual spot (FT8's dial frequency). Named and tuned as the
/// ARRL charts have them: channels 1, 2, 4 and 5 kept their numbers, the
/// segment took channel 3's place. In frequency order, which is also the
/// order of the radios' own 60 m memories.
const SIXTY_M: [SixtyM; 5] = [
    SixtyM { name: "Ch 1", dial: 5_330_500, centre: 5_332_000, watts: 100 },
    SixtyM { name: "Ch 2", dial: 5_346_500, centre: 5_348_000, watts: 100 },
    SixtyM { name: "Seg ", dial: 5_357_000, centre: 5_358_500, watts: 9 },
    SixtyM { name: "Ch 4", dial: 5_371_500, centre: 5_373_000, watts: 100 },
    SixtyM { name: "Ch 5", dial: 5_403_500, centre: 5_405_000, watts: 100 },
];

/// The 60 m place of the radio's own 60 m memory VFO-A is on, and whether
/// it's a CW one.
fn sixty_m_memory(s: &RadioState) -> Option<(&'static SixtyM, bool)> {
    let (usb, cw) = s.model.sixty_m_memories()?;
    let ch = s.info_a.channel;
    match ch.checked_sub(usb).filter(|i| (*i as usize) < SIXTY_M.len()) {
        Some(i) => Some((&SIXTY_M[i as usize], false)),
        None => ch.checked_sub(cw).filter(|i| (*i as usize) < SIXTY_M.len()).map(|i| (&SIXTY_M[i as usize], true)),
    }
}

/// The 60 m key: 60 m is channelized, so instead of a band change it opens
/// a list of the channels, in the mode the channel needs: CW on the channel
/// centre; data stays in DATA-USB on the dial frequency (the audio centred
/// on the channel); voice and anything else goes to USB, the only voice
/// mode allowed there. A radio with its own 60 m memories recalls those,
/// USB or CW, the only modes they take. Power comes down to
/// the legal limit if it's above it; it's never raised, and never held
/// down if it's turned up afterwards.
fn sixty_metres(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let on = (5_250_000..=5_450_000).contains(&s.freq_a);
    let key = egui::Button::new(RichText::new("60").strong()).min_size(egui::vec2(38.0, 24.0)).selected(on);
    let memories = s.model.sixty_m_memories().filter(|_| s.model.has("MC"));
    let (r, _) = egui::containers::menu::MenuButton::from_button(key).ui(ui, |ui| {
        ui.label(RichText::new("60 m — US").strong().color(ink(AMBER)));
        for (i, c) in SIXTY_M.iter().enumerate() {
            let (dial, centre) = (c.dial, c.centre);
            let (hz, mode) = match s.mode_a {
                Some(m @ (Mode::Cw | Mode::CwR)) => (centre, m),
                Some(Mode::DataUsb) => (dial, Mode::DataUsb),
                _ => (dial, Mode::Usb),
            };
            // A radio with 60 m memories transmits there only from them (an
            // FTDX3000 keys up on the VFO but puts out nothing), so data
            // goes to the USB memory too.
            let channel = memories.map(|(usb, cw)| match mode {
                Mode::Cw | Mode::CwR => cw + i as u16,
                _ => usb + i as u16,
            });
            let here = match channel {
                Some(ch) => s.info_a.mem_mode != 0 && s.info_a.channel == ch,
                None => s.info_a.mem_mode == 0 && s.freq_a == hz,
            };
            // The memories are USB or CW, nothing else.
            let shown_mode = match (channel, mode) {
                (Some(_), Mode::Cw | Mode::CwR) => Mode::Cw,
                (Some(_), _) => Mode::Usb,
                (None, m) => m,
            };
            // Where the dial goes and where the signal is: a USB (or data)
            // carrier sits 1.5 kHz below the centre, CW on it. A memory
            // shows the centre whatever the mode.
            let freqs = match shown_mode {
                Mode::Cw | Mode::CwR => format!("{:>6.1} on centre      ", centre as f64 / 1000.0),
                _ => format!("{:>6.1} → centre {:.1}", dial as f64 / 1000.0, centre as f64 / 1000.0),
            };
            let label = format!(
                "{}   {}   {:<6} ≤{:>3} W{}",
                c.name,
                freqs,
                shown_mode.label(),
                c.watts,
                if channel.is_some() { "   MEM" } else { "" }
            );
            let limit = if c.watts < 100 { "the 5351.5-5366.5 kHz segment: 9.15 W ERP" } else { "a channel: 100 W ERP" };
            let r = ui.add(egui::Button::new(RichText::new(label).monospace()).selected(here));
            let r = match channel {
                Some(ch) => r.on_hover_text(format!("Recall the radio's own 60 m memory ({ch}); {limit}")),
                None => r.on_hover_text(format!("Tune VFO-A; {limit}")),
            };
            if r.clicked() {
                if let Some(ch) = channel {
                    rig.send(format!("MC{ch:03};"));
                } else {
                    // Back to VFO, then the mode before the frequency:
                    // changing the mode moves the frequency shown (the
                    // radio's display offset for data and CW).
                    let vfo = vfo_first(s);
                    if !vfo.is_empty() {
                        rig.send(vfo);
                    }
                    if !vfo.is_empty() || s.mode_a != Some(mode) {
                        rig.send(set_mode_cmd(mode));
                    }
                    rig.send(set_freq_cmd_for(s.model, false, hz));
                }
                // Down to the limit with a dipole, if above it.
                if s.model.has("PC") && s.power > c.watts {
                    rig.send(format!("PC{:03};", c.watts.max(s.model.caps.power.0)));
                }
            }
        }
    });
    r.on_hover_text("60 m channels, in the right mode and power");
}

/// A slider bound to a radio value: sends `fmt(v)` whenever it moves.
/// A slider bound to a radio value: sends `fmt(v)` whenever it moves.
/// Scrolling over it (wheel or trackpad) adjusts it too, in about 1% steps.
fn slider(ui: &mut Ui, rig: &Rig, label: &str, value: i32, range: std::ops::RangeInclusive<i32>, fmt: impl Fn(i32) -> String) {
    let mut v = value;
    let (lo, hi) = (*range.start(), *range.end());
    ui.label(label);
    let r = ui.add(Slider::new(&mut v, range));
    let steps = widgets::scroll_steps(ui, &r, 12.0);
    if steps != 0 {
        let inc = ((hi - lo) / 100).max(1);
        rig.send(fmt((value + steps as i32 * inc).clamp(lo, hi)));
    } else if r.changed() {
        rig.send(fmt(v));
    }
    ui.end_row();
}

fn group(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        // Sliders take what the column has left after the labels and values.
        ui.spacing_mut().slider_width = (ui.available_width() - 165.0).clamp(70.0, 260.0);
        ui.label(RichText::new(title).strong().size(15.0).color(ink(AMBER)));
        body(ui);
    });
}

/// Where an SWR sits on a meter: 1, 1.5, 2, 3 and 5:1 spread out like the
/// radio's own SWR meter.
fn swr_fraction(swr: f32) -> f32 {
    const AT: [(f32, f32); 6] = [(1.0, 0.0), (1.5, 0.2), (2.0, 0.35), (3.0, 0.5), (5.0, 0.75), (10.0, 1.0)];
    for w in AT.windows(2) {
        let ((a, fa), (b, fb)) = (w[0], w[1]);
        if swr <= b {
            return fa + (fb - fa) * ((swr.max(a) - a) / (b - a));
        }
    }
    1.0
}

/// The LDG tuner: its own power and SWR readings, and its buttons.
fn tuner_group(ui: &mut Ui, app: &mut App, s: &RadioState) {
    use ftdx3000_rig::ldg::{LdgCommand, TuneResult, TunerMode};
    let Some(tuner) = &app.services.tuner else { return };
    let st = tuner.status();
    // The tuner's thread doesn't wake the window: look again soon.
    ui.ctx().request_repaint_after(Duration::from_millis(if s.transmitting() || st.busy.is_some() { 100 } else { 1000 }));
    group(ui, "LDG Tuner", |ui| {
        if !st.connected {
            ui.label(RichText::new(&st.text).color(ink(AMBER)));
            return;
        }
        let w = ui.available_width();
        // Over 100 W means an amplifier: switch to 1 kW and stay there a
        // while, so SSB peaks don't flip the scale back and forth.
        let now = Instant::now();
        if st.forward_w > 110.0 {
            app.operate.tuner_kw_until = Some(now + Duration::from_secs(10));
        }
        let big = app.operate.tuner_kw_until.is_some_and(|t| t > now);
        let full = if big { 1000.0 } else { 100.0 };
        const TICKS_100: &[(f32, &str)] = &[(0.0, "0"), (0.25, "25"), (0.5, "50"), (0.75, "75"), (1.0, "100")];
        const TICKS_1K: &[(f32, &str)] = &[(0.0, "0"), (0.25, "250"), (0.5, "500"), (0.75, "750"), (1.0, "1k")];
        // Like the radio's meters: the bar rises at once and falls back
        // slowly, the peak holds a moment, and SWR keeps its last reading
        // through the gaps in CW and speech.
        const HOLD: Duration = Duration::from_millis(1500);
        let op = &mut app.operate;
        let dt = now.duration_since(op.tuner_needle.1).as_secs_f32();
        let tau = if st.forward_w > op.tuner_needle.0 { 0.03 } else { 0.3 };
        op.tuner_needle = (op.tuner_needle.0 + (st.forward_w - op.tuner_needle.0) * (1.0 - (-dt / tau).exp()), now);
        if st.forward_w >= op.tuner_peak.0 || now.duration_since(op.tuner_peak.1) > HOLD {
            op.tuner_peak = (st.forward_w, now);
        }
        if let Some(v) = st.swr {
            op.tuner_swr = (v, now);
        }
        let swr_held = (now.duration_since(op.tuner_swr.1) < HOLD).then_some(op.tuner_swr.0);
        let (needle, peak) = (op.tuner_needle.0, op.tuner_peak.0);
        if needle > 0.05 || peak > 0.0 || swr_held.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }
        widgets::Meter {
            label: "PO",
            fraction: (needle / full).min(1.0),
            text: if peak >= 0.5 { format!("{peak:.0} W") } else { "—".into() },
            color: AMBER,
            ticks: if big { TICKS_1K } else { TICKS_100 },
            red_from: None,
            peak: (peak >= 0.5).then_some((peak / full).min(1.0)),
        }
        .show_sized(ui, w, 26.0)
        .on_hover_text(format!("Measured by the tuner; reflected {:.1} W", st.reflected_w));
        widgets::Meter {
            label: "SWR",
            fraction: swr_held.map_or(0.0, swr_fraction),
            text: swr_held.map_or("—".into(), |v| if v >= 99.0 { "∞".into() } else { format!("{v:.2}:1") }),
            color: CYAN,
            ticks: &[(0.0, "1"), (0.2, "1.5"), (0.35, "2"), (0.5, "3"), (0.75, "5"), (1.0, "∞")],
            red_from: Some(0.35),
            peak: None,
        }
        .show_sized(ui, w, 26.0);
        let busy = st.busy.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!busy && !s.transmitting(), |ui| {
                if ui
                    .add(egui::Button::new(RichText::new("TUNE").strong()).min_size(egui::vec2(62.0, 26.0)).fill(Color32::from_rgb(40, 34, 20)))
                    .on_hover_text("Transmits! A low-power CW carrier while the tuner tunes from memory (or searches); power and mode are put back afterwards. Put an amplifier in standby first.")
                    .clicked()
                {
                    tuner.send(LdgCommand::Tune { full: false });
                }
                if ui
                    .add(egui::Button::new("FULL").min_size(egui::vec2(54.0, 26.0)))
                    .on_hover_text("Transmits! A full search for a match, ignoring the tuner's memory (slower). Put an amplifier in standby first.")
                    .clicked()
                {
                    tuner.send(LdgCommand::Tune { full: true });
                }
            });
            ui.add_enabled_ui(!busy, |ui| {
                let mode = st.mode;
                if led_button(ui, mode == Some(TunerMode::Auto), "AUTO", GREEN) {
                    tuner.send(LdgCommand::Auto);
                }
                if led_button(ui, mode == Some(TunerMode::Manual), "MAN", AMBER) {
                    tuner.send(LdgCommand::Manual);
                }
                if led_button(ui, mode == Some(TunerMode::Bypass), "BYP", RED) {
                    tuner.send(LdgCommand::Bypass);
                }
                let ant = st.antenna.map_or("ANT ?".to_string(), |a| format!("ANT {a}"));
                if button(ui, &ant) {
                    tuner.send(LdgCommand::ToggleAntenna);
                }
            });
        });
        let line = match (st.busy, st.last_tune) {
            (Some(b), _) => RichText::new(format!("{b}…")).color(ink(AMBER)),
            (None, Some(r)) => RichText::new(r.label()).color(ink(if r == TuneResult::Failed { RED } else { GREEN })),
            (None, None) if st.mode.is_none() => RichText::new("mode and antenna show once set from here").color(DIM),
            _ => RichText::new("").color(DIM),
        };
        ui.label(line);
        if st.text != "connected" {
            ui.label(RichText::new(&st.text).color(ink(AMBER)));
        }
    });
}

/// One width control across the narrow and wide filter tables, IF shift, and
/// a picture of the resulting audio passband.
fn filter_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let Some(mode) = s.mode_a else { return };
    let model = s.model;
    let class = s.width_class(mode);
    let narrow_switch = model.caps.narrow && model.has("NA");
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Filter").strong().size(15.0).color(ink(AMBER)));
            ui.add_space(4.0);
            if class == WidthClass::Fixed {
                ui.label(RichText::new(format!("{} width is fixed", mode.label())).weak());
            } else {
                ui.label(RichText::new(format!("{:>4} Hz", s.width_hz().unwrap_or(0))).monospace().size(18.0).color(ink(AMBER)));
            }
            if narrow_switch {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if led_button(ui, s.narrow, "NAR", AMBER) {
                        rig.send(format!("NA0{};", (!s.narrow) as u8));
                    }
                });
            }
        });
        if class != WidthClass::Fixed && model.has("SH") {
            // Narrow and wide tables as one list, narrowest first. Index 0
            // ("the default for the narrow/wide setting") is left out where
            // there is a narrow switch.
            let widths = |n: bool| (model.caps.widths)(class, n).into_iter().filter(move |(i, w)| *w != 0 && (!narrow_switch || *i != 0));
            let mut all: Vec<(u32, bool, u8)> = widths(true)
                .map(|(i, w)| (w, true, i))
                .filter(|_| narrow_switch)
                .chain(widths(false).map(|(i, w)| (w, false, i)))
                .collect();
            all.sort_by_key(|x| (x.0, !x.1));
            all.dedup_by_key(|x| x.0);
            let cur = s.width_hz().unwrap_or(0);
            let mut pos = all
                .iter()
                .position(|x| (!narrow_switch || x.1 == s.narrow) && x.2 == s.width_index)
                .unwrap_or_else(|| all.iter().enumerate().min_by_key(|(_, x)| (x.0 as i64 - cur as i64).abs()).map(|(i, _)| i).unwrap_or(0));
            let set = |e: (u32, bool, u8)| {
                if narrow_switch && e.1 != s.narrow {
                    rig.send(format!("NA0{};", e.1 as u8));
                }
                rig.send(format!("SH0{:02};", e.2));
            };
            ui.horizontal(|ui| {
                ui.label("Width");
                if ui.small_button(" − ").clicked() && pos > 0 {
                    set(all[pos - 1]);
                }
                let was = pos;
                let w = (ui.available_width() - 34.0).max(60.0);
                let r = ui.add_sized([w, 20.0], Slider::new(&mut pos, 0..=all.len().saturating_sub(1)).show_value(false));
                let steps = widgets::scroll_steps(ui, &r, 16.0);
                if steps != 0 {
                    set(all[(was as i64 + steps).clamp(0, all.len() as i64 - 1) as usize]);
                } else if r.changed() {
                    set(all[pos]);
                }
                if ui.small_button(" + ").clicked() && pos + 1 < all.len() {
                    set(all[pos + 1]);
                }
            });
        }
        if model.has("IS") {
        ui.horizontal(|ui| {
            ui.label("IF shift");
            let mut shift = s.if_shift;
            let w = (ui.available_width() - 100.0).max(60.0);
            let r = ui.add_sized([w, 20.0], Slider::new(&mut shift, -1000..=1000).step_by(20.0).show_value(false));
            let steps = widgets::scroll_steps(ui, &r, 12.0);
            if steps != 0 {
                shift = (s.if_shift + steps as i32 * 20).clamp(-1000, 1000);
            }
            if steps != 0 || r.changed() {
                rig.send(format!("IS0{}{:04};", if shift < 0 { '-' } else { '+' }, shift.abs()));
            }
            ui.label(RichText::new(format!("{:+5} Hz", s.if_shift)).monospace());
            if ui.small_button("0").on_hover_text("Center the IF shift").clicked() {
                rig.send("IS0+0000;");
            }
        });
        }
        passband(ui, s, mode, class);
    });
}

/// Approximate audio passband: width and IF shift around the mode's centre
/// (the CW pitch in CW, ~1.5 kHz otherwise).
fn passband(ui: &mut Ui, s: &RadioState, mode: Mode, class: WidthClass) {
    use egui::{Align2, FontId, Pos2, Sense, Stroke, vec2};
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, widgets::PANEL);
    let span = 4000.0_f32;
    let x = |hz: f32| rect.left() + 6.0 + (rect.width() - 12.0) * (hz / span).clamp(0.0, 1.0);
    let base = rect.bottom() - 12.0;
    p.line_segment([Pos2::new(x(0.0), base), Pos2::new(x(span), base)], Stroke::new(1.0, Color32::GRAY));
    for k in [1000.0, 2000.0, 3000.0] {
        p.line_segment([Pos2::new(x(k), base), Pos2::new(x(k), base + 3.0)], Stroke::new(1.0, Color32::GRAY));
        p.text(Pos2::new(x(k), base + 3.0), Align2::CENTER_TOP, format!("{}k", k as u32 / 1000), FontId::proportional(9.0), Color32::GRAY);
    }
    // AM/FM: roughly 3 kHz of audio either way.
    let width = match class {
        WidthClass::Fixed => 3000.0,
        _ => s.width_hz().unwrap_or(2400) as f32,
    };
    let centre = if mode.is_cw() { 300.0 + 10.0 * s.key_pitch as f32 } else { 1500.0 } + s.if_shift as f32;
    let (lo, hi) = (centre - width / 2.0, centre + width / 2.0);
    let top = rect.top() + 5.0;
    let slope = 6.0;
    let shape = vec![
        Pos2::new(x(lo) - slope, base),
        Pos2::new(x(lo), top),
        Pos2::new(x(hi), top),
        Pos2::new(x(hi) + slope, base),
    ];
    p.add(egui::Shape::convex_polygon(shape, AMBER.gamma_multiply(0.35), Stroke::new(1.5, AMBER)));
    if mode.is_cw() {
        let pitch = 300.0 + 10.0 * s.key_pitch as f32;
        p.line_segment([Pos2::new(x(pitch), top - 2.0), Pos2::new(x(pitch), base)], Stroke::new(1.0, CYAN));
    }
}

fn receiver_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let m = s.model;
    let caps = &m.caps;
    group(ui, "Receiver", |ui| {
        egui::Grid::new("rx").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            if m.has("AG") {
                slider(ui, rig, "AF gain", s.af_gain as i32, 0..=255, |v| format!("AG0{v:03};"));
            }
            if m.has("RG") {
                slider(ui, rig, "RF gain", s.rf_gain as i32, 0..=255, |v| format!("RG0{v:03};"));
            }
            if m.has("SQ") {
                slider(ui, rig, "Squelch", s.squelch as i32, 0..=255, |v| format!("SQ0{v:03};"));
            }
            if m.has("GT") && !caps.agc.is_empty() {
            let agc: Vec<(&str, String)> = caps.agc.iter().map(|(c, l)| (*l, format!("GT0{c};"))).collect();
            let cur_agc = types::agc_label(s.agc);
            let cur_agc = if cur_agc.starts_with("AUTO") { "AUTO" } else { cur_agc };
            // What the radio is actually doing, when AUTO picks for it.
            let reports = types::agc_label(s.agc);
            ui.label("AGC");
            ui.horizontal(|ui| {
                ComboBox::from_id_salt("agc").width(90.0).selected_text(cur_agc).show_ui(ui, |ui| {
                    for (l, cmd) in &agc {
                        if ui.selectable_label(cur_agc == *l, *l).clicked() {
                            rig.send(cmd);
                        }
                    }
                });
                if reports != cur_agc {
                    ui.label(RichText::new(reports).weak().size(11.0)).on_hover_text("What the radio reports");
                }
            });
            ui.end_row();
            }
            if m.has("PA") && !caps.preamp.is_empty() {
                ui.label("Front end");
                ui.horizontal(|ui| {
                    for &(c, l) in caps.preamp {
                        if ui.selectable_label(s.preamp == c, l).clicked() {
                            rig.send(format!("PA0{c};"));
                        }
                    }
                });
                ui.end_row();
            }
            if m.has("RA") && !caps.att.is_empty() {
                ui.label("Attenuator");
                ui.horizontal(|ui| {
                    for &(c, l) in caps.att {
                        if ui.selectable_label(s.att == c, l).clicked() {
                            rig.send(format!("RA0{c};"));
                        }
                    }
                });
                ui.end_row();
            }
            if m.has("RF") && !caps.roofing.is_empty() {
                let roof: Vec<(&str, String)> = caps.roofing.iter().map(|(c, l)| (*l, format!("RF0{c};"))).collect();
                let cur = m.roofing_label(s.roofing);
                let auto = cur.is_some_and(|c| c.starts_with("AUTO"));
                ui.label("Roofing");
                ComboBox::from_id_salt("roof").width(150.0).selected_text(cur.unwrap_or("—")).show_ui(ui, |ui| {
                    for (l, cmd) in &roof {
                        let selected = cur.is_some_and(|c| c.trim_start_matches("AUTO ") == *l) || (*l == "AUTO" && auto);
                        if ui.selectable_label(selected, *l).clicked() {
                            rig.send(cmd);
                        }
                    }
                });
                ui.end_row();
            }
        });
    });
}

fn dsp_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let m = s.model;
    let caps = &m.caps;
    group(ui, "Filters & DSP", |ui| {
        egui::Grid::new("dsp").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            if m.has("NB") && !caps.nb.is_empty() {
                ui.label("Noise blanker");
                ui.horizontal(|ui| {
                    for &(c, l) in caps.nb {
                        if ui.selectable_label(s.nb == c, l).clicked() {
                            rig.send(format!("NB0{c};"));
                        }
                    }
                });
                ui.end_row();
            }
            if m.has("NL") {
                let max = caps.nb_level_max as i32;
                slider(ui, rig, "NB level", (s.nb_level as i32).min(max), 0..=max, |v| format!("NL0{v:03};"));
            }
            if m.has("NR") {
                ui.label("DNR");
                ui.horizontal(|ui| {
                    if led_button(ui, s.nr, "DNR", AMBER) {
                        rig.send(format!("NR0{};", (!s.nr) as u8));
                    }
                    if m.has("RL") {
                        let max = caps.nr_max.max(1) as i32;
                        let mut lvl = (s.nr_level.max(1) as i32).min(max);
                        // Shorter than the others: it shares the row with the switch.
                        ui.spacing_mut().slider_width = (ui.spacing().slider_width - 60.0).max(50.0);
                        let r = ui.add(Slider::new(&mut lvl, 1..=max));
                        let steps = widgets::scroll_steps(ui, &r, 16.0);
                        if steps != 0 {
                            lvl = (s.nr_level.max(1) as i32 + steps as i32).clamp(1, max);
                        }
                        if steps != 0 || r.changed() {
                            rig.send(format!("RL0{lvl:02};"));
                        }
                    }
                });
                ui.end_row();
            }
            if m.has("BC") || m.has("BP") {
                ui.label("Notch");
                ui.horizontal(|ui| {
                    if m.has("BC") && led_button(ui, s.auto_notch, "DNF", AMBER) {
                        rig.send(format!("BC0{};", (!s.auto_notch) as u8));
                    }
                    if m.has_key("BP00") && led_button(ui, s.manual_notch, "NOTCH", AMBER) {
                        rig.send(format!("BP00{:03};", (!s.manual_notch) as u8));
                    }
                });
                ui.end_row();
            }
            if m.has_key("BP01") {
                slider(ui, rig, "Notch freq", s.notch_freq.max(1) as i32 * 10, 10..=4000, |v| format!("BP01{:03};", (v / 10).clamp(1, 400)));
            }
            if m.has_key("CO00") {
                let contour = m.has_key("CO01");
                ui.label(if contour { "Contour / APF" } else { "APF" });
                ui.horizontal(|ui| {
                    for (c, l) in [(0, "OFF"), (1, "CONT"), (2, "APF")] {
                        if c == 1 && !contour {
                            continue;
                        }
                        if ui.selectable_label(s.contour_mode == c, l).clicked() {
                            rig.send(format!("CO00{c:02};"));
                        }
                    }
                });
                ui.end_row();
                if s.contour_mode == 2 && m.has_key("CO02") {
                    slider(ui, rig, "APF offset", (s.apf_freq as i32 - 25) * 10, -250..=250, |v| format!("CO02{:02};", (v / 10 + 25).clamp(0, 50)));
                } else if contour {
                    slider(ui, rig, "Contour freq", s.contour_freq.max(1) as i32 * 100, 100..=4000, |v| format!("CO01{:02};", (v / 100).clamp(1, 40)));
                }
            }
        });
    });
}

fn transmitter_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let m = s.model;
    let caps = &m.caps;
    group(ui, "Transmitter", |ui| {
        egui::Grid::new("tx").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            if m.has("PC") {
                let (lo, hi) = (caps.power.0 as i32, caps.power.1 as i32);
                slider(ui, rig, "Power (W)", (s.power as i32).clamp(lo, hi), lo..=hi, |v| format!("PC{v:03};"));
            }
            if m.has("MG") {
                slider(ui, rig, "Mic gain", s.mic_gain as i32, 0..=100, |v| format!("MG{v:03};"));
            }
            if m.has("PR") {
                ui.label("Processor");
                ui.horizontal(|ui| {
                    if led_button(ui, s.processor, "PROC", AMBER) {
                        rig.send(format!("PR0{};", (!s.processor) as u8));
                    }
                    if m.has_key("PR1") && led_button(ui, s.mic_eq, "MIC EQ", AMBER) {
                        rig.send(format!("PR1{};", (!s.mic_eq) as u8));
                    }
                });
                ui.end_row();
            }
            if m.has("PL") {
                slider(ui, rig, "Proc level", s.proc_level as i32, 0..=100, |v| format!("PL{v:03};"));
            }
            if m.has("VX") {
                ui.label("VOX");
                ui.horizontal(|ui| {
                    if led_button(ui, s.vox, "VOX", AMBER) {
                        rig.send(format!("VX{};", (!s.vox) as u8));
                    }
                });
                ui.end_row();
            }
            if m.has("VG") {
                slider(ui, rig, "VOX gain", s.vox_gain as i32, 0..=100, |v| format!("VG{v:03};"));
            }
            if m.has("VD") {
                slider(ui, rig, "VOX delay (ms)", s.vox_delay.max(30) as i32, 30..=3000, |v| format!("VD{:04};", (v / 10) * 10));
            }
            if m.has_key("ML0") {
                ui.label("Monitor");
                ui.horizontal(|ui| {
                    if led_button(ui, s.monitor, "MONI", AMBER) {
                        rig.send(format!("ML0{:03};", (!s.monitor) as u8));
                    }
                });
                ui.end_row();
            }
            if m.has_key("ML1") {
                slider(ui, rig, "Monitor level", s.monitor_level.max(1) as i32, 1..=100, |v| format!("ML1{v:03};"));
            }
            if m.has("PB") && caps.voice_memories > 0 {
            ui.label("Voice memory");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for ch in 1..=caps.voice_memories {
                    if ui.small_button(format!("▶{ch}")).on_hover_text(format!("Play DVS channel {ch} (transmits)")).clicked() {
                        rig.send(format!("PB0{ch};"));
                    }
                }
                if ui.small_button("■").on_hover_text("Stop playback").clicked() {
                    rig.send("PB00;");
                }
            });
            ui.end_row();
            }
        });
    });
}

fn clarifier_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let m = s.model;
    group(ui, "Clarifier (RIT / XIT)", |ui| {
        ui.horizontal(|ui| {
            if m.has("RT") && led_button(ui, s.rx_clar, "RX CLAR", AMBER) {
                rig.send(format!("RT{};", (!s.rx_clar) as u8));
            }
            if m.has("XT") && led_button(ui, s.tx_clar, "TX CLAR", AMBER) {
                rig.send(format!("XT{};", (!s.tx_clar) as u8));
            }
            ui.label(RichText::new(format!("{:+} Hz", s.info_a.clar_offset)).monospace().size(20.0).color(ink(if s.rx_clar || s.tx_clar { AMBER } else { DIM })));
        });
        ui.horizontal_wrapped(|ui| {
            if m.has("RU") {
                for (d, l) in [(-100, "−100"), (-10, "−10"), (10, "+10"), (100, "+100")] {
                    if ui.add(egui::Button::new(l).min_size(egui::vec2(44.0, 26.0))).clicked() {
                        rig.send(if d < 0 { format!("RD{:04};", -d) } else { format!("RU{d:04};") });
                    }
                }
            }
            if m.has("RC") && button(ui, "CLEAR") {
                rig.send("RC;");
            }
        });
        if m.caps.shared_clarifier {
            ui.label(RichText::new(format!("The {} has one clarifier offset shared by RX and TX clarifier.", m.name)).weak().size(11.0));
        }
    });
}

fn rotator_group(ui: &mut Ui, rig: &Rig, s: &RadioState) {
    group(ui, "Rotator (via the radio's ROT jack)", |ui| {
        ui.horizontal_wrapped(|ui| {
            match s.rotator {
                Some((_, dir, speed)) => {
                    ui.label(RichText::new(format!("{dir:>3}°")).monospace().size(22.0).color(ink(CYAN)));
                    ui.label(RichText::new(format!("speed {speed}%")).weak());
                }
                None => {
                    ui.label(RichText::new("no rotator data").color(DIM));
                }
            }
            if button(ui, "◀ CCW") {
                rig.send("RO1;");
            }
            if button(ui, "STOP") {
                rig.send("RO0;");
            }
            if button(ui, "CW ▶") {
                rig.send("RO2;");
            }
            if ui.small_button("speed −").clicked() {
                rig.send("RO3;");
            }
            if ui.small_button("speed +").clicked() {
                rig.send("RO4;");
            }
        });
        ui.label(RichText::new("Works with a Yaesu G-800DXA/1000DXA/2800DXA connected to the radio.").weak().size(11.0));
    });
}

fn macro_group(ui: &mut Ui, app: &mut App, rig: &Rig) {
    group(ui, "Macros", |ui| {
        ui.horizontal_wrapped(|ui| {
            for m in &app.services.config.macros {
                if ui.button(&m.label).on_hover_text(&m.commands).clicked() {
                    rig.send(&m.commands);
                }
            }
        });
        let mut save = false;
        ui.collapsing("Edit macros", |ui| {
            let macros = &mut app.services.config.macros;
            let mut remove = None;
            for (i, m) in macros.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 30.0;
                    ui.add_sized([(w * 0.3).max(60.0), 22.0], egui::TextEdit::singleline(&mut m.label).hint_text("label"));
                    ui.add_sized([(w * 0.7 - 6.0).max(120.0), 22.0], egui::TextEdit::singleline(&mut m.commands).hint_text("FA14074000;MD0C;").font(egui::TextStyle::Monospace));
                    if ui.small_button("✕").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                macros.remove(i);
            }
            ui.horizontal(|ui| {
                if ui.button("Add").clicked() {
                    macros.push(ftdx3000_rig::Macro::default());
                }
                save = ui.button("Save").clicked();
            });
            ui.label(RichText::new("Any RigHelm CAT commands (the FTDX3000's, whatever the radio), separated by ';'. See the CAT console to try them.").weak().size(11.0));
        });
        if save {
            match app.services.config.save() {
                Ok(()) => app.flash("macros saved"),
                Err(e) => app.flash(format!("could not save: {e}")),
            }
        }
    });
}

fn cw_group(ui: &mut Ui, op: &mut OperateState, rig: &Rig, s: &RadioState) {
    let m = s.model;
    let caps = &m.caps;
    group(ui, "CW", |ui| {
        egui::Grid::new("cw").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("Keyer");
            ui.horizontal(|ui| {
                if m.has("KR") && led_button(ui, s.keyer, "KEYER", AMBER) {
                    rig.send(format!("KR{};", (!s.keyer) as u8));
                }
                if m.has("BI") && led_button(ui, s.break_in, "BK-IN", AMBER) {
                    rig.send(format!("BI{};", (!s.break_in) as u8));
                }
                if m.has("CS") && led_button(ui, s.cw_spot, "SPOT", AMBER) {
                    rig.send(format!("CS{};", (!s.cw_spot) as u8));
                }
            });
            ui.end_row();
            if m.has("KS") {
                slider(ui, rig, "Speed (WPM)", s.key_speed.max(4) as i32, 4..=60, |v| format!("KS{v:03};"));
            }
            if m.has("KP") {
                slider(ui, rig, "Pitch (Hz)", 300 + 10 * s.key_pitch as i32, 300..=1050, |v| format!("KP{:02};", ((v - 300) / 10).clamp(0, 75)));
            }
            if m.has("SD") {
                slider(ui, rig, "BK-IN delay (ms)", s.bk_delay.max(30) as i32, 30..=3000, |v| format!("SD{:04};", (v / 10) * 10));
            }
        });
        let n = if m.has("KM") || m.has("KY") { caps.keyer_memories as usize } else { 0 };
        if n == 0 {
            return;
        }
        let len = caps.keyer_memory_len as usize;
        ui.label(RichText::new(format!("Keyer memories (up to {len} characters)")).strong());
        for i in 0..n.min(5) {
            if !op.keyer_loaded[i] {
                if let Some(t) = &s.keyer_memory[i] {
                    op.keyer_text[i] = t.trim_end().to_string();
                    op.keyer_loaded[i] = true;
                }
            }
            ui.horizontal(|ui| {
                if ui.button(format!("▶ {}", i + 1)).on_hover_text("Send this keyer memory (transmits)").clicked() {
                    rig.send(format!("KY{};", i + 1));
                }
                ui.add(egui::TextEdit::singleline(&mut op.keyer_text[i]).char_limit(len).desired_width((ui.available_width() - 56.0).max(80.0)));
                if m.has("KM") && ui.small_button("Save").on_hover_text("Write this text into the radio's keyer memory").clicked() {
                    let text: String = op.keyer_text[i].chars().filter(|c| c.is_ascii() && *c != ';').collect();
                    rig.send(format!("KM{}{};", i + 1, text.to_uppercase()));
                }
            });
        }
    });
}
