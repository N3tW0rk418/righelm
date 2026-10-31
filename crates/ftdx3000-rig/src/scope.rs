//! Band scope: a wide spectrum of the band from an RTL-SDR dongle or a
//! HackRF plugged into this computer and connected to the radio's IF OUT or
//! RX OUT jack.
//!
//! * No other software: the USB drivers are pure Rust (`rtlsdr-nusb`, and
//!   [`crate::hackrf`]), the same on Linux and macOS.
//! * The dongle is found by itself, also when it's plugged in later, and is
//!   only opened while somebody is watching.
//! * IF OUT carries the radio's 9 MHz first IF, so the dongle stays put and
//!   the picture follows the VFO. RX OUT carries the antenna's RF, so the
//!   dongle retunes to keep the VFO (or the whole band) in view.
//! * Every watcher asks for a view (centre, span, columns, rate) and gets rows
//!   of levels for exactly that view, in radio frequencies. That keeps a
//!   remote client's stream to about 1 kB per row instead of raw samples.
//!
//! Nothing here ever transmits.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use parking_lot::Mutex;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use crate::engine::Rig;
use crate::hackrf;

/// Dongle sample rate: the highest that doesn't drop samples on most hosts.
pub const SAMPLE_RATE: u32 = 2_048_000;
/// 31.25 Hz bins at 2.048 MS/s.
const FFT_SIZE: usize = 65_536;
/// FFTs overlap by half.
const FFT_HOP: usize = FFT_SIZE / 2;
/// The dongle's own filter rolls off near the edges; show the middle 80 %.
const USABLE: f64 = 0.8;
/// Fastest row rate a watcher may ask for.
pub const MAX_FPS: u8 = 30;
/// The radio's first IF.
pub const DEFAULT_IF_HZ: u32 = 9_000_000;

/// Which of the radio's rear jacks the dongle is connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Feed {
    /// IF OUT: the 9 MHz IF, before the roofing filter. Follows the VFO.
    #[default]
    IfOut,
    /// RX OUT: the receive antenna signal.
    RxOut,
}

impl Feed {
    pub fn label(self) -> &'static str {
        match self {
            Feed::IfOut => "IF OUT",
            Feed::RxOut => "RX OUT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeConfig {
    /// Use an RTL-SDR dongle or HackRF when one is plugged in.
    pub enabled: bool,
    pub feed: Feed,
    /// IF OUT: the IF frequency that corresponds to the receive dial.
    pub if_hz: u32,
    /// IF OUT: higher radio frequencies come out lower in the IF.
    pub if_inverted: bool,
    /// Correction for the dongle's crystal, in parts per million.
    pub ppm: i32,
    /// Serial number of the dongle to use (a HackRF's as `hackrf:<serial>`);
    /// empty = the first one found.
    pub serial: String,
    /// RX OUT: the part of the crystal correction finer than whole ppm,
    /// applied in software (-0.5..0.5).
    pub ppm_fine: f64,
    /// The frequency has been calibrated against broadcast carriers.
    pub calibrated: bool,
    /// HackRF: RF (0-40 dB, 8 dB steps) and baseband (0-62 dB, 2 dB steps)
    /// gain, and its extra 14 dB front-end amplifier. It has no AGC.
    pub hackrf_lna_db: u8,
    pub hackrf_vga_db: u8,
    pub hackrf_amp: bool,
    /// The user has said which jack the dongle is on (asked the first time
    /// one is plugged in: the picture is wrong if `feed` is).
    pub feed_confirmed: bool,
}

impl ScopeConfig {
    fn hackrf_gains(&self) -> hackrf::Gains {
        hackrf::Gains { lna_db: self.hackrf_lna_db, vga_db: self.hackrf_vga_db, amp: self.hackrf_amp }
    }
}

impl Default for ScopeConfig {
    fn default() -> Self {
        // The FTDX3000's first LO is above the signal, so the IF is inverted.
        Self {
            enabled: true,
            feed: Feed::IfOut,
            if_hz: DEFAULT_IF_HZ,
            if_inverted: true,
            ppm: 0,
            serial: String::new(),
            ppm_fine: 0.0,
            calibrated: false,
            feed_confirmed: false,
            hackrf_lna_db: 16,
            hackrf_vga_db: 20,
            hackrf_amp: false,
        }
    }
}

/// What a watcher wants to see.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeView {
    pub center_hz: f64,
    pub span_hz: f64,
    pub cols: u16,
    /// Rows per second.
    pub fps: u8,
}

impl Default for ScopeView {
    fn default() -> Self {
        Self { center_hz: 14_100_000.0, span_hz: 200_000.0, cols: 1024, fps: 20 }
    }
}

impl ScopeView {
    /// Clamp what a (possibly remote) watcher asked for to something sane.
    pub fn sanitized(self) -> ScopeView {
        ScopeView {
            center_hz: if self.center_hz.is_finite() { self.center_hz.clamp(0.0, 2.0e9) } else { 0.0 },
            span_hz: if self.span_hz.is_finite() { self.span_hz.clamp(1_000.0, SAMPLE_RATE as f64) } else { 200_000.0 },
            cols: self.cols.clamp(16, 4096),
            fps: self.fps.clamp(1, MAX_FPS),
        }
    }

    pub fn start_hz(&self) -> f64 {
        self.center_hz - self.span_hz / 2.0
    }
}

/// One line of the scope: a level per column over a range of radio
/// frequencies.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeRow {
    /// Radio frequency of the left edge of the first column.
    pub start_hz: f64,
    /// Width of each column in Hz.
    pub col_hz: f64,
    /// 0 = no data there (outside what the dongle covers), otherwise see
    /// [`level_db`].
    pub cols: Vec<u8>,
    /// The radio was transmitting: the picture means nothing.
    pub transmitting: bool,
}

/// Level of a column value in dB relative to the dongle's full scale.
pub fn level_db(v: u8) -> Option<f32> {
    (v > 0).then(|| v as f32 / 2.0 - 130.0)
}

fn encode_db(db: f32) -> u8 {
    ((db + 130.0) * 2.0).round().clamp(1.0, 255.0) as u8
}

/// What the scope is doing, for the display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScopeStatus {
    /// A dongle is plugged in (or the remote radio's computer has one).
    pub present: bool,
    /// Samples are flowing.
    pub running: bool,
    pub text: String,
    /// Radio frequencies the dongle currently covers.
    pub covers: Option<(f64, f64)>,
    pub feed: Option<Feed>,
    /// Dongles plugged into this computer (serial, description).
    pub dongles: Vec<(String, String)>,
}

// ---------------------------------------------------------------- the tap

struct Watcher {
    id: u64,
    view: ScopeView,
    tx: Sender<ScopeRow>,
}

#[derive(Default)]
struct TapInner {
    watchers: Vec<Watcher>,
    status: ScopeStatus,
    asks: Vec<DetailAsk>,
    calibrate: bool,
    calibration: Option<Calibration>,
}

/// The result of calibrating against stations whose frequency is exact:
/// shortwave broadcasters (on 5 kHz channels) and time signals.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    /// The settings to use from now on, when it worked.
    pub fix: Option<CalibrationFix>,
    /// Carriers measured, and how far off they were before (Hz at 10 MHz).
    pub carriers: usize,
    pub error_hz: f64,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalibrationFix {
    /// Whole ppm for the dongle, and the fine rest.
    Ppm(i32, f64),
    IfHz(u32),
}

/// A close look at part of the band, at the scope's full resolution: for
/// finding exactly where a clicked signal is.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// Radio frequency of the first value, and the spacing (increasing).
    pub start_hz: f64,
    pub bin_hz: f64,
    /// Level of each bin in dB (as [`level_db`]).
    pub db: Vec<f32>,
}

/// Widest detail anyone may ask for.
pub const MAX_DETAIL_HZ: f64 = 60_000.0;

impl Detail {
    pub fn freq(&self, i: f64) -> f64 {
        self.start_hz + i * self.bin_hz
    }

    /// Typical level where there's no signal.
    pub fn noise_db(&self) -> f32 {
        let mut v = self.db.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v.get(v.len() / 4).copied().unwrap_or(-130.0)
    }

    /// The signal nearest `near_hz` (within `search_hz`): its exact peak and
    /// how far its energy reaches either side.
    pub fn find_signal(&self, near_hz: f64, search_hz: f64) -> Option<Signal> {
        let n = self.db.len();
        if n < 8 {
            return None;
        }
        let noise = self.noise_db();
        let idx = |hz: f64| ((hz - self.start_hz) / self.bin_hz).round().clamp(0.0, (n - 1) as f64) as usize;
        let (a, b) = (idx(near_hz - search_hz), idx(near_hz + search_hz));
        // Strongest bin, favouring the click: one 12 dB stronger at the edge
        // of the search only just wins.
        let score = |i: usize| self.db[i] - 12.0 * ((self.freq(i as f64) - near_hz).abs() / search_hz.max(1.0)) as f32;
        let mut k = (a..=b).max_by(|&x, &y| score(x).total_cmp(&score(y)))?;
        // Then up to the top of that signal.
        while k > 0 && self.db[k - 1] > self.db[k] {
            k -= 1;
        }
        while k + 1 < n && self.db[k + 1] > self.db[k] {
            k += 1;
        }
        let snr = self.db[k] - noise;
        if snr < 8.0 {
            return None;
        }
        // Exact peak between bins: fit a parabola through the top three.
        let off = if k > 0 && k + 1 < n {
            let (l, c, r) = (self.db[k - 1] as f64, self.db[k] as f64, self.db[k + 1] as f64);
            let d = l - 2.0 * c + r;
            if d < 0.0 { (0.5 * (l - r) / d).clamp(-0.5, 0.5) } else { 0.0 }
        } else {
            0.0
        };
        // Extent: where it stays above the noise, bridging short gaps
        // (speech has holes between its formants).
        let thresh = noise + (snr * 0.25).clamp(5.0, 10.0);
        let gap = (160.0 / self.bin_hz).ceil() as usize;
        let reach = |dir: isize| -> usize {
            let (mut last, mut i, mut misses) = (k, k as isize, 0);
            loop {
                i += dir;
                if i < 0 || i >= n as isize {
                    break last;
                }
                if self.db[i as usize] > thresh {
                    last = i as usize;
                    misses = 0;
                } else {
                    misses += 1;
                    if misses > gap {
                        break last;
                    }
                }
            }
        };
        let (lo, hi) = (reach(-1), reach(1));
        Some(Signal {
            peak_hz: self.freq(k as f64 + off),
            lo_hz: self.freq(lo as f64) - self.bin_hz / 2.0,
            hi_hz: self.freq(hi as f64) + self.bin_hz / 2.0,
            snr_db: snr,
        })
    }
}

/// A signal found in a [`Detail`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signal {
    /// Frequency of its strongest point.
    pub peak_hz: f64,
    /// Lowest and highest frequency of its energy.
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// How far it stands above the noise.
    pub snr_db: f32,
}

impl Signal {
    pub fn width_hz(&self) -> f64 {
        self.hi_hz - self.lo_hz
    }
}

pub(crate) struct DetailAsk {
    pub lo: f64,
    pub hi: f64,
    pub reply: Sender<Detail>,
}

/// Fans scope rows out to watchers, each with its own view. Rows are
/// dropped for watchers that fall behind.
#[derive(Clone, Default)]
pub struct ScopeTap {
    inner: Arc<Mutex<TapInner>>,
    next_id: Arc<AtomicU64>,
}

/// A watcher's end of the tap. Dropping it stops the rows.
pub struct ScopeSubscription {
    id: u64,
    tap: ScopeTap,
    pub rows: Receiver<ScopeRow>,
}

impl ScopeSubscription {
    pub fn set_view(&self, view: ScopeView) {
        let view = view.sanitized();
        if let Some(w) = self.tap.inner.lock().watchers.iter_mut().find(|w| w.id == self.id) {
            w.view = view;
        }
    }

    pub fn view(&self) -> Option<ScopeView> {
        self.tap.inner.lock().watchers.iter().find(|w| w.id == self.id).map(|w| w.view)
    }
}

impl Drop for ScopeSubscription {
    fn drop(&mut self) {
        self.tap.inner.lock().watchers.retain(|w| w.id != self.id);
    }
}

impl ScopeTap {
    pub fn subscribe(&self, view: ScopeView) -> ScopeSubscription {
        // A second's worth at the fastest rate, for a slow-drawing window.
        let (tx, rows) = bounded(MAX_FPS as usize);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().watchers.push(Watcher { id, view: view.sanitized(), tx });
        ScopeSubscription { id, tap: self.clone(), rows }
    }

    pub fn has_watchers(&self) -> bool {
        !self.inner.lock().watchers.is_empty()
    }

    /// Current views, by watcher id.
    fn views(&self) -> Vec<(u64, ScopeView)> {
        self.inner.lock().watchers.iter().map(|w| (w.id, w.view)).collect()
    }

    /// The first watcher's view (what a remote client asks the server for).
    pub fn first_view(&self) -> Option<ScopeView> {
        self.inner.lock().watchers.first().map(|w| w.view)
    }

    /// Send a row to one watcher.
    fn send_to(&self, id: u64, row: ScopeRow) {
        if let Some(w) = self.inner.lock().watchers.iter().find(|w| w.id == id) {
            match w.tx.try_send(row) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    /// Send a row to every watcher (rows that were made for one view).
    pub fn publish(&self, row: &ScopeRow) {
        for w in &self.inner.lock().watchers {
            let _ = w.tx.try_send(row.clone());
        }
    }

    pub fn status(&self) -> ScopeStatus {
        self.inner.lock().status.clone()
    }

    /// Ask for a [`Detail`] of `lo..hi`; the answer arrives on the returned
    /// channel (or never, if the scope isn't running).
    pub fn ask_detail(&self, lo: f64, hi: f64) -> Receiver<Detail> {
        let (reply, rx) = bounded(1);
        let (lo, hi) = (lo.min(hi), lo.max(hi).min(lo.min(hi) + MAX_DETAIL_HZ));
        let mut inner = self.inner.lock();
        // Nobody answers forever-unanswered asks: keep only a few.
        if inner.asks.len() >= 8 {
            inner.asks.remove(0);
        }
        inner.asks.push(DetailAsk { lo, hi, reply });
        rx
    }

    /// Calibrate against broadcast carriers in view, as soon as possible.
    pub fn request_calibration(&self) {
        self.inner.lock().calibrate = true;
    }

    /// A finished calibration, once.
    pub fn take_calibration(&self) -> Option<Calibration> {
        self.inner.lock().calibration.take()
    }

    fn calibration_wanted(&self) -> bool {
        self.inner.lock().calibrate
    }

    fn calibration_done(&self, c: Calibration) {
        let mut inner = self.inner.lock();
        inner.calibrate = false;
        inner.calibration = Some(c);
    }

    pub(crate) fn take_asks(&self) -> Vec<DetailAsk> {
        std::mem::take(&mut self.inner.lock().asks)
    }

    pub fn set_status(&self, status: ScopeStatus) {
        self.inner.lock().status = status;
    }
}

// ------------------------------------------------------- frequency mapping

/// Where the dongle's samples sit in radio frequency.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mapping {
    /// The dongle's centre frequency.
    center: f64,
    feed: Feed,
    /// IF OUT: the receive dial frequency, its IF, and the IF's direction.
    vfo: f64,
    if_hz: f64,
    inverted: bool,
    /// RX OUT: true / reported frequency (the fine part of the crystal
    /// correction, below the dongle's whole ppm).
    scale: f64,
}

impl Mapping {
    fn sign(&self) -> f64 {
        if self.inverted { -1.0 } else { 1.0 }
    }

    /// Offset from the dongle's centre (baseband Hz) of a radio frequency.
    fn offset_of(&self, rf: f64) -> f64 {
        match self.feed {
            Feed::RxOut => rf / self.scale - self.center,
            Feed::IfOut => self.if_hz + self.sign() * (rf - self.vfo) - self.center,
        }
    }

    /// Radio frequency at an offset from the dongle's centre.
    fn rf_of(&self, offset: f64) -> f64 {
        match self.feed {
            Feed::RxOut => (self.center + offset) * self.scale,
            Feed::IfOut => self.vfo + self.sign() * (self.center + offset - self.if_hz),
        }
    }

    /// Radio frequencies covered by the usable part of the dongle's band.
    fn covers(&self, rate: f64) -> (f64, f64) {
        let half = rate * USABLE / 2.0;
        let (a, b) = (self.rf_of(-half), self.rf_of(half));
        (a.min(b), a.max(b))
    }
}

/// The dongle frequency for the IF, or (RX OUT) one that keeps the VFO in
/// view: the whole band when it fits, otherwise centred on the VFO, and
/// left alone while the VFO stays well inside.
fn wanted_center(cfg: &ScopeConfig, vfo: u64, current: Option<u64>) -> u64 {
    match cfg.feed {
        Feed::IfOut => cfg.if_hz as u64,
        Feed::RxOut => {
            let half = SAMPLE_RATE as f64 * USABLE / 2.0;
            if let Some(c) = current {
                if (vfo as f64 - c as f64).abs() < half * 0.8 {
                    return c;
                }
            }
            match ftdx3000_cat::types::band_of(vfo) {
                Some(b) if ((b.hi - b.lo) as f64) < half * 2.0 * 0.95 => (b.lo + b.hi) / 2,
                _ => vfo,
            }
            .max(SAMPLE_RATE as u64 / 2)
        }
    }
}

// ----------------------------------------------------------------- the DSP

/// Averaged power spectra from complex samples.
struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Samples waiting for the next FFT.
    pending: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// Running DC estimate (the dongle's 8-bit samples aren't centred).
    dc: Complex32,
    /// Normalises a full-scale tone to 0 dB.
    norm: f32,
}

impl Analyzer {
    fn new() -> Analyzer {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos()).collect();
        let sum: f32 = window.iter().sum();
        Analyzer { fft, window, pending: Vec::with_capacity(FFT_SIZE * 2), scratch: vec![Complex32::default(); FFT_SIZE], dc: Complex32::default(), norm: 1.0 / (sum * sum) }
    }

    fn reset(&mut self) {
        self.pending.clear();
    }

    /// Feed samples; calls `out` with each new power spectrum (linear,
    /// lowest frequency first).
    fn push(&mut self, samples: &[Complex32], mut out: impl FnMut(&[f32])) {
        if samples.is_empty() {
            return;
        }
        let mean = samples.iter().fold(Complex32::default(), |a, s| a + s) / samples.len() as f32;
        self.dc = self.dc * 0.9 + mean * 0.1;
        let dc = self.dc;
        self.pending.extend(samples.iter().map(|s| s - dc));
        let mut power = vec![0f32; FFT_SIZE];
        let mut start = 0;
        while self.pending.len() - start >= FFT_SIZE {
            for (i, c) in self.scratch.iter_mut().enumerate() {
                *c = self.pending[start + i] * self.window[i];
            }
            self.fft.process(&mut self.scratch);
            // FFT order is 0..+fs/2 then -fs/2..0; put the lowest first.
            let half = FFT_SIZE / 2;
            for (i, p) in power.iter_mut().enumerate() {
                *p = self.scratch[(i + half) % FFT_SIZE].norm_sqr() * self.norm;
            }
            out(&power);
            start += FFT_HOP;
        }
        self.pending.drain(..start);
    }
}

/// One row for `view` from an averaged power spectrum.
fn build_row(power: &[f32], map: &Mapping, rate: f64, view: &ScopeView, transmitting: bool) -> ScopeRow {
    let n = power.len();
    let bin_hz = rate / n as f64;
    let half = n as f64 / 2.0;
    // Continuous bin coordinate of a radio frequency (bin k is centred on k).
    let x_of = |rf: f64| map.offset_of(rf) / bin_hz + half;
    let lo_bin = n as f64 * (1.0 - USABLE) / 2.0;
    let hi_bin = n as f64 - lo_bin;
    let col_hz = view.span_hz / view.cols as f64;
    let start = view.start_hz();
    let cols = (0..view.cols as usize)
        .map(|c| {
            let (a, b) = (x_of(start + c as f64 * col_hz), x_of(start + (c + 1) as f64 * col_hz));
            let (x0, x1) = (a.min(b).max(lo_bin), a.max(b).min(hi_bin));
            if x0 > x1 {
                return 0;
            }
            let (k0, k1) = (x0.ceil() as usize, x1.floor() as usize);
            let p = if k0 <= k1 {
                // Wide column: the strongest bin, so narrow signals show.
                power[k0..=k1.min(n - 1)].iter().copied().fold(0.0, f32::max)
            } else {
                // Narrow column: between two bins.
                let x = (x0 + x1) / 2.0;
                let i = (x.floor() as usize).min(n - 2);
                let f = (x - i as f64) as f32;
                power[i] * (1.0 - f) + power[i + 1] * f
            };
            encode_db(10.0 * p.max(1e-14).log10())
        })
        .collect();
    ScopeRow { start_hz: start, col_hz, cols, transmitting }
}

/// Full-resolution levels of `lo..hi` from a power spectrum, lowest radio
/// frequency first (the IF may be inverted).
fn detail(power: &[f32], map: &Mapping, rate: f64, lo: f64, hi: f64) -> Option<Detail> {
    let n = power.len();
    let bin_hz = rate / n as f64;
    let half = n as f64 / 2.0;
    let (lo_bin, hi_bin) = (n as f64 * (1.0 - USABLE) / 2.0, n as f64 * (1.0 + USABLE) / 2.0);
    let (a, b) = (map.offset_of(lo) / bin_hz + half, map.offset_of(hi) / bin_hz + half);
    let (k0, k1) = (a.min(b).max(lo_bin).ceil() as usize, a.max(b).min(hi_bin).floor() as usize);
    if k0 >= k1 {
        return None;
    }
    let mut ks: Vec<usize> = (k0..=k1.min(n - 1)).collect();
    if map.rf_of(1.0) < map.rf_of(0.0) {
        ks.reverse();
    }
    let start_hz = map.rf_of((ks[0] as f64 - half) * bin_hz);
    let db = ks.iter().map(|&k| 10.0 * power[k].max(1e-14).log10()).collect();
    Some(Detail { start_hz, bin_hz, db })
}

/// Frequencies that are exact: time signals, and the 5 kHz channels of the
/// shortwave broadcast bands.
fn reference_frequencies(lo: f64, hi: f64) -> Vec<f64> {
    const TIME: &[f64] = &[2_500_000.0, 3_330_000.0, 5_000_000.0, 7_850_000.0, 10_000_000.0, 14_670_000.0, 15_000_000.0, 20_000_000.0, 25_000_000.0];
    const BROADCAST: &[(f64, f64)] = &[
        (2_300_000.0, 2_495_000.0),
        (3_200_000.0, 3_400_000.0),
        (3_900_000.0, 4_000_000.0),
        (4_750_000.0, 5_060_000.0),
        (5_900_000.0, 6_200_000.0),
        (7_200_000.0, 7_600_000.0),
        (9_400_000.0, 9_900_000.0),
        (11_600_000.0, 12_100_000.0),
        (13_570_000.0, 13_870_000.0),
        (15_100_000.0, 15_800_000.0),
        (17_480_000.0, 17_900_000.0),
        (21_450_000.0, 21_850_000.0),
        (25_670_000.0, 26_100_000.0),
    ];
    let mut out: Vec<f64> = TIME.iter().copied().filter(|f| *f > lo && *f < hi).collect();
    for &(a, b) in BROADCAST {
        let mut f = (a.max(lo) / 5_000.0).ceil() * 5_000.0;
        while f <= b.min(hi) {
            out.push(f);
            f += 5_000.0;
        }
    }
    out
}

/// Strong, clean carriers near the reference frequencies:
/// (true frequency, measured − true in Hz).
fn measure_carriers(power: &[f32], map: &Mapping, rate: f64) -> Vec<(f64, f64)> {
    let (lo, hi) = map.covers(rate);
    let mut out = Vec::new();
    for f in reference_frequencies(lo + 2_000.0, hi - 2_000.0) {
        // A carrier stands far above everything within 2 kHz but its own bin.
        let Some(d) = detail(power, map, rate, f - 2_000.0, f + 2_000.0) else { continue };
        let Some(sig) = d.find_signal(f, 150.0) else { continue };
        let k = ((sig.peak_hz - d.start_hz) / d.bin_hz).round() as usize;
        let peak = d.db.get(k).copied().unwrap_or(-200.0);
        let around = d.db.iter().enumerate().filter(|(i, _)| i.abs_diff(k) > 3).map(|(_, v)| *v).fold(f32::MIN, f32::max);
        if sig.snr_db > 20.0 && peak - around > 8.0 {
            out.push((f, sig.peak_hz - f));
        }
    }
    out
}

/// Turn measured carriers into new settings: RX OUT corrects the dongle's
/// crystal (ppm, the error grows with frequency), IF OUT the IF frequency
/// (a fixed offset).
fn calibrate(cfg: &ScopeConfig, carriers: &[(f64, f64)]) -> Calibration {
    let median = |mut v: Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let none = |message: String| Calibration { fix: None, carriers: carriers.len(), error_hz: 0.0, message };
    if carriers.len() < 3 {
        return none("not enough broadcast or time-signal carriers in view; try again tuned near a shortwave broadcast band (e.g. 40 m at night, 20 m by day)".into());
    }
    match cfg.feed {
        Feed::RxOut => {
            let ppms: Vec<f64> = carriers.iter().map(|(f, e)| e / f * 1e6).collect();
            let m = median(ppms.clone());
            let agree = ppms.iter().filter(|p| (*p - m).abs() < 0.6).count();
            if agree < 3 {
                return none(format!("the {} carriers found disagree; try again later", carriers.len()));
            }
            // Seen too high = the crystal is slow: tell the dongle so.
            let total = (cfg.ppm as f64 + cfg.ppm_fine - m).clamp(-200.0, 200.0);
            let ppm = total.round();
            Calibration {
                fix: Some(CalibrationFix::Ppm(ppm as i32, ((total - ppm) * 100.0).round() / 100.0)),
                carriers: agree,
                error_hz: m * 10.0,
                message: format!("{agree} carriers were {m:+.2} ppm off; the dongle's correction is now {total:+.2} ppm"),
            }
        }
        Feed::IfOut => {
            let errs: Vec<f64> = carriers.iter().map(|(_, e)| *e).collect();
            let m = median(errs.clone());
            let agree = errs.iter().filter(|e| (*e - m).abs() < 8.0).count();
            if agree < 3 {
                return none(format!("the {} carriers found disagree; check Flipped", carriers.len()));
            }
            let sign = if cfg.if_inverted { -1.0 } else { 1.0 };
            let if_hz = (cfg.if_hz as f64 + sign * m).round() as u32;
            Calibration {
                fix: Some(CalibrationFix::IfHz(if_hz)),
                carriers: agree,
                error_hz: m,
                message: format!("{agree} carriers were {m:+.0} Hz off; IF frequency is now {if_hz} Hz"),
            }
        }
    }
}

/// Per-watcher averaging between rows.
struct Accum {
    sum: Vec<f32>,
    count: u32,
    /// Samples analysed since the last row: rows are paced by the samples,
    /// which arrive in bursts, not by the clock.
    samples: usize,
}

/// Turns spectra into rows for every watcher at its own rate.
struct Rows {
    acc: HashMap<u64, Accum>,
    /// Mapping the accumulated spectra were taken with.
    map: Option<Mapping>,
}

impl Rows {
    fn new() -> Rows {
        Rows { acc: HashMap::new(), map: None }
    }

    fn reset(&mut self) {
        self.acc.clear();
        self.map = None;
    }

    fn push(&mut self, tap: &ScopeTap, power: &[f32], map: Mapping, transmitting: bool) {
        // Tuning moves everything: don't smear the old picture into the new.
        if self.map != Some(map) {
            for a in self.acc.values_mut() {
                a.count = 0;
                a.sum.iter_mut().for_each(|s| *s = 0.0);
            }
            self.map = Some(map);
        }
        let views = tap.views();
        self.acc.retain(|id, _| views.iter().any(|(v, _)| v == id));
        for (id, view) in views {
            let a = self.acc.entry(id).or_insert_with(|| Accum { sum: vec![0.0; power.len()], count: 0, samples: usize::MAX / 2 });
            for (s, p) in a.sum.iter_mut().zip(power) {
                *s += p;
            }
            a.count += 1;
            a.samples = a.samples.saturating_add(FFT_HOP);
            let every = SAMPLE_RATE as usize / view.fps as usize;
            if a.samples >= every {
                let k = 1.0 / a.count as f32;
                let avg: Vec<f32> = a.sum.iter().map(|s| s * k).collect();
                tap.send_to(id, build_row(&avg, &map, SAMPLE_RATE as f64, &view, transmitting));
                a.sum.iter_mut().for_each(|s| *s = 0.0);
                a.count = 0;
                // Keep the remainder so the rate comes out exact.
                a.samples = (a.samples - every).min(every);
            }
        }
    }
}

// -------------------------------------------------------------- the source

/// Something that delivers complex samples at [`SAMPLE_RATE`]: the dongle,
/// or the built-in signal simulator.
trait Source {
    fn describe(&self) -> String;
    /// Fill `buf`; returns how many samples were written.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize, String>;
    /// Retune; queued samples from the old frequency are discarded.
    fn tune(&mut self, hz: u64) -> Result<(), String>;
    fn set_ppm(&mut self, ppm: i32) -> Result<(), String>;
    fn set_hackrf_gains(&mut self, _gains: hackrf::Gains) -> Result<(), String> {
        Ok(())
    }
    /// The simulator places its signals with the scope's current mapping.
    fn set_mapping(&mut self, _map: Mapping) {}
}

struct Dongle {
    device: rtlsdr_nusb::Device,
    rx: Option<rtlsdr_nusb::RxStream>,
    name: String,
}

use rtlsdr_nusb::MaybeFuture;

fn usb_error(e: rtlsdr_nusb::Error) -> String {
    let text = e.to_string();
    if cfg!(target_os = "linux") && (text.contains("ermission") || text.contains("EACCES") || text.contains("denied")) {
        format!("{text} — this user may not open the dongle; plug it in again, or see the README (Band scope)")
    } else {
        text
    }
}

fn is_blog_v4(d: &rtlsdr_nusb::DeviceDescriptor) -> bool {
    d.manufacturer.as_deref() == Some("RTLSDRBlog") && d.product.as_deref() == Some("Blog V4")
}

fn describe_dongle(d: &rtlsdr_nusb::DeviceDescriptor) -> String {
    let name = if is_blog_v4(d) {
        "RTL-SDR Blog V4".to_string()
    } else {
        match (d.manufacturer.as_deref(), d.product.as_deref()) {
            (Some(m), Some(p)) => format!("{m} {p}"),
            (None, Some(p)) => p.to_string(),
            _ => "RTL-SDR".to_string(),
        }
    };
    match d.serial.as_deref() {
        Some(s) if !s.is_empty() => format!("{name} (serial {s})"),
        _ => name,
    }
}

impl Dongle {
    fn open(desc: &rtlsdr_nusb::DeviceDescriptor, hz: u64, ppm: i32) -> Result<Dongle, String> {
        use rtlsdr_nusb::{Device, DirectSampling};
        // Below 28.8 MHz a V4 uses its upconverter; others sample the HF
        // input directly (the Q branch, as on the RTL-SDR Blog V3).
        let direct = if is_blog_v4(desc) { DirectSampling::Off } else { DirectSampling::Q };
        let mut b = Device::builder().index(desc.index).frequency_hz(hz).sample_rate_hz(SAMPLE_RATE).correction_ppm(ppm.clamp(-200, 200)).direct_sampling(direct);
        if let Some(s) = desc.serial.as_deref().filter(|s| !s.is_empty()) {
            b = b.serial(s);
        }
        let device = b.open().wait().map_err(usb_error)?;
        let tuner = format!("{:?}", device.info().tuner);
        let name = format!("{}, {tuner} tuner", describe_dongle(desc));
        let mut rx = device.rx_stream().map_err(usb_error)?;
        rx.start().wait().map_err(usb_error)?;
        Ok(Dongle { device, rx: Some(rx), name })
    }

    fn restart<T>(&mut self, f: impl FnOnce(&mut rtlsdr_nusb::Device) -> Result<T, rtlsdr_nusb::Error>) -> Result<(), String> {
        // On Windows a stopped stream delivers nothing once started again
        // (RX OUT retunes froze the scope); a fresh stream does.
        if cfg!(windows) {
            self.rx.take().ok_or("stream closed")?.close().wait().map_err(usb_error)?;
            f(&mut self.device).map_err(usb_error)?;
            let mut rx = self.device.rx_stream().map_err(usb_error)?;
            rx.start().wait().map_err(usb_error)?;
            self.rx = Some(rx);
            return Ok(());
        }
        let rx = self.rx.as_mut().ok_or("stream closed")?;
        rx.stop().wait().map_err(usb_error)?;
        f(&mut self.device).map_err(usb_error)?;
        self.rx.as_mut().ok_or("stream closed")?.start().wait().map_err(usb_error)
    }
}

impl Source for Dongle {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize, String> {
        let rx = self.rx.as_mut().ok_or("stream closed")?;
        // Same num-complex type: rustfft re-exports it too.
        rx.read(buf, Some(Duration::from_millis(500))).wait().map_err(usb_error)
    }

    fn tune(&mut self, hz: u64) -> Result<(), String> {
        self.restart(|d| d.set_frequency_hz(hz).wait())
    }

    fn set_ppm(&mut self, ppm: i32) -> Result<(), String> {
        self.restart(|d| d.set_correction_ppm(ppm.clamp(-200, 200)).wait())
    }
}

impl Drop for Dongle {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take() {
            let _ = rx.close().wait();
        }
        let _ = self.device.shutdown().wait();
    }
}

/// A HackRF. It samples 4 times faster than the scope needs, tuned
/// [`HACKRF_OFFSET`] above where the scope looks, and the samples are moved
/// back down and narrowed to [`SAMPLE_RATE`]: its strong spike at its own
/// tuning (and the mirror images of signals around it) stay out of the
/// picture. It has no crystal correction of its own: the tuning is
/// corrected instead.
struct HackRfSource {
    device: hackrf::HackRf,
    name: String,
    center: u64,
    ppm: i32,
    down: Downconverter,
    raw: Vec<Complex32>,
    /// Converted samples not yet read.
    ready: Vec<Complex32>,
}

const HACKRF_DECIMATION: usize = 4;
const HACKRF_RATE: u32 = SAMPLE_RATE * HACKRF_DECIMATION as u32;
/// An eighth of the HackRF's rate: past the edge of what the scope shows,
/// and the frequency shift is then a cycle of 8 fixed steps.
const HACKRF_OFFSET: u64 = HACKRF_RATE as u64 / 8;

/// A half-band low-pass that halves the sample rate (Blackman-windowed
/// sinc). Passes the middle 40 % of the output band, and keeps what would
/// fold into it down by about 70 dB.
struct HalfBand {
    /// The taps that aren't zero (every other one in a half-band filter):
    /// (position, value).
    taps: Vec<(usize, f32)>,
    len: usize,
    history: Vec<Complex32>,
}

impl HalfBand {
    fn new(len: usize) -> HalfBand {
        debug_assert!(len % 4 == 3, "half-band length must be 4k+3");
        let m = (len / 2) as f64;
        let pi = std::f64::consts::PI;
        let h: Vec<f64> = (0..len)
            .map(|k| {
                let x = k as f64 - m;
                let sinc = if x == 0.0 { 1.0 } else { (pi * x / 2.0).sin() / (pi * x / 2.0) };
                let w = 0.42 - 0.5 * (2.0 * pi * k as f64 / (len - 1) as f64).cos() + 0.08 * (4.0 * pi * k as f64 / (len - 1) as f64).cos();
                sinc * w
            })
            .collect();
        let sum: f64 = h.iter().sum();
        let taps = h.iter().enumerate().filter(|(_, v)| v.abs() > 1e-12).map(|(k, v)| (k, (v / sum) as f32)).collect();
        HalfBand { taps, len, history: Vec::new() }
    }

    fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.history.extend_from_slice(input);
        let mut i = 0;
        while i + self.len <= self.history.len() {
            let w = &self.history[i..i + self.len];
            out.push(self.taps.iter().fold(Complex32::default(), |a, &(k, t)| a + w[k] * t));
            i += 2;
        }
        self.history.drain(..i);
    }

    fn reset(&mut self) {
        self.history.clear();
    }
}

/// From the HackRF's rate down to the scope's: shift up by [`HACKRF_OFFSET`]
/// (to undo tuning that much higher), then halve the rate twice.
struct Downconverter {
    /// Where in the shift's cycle of 8 the next sample is.
    step: usize,
    first: HalfBand,
    second: HalfBand,
    mid: Vec<Complex32>,
}

impl Downconverter {
    fn new() -> Downconverter {
        // The second stage has the narrow transition (40 % → 60 % of its
        // output band); the first only has to keep the far side away.
        Downconverter { step: 0, first: HalfBand::new(23), second: HalfBand::new(59), mid: Vec::new() }
    }

    fn reset(&mut self) {
        self.first.reset();
        self.second.reset();
    }

    fn process(&mut self, input: &mut [Complex32], out: &mut Vec<Complex32>) {
        let r = std::f32::consts::FRAC_1_SQRT_2;
        let turn = [(1.0, 0.0), (r, r), (0.0, 1.0), (-r, r), (-1.0, 0.0), (-r, -r), (0.0, -1.0), (r, -r)];
        for s in input.iter_mut() {
            let (c, d) = turn[self.step];
            *s *= Complex32::new(c, d);
            self.step = (self.step + 1) % 8;
        }
        self.mid.clear();
        self.first.process(input, &mut self.mid);
        self.second.process(&self.mid, out);
    }
}

/// What to ask a HackRF whose crystal is `ppm` fast for, to get `hz`.
fn hackrf_tuning(hz: u64, ppm: i32) -> u64 {
    (hz as f64 / (1.0 + ppm.clamp(-200, 200) as f64 * 1e-6)).round() as u64
}

impl HackRfSource {
    fn open(info: &hackrf::Info, hz: u64, cfg: &ScopeConfig) -> Result<HackRfSource, String> {
        let device = hackrf::HackRf::open(info, hackrf_tuning(hz + HACKRF_OFFSET, cfg.ppm), HACKRF_RATE, cfg.hackrf_gains())?;
        Ok(HackRfSource { device, name: info.describe(), center: hz, ppm: cfg.ppm, down: Downconverter::new(), raw: Vec::new(), ready: Vec::new() })
    }

    fn retune(&mut self) -> Result<(), String> {
        self.down.reset();
        self.ready.clear();
        self.device.set_frequency(hackrf_tuning(self.center + HACKRF_OFFSET, self.ppm))
    }
}

impl Source for HackRfSource {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize, String> {
        while self.ready.len() < buf.len() {
            self.raw.resize(buf.len() * HACKRF_DECIMATION, Complex32::default());
            self.device.read(&mut self.raw)?;
            self.down.process(&mut self.raw, &mut self.ready);
        }
        let n = buf.len();
        buf.copy_from_slice(&self.ready[..n]);
        self.ready.drain(..n);
        Ok(n)
    }

    fn tune(&mut self, hz: u64) -> Result<(), String> {
        self.center = hz;
        self.retune()
    }

    fn set_ppm(&mut self, ppm: i32) -> Result<(), String> {
        self.ppm = ppm;
        self.retune()
    }

    fn set_hackrf_gains(&mut self, gains: hackrf::Gains) -> Result<(), String> {
        self.device.set_gains(gains)
    }
}

/// A receiver the scope can use.
#[derive(Clone)]
enum Found {
    Rtl(rtlsdr_nusb::DeviceDescriptor),
    HackRf(hackrf::Info),
}

impl Found {
    /// What [`ScopeConfig::serial`] holds to pick this one. An RTL-SDR's is
    /// its bare serial, as before HackRFs were supported.
    fn key(&self) -> String {
        match self {
            Found::Rtl(d) => d.serial.clone().unwrap_or_default(),
            Found::HackRf(h) => format!("hackrf:{}", h.serial),
        }
    }

    fn describe(&self) -> String {
        match self {
            Found::Rtl(d) => describe_dongle(d),
            Found::HackRf(h) => h.describe(),
        }
    }

    fn open(&self, hz: u64, cfg: &ScopeConfig) -> Result<Box<dyn Source>, String> {
        Ok(match self {
            Found::Rtl(d) => Box::new(Dongle::open(d, hz, cfg.ppm)?),
            Found::HackRf(h) => Box::new(HackRfSource::open(h, hz, cfg)?),
        })
    }
}

/// Plugged-in dongles and HackRFs.
fn list_dongles() -> Vec<Found> {
    let rtl = rtlsdr_nusb::Device::list().wait().unwrap_or_default().into_iter().map(Found::Rtl);
    rtl.chain(hackrf::list().into_iter().map(Found::HackRf)).collect()
}

// --------------------------------------------------------------- the scope

/// Runs the local dongle (or the simulator) for whoever watches `tap`.
pub struct Scope {
    stop: Arc<AtomicBool>,
    config: Arc<Mutex<ScopeConfig>>,
}

/// `RIGHELM_SCOPE_SIM=1` replaces the dongle with simulated band activity.
fn simulated() -> bool {
    std::env::var_os("RIGHELM_SCOPE_SIM").is_some_and(|v| !v.is_empty() && v != "0")
}

impl Scope {
    pub fn start(rig: Rig, tap: ScopeTap, config: ScopeConfig) -> Scope {
        let stop = Arc::new(AtomicBool::new(false));
        let config = Arc::new(Mutex::new(config));
        let (stop2, config2) = (stop.clone(), config.clone());
        std::thread::Builder::new()
            .name("scope".into())
            .spawn(move || run(&rig, &tap, &config2, &stop2))
            .expect("spawn scope");
        Scope { stop, config }
    }

    pub fn update(&self, config: ScopeConfig) {
        *self.config.lock() = config;
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < d && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(rig: &Rig, tap: &ScopeTap, config: &Mutex<ScopeConfig>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let cfg = config.lock().clone();
        if !cfg.enabled {
            tap.set_status(ScopeStatus { text: "band scope is turned off".into(), ..Default::default() });
            sleep_unless(stop, Duration::from_millis(500));
            continue;
        }
        let sim = simulated();
        let found = if sim { Vec::new() } else { list_dongles() };
        let dongles: Vec<(String, String)> = found.iter().map(|d| (d.key(), d.describe())).collect();
        let pick = found.iter().find(|d| !cfg.serial.is_empty() && d.key() == cfg.serial).or(found.first()).cloned();
        if pick.is_none() && !sim {
            tap.set_status(ScopeStatus { text: "no RTL-SDR dongle or HackRF plugged in".into(), ..Default::default() });
            sleep_unless(stop, Duration::from_secs(2));
            continue;
        }
        let present = ScopeStatus { present: true, feed: Some(cfg.feed), dongles: dongles.clone(), ..Default::default() };
        if !tap.has_watchers() && !tap.calibration_wanted() {
            let name = pick.as_ref().map(Found::describe).unwrap_or_else(|| "simulated dongle".into());
            tap.set_status(ScopeStatus { text: format!("{name} — ready (opens while the band scope is on screen)"), ..present });
            sleep_unless(stop, Duration::from_millis(300));
            continue;
        }
        let vfo = rig.state().rx_freq();
        let center = wanted_center(&cfg, vfo, None);
        let source: Result<Box<dyn Source>, String> = match &pick {
            None => Ok(Box::new(SimSource::new(center))),
            Some(d) => d.open(center, &cfg),
        };
        match source {
            Ok(source) => {
                let name = source.describe();
                rig.note(format!("band scope: {name} on {}", cfg.feed.label()));
                if let Err(e) = stream(rig, tap, config, stop, source, center, &present) {
                    rig.note(format!("band scope: {e}"));
                    tap.set_status(ScopeStatus { text: format!("{name}: {e}"), ..present });
                    sleep_unless(stop, Duration::from_secs(2));
                }
            }
            Err(e) => {
                // On Windows, usually the driver: say which one to install.
                let driver = crate::detect::driver_problems().into_iter().find(|p| p.what == crate::detect::DriverFor::Scope);
                let text = driver.map_or_else(|| format!("could not open the dongle: {e}"), |p| p.text());
                tap.set_status(ScopeStatus { text, ..present });
                sleep_unless(stop, Duration::from_secs(3));
            }
        }
    }
    tap.set_status(ScopeStatus::default());
}

/// Read and analyse until nobody watches for a while, the settings need a
/// fresh start, or the dongle fails.
fn stream(
    rig: &Rig,
    tap: &ScopeTap,
    config: &Mutex<ScopeConfig>,
    stop: &AtomicBool,
    mut source: Box<dyn Source>,
    mut center: u64,
    present: &ScopeStatus,
) -> Result<(), String> {
    let opened = config.lock().clone();
    let name = source.describe();
    let mut analyzer = Analyzer::new();
    let mut rows = Rows::new();
    // Peak-hold spectrum, decaying 10 dB a second, for answering detail
    // asks: keyed CW and the syllables of speech stay visible between beats.
    let mut smooth: Vec<f32> = Vec::new();
    // A plain running average (about half a second) for calibrating: a
    // broadcaster's carrier is steady, its programme averages out.
    let mut steady: Vec<f32> = Vec::new();
    let decay = 10f32.powf(-1.0 / (SAMPLE_RATE as f32 / FFT_HOP as f32));
    // 16 ms at a time: rows come out evenly.
    let mut buf = vec![Complex32::default(); 32_768];
    let mut idle_since: Option<Instant> = None;
    let mut ppm = opened.ppm;
    let mut gains = opened.hackrf_gains();
    let mut last_status = Instant::now() - Duration::from_secs(1);
    // Calibrate by itself once the picture has settled, until it works.
    let started = Instant::now();
    let mut next_auto = Instant::now() + Duration::from_secs(6);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let cfg = config.lock().clone();
        if !cfg.enabled || cfg.serial != opened.serial {
            return Ok(());
        }
        if tap.has_watchers() || tap.calibration_wanted() {
            idle_since = None;
        } else if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
            return Ok(());
        }
        if cfg.ppm != ppm {
            source.set_ppm(cfg.ppm)?;
            ppm = cfg.ppm;
            analyzer.reset();
            rows.reset();
            smooth.clear();
            steady.clear();
        }
        if cfg.hackrf_gains() != gains {
            gains = cfg.hackrf_gains();
            source.set_hackrf_gains(gains)?;
        }
        let (vfo, transmitting) = {
            let s = rig.state();
            (s.rx_freq(), s.transmitting())
        };
        let want = wanted_center(&cfg, vfo, Some(center));
        if want != center {
            source.tune(want)?;
            center = want;
            analyzer.reset();
            rows.reset();
            smooth.clear();
        }
        let map = Mapping { center: center as f64, feed: cfg.feed, vfo: vfo as f64, if_hz: cfg.if_hz as f64, inverted: cfg.if_inverted, scale: 1.0 + cfg.ppm_fine * 1e-6 };
        if last_status.elapsed() > Duration::from_millis(250) {
            last_status = Instant::now();
            tap.set_status(ScopeStatus {
                running: true,
                text: format!("{name} on {}", cfg.feed.label()),
                covers: Some(map.covers(SAMPLE_RATE as f64)),
                feed: Some(cfg.feed),
                ..present.clone()
            });
        }
        source.set_mapping(map);
        let n = source.read(&mut buf)?;
        if rows.map.is_some_and(|m| m != map) {
            smooth.clear();
            steady.clear();
        }
        analyzer.push(&buf[..n], |power| {
            rows.push(tap, power, map, transmitting);
            if smooth.len() != power.len() {
                smooth = power.to_vec();
                steady = power.to_vec();
            } else {
                for ((s, a), p) in smooth.iter_mut().zip(steady.iter_mut()).zip(power) {
                    *s = (*s * decay).max(*p);
                    *a = 0.97 * *a + 0.03 * p;
                }
            }
        });
        let auto = !cfg.calibrated && Instant::now() >= next_auto;
        if (auto || tap.calibration_wanted()) && started.elapsed() > Duration::from_secs(4) && !smooth.is_empty() && !transmitting {
            let found = measure_carriers(&steady, &map, SAMPLE_RATE as f64);
            log::debug!("band scope carriers (Hz, error): {found:?}");
            let c = calibrate(&cfg, &found);
            log::info!("band scope calibration: {}", c.message);
            next_auto = Instant::now() + Duration::from_secs(30);
            // Quietly retry the automatic one; always answer a request.
            if c.fix.is_some() || tap.calibration_wanted() {
                tap.calibration_done(c);
                // Don't keep correcting before the new setting has taken hold.
                next_auto = Instant::now() + Duration::from_secs(3600);
            }
        }
        if !smooth.is_empty() {
            for ask in tap.take_asks() {
                if let Some(d) = detail(&smooth, &map, SAMPLE_RATE as f64, ask.lo, ask.hi) {
                    let _ = ask.reply.try_send(d);
                }
            }
        }
    }
}

// ------------------------------------------------------------- simulation

/// Simulated band activity for testing without a dongle: CW, SSB and FT8
/// signals at fixed radio frequencies, placed as the configured feed would
/// deliver them. Follows the radio's VFO like a real IF OUT tap would.
struct SimSource {
    center: u64,
    t0: Instant,
    sent: u64,
    phases: Vec<Complex32>,
    noise: u64,
    map: Option<Mapping>,
}

/// (radio frequency, level dBFS, kind) — 0 carrier, 1 CW keying, 2 SSB, 3 FT8.
const SIM_SIGNALS: &[(f64, f32, u8)] = &[
    (14_010_000.0, -40.0, 1),
    (14_023_500.0, -55.0, 1),
    (14_025_000.0, -30.0, 1),
    (14_031_200.0, -62.0, 1),
    (14_040_000.0, -48.0, 1),
    (14_074_000.0, -58.0, 3),
    (14_075_200.0, -52.0, 3),
    (14_075_900.0, -64.0, 3),
    (14_076_400.0, -50.0, 3),
    (14_100_000.0, -45.0, 0),
    (14_150_000.0, -38.0, 2),
    (14_195_000.0, -30.0, 2),
    (14_230_000.0, -50.0, 2),
    (14_270_000.0, -44.0, 2),
    (14_313_000.0, -36.0, 2),
    (7_030_000.0, -35.0, 1),
    (7_074_000.0, -50.0, 3),
    (7_074_900.0, -46.0, 3),
    (7_150_000.0, -32.0, 2),
    (7_200_000.0, -28.0, 2),
    (7_250_000.0, -40.0, 2),
];

impl SimSource {
    fn new(center: u64) -> SimSource {
        SimSource { center, t0: Instant::now(), sent: 0, phases: vec![Complex32::new(1.0, 0.0); SIM_SIGNALS.len() * 8], noise: 0x2545_f491_4f6c_dd1d, map: None }
    }

    fn gauss(&mut self) -> f32 {
        // Sum of uniforms: cheap, close enough to Gaussian for noise.
        let mut s = 0.0;
        for _ in 0..4 {
            self.noise ^= self.noise << 13;
            self.noise ^= self.noise >> 7;
            self.noise ^= self.noise << 17;
            s += (self.noise >> 40) as f32 / (1u64 << 24) as f32;
        }
        s - 2.0
    }
}

impl Source for SimSource {
    fn describe(&self) -> String {
        "simulated dongle".into()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize, String> {
        // Real time: never faster than the dongle would be.
        let due = (self.t0.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
        if self.sent + buf.len() as u64 > due {
            let wait = (self.sent + buf.len() as u64 - due) as f64 / SAMPLE_RATE as f64;
            std::thread::sleep(Duration::from_secs_f64(wait.min(0.2)));
        }
        let map = self.map.unwrap_or(Mapping { center: self.center as f64, feed: Feed::RxOut, vfo: 0.0, if_hz: 0.0, inverted: false, scale: 1.0 });
        let rate = SAMPLE_RATE as f64;
        let t = self.sent as f64 / rate;
        let noise_amp = 10f32.powf(-55.0 / 20.0);
        for s in buf.iter_mut() {
            *s = Complex32::new(self.gauss(), self.gauss()) * noise_amp;
        }
        for (i, &(rf, db, kind)) in SIM_SIGNALS.iter().enumerate() {
            let off = map.offset_of(rf);
            if off.abs() > rate / 2.0 {
                continue;
            }
            let amp = 10f32.powf(db / 20.0);
            // A few tones per signal: SSB is a spread of tones that come and go.
            let tones: &[(f64, f32)] = match kind {
                2 => &[(400.0, 0.5), (700.0, 1.0), (1100.0, 0.7), (1500.0, 0.5), (1900.0, 0.4), (2300.0, 0.25)],
                3 => &[(0.0, 1.0)],
                _ => &[(0.0, 1.0)],
            };
            let on = match kind {
                1 => ((t * 8.0 + i as f64 * 0.37).sin() > -0.2) as u8 as f32,
                2 => (0.4 + 0.6 * ((t * 1.3 + i as f64).sin() * 0.5 + 0.5)) as f32,
                3 => (((t / 15.0).floor() as i64 + i as i64) % 2 == 0) as u8 as f32,
                _ => 1.0,
            };
            if on == 0.0 {
                continue;
            }
            for (j, &(af, ta)) in tones.iter().enumerate() {
                // FT8 hops over 8 tones every 160 ms.
                let hop = if kind == 3 { 6.25 * (((t / 0.16).floor() as i64 * 3 + i as i64) % 8) as f64 } else { 0.0 };
                let f = off + map.sign_rf() * (af + hop);
                let w = (2.0 * std::f64::consts::PI * f / rate) as f32;
                let step = Complex32::new(w.cos(), w.sin());
                let k = (i * 8 + j).min(self.phases.len() - 1);
                let mut ph = self.phases[k];
                let a = amp * ta * on;
                for s in buf.iter_mut() {
                    *s += ph * a;
                    ph *= step;
                }
                self.phases[k] = ph / ph.norm();
            }
        }
        self.sent += buf.len() as u64;
        Ok(buf.len())
    }

    fn tune(&mut self, hz: u64) -> Result<(), String> {
        self.center = hz;
        Ok(())
    }

    fn set_ppm(&mut self, _ppm: i32) -> Result<(), String> {
        Ok(())
    }

    fn set_mapping(&mut self, map: Mapping) {
        self.map = Some(map);
    }
}

impl Mapping {
    /// +1 when higher radio frequencies are higher in the dongle's band.
    fn sign_rf(&self) -> f64 {
        match self.feed {
            Feed::RxOut => 1.0,
            Feed::IfOut => self.sign(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: f64, hz: f64, db: f32, n: usize) -> Vec<Complex32> {
        let a = 10f32.powf(db / 20.0);
        (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / rate;
                Complex32::new(ph.cos() as f32, ph.sin() as f32) * a
            })
            .collect()
    }

    #[test]
    fn mapping_round_trips_and_inverts() {
        let m = Mapping { center: 9_000_000.0, feed: Feed::IfOut, vfo: 14_200_000.0, if_hz: 9_000_000.0, inverted: true, scale: 1.0 };
        // 10 kHz above the dial is 10 kHz below the IF.
        assert_eq!(m.offset_of(14_210_000.0), -10_000.0);
        assert_eq!(m.rf_of(-10_000.0), 14_210_000.0);
        let (lo, hi) = m.covers(SAMPLE_RATE as f64);
        assert!((lo - (14_200_000.0 - 819_200.0)).abs() < 1.0 && (hi - (14_200_000.0 + 819_200.0)).abs() < 1.0);
        let r = Mapping { center: 14_175_000.0, feed: Feed::RxOut, vfo: 0.0, if_hz: 0.0, inverted: true, scale: 1.0 };
        assert_eq!(r.offset_of(14_200_000.0), 25_000.0);
        assert_eq!(r.rf_of(25_000.0), 14_200_000.0);
    }

    #[test]
    fn rx_out_shows_the_whole_band_and_follows_the_vfo() {
        let cfg = ScopeConfig { feed: Feed::RxOut, ..Default::default() };
        // 20 m fits: centre on the band.
        assert_eq!(wanted_center(&cfg, 14_074_000, None), 14_175_000);
        // Stays put while the VFO is well inside...
        assert_eq!(wanted_center(&cfg, 14_300_000, Some(14_175_000)), 14_175_000);
        // ...and moves when it leaves.
        assert_eq!(wanted_center(&cfg, 7_074_000, Some(14_175_000)), 7_150_000);
        // 10 m doesn't fit: centre on the VFO.
        assert_eq!(wanted_center(&cfg, 28_400_000, None), 28_400_000);
        // Outside the bands: the VFO.
        assert_eq!(wanted_center(&cfg, 9_500_000, None), 9_500_000);
        // IF OUT never moves.
        assert_eq!(wanted_center(&ScopeConfig::default(), 7_074_000, Some(1)), 9_000_000);
    }

    #[test]
    fn a_tone_shows_at_its_radio_frequency_and_level() {
        let rate = SAMPLE_RATE as f64;
        let mut a = Analyzer::new();
        let mut spectra = Vec::new();
        // IF OUT, inverted: a signal 25 kHz above the dial is 25 kHz below the IF.
        a.push(&tone(rate, -25_000.0, -20.0, FFT_SIZE * 2), |p| spectra.push(p.to_vec()));
        assert!(spectra.len() >= 2);
        let map = Mapping { center: 9_000_000.0, feed: Feed::IfOut, vfo: 7_100_000.0, if_hz: 9_000_000.0, inverted: true, scale: 1.0 };
        let view = ScopeView { center_hz: 7_100_000.0, span_hz: 100_000.0, cols: 1000, fps: 20 };
        let row = build_row(&spectra[1], &map, rate, &view, false);
        let (peak, v) = row.cols.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, v)| (i, *v)).unwrap();
        let peak_hz = row.start_hz + (peak as f64 + 0.5) * row.col_hz;
        assert!((peak_hz - 7_125_000.0).abs() <= 100.0, "{peak_hz}");
        let db = level_db(v).unwrap();
        assert!((-21.6..=-19.0).contains(&db), "{db}");
        // Far from it, it's quiet.
        assert!(level_db(row.cols[100]).unwrap() < -90.0);
    }

    #[test]
    fn detail_finds_a_signal_to_a_few_hertz() {
        let rate = SAMPLE_RATE as f64;
        let mut a = Analyzer::new();
        let mut spectra = Vec::new();
        // IF OUT, inverted: 14.201234 MHz with the dial on 14.2 is 1234 Hz below the IF.
        a.push(&tone(rate, -1234.0, -40.0, FFT_SIZE * 2), |p| spectra.push(p.to_vec()));
        let map = Mapping { center: 9_000_000.0, feed: Feed::IfOut, vfo: 14_200_000.0, if_hz: 9_000_000.0, inverted: true, scale: 1.0 };
        let d = detail(&spectra[1], &map, rate, 14_199_000.0, 14_203_000.0).unwrap();
        assert!(d.bin_hz > 0.0 && d.start_hz >= 14_199_000.0 - d.bin_hz && d.start_hz < 14_199_100.0, "{}", d.start_hz);
        let (i, _) = d.db.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert!((d.freq(i as f64) - 14_201_234.0).abs() <= d.bin_hz, "{}", d.freq(i as f64));
        // Asks are answered through the tap.
        let tap = ScopeTap::default();
        let rx = tap.ask_detail(1.0, 1e9);
        let asks = tap.take_asks();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].hi - asks[0].lo, MAX_DETAIL_HZ);
        drop(asks);
        assert!(rx.try_recv().is_err());
    }

    fn detail_with(signals: &[(f64, f64, f32)]) -> Detail {
        // 31.25 Hz bins from 7.000 MHz, noise at -90 dB, plus (lo, hi, dB) blocks.
        let mut d = Detail { start_hz: 7_000_000.0, bin_hz: 31.25, db: vec![-90.0; 640] };
        for (i, v) in d.db.iter_mut().enumerate() {
            let f = 7_000_000.0 + i as f64 * 31.25;
            *v += ((i * 7919) % 13) as f32 * 0.3;
            for &(lo, hi, db) in signals {
                if f >= lo && f <= hi {
                    *v = db;
                }
            }
        }
        d
    }

    #[test]
    fn finds_cw_peaks_between_bins() {
        let mut d = detail_with(&[]);
        // A carrier at 7,005,040 Hz: between bins 161 (7,005,031.25) and 162.
        d.db[161] = -40.0;
        d.db[162] = -46.0;
        d.db[160] = -52.0;
        let s = d.find_signal(7_005_200.0, 1_000.0).unwrap();
        assert!((s.peak_hz - 7_005_040.0).abs() < 12.0, "{}", s.peak_hz);
        assert!(s.width_hz() < 200.0 && s.snr_db > 40.0);
        // Nothing there: no signal.
        assert_eq!(detail_with(&[]).find_signal(7_005_000.0, 500.0), None);
    }

    #[test]
    fn finds_the_edges_of_speech() {
        // USB voice: 7,010,300..7,012,700 with a hole in the middle.
        let d = detail_with(&[(7_010_300.0, 7_011_000.0, -55.0), (7_011_100.0, 7_012_700.0, -60.0)]);
        let s = d.find_signal(7_011_500.0, 1_500.0).unwrap();
        assert!((s.lo_hz - 7_010_300.0).abs() < 40.0, "{}", s.lo_hz);
        assert!((s.hi_hz - 7_012_700.0).abs() < 40.0, "{}", s.hi_hz);
        // Prefers the signal near the click over a stronger one further off.
        let d = detail_with(&[(7_003_000.0, 7_003_030.0, -40.0), (7_006_000.0, 7_006_030.0, -42.0)]);
        assert!((d.find_signal(7_005_800.0, 3_500.0).unwrap().peak_hz - 7_006_015.0).abs() < 40.0);
    }

    #[test]
    fn calibrates_from_broadcast_carriers() {
        let rate = SAMPLE_RATE as f64;
        // RX OUT on 41 m, dongle 2.3 ppm fast: carriers appear 2.3 ppm high.
        let map = Mapping { center: 7_400_000.0, feed: Feed::RxOut, vfo: 0.0, if_hz: 0.0, inverted: false, scale: 1.0 };
        let mut samples = vec![Complex32::default(); FFT_SIZE * 2];
        for f in [7_350_000.0, 7_490_000.0, 7_505_000.0, 7_585_000.0] {
            let seen = f * (1.0 + 2.3e-6);
            for (s, t) in samples.iter_mut().zip(tone(rate, map.offset_of(seen), -30.0, FFT_SIZE * 2)) {
                *s += t;
            }
        }
        // A voice-like blob off the grid must not count.
        for (s, t) in samples.iter_mut().zip(tone(rate, map.offset_of(7_401_234.0), -25.0, FFT_SIZE * 2)) {
            *s += t;
        }
        let mut a = Analyzer::new();
        let mut spectra = Vec::new();
        a.push(&samples, |p| spectra.push(p.to_vec()));
        let found = measure_carriers(&spectra[1], &map, rate);
        assert_eq!(found.len(), 4, "{found:?}");
        let cfg = ScopeConfig { feed: Feed::RxOut, ppm: 1, ..Default::default() };
        let c = calibrate(&cfg, &found);
        let Some(CalibrationFix::Ppm(ppm, fine)) = c.fix else { panic!("{}", c.message) };
        assert!(ppm == -1 && (fine + 0.3).abs() < 0.11, "{ppm} {fine}");
        // IF OUT: a fixed offset moves the IF frequency.
        let cfg = ScopeConfig::default();
        let c = calibrate(&cfg, &[(7.2e6, 17.0), (7.3e6, 18.0), (7.5e6, 17.5), (7.6e6, 300.0)]);
        assert_eq!(c.fix, Some(CalibrationFix::IfHz(8_999_982)), "{}", c.message);
        // Too few: no change.
        assert_eq!(calibrate(&cfg, &[(7.2e6, 17.0)]).fix, None);
    }

    #[test]
    fn reference_frequencies_are_on_the_grid() {
        let r = reference_frequencies(9_990_000.0, 10_010_000.0);
        assert_eq!(r, vec![10_000_000.0]);
        let r = reference_frequencies(7_590_000.0, 7_900_000.0);
        assert_eq!(r, vec![7_850_000.0, 7_590_000.0, 7_595_000.0, 7_600_000.0]);
    }

    #[test]
    fn outside_the_dongle_is_no_data() {
        let map = Mapping { center: 14_175_000.0, feed: Feed::RxOut, vfo: 0.0, if_hz: 0.0, inverted: false, scale: 1.0 };
        let power = vec![1e-6f32; FFT_SIZE];
        // 2 MHz view centred 1 MHz above the dongle: the top half is empty.
        let view = ScopeView { center_hz: 15_175_000.0, span_hz: 2_000_000.0, cols: 100, fps: 20 };
        let row = build_row(&power, &map, SAMPLE_RATE as f64, &view, false);
        assert!(row.cols[..30].iter().all(|&v| v > 0));
        assert!(row.cols[60..].iter().all(|&v| v == 0));
    }

    #[test]
    fn watchers_get_rows_for_their_own_views() {
        let tap = ScopeTap::default();
        let a = tap.subscribe(ScopeView { span_hz: 100_000.0, cols: 64, ..Default::default() });
        let b = tap.subscribe(ScopeView { span_hz: 20_000.0, cols: 32, fps: 30, ..Default::default() });
        let mut rows = Rows::new();
        let map = Mapping { center: 14_100_000.0, feed: Feed::RxOut, vfo: 0.0, if_hz: 0.0, inverted: false, scale: 1.0 };
        rows.push(&tap, &vec![1e-8; FFT_SIZE], map, false);
        assert_eq!(a.rows.try_recv().unwrap().cols.len(), 64);
        assert_eq!(b.rows.try_recv().unwrap().cols.len(), 32);
        assert!(tap.has_watchers());
        drop(a);
        drop(b);
        assert!(!tap.has_watchers());
    }

    #[test]
    fn views_are_sanitized() {
        let v = ScopeView { center_hz: f64::NAN, span_hz: 1e12, cols: 0, fps: 200 }.sanitized();
        assert_eq!((v.center_hz, v.span_hz, v.cols, v.fps), (0.0, SAMPLE_RATE as f64, 16, MAX_FPS));
    }

    #[test]
    fn levels_encode_in_half_db_steps() {
        assert_eq!(level_db(encode_db(-20.0)), Some(-20.0));
        assert_eq!(level_db(encode_db(-73.5)), Some(-73.5));
        assert_eq!(encode_db(-500.0), 1);
        assert_eq!(level_db(0), None);
    }

    /// Level (dB) of a tone through the HackRF's downconverter, which moves
    /// it up by the offset.
    fn through_downconverter(hz: f64) -> f32 {
        let rate = HACKRF_RATE as f64;
        let mut input: Vec<Complex32> = (0..HACKRF_RATE as usize / 50)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / rate;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut out = Vec::new();
        let mut d = Downconverter::new();
        d.process(&mut input, &mut out);
        let tail = &out[out.len() / 2..];
        let p = tail.iter().map(|s| s.norm_sqr()).sum::<f32>() / tail.len() as f32;
        10.0 * p.log10()
    }

    #[test]
    fn hackrf_offset_keeps_the_view_and_drops_its_own_spike() {
        let off = HACKRF_OFFSET as f64;
        let half = SAMPLE_RATE as f64 * USABLE / 2.0;
        // What the scope shows comes through whole.
        for f in [-half, -half / 2.0, 0.0, half / 2.0, half] {
            let db = through_downconverter(f - off);
            assert!(db.abs() < 0.2, "{f} Hz: {db} dB");
        }
        // The HackRF's own spike (at its tuning, 0 Hz) lands past the edge.
        assert!(off > half * 1.2);
        // Anything that would fold into the view is gone.
        for f in [0.5e6, 1.0e6, 1.5e6, 2.0e6, 3.0e6, -3.5e6] {
            let db = through_downconverter(f);
            let lands = f + off;
            let folds = (lands / SAMPLE_RATE as f64).round() * SAMPLE_RATE as f64;
            if (lands - folds).abs() < half {
                assert!(db < -60.0, "{f} Hz lands in view: {db} dB");
            }
        }
    }

    #[test]
    fn hackrf_tuning_corrects_the_crystal() {
        assert_eq!(hackrf_tuning(9_000_000, 0), 9_000_000);
        // A crystal 10 ppm fast is asked for 10 ppm less.
        assert_eq!(hackrf_tuning(10_000_000, 10), 9_999_900);
        assert_eq!(hackrf_tuning(10_000_000, -10), 10_000_100);
    }
}
