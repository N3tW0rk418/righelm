//! Band scope: the wide spectrum and waterfall from an RTL-SDR dongle on the
//! radio's IF OUT or RX OUT jack (here, or on the remote radio's computer).
//! It appears by itself when a dongle is found and keeps the VFO in view.
//!
//! Clicking a signal tunes straight to it: the scope takes a close look at
//! full resolution, finds the signal, and tunes the way the mode needs (CW
//! zero-beat, SSB on the suppressed carrier, AM on the carrier, digital on
//! the calling frequency). In CW the audio then fine-tunes the zero-beat.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Mesh, Pos2, Rect, RichText, Sense, Shape, Stroke, TextureHandle, TextureOptions, Ui, vec2,
};
use ftdx3000_cat::types::Mode;
use ftdx3000_cat::{RadioState, set_freq_cmd_for};
use ftdx3000_rig::scope::{Detail, ScopeRow, ScopeStatus, ScopeSubscription, ScopeTap, ScopeView, Signal, level_db};
use ftdx3000_rig::{Rig, UiPrefs};

use crate::waterfall::{cw_upper, passband, pitch, rf_of_audio, rx_vfo};
use crate::widgets::{self, AMBER, RED, ink};

/// `scope_span` for the Band button (0 is Full, the rest are in Hz).
const BAND_SPAN: u32 = 1;
/// What the Band span shows away from any band.
const BAND_SPAN_ELSEWHERE: f64 = 200_000.0;
const COLS: usize = 1024;
const ROWS: usize = 300;
/// Where there's no data (outside what the dongle covers).
const NO_DATA: Color32 = Color32::from_rgb(14, 15, 20);
const TRACE: Color32 = Color32::from_rgb(110, 214, 255);
const PEAK: Color32 = Color32::from_rgba_premultiplied(150, 150, 170, 90);
const GRID: Color32 = Color32::from_rgba_premultiplied(40, 46, 60, 60);
const CW_DATA: Color32 = Color32::from_rgb(64, 120, 220);
const PHONE: Color32 = Color32::from_rgb(70, 180, 110);
const DIGI: Color32 = Color32::from_rgb(190, 120, 255);
/// Span choices in Hz; 0 = everything the dongle covers.
const SPANS: [(u32, &str); 8] =
    [(10_000, "10k"), (25_000, "25k"), (50_000, "50k"), (100_000, "100k"), (200_000, "200k"), (500_000, "500k"), (1_000_000, "1M"), (0, "Full")];

/// US amateur sub-bands: (lo, hi, phone allowed). Below the phone segment is
/// CW and data.
const PLAN: &[(f64, f64, bool)] = &[
    (1_800_000.0, 1_843_000.0, false),
    (1_843_000.0, 2_000_000.0, true),
    (3_500_000.0, 3_600_000.0, false),
    (3_600_000.0, 4_000_000.0, true),
    (7_000_000.0, 7_125_000.0, false),
    (7_125_000.0, 7_300_000.0, true),
    (10_100_000.0, 10_150_000.0, false),
    (14_000_000.0, 14_150_000.0, false),
    (14_150_000.0, 14_350_000.0, true),
    (18_068_000.0, 18_110_000.0, false),
    (18_110_000.0, 18_168_000.0, true),
    (21_000_000.0, 21_200_000.0, false),
    (21_200_000.0, 21_450_000.0, true),
    (24_890_000.0, 24_930_000.0, false),
    (24_930_000.0, 24_990_000.0, true),
    (28_000_000.0, 28_300_000.0, false),
    (28_300_000.0, 29_700_000.0, true),
    (50_000_000.0, 50_100_000.0, false),
    (50_100_000.0, 54_000_000.0, true),
];

/// Calling frequencies (USB dial) of the weak-signal digital modes.
const DIGITAL: &[(f64, &str)] = &[
    (1_840_000.0, "FT8"),
    (3_573_000.0, "FT8"),
    (3_575_000.0, "FT4"),
    (5_357_000.0, "FT8"),
    (7_047_500.0, "FT4"),
    (7_074_000.0, "FT8"),
    (10_136_000.0, "FT8"),
    (10_140_000.0, "FT4"),
    (14_074_000.0, "FT8"),
    (14_080_000.0, "FT4"),
    (18_100_000.0, "FT8"),
    (18_104_000.0, "FT4"),
    (21_074_000.0, "FT8"),
    (21_140_000.0, "FT4"),
    (24_915_000.0, "FT8"),
    (24_919_000.0, "FT4"),
    (28_074_000.0, "FT8"),
    (28_180_000.0, "FT4"),
    (50_313_000.0, "FT8"),
    (50_318_000.0, "FT4"),
];

/// The digital calling frequency whose 3 kHz window `rf` is in.
fn digital_dial(rf: f64) -> Option<(f64, &'static str)> {
    DIGITAL.iter().copied().find(|(f, _)| rf >= *f && rf <= f + 3_000.0)
}

/// Where a click tunes the radio.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    dial: u64,
    /// The signal it found, if any.
    signal: Option<Signal>,
    how: &'static str,
}

/// A click waiting for the scope's close look.
struct Pending {
    answer: Receiver<Detail>,
    rf: f64,
    search: f64,
    at: Instant,
}

/// The hover preview: where a click would go.
struct Preview {
    answer: Option<Receiver<Detail>>,
    rf: f64,
    search: f64,
    asked: Instant,
    target: Option<Target>,
}

#[derive(Default)]
pub struct BandScope {
    sub: Option<ScopeSubscription>,
    texture: Option<TextureHandle>,
    /// Rows received, newest first, each in its own frequency range.
    history: VecDeque<ScopeRow>,
    /// Frequency range (start, column width) the texture shows.
    drawn: Option<(f64, f64)>,
    /// Texture row the next line goes to (moves upwards, newest at the top).
    row: usize,
    /// Centre of the view (moves to keep the VFO in sight).
    center: Option<f64>,
    /// Smoothed spectrum and decaying peaks per column (dB, None = no data).
    trace: Vec<Option<f32>>,
    peak: Vec<Option<f32>>,
    noise: Option<f32>,
    lut: Vec<Color32>,
    last_row: Option<Instant>,
    transmitting: bool,
    click: Option<Pending>,
    preview: Option<Preview>,
    /// CW: after tuning, fine-tune the zero-beat from the audio once the
    /// radio has settled (audio waterfall line count, when tuned).
    zero_beat: Option<(u64, Instant)>,
    /// What the last click did, shown for a moment.
    tuned: Option<(String, Instant)>,
    /// The scope's own window: where it opened (kept while it's open, so
    /// moving it isn't fought), frames since and the size it settled at
    /// (some desktops give a little less than asked; only the user's
    /// resizing is kept), and its keyboard for push-to-talk.
    window_opened: Option<(Option<[i32; 2]>, [u32; 2])>,
    window_settled: (u32, Option<egui::Vec2>),
    pub window_keys: Option<crate::Keys>,
}

impl BandScope {
    /// Stop watching (the dongle is closed when nobody watches).
    pub fn pause(&mut self) {
        self.sub = None;
        self.preview = None;
    }
}

/// The amateur band the dial is in.
fn band_at(s: &RadioState, hz: f64) -> Option<(f64, f64)> {
    s.model.bands.iter().filter(|b| b.label != "GEN").map(|b| (b.lo as f64, b.hi as f64)).find(|(lo, hi)| (*lo..=*hi).contains(&hz))
}

/// The band to fit the view to: only with the Band span, the others follow
/// the dial.
fn view_band(prefs: &UiPrefs, s: &RadioState, vfo: f64) -> Option<(f64, f64)> {
    if prefs.scope_band_span && prefs.scope_span == BAND_SPAN { band_at(s, vfo) } else { None }
}

/// The span to show, for the dongle's coverage and the band to fit, if any.
fn span_hz(prefs: &UiPrefs, covers: Option<(f64, f64)>, band: Option<(f64, f64)>) -> f64 {
    let full = covers.map_or(1_600_000.0, |(lo, hi)| hi - lo);
    match prefs.scope_span {
        0 => full,
        // A little more than the band, so its edges show.
        BAND_SPAN => band.map_or(BAND_SPAN_ELSEWHERE, |(lo, hi)| (hi - lo) * 1.1).min(full),
        hz => (hz as f64).min(full),
    }
}

/// Keep the VFO in view: show everything when the span covers the dongle's
/// whole range, otherwise move the view a page when the VFO nears an edge.
fn follow(center: Option<f64>, vfo: f64, span: f64, covers: Option<(f64, f64)>) -> f64 {
    if let Some((lo, hi)) = covers {
        if span >= (hi - lo) * 0.999 {
            return (lo + hi) / 2.0;
        }
    }
    let c = match center {
        Some(c) if (vfo - c).abs() <= span * 0.35 => c,
        _ => vfo,
    };
    match covers {
        // Don't show emptiness beside the dongle's range when there's data.
        Some((lo, hi)) if hi - lo > span => c.clamp(lo + span / 2.0, hi - span / 2.0),
        _ => c,
    }
}

/// Fit a view to the band: centred on it, or inside its edges where the
/// dongle can't show it all (10 m), and within what the dongle covers.
fn in_band(center: f64, span: f64, band: Option<(f64, f64)>, covers: Option<(f64, f64)>) -> f64 {
    let Some((lo, hi)) = band else { return center };
    if covers.is_some_and(|(a, b)| span >= (b - a) * 0.999) {
        return center;
    }
    let c = if span >= hi - lo { (lo + hi) / 2.0 } else { center.clamp(lo + span / 2.0, hi - span / 2.0) };
    match covers {
        Some((a, b)) => c.clamp(a + span / 2.0, b - span / 2.0),
        None => c,
    }
}

/// Radio frequencies of the receive filter's passband.
fn rf_passband(s: &RadioState) -> Option<(f64, f64)> {
    let (dial, mode) = rx_vfo(s)?;
    match mode {
        m if m.is_am_fm() => {
            let w = s.width_hz().unwrap_or(if matches!(mode, Mode::Am | Mode::AmN) { 6000 } else { 12000 }) as f64;
            Some((dial as f64 - w / 2.0, dial as f64 + w / 2.0))
        }
        _ => {
            let (lo, hi) = passband(s)?;
            let (a, b) = (rf_of_audio(s, lo.max(0.0))?, rf_of_audio(s, hi)?);
            Some((a.min(b), a.max(b)))
        }
    }
}

/// Where to put the dial for a click at `rf`, given the signal found there:
/// * CW: zero-beat (the signal's peak heard at the CW pitch).
/// * SSB: the suppressed carrier, just outside the edge of the voice.
/// * AM/FM: the carrier.
/// * Digital: the calling frequency when inside its window, else the signal
///   centred in the passband.
fn target(s: &RadioState, signal: Option<Signal>, rf: f64) -> Option<Target> {
    let (dial, mode) = rx_vfo(s)?;
    // The dial that makes a signal at `at` heard at audio `af`.
    let heard_at = |af: f32, at: f64| -> Option<f64> { Some(dial as f64 + at - rf_of_audio(s, af)?) };
    let centre_af = || passband(s).map_or(1500.0, |(lo, hi)| (lo + hi) / 2.0);
    let middle = signal.map_or(rf, |g| (g.lo_hz + g.hi_hz) / 2.0);
    let (hz, how) = match mode {
        Mode::Cw | Mode::CwR => (heard_at(pitch(s), signal.map_or(rf, |g| g.peak_hz))?, "zero-beat"),
        Mode::DataUsb | Mode::DataLsb => match digital_dial(rf) {
            Some((f, name)) => (f, name),
            None => (heard_at(centre_af(), middle)?, "centred"),
        },
        Mode::RttyUsb | Mode::RttyLsb | Mode::Psk => (heard_at(centre_af(), middle)?, "centred"),
        Mode::Usb | Mode::Lsb => match signal {
            Some(g) if g.width_hz() >= 500.0 => (if mode == Mode::Usb { g.lo_hz - 150.0 } else { g.hi_hz + 150.0 }, "on the carrier"),
            Some(g) => (heard_at(centre_af(), g.peak_hz)?, "centred"),
            None => (rf, ""),
        },
        _ => (signal.map_or(rf, |g| g.peak_hz), "on the carrier"),
    };
    // CW tunes to the hertz; everything else to 10 Hz.
    let hz = if mode.is_cw() { hz.round() } else { (hz / 10.0).round() * 10.0 };
    Some(Target { dial: s.model.clamp_freq((hz as i64).max(0) as u64), signal, how })
}

/// How far either side of the pointer to look for a signal.
fn search_hz(col_hz: f64) -> f64 {
    (col_hz * 4.0).clamp(300.0, 20_000.0)
}

/// A close look around `rf`: wide enough for a whole SSB signal either side.
fn ask(tap: &ScopeTap, rf: f64, search: f64) -> Receiver<Detail> {
    tap.ask_detail(rf - search - 3_500.0, rf + search + 3_500.0)
}

/// A round step for scale marks: about 8 across the span.
fn nice_step(span: f64) -> f64 {
    let raw = span / 8.0;
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag)
}

fn mhz_label(hz: f64, step: f64) -> String {
    let decimals = if step >= 100_000.0 { 1 } else if step >= 10_000.0 { 2 } else if step >= 1_000.0 { 3 } else { 4 };
    format!("{:.*}", decimals, hz / 1e6)
}

fn freq_text(hz: f64) -> String {
    widgets::format_freq(hz.round().max(0.0) as u64).trim().to_string()
}

/// A row's noise floor: a low percentile of what it covers.
fn row_noise(row: &ScopeRow) -> Option<f32> {
    let mut v: Vec<u8> = row.cols.iter().copied().filter(|&v| v > 0).collect();
    if v.len() < 8 {
        return None;
    }
    v.sort_unstable();
    level_db(v[v.len() / 5])
}

/// The level a stored row has at radio frequency `f`.
fn level_at(row: &ScopeRow, f: f64) -> Option<f32> {
    let src = (f - row.start_hz) / row.col_hz;
    if src < 0.0 { None } else { row.cols.get(src as usize).copied().and_then(level_db) }
}

impl BandScope {
    /// A row's colours. With auto level each row sits on its own noise
    /// floor, so the dongle's gain control doesn't stripe the waterfall.
    fn pixels(&self, row: &ScopeRow, start: f64, col_hz: f64, floor: f32, range: f32, auto: bool) -> Vec<Color32> {
        let floor = match (auto, row_noise(row), self.noise) {
            (true, Some(rn), Some(n)) => floor + (rn - n),
            _ => floor,
        };
        (0..COLS)
            .map(|c| match level_at(row, start + (c as f64 + 0.5) * col_hz) {
                Some(db) => self.lut[(((db - floor) / range).clamp(0.0, 1.0) * 255.0) as usize],
                None => NO_DATA,
            })
            .collect()
    }

    /// Redraw the whole waterfall for a new view, from the stored rows.
    fn redraw(&mut self, ctx: &egui::Context, start: f64, col_hz: f64, floor: f32, range: f32, auto: bool) {
        let mut img = ColorImage::filled([COLS, ROWS], NO_DATA);
        // Zoomed in a long way: old rows would be stretched into blocks, so
        // start the waterfall afresh.
        self.history.retain(|r| r.col_hz <= col_hz * 3.0);
        for (i, r) in self.history.iter().enumerate().take(ROWS) {
            let px = self.pixels(r, start, col_hz, floor, range, auto);
            img.pixels[i * COLS..(i + 1) * COLS].copy_from_slice(&px);
        }
        match &mut self.texture {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("band-scope", img, TextureOptions::LINEAR)),
        }
        // Newest is row 0; the next one goes above it (at the bottom, wrapped).
        self.row = ROWS - 1;
        self.drawn = Some((start, col_hz));
        self.trace.clear();
        self.peak.clear();
    }

    /// Take in new rows for the view (`start`, `col_hz`).
    fn ingest(&mut self, ctx: &egui::Context, prefs: &UiPrefs, start: f64, col_hz: f64) {
        if self.lut.is_empty() {
            self.lut = crate::waterfall::palette();
        }
        let mut new = Vec::new();
        if let Some(sub) = &self.sub {
            while let Ok(r) = sub.rows.try_recv() {
                new.push(r);
            }
        }
        if let Some(r) = new.last() {
            self.transmitting = r.transmitting;
        }
        // The picture during transmit means nothing: keep the last one.
        new.retain(|r| !r.transmitting);
        for r in &new {
            // Noise: a low percentile of what the dongle covers, smoothed.
            if let Some(db) = row_noise(r) {
                self.noise = Some(self.noise.map_or(db, |n| 0.95 * n + 0.05 * db));
            }
        }
        let (floor, range) = levels(prefs, self.noise.unwrap_or(-90.0));
        let moved = self.drawn.is_none_or(|(s, w)| (s - start).abs() > col_hz * 0.25 || (w - col_hz).abs() > col_hz * 1e-6);
        for r in new {
            self.last_row = Some(Instant::now());
            self.history.push_front(r);
            self.history.truncate(ROWS);
            if moved {
                continue;
            }
            let r = &self.history[0];
            let px = self.pixels(r, start, col_hz, floor, range, prefs.scope_auto);
            let cols: Vec<Option<f32>> = (0..COLS).map(|c| level_at(r, start + (c as f64 + 0.5) * col_hz)).collect();
            if self.trace.len() != COLS {
                self.trace = cols.clone();
                self.peak = cols;
            } else {
                for ((t, p), c) in self.trace.iter_mut().zip(self.peak.iter_mut()).zip(cols) {
                    *t = match (*t, c) {
                        (Some(a), Some(b)) => Some(0.55 * a + 0.45 * b),
                        (_, c) => c,
                    };
                    // Peaks fall back slowly (about 6 dB a second at normal speed).
                    *p = match (*p, c) {
                        (Some(a), Some(b)) => Some((a - 0.4).max(b)),
                        (_, c) => c,
                    };
                }
            }
            if let Some(t) = &mut self.texture {
                t.set_partial([0, self.row], ColorImage::new([COLS, 1], px), TextureOptions::LINEAR);
                self.row = (self.row + ROWS - 1) % ROWS;
            }
        }
        if moved {
            self.redraw(ctx, start, col_hz, floor, range, prefs.scope_auto);
        }
    }
}

/// dB at the bottom of the colour scale, and the span of the scale.
fn levels(prefs: &UiPrefs, noise: f32) -> (f32, f32) {
    let range = prefs.scope_range_db.clamp(20, 120) as f32;
    let floor = if prefs.scope_auto { noise - range * 0.12 } else { prefs.scope_floor_db as f32 };
    (floor, range)
}

/// A rectangle filled with a vertical gradient.
fn gradient(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.right_bottom(), bottom);
    m.colored_vertex(r.left_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

/// Text on a dark rounded pill.
fn pill(p: &egui::Painter, at: Pos2, anchor: Align2, text: &str, size: f32, color: Color32) {
    let galley = p.layout_no_wrap(text.to_string(), FontId::monospace(size), color);
    let r = anchor.anchor_size(at, galley.size()).expand2(vec2(5.0, 2.0));
    p.rect_filled(r, CornerRadius::same(4), Color32::from_black_alpha(190));
    p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, color.gamma_multiply(0.35)), egui::StrokeKind::Inside);
    p.galley(r.min + vec2(5.0, 2.0), galley, color);
}

/// Send the radio to `t`, and remember to fine-tune a CW zero-beat.
fn tune(bs: &mut BandScope, rig: &Rig, s: &RadioState, t: &Target, audio_lines: u64) {
    rig.send(set_freq_cmd_for(s.model, s.rx_on_b(), t.dial));
    let how = if t.how.is_empty() { String::new() } else { format!(" · {}", t.how) };
    bs.tuned = Some((format!("{}{how}", freq_text(t.dial as f64)), Instant::now()));
    bs.zero_beat = rx_vfo(s).filter(|(_, m)| m.is_cw() && t.signal.is_some()).map(|_| (audio_lines, Instant::now()));
}

/// Whether the band scope has anything to show (a dongle, and not switched off).
pub fn visible(app: &crate::App) -> bool {
    let status = app.services.scope_status();
    app.services.config.ui.show_scope && (status.present || status.running)
}

/// The band scope is showing in its own window.
pub fn detached(app: &crate::App) -> bool {
    app.services.config.ui.scope_window && visible(app)
}

/// The band scope's own window.
pub fn window_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("righelm_band_scope")
}

/// The band scope's own window, while it's popped out. Closing the window
/// puts the scope back on the Operate page. eframe draws it on its own
/// schedule, so it keeps going while the main window is covered or minimized.
pub fn window(app: &mut crate::App, ctx: &egui::Context, shared: &crate::Shared) {
    if !detached(app) {
        app.scope.window_opened = None;
        app.scope.window_settled = (0, None);
        app.scope.window_keys = None;
        return;
    }
    let prefs = &app.services.config.ui;
    // Placed where it was last time (where the desktop lets programs place
    // windows), then left alone while it's open.
    let (pos, size) = *app.scope.window_opened.get_or_insert((prefs.scope_window_pos, prefs.scope_window_size));
    let mut builder = egui::ViewportBuilder::default()
        .with_title("RigHelm — Band scope")
        .with_app_id("righelm")
        .with_icon(eframe::icon_data::from_png_bytes(crate::ICON_PNG).expect("embedded icon"))
        .with_inner_size([size[0].max(480) as f32, size[1].max(300) as f32])
        .with_min_inner_size([480.0, 300.0]);
    if let Some([x, y]) = pos {
        builder = builder.with_position([x as f32, y as f32]);
    }
    if ctx.embed_viewports() {
        // No separate windows here: it's drawn inside this one, right now.
        ctx.show_viewport_immediate(window_id(), builder, |ui, _| window_contents(app, ui));
        return;
    }
    let shared = shared.clone();
    ctx.show_viewport_deferred(window_id(), builder, move |ui, _| {
        // Never wait on the main window (it is never mid-frame when this
        // runs, but if it were, skipping a frame beats a hang).
        match shared.try_lock() {
            Some(mut app) => window_contents(&mut app, ui),
            None => ui.ctx().request_repaint(),
        }
    });
}

fn window_contents(app: &mut crate::App, ui: &mut Ui) {
    let rig = app.rig().clone();
    let s = rig.state().clone();
    let was_out = app.services.config.ui.scope_window;
    app.space_ptt(ui.ctx());
    egui::CentralPanel::default().show(ui, |ui| {
        let live = rig.link().is_open() && s.power_on != Some(false);
        if live && !ui.ctx().egui_wants_keyboard_input() {
            crate::operate::keyboard(ui, &rig, &s, app.services.config.ui.tune_step);
        }
        show(app, ui, &rig, &s);
    });
    let (close, outer, size) = ui.input(|i| (i.viewport().close_requested(), i.viewport().outer_rect, i.content_rect().size()));
    let prefs = &mut app.services.config.ui;
    // The size it opened at plus the user's resizing since it settled; the
    // position only where the desktop tells it (Wayland doesn't).
    let opened = app.scope.window_opened.map_or(prefs.scope_window_size, |o| o.1);
    let settled = &mut app.scope.window_settled;
    settled.0 += 1;
    if settled.0 == 5 {
        settled.1 = Some(size);
    }
    if let Some(base) = settled.1 {
        let grown = size - base;
        prefs.scope_window_size = [(opened[0] as f32 + grown.x).max(480.0).round() as u32, (opened[1] as f32 + grown.y).max(300.0).round() as u32];
    }
    if let Some(r) = outer {
        prefs.scope_window_pos = Some([r.min.x.round() as i32, r.min.y.round() as i32]);
    }
    if close {
        prefs.scope_window = false;
    }
    // Only the main window can take this one away, and Wayland stops drawing
    // a window that's completely covered (as it is under this one full
    // screen), so get out of its way.
    if was_out && !prefs.scope_window && ui.ctx().viewport_id() == window_id() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }
}

/// The band scope, filling the height it's given (on the Operate page, or
/// in its own window).
/// The first time a dongle turns up, ask which jack it's on: RigHelm can't
/// tell, and with the wrong one the picture shows the wrong frequencies.
fn feed_question(app: &mut crate::App, ui: &mut Ui) {
    use ftdx3000_rig::scope::Feed;
    let mut chosen = None;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("An RTL-SDR dongle is plugged in. Which of the radio's jacks is it connected to?").strong().color(ink(AMBER)));
        ui.horizontal_wrapped(|ui| {
            if ui.button("IF OUT (9 MHz)").on_hover_text("The radio's first IF. The picture follows the VFO.").clicked() {
                chosen = Some(Feed::IfOut);
            }
            if ui.button("RX OUT (antenna)").on_hover_text("The receive antenna signal. The dongle tunes itself to the band.").clicked() {
                chosen = Some(Feed::RxOut);
            }
        });
        ui.label(RichText::new("You can change it later in Settings → Band scope.").weak());
    });
    if let Some(feed) = chosen {
        let cfg = &mut app.services.config.scope;
        (cfg.feed, cfg.feed_confirmed) = (feed, true);
        app.services.apply_scope();
        let _ = app.services.config.save();
    }
}

pub fn show(app: &mut crate::App, ui: &mut Ui, rig: &Rig, s: &RadioState) {
    let status: ScopeStatus = app.services.scope_status();
    if !app.services.config.ui.show_scope || !(status.present || status.running) {
        app.scope.pause();
        return;
    }
    let own_window = ui.ctx().viewport_id() != egui::ViewportId::ROOT;
    if app.services.config.ui.scope_window && !own_window {
        return; // it's in its own window
    }
    // CW: fine zero-beat from the audio once the radio has settled.
    let audio_lines = app.waterfall.lines();
    if let Some((lines, at)) = app.scope.zero_beat {
        if at.elapsed() > Duration::from_secs(3) {
            app.scope.zero_beat = None;
        } else if audio_lines >= lines + 14 && at.elapsed() > Duration::from_millis(500) {
            app.scope.zero_beat = None;
            let max_hz = app.services.config.ui.wf_max_hz as f32;
            if let (Some((dial, mode)), Some(tone)) = (rx_vfo(s), app.waterfall.tone_near(max_hz, pitch(s), 120.0)) {
                let err = (tone - pitch(s)).round() as i64;
                if err.abs() >= 3 {
                    let err = if cw_upper(s, mode, dial) { err } else { -err };
                    rig.send(set_freq_cmd_for(s.model, s.rx_on_b(), (dial as i64 + err).max(0) as u64));
                }
            }
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
    if !app.services.config.scope.feed_confirmed && app.services.config.engine.remote.is_none() && status.present {
        feed_question(app, ui);
    }
    let tap = app.services.scope_tap.clone();
    let prefs = &mut app.services.config.ui;
    let bs = &mut app.scope;
    let vfo = s.rx_freq() as f64;
    let band = view_band(prefs, s, vfo);
    let span = span_hz(prefs, status.covers, band);
    let center = in_band(follow(bs.center, vfo, span, status.covers), span, band, status.covers);
    bs.center = Some(center);
    let col_hz = span / COLS as f64;
    let start = center - span / 2.0;
    let view = ScopeView { center_hz: center, span_hz: span, cols: COLS as u16, fps: prefs.scope_fps.clamp(1, 30) as u8 };
    match &bs.sub {
        Some(sub) if sub.view() != Some(view.sanitized()) => sub.set_view(view),
        Some(_) => {}
        None => bs.sub = Some(tap.subscribe(view)),
    }
    bs.ingest(ui.ctx(), prefs, start, col_hz);
    // Keep the picture moving at the scope's own pace.
    ui.ctx().request_repaint_after(Duration::from_millis(1000 / view.fps.max(1) as u64));
    let tune_step = prefs.tune_step;

    // A click waiting for its close look: tune when it arrives, or tune
    // where clicked if it doesn't come.
    if let Some(c) = &bs.click {
        let found = c.answer.try_recv().ok().map(|d| d.find_signal(c.rf, c.search));
        if found.is_some() || c.at.elapsed() > Duration::from_millis(800) {
            let rf = c.rf;
            bs.click = None;
            if let Some(t) = target(s, found.flatten(), rf) {
                tune(bs, rig, s, &t, audio_lines);
            }
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }
    // Hover preview answers.
    if let Some(pv) = &mut bs.preview {
        if let Some(d) = pv.answer.as_ref().and_then(|rx| rx.try_recv().ok()) {
            pv.target = target(s, d.find_signal(pv.rf, pv.search), pv.rf);
            pv.answer = None;
        }
    }

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            // Rows as tall as the buttons, so a label starting one lines up.
            ui.set_row_height(22.0);
            ui.label(RichText::new("BAND SCOPE").strong().size(15.0).color(ink(AMBER))).on_hover_text(&status.text);
            let (text, hover) = if own_window {
                ("⤓ Put back", "Put the band scope back on the Operate page (or just close this window)")
            } else {
                ("⧉ Own window", "Open the band scope in a window of its own, to make it bigger or move it to another monitor")
            };
            if ui.small_button(text).on_hover_text(hover).clicked() {
                prefs.scope_window = !own_window;
            }
            ui.label("Span");
            if prefs.scope_band_span && ui.selectable_label(prefs.scope_span == BAND_SPAN, "Band").on_hover_text("Show the whole band the dial is in").clicked() {
                prefs.scope_span = BAND_SPAN;
            }
            for (hz, label) in SPANS {
                let r = ui.selectable_label(prefs.scope_span == hz, label);
                crate::demo::mark(format!("span:{label}"), r.rect);
                if r.clicked() {
                    prefs.scope_span = hz;
                }
            }
            ui.separator();
            widgets::wrap_unless_room(ui, 200.0);
            ui.add_sized([40.0, 22.0], egui::Label::new("Speed"));
            for (v, l) in [(6u32, "slow"), (15, "normal"), (30, "fast")] {
                if ui.selectable_label(prefs.scope_fps == v, l).clicked() {
                    prefs.scope_fps = v;
                }
            }
            ui.separator();
            widgets::wrap_unless_room(ui, 100.0);
            ui.checkbox(&mut prefs.scope_auto, "Auto level");
            ui.spacing_mut().slider_width = 110.0;
            if !prefs.scope_auto {
                widgets::wrap_unless_room(ui, 230.0);
                let r = ui.add(egui::Slider::new(&mut prefs.scope_floor_db, -130..=-30).text("floor dB"));
                prefs.scope_floor_db = (prefs.scope_floor_db + widgets::scroll_steps(ui, &r, 12.0) as i32).clamp(-130, -30);
            }
            widgets::wrap_unless_room(ui, 180.0);
            let r = ui.add(egui::Slider::new(&mut prefs.scope_range_db, 20..=120).text("contrast").show_value(false));
            prefs.scope_range_db = (prefs.scope_range_db + widgets::scroll_steps(ui, &r, 12.0) as i32).clamp(20, 120);
            let stale = bs.last_row.is_none_or(|t| t.elapsed().as_secs_f32() > 1.5);
            let text = if stale && !bs.transmitting { format!("{} — waiting for the dongle…", status.text) } else { status.text.clone() };
            // The status where there's room for it (it's on the title too).
            if widgets::row_room(ui) >= 120.0 {
                ui.add(egui::Label::new(RichText::new(&text).weak()).truncate()).on_hover_text(text);
            }
        });

        // Spectrum and waterfall share the height left, the spectrum up to 120.
        let width = ui.available_width();
        let plan_h = if prefs.scope_band_plan { 5.0 } else { 0.0 };
        let scale_h = 16.0;
        let graphs = (ui.available_height() - 2.0 - plan_h - scale_h).max(100.0);
        let spec_h = (graphs * 0.4).clamp(60.0, 120.0);
        let wf_h = graphs - spec_h;
        let (rect, resp) = ui.allocate_exact_size(vec2(width, spec_h + plan_h + scale_h + wf_h), Sense::click());
        let p = ui.painter_at(rect);
        let spec = Rect::from_min_size(rect.min, vec2(width, spec_h));
        let plan = Rect::from_min_size(Pos2::new(rect.left(), spec.bottom()), vec2(width, plan_h));
        let scale = Rect::from_min_size(Pos2::new(rect.left(), plan.bottom()), vec2(width, scale_h));
        let water = Rect::from_min_size(Pos2::new(rect.left(), scale.bottom()), vec2(width, wf_h));
        let view_hi = start + span;
        let x_of = |hz: f64| rect.left() + width * ((hz - start) / span) as f32;
        let hz_of = |x: f32| start + ((x - rect.left()) / width) as f64 * span;
        let vline = |hz: f64, top: f32, bottom: f32, stroke: Stroke| {
            if hz > start && hz < view_hi {
                p.line_segment([Pos2::new(x_of(hz), top), Pos2::new(x_of(hz), bottom)], stroke);
            }
        };

        crate::demo::scope(|| crate::demo::ScopeGeo { spec, start, span, vfo, peaks: bs.peak.clone() });

        // Backgrounds.
        gradient(&p, spec, Color32::from_rgb(18, 24, 38), Color32::from_rgb(6, 8, 13));
        p.rect_filled(water, 0.0, NO_DATA);
        p.rect_filled(scale, 0.0, Color32::from_rgb(9, 10, 14));

        // Waterfall, newest line at the top.
        if let Some(tex) = &bs.texture {
            let top = (bs.row + 1) % ROWS;
            let first = (ROWS - top) as f32 / ROWS as f32;
            let split = water.top() + wf_h * first;
            let uv_a = Rect::from_min_max(Pos2::new(0.0, top as f32 / ROWS as f32), Pos2::new(1.0, 1.0));
            let uv_b = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, top as f32 / ROWS as f32));
            p.image(tex.id(), Rect::from_min_max(water.min, Pos2::new(water.right(), split)), uv_a, Color32::WHITE);
            p.image(tex.id(), Rect::from_min_max(Pos2::new(water.left(), split), water.max), uv_b, Color32::WHITE);
        }

        // Grid: frequency marks and 10 dB lines.
        let (floor, range) = levels(prefs, bs.noise.unwrap_or(-90.0));
        let y_of = |db: f32| spec.bottom() - 3.0 - ((db - floor) / range).clamp(0.0, 1.0) * (spec_h - 10.0);
        let step = nice_step(span);
        let mut f = (start / step).ceil() * step;
        while f <= view_hi {
            vline(f, spec.top(), spec.bottom(), Stroke::new(1.0, GRID));
            f += step;
        }
        let mut db = (floor / 10.0).ceil() * 10.0;
        while db < floor + range {
            let y = y_of(db);
            p.line_segment([Pos2::new(spec.left(), y), Pos2::new(spec.right(), y)], Stroke::new(1.0, GRID));
            p.text(Pos2::new(spec.right() - 3.0, y - 1.0), Align2::RIGHT_BOTTOM, format!("{db:.0}"), FontId::proportional(9.0), Color32::from_gray(80));
            db += 10.0;
        }

        // Outside the amateur bands: dimmed. Band names in the corner.
        let mut bands: Vec<(f64, f64, &str)> = s.model.bands.iter().filter(|b| b.label != "GEN").map(|b| (b.lo as f64, b.hi as f64, b.label)).collect();
        bands.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut x = start;
        for (lo, hi, _) in bands.iter().copied().chain(std::iter::once((f64::MAX, f64::MAX, ""))) {
            if lo > x && x < view_hi {
                let (a, b) = (x_of(x), x_of(lo.min(view_hi)));
                p.rect_filled(Rect::from_min_max(Pos2::new(a, spec.top()), Pos2::new(b, spec.bottom())), 0.0, Color32::from_black_alpha(120));
                p.rect_filled(Rect::from_min_max(Pos2::new(a, water.top()), Pos2::new(b, water.bottom())), 0.0, Color32::from_black_alpha(70));
            }
            x = x.max(hi);
        }
        for (lo, hi, label) in &bands {
            for e in [*lo, *hi] {
                vline(e, spec.top(), water.bottom(), Stroke::new(1.0, RED.gamma_multiply(0.45)));
            }
            if *hi > start && *lo < view_hi {
                let at = Pos2::new(x_of(lo.max(start)) + 4.0, spec.top() + 3.0);
                p.text(at, Align2::LEFT_TOP, format!("{label} m"), FontId::proportional(12.0), Color32::from_gray(120));
            }
        }

        // Band plan strip and digital calling frequencies.
        if prefs.scope_band_plan {
            p.rect_filled(plan, 0.0, Color32::from_rgb(9, 10, 14));
            for &(lo, hi, phone) in PLAN {
                if hi > start && lo < view_hi {
                    let r = Rect::from_min_max(Pos2::new(x_of(lo.max(start)), plan.top() + 1.0), Pos2::new(x_of(hi.min(view_hi)), plan.bottom() - 1.0));
                    p.rect_filled(r, 0.0, if phone { PHONE } else { CW_DATA }.gamma_multiply(0.75));
                }
            }
            for &(f, name) in DIGITAL {
                if f + 3_000.0 > start && f < view_hi {
                    let r = Rect::from_min_max(Pos2::new(x_of(f), spec.bottom() - 4.0), Pos2::new(x_of(f + 3_000.0).max(x_of(f) + 2.0), spec.bottom()));
                    p.rect_filled(r, 0.0, DIGI.gamma_multiply(0.8));
                    if span <= 600_000.0 {
                        p.text(Pos2::new(r.center().x, r.top() - 1.0), Align2::CENTER_BOTTOM, name, FontId::proportional(9.0), DIGI);
                    }
                }
            }
        }

        // Receive passband (behind the trace).
        if let Some((lo, hi)) = rf_passband(s) {
            let (a, b) = (x_of(lo), x_of(hi).max(x_of(lo) + 2.0));
            p.rect_filled(Rect::from_min_max(Pos2::new(a, spec.top()), Pos2::new(b, spec.bottom())), 0.0, AMBER.gamma_multiply(0.13));
            p.rect_filled(Rect::from_min_max(Pos2::new(a, water.top()), Pos2::new(b, water.bottom())), 0.0, AMBER.gamma_multiply(0.05));
            for x in [a, b] {
                p.line_segment([Pos2::new(x, spec.top()), Pos2::new(x, water.bottom())], Stroke::new(1.0, AMBER.gamma_multiply(0.35)));
            }
        }

        // Spectrum: gradient fill, trace, and falling peaks; broken where
        // there's no data.
        let col_x = |c: usize| spec.left() + width * (c as f32 + 0.5) / COLS as f32;
        let mut fill = Mesh::default();
        let mut lines: Vec<Vec<Pos2>> = Vec::new();
        let mut run: Vec<Pos2> = Vec::new();
        let mut end_run = |run: &mut Vec<Pos2>| {
            if run.len() >= 2 {
                for w in run.windows(2) {
                    let i = fill.vertices.len() as u32;
                    fill.colored_vertex(w[0], TRACE.gamma_multiply(0.38));
                    fill.colored_vertex(w[1], TRACE.gamma_multiply(0.38));
                    fill.colored_vertex(Pos2::new(w[1].x, spec.bottom()), TRACE.gamma_multiply(0.02));
                    fill.colored_vertex(Pos2::new(w[0].x, spec.bottom()), TRACE.gamma_multiply(0.02));
                    fill.add_triangle(i, i + 1, i + 2);
                    fill.add_triangle(i, i + 2, i + 3);
                }
                lines.push(std::mem::take(run));
            }
            run.clear();
        };
        for (c, db) in bs.trace.iter().enumerate() {
            match db {
                Some(db) => run.push(Pos2::new(col_x(c), y_of(*db))),
                None => end_run(&mut run),
            }
        }
        end_run(&mut run);
        p.add(Shape::mesh(fill));
        let mut peaks: Vec<Pos2> = Vec::new();
        for (c, db) in bs.peak.iter().enumerate() {
            match db {
                Some(db) => peaks.push(Pos2::new(col_x(c), y_of(*db))),
                None if peaks.len() >= 2 => {
                    p.add(Shape::line(std::mem::take(&mut peaks), Stroke::new(1.0, PEAK)));
                }
                None => peaks.clear(),
            }
        }
        if peaks.len() >= 2 {
            p.add(Shape::line(peaks, Stroke::new(1.0, PEAK)));
        }
        for l in lines {
            p.add(Shape::line(l, Stroke::new(1.4, TRACE)));
        }

        // Frequency scale.
        let mut f = (start / step).ceil() * step;
        while f <= view_hi {
            let x = x_of(f);
            p.line_segment([Pos2::new(x, scale.top()), Pos2::new(x, scale.top() + 4.0)], Stroke::new(1.0, Color32::GRAY));
            // A label cut off at an edge is worse than none.
            if x > rect.left() + 24.0 && x < rect.right() - 24.0 {
                p.text(Pos2::new(x, scale.top() + 3.0), Align2::CENTER_TOP, mhz_label(f, step), FontId::proportional(10.0), Color32::LIGHT_GRAY);
            }
            f += step;
        }

        // The transmit frequency when split, and the dial with its readout.
        if s.split() {
            let tx = s.tx_freq() as f64;
            vline(tx, spec.top(), water.bottom(), Stroke::new(1.5, RED));
            if tx > start && tx < view_hi {
                pill(&p, Pos2::new(x_of(tx) + 3.0, spec.top() + 22.0), Align2::LEFT_TOP, "TX", 11.0, RED);
            }
        }
        if vfo > start && vfo < view_hi {
            let x = x_of(vfo);
            p.line_segment([Pos2::new(x, spec.top()), Pos2::new(x, water.bottom())], Stroke::new(1.5, AMBER));
            let tri = vec![Pos2::new(x - 5.0, spec.top()), Pos2::new(x + 5.0, spec.top()), Pos2::new(x, spec.top() + 6.0)];
            p.add(Shape::convex_polygon(tri, AMBER, Stroke::NONE));
            let right = x < rect.right() - 120.0;
            let at = Pos2::new(x + if right { 6.0 } else { -6.0 }, spec.top() + 4.0);
            pill(&p, at, if right { Align2::LEFT_TOP } else { Align2::RIGHT_TOP }, &freq_text(vfo), 12.0, AMBER);
        }

        if bs.transmitting || s.transmitting() {
            pill(&p, spec.center(), Align2::CENTER_CENTER, "TX — scope paused", 15.0, RED);
        } else if bs.history.is_empty() {
            p.text(water.center(), Align2::CENTER_CENTER, &status.text, FontId::proportional(14.0), Color32::GRAY);
        }
        if let Some((text, at)) = &bs.tuned {
            if at.elapsed() < Duration::from_millis(1800) {
                pill(&p, Pos2::new(spec.right() - 6.0, spec.top() + 4.0), Align2::RIGHT_TOP, &format!("→ {text}"), 12.0, Color32::WHITE);
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }

        // Hover: crosshair, readout, and where a click would go.
        let shift = ui.input(|i| i.modifiers.shift);
        match resp.hover_pos() {
            Some(pos) => {
                let rf = hz_of(pos.x);
                let search = search_hz(col_hz);
                p.line_segment([Pos2::new(pos.x, spec.top()), Pos2::new(pos.x, water.bottom())], Stroke::new(1.0, Color32::from_white_alpha(70)));
                let mut text = freq_text(rf);
                if let Some(snr) = bs.history.front().and_then(|r| level_at(r, rf)).zip(bs.noise).map(|(l, n)| l - n).filter(|v| *v > 3.0) {
                    text = format!("{text}  +{snr:.0} dB");
                }
                if shift {
                    bs.preview = None;
                    text = format!("{text}\nclick → exactly here");
                } else {
                    // Ask again when the pointer has moved, a few times a second.
                    let stale = bs.preview.as_ref().is_none_or(|pv| {
                        ((pv.rf - rf).abs() > col_hz * 2.0 && pv.asked.elapsed() > Duration::from_millis(120)) || pv.asked.elapsed() > Duration::from_millis(600)
                    });
                    if stale && status.running {
                        // Keep showing the last answer until the new one comes, if it's still nearby.
                        let target = bs.preview.take().and_then(|pv| pv.target).filter(|t| t.signal.is_some_and(|g| (g.peak_hz - rf).abs() < search * 2.0));
                        bs.preview = Some(Preview { answer: Some(ask(&tap, rf, search)), rf, search, asked: Instant::now(), target });
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(60));
                    // The signal a click would tune to.
                    if let Some(t) = bs.preview.as_ref().and_then(|pv| pv.target.clone()) {
                        if let Some(g) = t.signal {
                            let (a, b) = (x_of(g.lo_hz), x_of(g.hi_hz).max(x_of(g.lo_hz) + 3.0));
                            let c = ((g.peak_hz - start) / col_hz) as usize;
                            let y = y_of(bs.peak.get(c).copied().flatten().unwrap_or(floor + range * 0.5)) - 7.0;
                            p.line_segment([Pos2::new(a, y), Pos2::new(b, y)], Stroke::new(2.0, Color32::WHITE));
                            let d = Pos2::new(x_of(g.peak_hz), y - 6.0);
                            let diamond = vec![d + vec2(0.0, -4.0), d + vec2(4.0, 0.0), d + vec2(0.0, 4.0), d + vec2(-4.0, 0.0)];
                            p.add(Shape::convex_polygon(diamond, Color32::WHITE, Stroke::NONE));
                            let how = if t.how.is_empty() { String::new() } else { format!(" {}", t.how) };
                            text = format!("{text}\nclick → {}{how}", freq_text(t.dial as f64));
                        }
                    }
                }
                let right = pos.x > rect.center().x;
                let at = Pos2::new(pos.x + if right { -8.0 } else { 8.0 }, spec.top() + 26.0);
                pill(&p, at, if right { Align2::RIGHT_TOP } else { Align2::LEFT_TOP }, &text, 12.0, Color32::WHITE);
            }
            None => bs.preview = None,
        }

        // Scroll tunes, Ctrl+scroll zooms; click tunes to the signal,
        // Shift+click exactly where clicked.
        let zoom = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        let ticks = widgets::wheel_ticks(ui, &resp);
        if ticks != 0 {
            if zoom {
                let i = SPANS.iter().position(|(hz, _)| *hz == prefs.scope_span).unwrap_or(4) as i64;
                prefs.scope_span = SPANS[(i - ticks).clamp(0, SPANS.len() as i64 - 1) as usize].0;
            } else if let Some((dial, _)) = rx_vfo(s) {
                let hz = s.model.clamp_freq((dial as i64 + ticks * tune_step).max(0) as u64);
                rig.send(set_freq_cmd_for(s.model, s.rx_on_b(), hz));
            }
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let rf = hz_of(pos.x);
                if shift {
                    let hz = s.model.clamp_freq((rf / 10.0).round() as u64 * 10);
                    tune(bs, rig, s, &Target { dial: hz, signal: None, how: "" }, audio_lines);
                } else if status.running {
                    let search = search_hz(col_hz);
                    bs.click = Some(Pending { answer: ask(&tap, rf, search), rf, search, at: Instant::now() });
                    ui.ctx().request_repaint_after(Duration::from_millis(30));
                } else if let Some(t) = target(s, None, rf) {
                    tune(bs, rig, s, &t, audio_lines);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(mode: &str, freq: u64) -> RadioState {
        let mut s = RadioState::new();
        s.apply(&format!("FA{freq:08};"));
        s.apply(&format!("MD0{mode};"));
        s.apply("KP40;"); // 700 Hz pitch
        s
    }

    fn sig(peak: f64, lo: f64, hi: f64) -> Option<Signal> {
        Some(Signal { peak_hz: peak, lo_hz: lo, hi_hz: hi, snr_db: 30.0 })
    }

    #[test]
    fn only_the_band_span_is_fitted_to_the_band() {
        let s = radio("2", 14_323_000);
        let prefs = |scope_span, scope_band_span| UiPrefs { scope_span, scope_band_span, ..Default::default() };
        assert!(view_band(&prefs(BAND_SPAN, true), &s, 14_323_000.0).is_some());
        for span in SPANS.map(|(hz, _)| hz) {
            assert_eq!(view_band(&prefs(span, true), &s, 14_323_000.0), None, "{span}");
        }
        // Turned off while on the Band span.
        assert_eq!(view_band(&prefs(BAND_SPAN, false), &s, 14_323_000.0), None);
        // Outside the amateur bands.
        assert_eq!(view_band(&prefs(BAND_SPAN, true), &s, 15_000_000.0), None);
    }

    #[test]
    fn fits_the_view_to_the_band() {
        let band = Some((18_068_000.0, 18_168_000.0));
        let covers = Some((17.0e6, 19.0e6));
        // No band to fit: as it was.
        assert_eq!(in_band(18_160_000.0, 50_000.0, None, covers), 18_160_000.0);
        // Narrower than the band (10 m): it stops at the band's edges.
        assert_eq!(in_band(18_160_000.0, 50_000.0, band, covers), 18_143_000.0);
        assert_eq!(in_band(18_070_000.0, 50_000.0, band, covers), 18_093_000.0);
        // In the middle it isn't moved.
        assert_eq!(in_band(18_110_000.0, 50_000.0, band, covers), 18_110_000.0);
        // A view wider than the band is centred on it, wherever the dial is.
        assert_eq!(in_band(18_160_000.0, 110_000.0, band, covers), 18_118_000.0);
        // Everything the dongle covers stays as it is.
        assert_eq!(in_band(18_000_000.0, 2.0e6, band, covers), 18_000_000.0);
        // Never beyond what the dongle covers.
        assert_eq!(in_band(18_110_000.0, 110_000.0, band, Some((18_100_000.0, 19.0e6))), 18_155_000.0);
    }

    #[test]
    fn band_span_fits_the_band() {
        let prefs = UiPrefs { scope_span: BAND_SPAN, ..Default::default() };
        let covers = Some((17.0e6, 19.0e6));
        assert!((span_hz(&prefs, covers, Some((18_068_000.0, 18_168_000.0))) - 110_000.0).abs() < 1.0);
        // Outside a band, or with the setting turned off afterwards.
        assert_eq!(span_hz(&prefs, covers, None), BAND_SPAN_ELSEWHERE);
        // Never wider than the dongle covers (10 m is 1.7 MHz).
        assert_eq!(span_hz(&prefs, Some((27.0e6, 28.6e6)), Some((28.0e6, 29.7e6))), 1_600_000.0);
        // The ordinary spans are as they were.
        assert_eq!(span_hz(&UiPrefs { scope_span: 50_000, ..Default::default() }, covers, None), 50_000.0);
        assert_eq!(span_hz(&UiPrefs { scope_span: 0, ..Default::default() }, covers, None), 2.0e6);
    }

    #[test]
    fn the_band_comes_from_the_radio_model() {
        let s = radio("3", 18_100_000);
        assert_eq!(band_at(&s, 18_100_000.0).map(|(lo, hi)| lo < 18_100_000.0 && hi > 18_100_000.0), Some(true));
        assert_eq!(band_at(&s, 15_000_000.0), None);
    }

    #[test]
    fn the_band_span_is_off_by_default() {
        assert!(!UiPrefs::default().scope_band_span);
    }

    #[test]
    fn follows_the_vfo() {
        // Full span: show everything the dongle covers.
        assert_eq!(follow(None, 14_074_000.0, 1_600_000.0, Some((13_274_000.0, 14_874_000.0))), 14_074_000.0);
        // Narrow span: stays put while the VFO is near the middle...
        assert_eq!(follow(Some(14_070_000.0), 14_080_000.0, 50_000.0, Some((13.0e6, 15.0e6))), 14_070_000.0);
        // ...and jumps to it when it nears an edge.
        assert_eq!(follow(Some(14_070_000.0), 14_090_000.0, 50_000.0, Some((13.0e6, 15.0e6))), 14_090_000.0);
        // Kept inside the dongle's range.
        assert_eq!(follow(None, 14_000_000.0, 200_000.0, Some((14_000_000.0, 14_350_000.0))), 14_100_000.0);
        // No coverage known yet: centre on the VFO.
        assert_eq!(follow(None, 7_074_000.0, 100_000.0, None), 7_074_000.0);
    }

    #[test]
    fn cw_click_zero_beats_the_peak() {
        // CW on the upper side, dial on the carrier (menu 064 off): the
        // signal at 14,025,812 must be heard at 700 Hz -> dial 14,025,112.
        let s = radio("3", 14_020_000);
        let t = target(&s, sig(14_025_812.0, 14_025_790.0, 14_025_830.0), 14_025_900.0).unwrap();
        assert_eq!((t.dial, t.how), (14_025_112, "zero-beat"));
        // Pitch-offset display: the dial lands on the signal.
        let mut s = radio("3", 14_020_000);
        s.apply("EX0641;");
        assert_eq!(target(&s, sig(14_025_812.0, 14_025_790.0, 14_025_830.0), 14_025_900.0).unwrap().dial, 14_025_812);
        // CW below 10 MHz with BFO AUTO hears the lower side.
        let mut s = radio("3", 7_020_000);
        s.apply("EX0602;");
        assert_eq!(target(&s, sig(7_025_000.0, 7_024_990.0, 7_025_010.0), 7_025_000.0).unwrap().dial, 7_025_700);
    }

    #[test]
    fn ssb_click_lands_on_the_carrier() {
        // USB voice from 14,200,300 to 14,202,700: carrier at 14,200,150.
        let s = radio("2", 14_190_000);
        let t = target(&s, sig(14_201_000.0, 14_200_300.0, 14_202_700.0), 14_201_500.0).unwrap();
        assert_eq!((t.dial, t.how), (14_200_150, "on the carrier"));
        // LSB voice from 7,197,300 to 7,199,700: carrier at 7,199,850.
        let s = radio("1", 7_180_000);
        assert_eq!(target(&s, sig(7_198_500.0, 7_197_300.0, 7_199_700.0), 7_198_000.0).unwrap().dial, 7_199_850);
        // Nothing found: right where clicked (to 10 Hz).
        assert_eq!(target(&s, None, 7_123_456.0).unwrap().dial, 7_123_460);
    }

    #[test]
    fn digital_click_goes_to_the_calling_frequency() {
        let s = radio("C", 14_200_000); // DATA-U
        let t = target(&s, sig(14_075_620.0, 14_075_600.0, 14_075_650.0), 14_075_600.0).unwrap();
        assert_eq!((t.dial, t.how), (14_074_000, "FT8"));
        assert_eq!(target(&s, None, 7_048_000.0).unwrap().dial, 7_047_500);
    }

    #[test]
    fn am_click_lands_on_the_carrier() {
        let s = radio("5", 7_200_000);
        assert_eq!(target(&s, sig(7_490_003.0, 7_485_000.0, 7_495_000.0), 7_491_000.0).unwrap().dial, 7_490_000);
    }

    #[test]
    fn passband_in_radio_frequency() {
        let s = radio("1", 7_200_000); // LSB: below the dial
        let (lo, hi) = rf_passband(&s).unwrap();
        assert!(hi <= 7_200_000.0 && lo < hi && lo > 7_196_000.0, "{lo} {hi}");
    }

    #[test]
    fn scale_steps_are_round() {
        assert_eq!(nice_step(200_000.0), 50_000.0);
        assert_eq!(nice_step(10_000.0), 2_000.0);
        assert_eq!(nice_step(1_600_000.0), 200_000.0);
        assert_eq!(mhz_label(14_200_000.0, 50_000.0), "14.20");
        assert_eq!(mhz_label(14_074_000.0, 2_000.0), "14.074");
    }
}
