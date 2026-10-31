//! Logger Connect: its Settings panel, and the DX spot markers on the band
//! scope. The listening itself is in `ftdx3000_rig::logger`.

use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Stroke, Ui, vec2};
use ftdx3000_cat::{RadioState, set_freq_cmd_for, set_mode_cmd};
use ftdx3000_rig::Rig;
use ftdx3000_rig::logger::{self, DEFAULT_PORT, DEFAULT_SEND_TO, LoggerConfig, Spot, SpotStatus};

use crate::App;
use crate::widgets::{AMBER, DIM, GREEN, ink};

const ALERT: Color32 = Color32::from_rgb(255, 96, 208);
const NEW_ENTITY: Color32 = Color32::from_rgb(255, 150, 70);
const NEW_SLOT: Color32 = Color32::from_rgb(120, 230, 150);
const PLAIN: Color32 = Color32::from_rgb(205, 215, 230);
const WORKED: Color32 = Color32::from_rgb(120, 125, 135);
/// Height of a row of labels, and where the first starts below the top of
/// the spectrum (leaving room for the band name and the dial readout).
const ROW_H: f32 = 14.0;
const FIRST_ROW: f32 = 22.0;
const LABEL_PAD: f32 = 3.0;
/// How far either side of a tick the pointer picks it, where it has no label.
const TICK_REACH: f32 = 3.0;

fn color(s: &Spot) -> Color32 {
    if s.alert() {
        return ALERT;
    }
    match s.status {
        SpotStatus::NewEntity => NEW_ENTITY,
        SpotStatus::NewSlot => NEW_SLOT,
        SpotStatus::Other => PLAIN,
        SpotStatus::Worked => WORKED,
    }
}

/// A spot's place on the scope: its tick, and its label if one fit.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub spot: usize,
    pub x: f32,
    /// Label's row and left edge.
    pub label: Option<(usize, f32)>,
}

/// Lay out the labels: what matters most gets the first choice, each in the
/// first row where it fits. What doesn't fit keeps its tick.
pub fn place(spots: &[Spot], x_of: impl Fn(f64) -> f32, width_of: impl Fn(&str) -> f32, left: f32, right: f32, rows: usize) -> Vec<Placed> {
    let mut order: Vec<usize> = (0..spots.len()).collect();
    order.sort_by_key(|&i| {
        let s = &spots[i];
        (std::cmp::Reverse(s.alert()), std::cmp::Reverse(s.status), Instant::now().saturating_duration_since(s.seen))
    });
    let mut used: Vec<Vec<(f32, f32)>> = vec![Vec::new(); rows];
    order
        .into_iter()
        .map(|i| {
            let x = x_of(spots[i].hz as f64);
            let w = width_of(&spots[i].call) + LABEL_PAD * 2.0;
            // Beside the tick, or on its left at the right edge.
            let a = if x + w > right { (x - w).max(left) } else { x };
            let free = used.iter().position(|r| r.iter().all(|&(l, h)| a + w + 2.0 <= l || h + 2.0 <= a));
            if let Some(r) = free {
                used[r].push((a, a + w));
            }
            Placed { spot: i, x, label: free.map(|r| (r, a)) }
        })
        .collect()
}

/// The spots on screen this frame, for the pointer to find.
#[derive(Default)]
pub struct Markers {
    spots: Vec<Spot>,
    hits: Vec<(Rect, usize)>,
}

impl Markers {
    pub fn at(&self, pos: Pos2) -> Option<&Spot> {
        self.hits.iter().find(|(r, _)| r.contains(pos)).map(|(_, i)| &self.spots[*i])
    }
}

/// Draw the ticks and labels over the spectrum.
pub fn draw(p: &egui::Painter, spots: Vec<Spot>, cfg: &LoggerConfig, x_of: impl Fn(f64) -> f32, spec: Rect) -> Markers {
    let font = FontId::proportional(11.0);
    let room = ((spec.height() - FIRST_ROW - 4.0) / ROW_H).max(0.0) as usize;
    let rows = if cfg.show_labels { (cfg.label_rows as usize).min(room) } else { 0 };
    let width = |t: &str| p.layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x;
    let placed = place(&spots, x_of, width, spec.left(), spec.right(), rows);
    let top = spec.top() + FIRST_ROW;
    let mut hits = Vec::new();
    // Least important first, so the best is on top; `placed` is best first.
    for m in placed.iter().rev() {
        let s = &spots[m.spot];
        let c = color(s);
        p.line_segment([Pos2::new(m.x, spec.top()), Pos2::new(m.x, spec.bottom())], Stroke::new(1.0, c.gamma_multiply(0.55)));
        if let Some((row, left)) = m.label {
            let y = top + row as f32 * ROW_H;
            let r = Rect::from_min_size(Pos2::new(left, y), vec2(width(&s.call) + LABEL_PAD * 2.0, ROW_H - 1.0));
            p.rect_filled(r, 2.0, Color32::from_black_alpha(190));
            p.text(Pos2::new(left + LABEL_PAD, r.center().y), Align2::LEFT_CENTER, &s.call, font.clone(), c);
            hits.push((r, m.spot));
        }
    }
    // A tick alone is picked up in the top of the spectrum, leaving the rest
    // to the click that tunes to a signal; labels come first.
    for m in placed.iter().filter(|m| m.label.is_none()) {
        let r = Rect::from_min_max(Pos2::new(m.x - TICK_REACH, spec.top()), Pos2::new(m.x + TICK_REACH, top + 2.0 * ROW_H));
        hits.push((r, m.spot));
    }
    Markers { spots, hits }
}

/// What the pointer shows over a spot.
pub fn details(s: &Spot, freq: &str, send: bool) -> String {
    let mut lines = vec![format!("{}  {freq}{}", s.call, if s.mode_text.is_empty() { String::new() } else { format!("  {}", s.mode_text) })];
    if !s.country.is_empty() {
        lines.push(s.country.clone());
    }
    if s.pota() {
        lines.push(s.park().map_or("POTA".to_string(), |p| format!("POTA {p}")));
    }
    if s.alert() {
        lines.push(format!("alert: {}", s.rules.join(", ")));
    }
    match s.status {
        SpotStatus::NewEntity => lines.push("new one!".into()),
        SpotStatus::NewSlot => lines.push("new band or mode".into()),
        SpotStatus::Worked => lines.push("already worked".into()),
        SpotStatus::Other => {}
    }
    if !s.comment.is_empty() {
        lines.push(s.comment.clone());
    }
    let mins = s.seen.elapsed().as_secs() / 60;
    let by = if s.spotter.is_empty() { String::new() } else { format!("by {} ", s.spotter) };
    lines.push(format!("{by}{mins} min ago, via {}", s.source));
    lines.push(if send { "click → tune here, fill in the logger" } else { "click → tune here" }.into());
    lines.join("\n")
}

/// Go to a spot, in its mode if the settings say so and the radio has it.
/// The mode goes first: changing it moves the frequency shown on some radios.
pub fn tune(rig: &Rig, s: &RadioState, spot: &Spot, cfg: &LoggerConfig) {
    let mode = spot.radio_mode().filter(|m| cfg.click_sets_mode && !s.rx_on_b() && s.model.modes.contains(m));
    if let Some(mode) = mode.filter(|m| s.mode_a != Some(*m)) {
        rig.send(set_mode_cmd(mode));
    }
    rig.send(set_freq_cmd_for(s.model, s.rx_on_b(), s.model.clamp_freq(spot.hz)));
}

fn ago(at: Instant) -> String {
    match at.elapsed().as_secs() {
        s if s < 90 => format!("{s} s ago"),
        s if s < 5400 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

/// Logger Connect's part of the Settings tab.
pub fn settings_panel(app: &mut App, ui: &mut Ui) {
    let before = app.services.config.logger.clone();
    let stats = app.services.spots.stats();
    let status = app.services.logger_status();
    let vfo = app.rig().state().rx_freq();
    let scope_here = crate::scope::visible(app);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Logger Connect").strong().size(16.0).color(ink(AMBER)));
        ui.label(RichText::new("Show DX spots from your logging program on the band scope. RigHelm only listens: spots never change anything by themselves.").color(DIM));
        let cfg = &mut app.services.config.logger;
        ui.checkbox(&mut cfg.enabled, "Listen for spots from logging programs");
        if !cfg.enabled {
            return;
        }

        // Is it working?
        match &status {
            Some(Ok(addr)) => {
                let text = if stats.sources.is_empty() { format!("Listening on {addr}, nothing received yet") } else { format!("Listening on {addr} · {} spots held", stats.held) };
                ui.label(RichText::new(text).color(ink(GREEN)));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Can't listen on port {}: {e}. Another program may be using it; try another port.", cfg.port)).color(ink(AMBER)));
            }
            None => {}
        }
        if !stats.sources.is_empty() {
            egui::Grid::new("logger-sources").num_columns(4).spacing([16.0, 2.0]).show(ui, |ui| {
                for s in &stats.sources {
                    ui.label(&s.name);
                    ui.label(s.from.ip().to_string());
                    ui.label(format!("{} spots", s.spots));
                    ui.label(RichText::new(format!("last {}", ago(s.last))).color(DIM));
                    ui.end_row();
                }
            });
        }
        if stats.refused > 0 {
            ui.label(RichText::new(format!("{} packets refused: that format is turned off below.", stats.refused)).color(ink(AMBER)));
        }
        if stats.bad > 0 {
            let why = stats.last_bad.as_deref().unwrap_or("");
            ui.label(RichText::new(format!("{} packets not understood (last: {why}).", stats.bad)).color(ink(AMBER)));
        }
        ui.horizontal(|ui| {
            let listening = matches!(status, Some(Ok(_)));
            let test = ui.add_enabled(listening && (cfg.accept_qlog || cfg.accept_n1mm), egui::Button::new("Send a test spot"));
            if test.on_hover_text("Sends a spot called TEST to the port RigHelm listens on, at the dial frequency. It should appear on the band scope, unless the Show list below hides it.").clicked() {
                if let Err(e) = logger::send_test(cfg, vfo) {
                    log::warn!("test spot: {e}");
                }
            }
            if ui.button("Clear spots").clicked() {
                app.services.spots.clear();
            }
        });
        if !scope_here {
            ui.label(RichText::new("No band scope is showing, so spots have nowhere to appear yet.").color(ink(AMBER)));
        }

        ui.add_space(4.0);
        ui.label(RichText::new("Connection").strong());
        ui.horizontal(|ui| {
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut cfg.port).range(1024..=65535).speed(0.0));
            if ui.small_button("reset").on_hover_text(format!("Back to {DEFAULT_PORT}, the port N1MM+ recommends")).clicked() {
                cfg.port = DEFAULT_PORT;
            }
            ui.selectable_value(&mut cfg.lan, false, "This computer").on_hover_text("For a logger running on this computer.");
            ui.selectable_value(&mut cfg.lan, true, "This computer and the network").on_hover_text("Also for loggers on other computers. Anyone on the network can then send spots, which only ever appear as markers.");
        });
        if cfg.lan {
            let addrs = ftdx3000_rig::remote::local_addresses();
            ui.label(RichText::new(format!("Send to {} from the other computer; the firewall must let UDP port {} in.", addrs.first().map_or("this computer's address", String::as_str), cfg.port)).color(DIM));
        }
        ui.horizontal(|ui| {
            ui.label("Accept");
            ui.checkbox(&mut cfg.accept_qlog, "QLog").on_hover_text("QLog's network notifications (JSON)");
            ui.checkbox(&mut cfg.accept_n1mm, "N1MM+ format").on_hover_text("The <spot> broadcast of N1MM+, also sent by DXLog and Logger32 (XML)");
        });
        let addr = format!("{}:{}", if cfg.lan { "<this computer>" } else { "127.0.0.1" }, cfg.port);
        egui::CollapsingHeader::new("How to set up your logger").id_salt("logger-howto").show(ui, |ui| {
            ui.label("QLog");
            ui.horizontal(|ui| {
                ui.label(format!("  Settings → Network → Notifications: set DX Spots to {addr}, and Spot Alerts too for your alert rules."));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(addr.clone());
                }
            });
            ui.label("N1MM+");
            ui.label(format!("  Config → Configure Ports… → Broadcast Data: tick Spots and set its address to {addr}."));
            ui.label("DXLog, Logger32");
            ui.label(format!("  Turn on their N1MM-format spot broadcast and point it at {addr}."));
        });

        ui.add_space(4.0);
        ui.label(RichText::new("On the band scope").strong());
        ui.checkbox(&mut cfg.show_on_scope, "Show spots");
        ui.horizontal(|ui| {
            ui.checkbox(&mut cfg.show_labels, "Callsign labels");
            ui.add_enabled_ui(cfg.show_labels, |ui| {
                ui.label("in up to");
                ui.add(egui::DragValue::new(&mut cfg.label_rows).range(1..=6).suffix(" rows"));
            });
        });
        ui.horizontal(|ui| {
            ui.label("Forget a spot after");
            ui.add(egui::Slider::new(&mut cfg.max_age_min, 1..=120).suffix(" min"));
        });
        ui.horizontal(|ui| {
            ui.label("Show");
            if ui.small_button("All").clicked() {
                cfg.show_all(true);
            }
            if ui.small_button("None").clicked() {
                cfg.show_all(false);
            }
        });
        let on_dark = |text: &str, c: Color32| RichText::new(text).color(c).background_color(Color32::from_rgb(9, 10, 14));
        ui.checkbox(&mut cfg.show_alerts, on_dark("Alerts", ALERT))
            .on_hover_text("Spots that matched an alert rule in your logger. QLog needs its Spot Alerts output set for these; N1MM-format loggers don't send alerts.");
        ui.checkbox(&mut cfg.show_pota, "POTA (Parks on the Air)")
            .on_hover_text("Spots whose comment says POTA or gives a park reference like US-2303. Colored by what your logger says about the station.");
        ui.checkbox(&mut cfg.show_new, on_dark("New ones", NEW_ENTITY)).on_hover_text("A new country in your log, or a multiplier in a contest.");
        ui.checkbox(&mut cfg.show_new_slot, on_dark("New band or mode", NEW_SLOT)).on_hover_text("A country you need on this band or mode, or a new contact in a contest.");
        ui.checkbox(&mut cfg.show_worked, on_dark("Already worked", WORKED)).on_hover_text("QLog: that country on this band and mode, not always that station. In a contest: dupes.");
        ui.checkbox(&mut cfg.show_other, on_dark("No status from the logger", PLAIN))
            .on_hover_text("Spots your logger gave no verdict on: QLog says unknown, or an N1MM-format logger sends nothing useful (CQ, busy, or no status).");
        ui.horizontal(|ui| {
            ui.label("Modes");
            ui.checkbox(&mut cfg.show_cw, "CW");
            ui.checkbox(&mut cfg.show_phone, "Phone");
            ui.checkbox(&mut cfg.show_digital, "Digital");
        });
        ui.label(RichText::new("A spot appears if anything about it is ticked above, in a mode that's ticked.").color(DIM));
        if (cfg.show_alerts, cfg.show_pota, cfg.show_new, cfg.show_new_slot, cfg.show_worked, cfg.show_other) == (false, false, false, false, false, false) {
            ui.label(RichText::new("Nothing is ticked, so no spots will show.").color(ink(AMBER)));
        }
        ui.checkbox(&mut cfg.click_sets_mode, "Clicking a spot sets the radio's mode too")
            .on_hover_text("Otherwise only the frequency changes. Phone is LSB below 10 MHz and USB above (60 m USB), digital is DATA-U.");
        ui.checkbox(&mut cfg.send_call, "Clicking a spot fills in the callsign in the logger")
            .on_hover_text("Sends the call the way WSJT-X does when you pick a station, so the logger looks it up. It replaces whatever is in the logger's new contact form. Works with QLog; N1MM+ has no way to receive it.");
        if cfg.send_call {
            ui.horizontal(|ui| {
                ui.label("Logger's WSJT-X port");
                ui.add(egui::TextEdit::singleline(&mut cfg.send_to).desired_width(150.0));
                if ui.small_button("reset").on_hover_text(format!("Back to {DEFAULT_SEND_TO}, QLog on this computer")).clicked() {
                    cfg.send_to = DEFAULT_SEND_TO.into();
                }
            });
            match &stats.last_sent {
                Some(Ok(call)) => {
                    ui.label(RichText::new(format!("Last sent to the logger: {call}")).color(ink(GREEN)));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("Couldn't send to the logger: {e}")).color(ink(AMBER)));
                }
                None => {}
            }
        }
    });
    if app.services.config.logger != before {
        app.services.apply_logger();
        let _ = app.services.config.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ftdx3000_rig::logger::SpotMode;

    fn spot(call: &str, hz: u64, status: SpotStatus, alert: bool) -> Spot {
        Spot {
            call: call.into(),
            hz,
            mode: SpotMode::Cw,
            mode_text: String::new(),
            status,
            comment: String::new(),
            spotter: String::new(),
            country: String::new(),
            rules: if alert { vec!["r".into()] } else { Vec::new() },
            source: "QLog".into(),
            seen: Instant::now(),
        }
    }

    fn lay(spots: &[Spot], rows: usize) -> Vec<Placed> {
        place(spots, |hz| hz as f32, |_| 40.0, 0.0, 200.0, rows)
    }

    fn of<'a>(p: &'a [Placed], spot: usize) -> &'a Placed {
        p.iter().find(|m| m.spot == spot).unwrap()
    }

    #[test]
    fn apart_they_share_a_row() {
        let s = [spot("A", 10, SpotStatus::Other, false), spot("B", 100, SpotStatus::Other, false)];
        let p = lay(&s, 3);
        assert_eq!(of(&p, 0).label.unwrap().0, 0);
        assert_eq!(of(&p, 1).label.unwrap().0, 0);
    }

    #[test]
    fn crowded_labels_stack_and_the_rest_keep_only_a_tick() {
        let s: Vec<Spot> = (0..4).map(|i| spot("A", 10 + i, SpotStatus::Other, false)).collect();
        let p = lay(&s, 3);
        assert_eq!(p.iter().filter(|m| m.label.is_some()).count(), 3);
        assert_eq!(p.len(), 4, "every spot keeps its tick");
    }

    #[test]
    fn what_matters_most_gets_a_label() {
        let s = [spot("W", 10, SpotStatus::Worked, false), spot("O", 11, SpotStatus::Other, false), spot("N", 12, SpotStatus::NewEntity, false), spot("A", 13, SpotStatus::Worked, true)];
        let p = lay(&s, 2);
        assert_eq!(of(&p, 3).label.unwrap().0, 0, "alert first");
        assert_eq!(of(&p, 2).label.unwrap().0, 1, "new one next");
        assert!(of(&p, 0).label.is_none());
        assert!(of(&p, 1).label.is_none());
    }

    #[test]
    fn a_label_at_the_right_edge_stays_on_screen() {
        let s = [spot("A", 190, SpotStatus::Other, false)];
        let (_, left) = lay(&s, 1)[0].label.unwrap();
        assert!(left + 40.0 + LABEL_PAD * 2.0 <= 200.0);
    }

    #[test]
    fn no_rows_means_ticks_only() {
        let p = lay(&[spot("A", 10, SpotStatus::Other, false)], 0);
        assert!(p[0].label.is_none());
    }
}
