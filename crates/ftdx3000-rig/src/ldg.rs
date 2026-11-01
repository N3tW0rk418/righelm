//! LDG AT-1000ProII / AT-600ProII automatic tuner on its rear "Ext. Meter"
//! jack (4-pin mini-DIN, TTL serial: needs a USB-TTL adapter), from LDG's
//! "External Meter Serial Communications Protocol Specification".
//!
//! * 38,400 bps 8N1. Every command is one ASCII letter after a wake-up
//!   space (the tuner's CPU sleeps), and the tuner answers with one letter
//!   when it's done.
//! * `S` makes it stream 8-byte meter frames (forward, reflected, a status
//!   word, all big-endian, then `;;`) about 19 times a second, for about a
//!   second before its CPU sleeps again; `X` stops that so a command's
//!   answer can be read.
//! * It sleeps so as not to put digital noise into the receiver, so it's
//!   only kept awake while the radio transmits; otherwise it is just asked
//!   now and then whether it's still there.
//! * Switched off at its power button, it doesn't answer at all: shown as
//!   off, and found again when it's switched back on.
//!
//! A tuning cycle needs RF: [`LdgCommand::Tune`] holds the radio's CW key
//! down at low power for it (CAT TX alone sends no carrier in CW) and
//! always lets go.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::cw::{KeyLine, LocalKeyLine};
use crate::engine::{Freshness, Rig, Source};

const BAUD: u32 = 38_400;
/// What the tuner sends for `Z`.
const SYNC_ANSWER: &[u8] = b"000000000000000AzAz";
/// While transmitting: wake the meter this often, before its second of
/// readings runs out, so they don't stop between CW elements or words.
const POKE: Duration = Duration::from_millis(600);
/// While receiving: check this often that the tuner is still there.
const ALIVE_EVERY: Duration = Duration::from_secs(5);
/// LDG: wait at least 200 ms after an answer before the next command.
const SETTLE: Duration = Duration::from_millis(250);
/// The carrier is never on longer than this.
const MAX_CARRIER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdgConfig {
    pub enabled: bool,
    /// Serial port of the USB-TTL adapter on the tuner's meter jack (best a
    /// `/dev/serial/by-id/...` path).
    pub port: String,
    /// Meter calibration: watts = ((raw + offset) / this)². The detector is
    /// square-law.
    pub meter_k: f32,
    /// Carrier power for a tuning cycle, in watts.
    pub tune_watts: u8,
}

impl Default for LdgConfig {
    fn default() -> Self {
        Self { enabled: false, port: String::new(), meter_k: METER_K, tune_watts: 15 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunerMode {
    /// Tunes by itself when it sees a high SWR.
    Auto,
    /// Tunes only when asked.
    Manual,
    /// Out of line.
    Bypass,
}

impl TunerMode {
    pub fn label(self) -> &'static str {
        match self {
            TunerMode::Auto => "auto",
            TunerMode::Manual => "manual",
            TunerMode::Bypass => "bypass",
        }
    }
}

/// What the tuner last said its antenna and mode were. It can't be asked,
/// so this is kept across restarts (it goes stale if they're changed on
/// the tuner's front panel meanwhile, until the next change from here).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdgMemory {
    pub antenna: Option<u8>,
    pub mode: Option<TunerMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuneResult {
    /// Below 1.5:1.
    Good,
    /// 1.5 to 3:1.
    Usable,
    Failed,
}

impl TuneResult {
    pub fn label(self) -> &'static str {
        match self {
            TuneResult::Good => "tuned, SWR below 1.5",
            TuneResult::Usable => "tuned, SWR 1.5 to 3",
            TuneResult::Failed => "tune failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LdgCommand {
    /// A tuning cycle with a carrier from the radio: from memory, or a full
    /// search.
    Tune { full: bool },
    /// Take the tuner out of line.
    Bypass,
    /// Back in line, tuning by itself.
    Auto,
    Manual,
    ToggleAntenna,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LdgStatus {
    /// The tuner answers.
    pub connected: bool,
    pub text: String,
    pub forward_w: f32,
    pub reflected_w: f32,
    /// Only while there's power to measure it with.
    pub swr: Option<f32>,
    /// The third word of the meter frame (its meaning isn't documented).
    pub status_word: u16,
    /// What the tuner last said; unknown until a command has been answered.
    pub antenna: Option<u8>,
    pub mode: Option<TunerMode>,
    pub last_tune: Option<TuneResult>,
    /// A command in progress.
    pub busy: Option<&'static str>,
}

/// Meter calibration of an AT-1000ProII, from steady CW carriers on 40 m:
/// 10 W read raw 77 and 50 W raw 191. Also within 2 W of an FT8 reading of
/// 65 W at raw 222.5. Unverified above about 100 W, where diode detectors
/// start to compress.
pub const METER_K: f32 = 29.16;
/// The detector diode's turn-on voltage, in raw units: below it the tuner
/// reads 0.
const METER_OFFSET: f32 = 15.2;

/// Raw meter units to watts: the detectors are square-law (the raw value
/// follows the RF voltage, less the diode's drop).
pub fn meter_watts(raw: u16, k: f32) -> f32 {
    if raw == 0 {
        return 0.0;
    }
    let v = (raw as f32 + METER_OFFSET) / k.max(1.0);
    v * v
}

pub fn swr(forward_w: f32, reflected_w: f32) -> Option<f32> {
    if forward_w < 0.5 {
        return None;
    }
    let g = (reflected_w / forward_w).sqrt();
    Some(if g >= 0.99 { 99.0 } else { (1.0 + g) / (1.0 - g) })
}

/// Meter frames out of a byte stream, resynchronising on the `;;` ending.
#[derive(Default)]
struct Frames {
    buf: Vec<u8>,
    /// Bytes that weren't part of a frame, such as a command's answer.
    strays: Vec<u8>,
}

impl Frames {
    /// (forward, reflected, status) of each complete frame.
    fn push(&mut self, bytes: &[u8]) -> Vec<(u16, u16, u16)> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while self.buf.len() >= 8 {
            if self.buf[6..8] == *b";;" {
                let w = |i: usize| u16::from_be_bytes([self.buf[i], self.buf[i + 1]]);
                out.push((w(0), w(2), w(4)));
                self.buf.drain(..8);
            } else {
                self.strays.push(self.buf.remove(0));
            }
        }
        out
    }

    /// A command's answer among the frames: a letter on its own between
    /// them (or with nothing after it yet).
    fn answer(&mut self, letters: &[u8]) -> Option<u8> {
        let alone = |b: &u8| letters.contains(b);
        let found = self.strays.iter().copied().find(alone).or_else(|| (self.buf.len() == 1).then(|| self.buf[0]).filter(alone));
        self.strays.clear();
        found
    }
}

/// Runs the tuner's serial link for as long as it lives.
pub struct LdgTuner {
    status: Arc<Mutex<LdgStatus>>,
    config: Arc<Mutex<LdgConfig>>,
    commands: Sender<LdgCommand>,
    stop: Arc<AtomicBool>,
}

impl LdgTuner {
    pub fn start(rig: Rig, config: LdgConfig, memory: LdgMemory) -> LdgTuner {
        let status = Arc::new(Mutex::new(LdgStatus { antenna: memory.antenna, mode: memory.mode, ..Default::default() }));
        let config = Arc::new(Mutex::new(config));
        let stop = Arc::new(AtomicBool::new(false));
        let (commands, rx) = unbounded();
        let (s, c, st) = (status.clone(), config.clone(), stop.clone());
        std::thread::Builder::new().name("ldg".into()).spawn(move || run(&rig, &s, &c, &st, &rx)).expect("spawn ldg");
        LdgTuner { status, config, commands, stop }
    }

    pub fn update(&self, config: LdgConfig) {
        *self.config.lock() = config;
    }

    pub fn status(&self) -> LdgStatus {
        self.status.lock().clone()
    }

    pub fn memory(&self) -> LdgMemory {
        let s = self.status.lock();
        LdgMemory { antenna: s.antenna, mode: s.mode }
    }

    pub fn send(&self, cmd: LdgCommand) {
        let _ = self.commands.send(cmd);
    }
}

impl Drop for LdgTuner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

type Port = Box<dyn serialport::SerialPort>;

fn open(path: &str) -> Result<Port, String> {
    let mut p = serialport::new(path, BAUD)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| e.to_string())?;
    // Some adapters power their level shifter from DTR.
    let _ = p.write_data_terminal_ready(true);
    Ok(p)
}

/// Read whatever arrives within `d`.
fn read_for(port: &mut Port, d: Duration) -> Vec<u8> {
    let (t0, mut out, mut buf) = (Instant::now(), Vec::new(), [0u8; 256]);
    while t0.elapsed() < d {
        match port.read(&mut buf) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    out
}

/// Wake the tuner and send one command letter.
fn send_letter(port: &mut Port, letter: u8) -> std::io::Result<()> {
    port.write_all(b" ")?;
    port.flush()?;
    std::thread::sleep(Duration::from_millis(2));
    port.write_all(&[letter])?;
    port.flush()
}

/// The first answer letter within `timeout` (stray meter bytes are below
/// `'0'`, or end in `;`).
fn answer(port: &mut Port, timeout: Duration, stop: &AtomicBool) -> Option<u8> {
    let t0 = Instant::now();
    let mut buf = [0u8; 64];
    while t0.elapsed() < timeout && !stop.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(n) => {
                if let Some(&b) = buf[..n].iter().find(|&&b| b >= b'0' && b != b';') {
                    return Some(b);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    None
}

/// Stop the meter stream and drop what's left of it.
fn control_mode(port: &mut Port) -> std::io::Result<()> {
    send_letter(port, b'X')?;
    read_for(port, SETTLE);
    let _ = port.clear(serialport::ClearBuffer::Input);
    Ok(())
}

fn sync(port: &mut Port) -> bool {
    let _ = port.clear(serialport::ClearBuffer::Input);
    if send_letter(port, b'Z').is_err() {
        return false;
    }
    let got = read_for(port, Duration::from_millis(600));
    got.windows(SYNC_ANSWER.len()).any(|w| w == SYNC_ANSWER) || got.ends_with(b"AzAz")
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < d && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(rig: &Rig, status: &Mutex<LdgStatus>, config: &Mutex<LdgConfig>, stop: &AtomicBool, commands: &Receiver<LdgCommand>) {
    let set_text = |text: String| {
        let mut s = status.lock();
        *s = LdgStatus { text, antenna: s.antenna, mode: s.mode, last_tune: s.last_tune, ..Default::default() };
    };
    while !stop.load(Ordering::Relaxed) {
        let cfg = config.lock().clone();
        if !cfg.enabled || cfg.port.is_empty() {
            set_text(if cfg.enabled { "choose the tuner's serial port in Settings".into() } else { "LDG tuner is turned off in Settings".into() });
            // Commands while it's off are dropped, not kept for later.
            while commands.try_recv().is_ok() {}
            sleep_unless(stop, Duration::from_millis(500));
            continue;
        }
        let mut port = match open(&cfg.port) {
            Ok(p) => p,
            Err(e) => {
                set_text(format!("can't open {}: {e}", cfg.port));
                sleep_unless(stop, Duration::from_secs(3));
                continue;
            }
        };
        // Switched off at its button, it doesn't answer: keep asking.
        loop {
            if stop.load(Ordering::Relaxed) || *config.lock() != cfg {
                break;
            }
            if sync(&mut port) {
                rig.note("LDG tuner: connected");
                if let Err(e) = session(rig, &mut port, status, config, &cfg, stop, commands) {
                    rig.note(format!("LDG tuner: {e}"));
                    set_text(e);
                }
                break;
            }
            set_text("no answer — is the tuner switched on?".into());
            while commands.try_recv().is_ok() {}
            sleep_unless(stop, Duration::from_secs(2));
        }
    }
    *status.lock() = LdgStatus::default();
}

/// Meter readings and commands until the tuner stops answering or the
/// settings change.
fn session(
    rig: &Rig,
    port: &mut Port,
    status: &Mutex<LdgStatus>,
    config: &Mutex<LdgConfig>,
    cfg: &LdgConfig,
    stop: &AtomicBool,
    commands: &Receiver<LdgCommand>,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    {
        let mut s = status.lock();
        s.connected = true;
        s.text = "connected".into();
    }
    let mut frames = Frames::default();
    let mut last_frame = Instant::now();
    let mut last_poke = Instant::now() - POKE;
    let mut last_alive = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        if stop.load(Ordering::Relaxed) || *config.lock() != *cfg {
            let _ = control_mode(port);
            return Ok(());
        }
        if let Ok(cmd) = commands.try_recv() {
            control_mode(port).map_err(io)?;
            status.lock().busy = Some(match cmd {
                LdgCommand::Tune { full: false } => "tuning from memory",
                LdgCommand::Tune { full: true } => "full tune",
                _ => "busy",
            });
            let r = command(rig, port, status, cfg, stop, cmd);
            {
                let mut s = status.lock();
                s.busy = None;
                if let Err(e) = &r {
                    s.text = e.clone();
                } else {
                    s.text = "connected".into();
                }
            }
            if let Err(e) = r {
                rig.note(format!("LDG tuner: {e}"));
            }
            std::thread::sleep(SETTLE);
            last_alive = Instant::now();
        }
        if rig.state().transmitting() {
            if last_poke.elapsed() > POKE {
                send_letter(port, b'S').map_err(io)?;
                last_poke = Instant::now();
            }
            if last_frame.elapsed() > POKE * 6 {
                last_frame = Instant::now();
                if !alive(port) {
                    return Err("the tuner stopped answering (switched off?)".into());
                }
            }
            last_alive = Instant::now();
        } else {
            // Receiving: nothing to measure, let it sleep.
            if last_frame.elapsed() > POKE * 2 {
                let mut s = status.lock();
                (s.forward_w, s.reflected_w, s.swr) = (0.0, 0.0, None);
            }
            if last_alive.elapsed() > ALIVE_EVERY {
                last_alive = Instant::now();
                if !alive(port) {
                    return Err("the tuner stopped answering (switched off?)".into());
                }
            }
        }
        match port.read(&mut buf) {
            Ok(n) => {
                for (f, r, w) in frames.push(&buf[..n]) {
                    last_frame = Instant::now();
                    show(status, cfg, f, r, w);
                }
                frames.strays.clear();
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn show(status: &Mutex<LdgStatus>, cfg: &LdgConfig, f: u16, r: u16, w: u16) {
    let (fw, rw) = (meter_watts(f, cfg.meter_k), meter_watts(r, cfg.meter_k));
    let mut s = status.lock();
    s.forward_w = fw;
    s.reflected_w = rw;
    s.swr = swr(fw, rw);
    s.status_word = w;
}

/// Is the tuner still there? Leaves it in control mode (and asleep).
fn alive(port: &mut Port) -> bool {
    control_mode(port).is_ok() && sync(port)
}

/// The answer to a tuning cycle, showing the meter meanwhile: the meter
/// is woken every so often, and the answer picked out from between its
/// frames.
fn metered_answer(rig: &Rig, port: &mut Port, status: &Mutex<LdgStatus>, cfg: &LdgConfig, stop: &AtomicBool) -> Option<u8> {
    let t0 = Instant::now();
    let mut frames = Frames::default();
    let (mut last_frame, mut last_poke) = (Instant::now(), Instant::now());
    let mut buf = [0u8; 256];
    while t0.elapsed() < MAX_CARRIER && !stop.load(Ordering::Relaxed) {
        if last_frame.elapsed() > Duration::from_millis(300) && last_poke.elapsed() > Duration::from_millis(300) {
            let _ = send_letter(port, b'S');
            last_poke = Instant::now();
        }
        match port.read(&mut buf) {
            Ok(n) => {
                log::debug!("LDG tuning: {:02x?}", &buf[..n]);
                for (f, r, w) in frames.push(&buf[..n]) {
                    last_frame = Instant::now();
                    show(status, cfg, f, r, w);
                    // For calibrating the meter against the radio's own.
                    if log::log_enabled!(log::Level::Debug) {
                        let s = rig.state();
                        let po = s.meter(ftdx3000_cat::types::Meter::Po).map(|v| ftdx3000_cat::types::interpolate(&ftdx3000_cat::types::PO_CAL, v));
                        log::debug!("LDG frame: fwd {f} ref {r} word {w:#06x}; radio PO {po:?} W, power set {} W", s.power);
                    }
                }
                if let Some(a) = frames.answer(b"TMF") {
                    let _ = control_mode(port);
                    return Some(a);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    let _ = control_mode(port);
    None
}

/// The CW key held down while it lives. The key line lets go by itself
/// when dropped; TX0 as well, in case anything else keyed the radio.
struct Carrier<'a> {
    rig: &'a Rig,
    line: LocalKeyLine,
}

impl Drop for Carrier<'_> {
    fn drop(&mut self) {
        let _ = self.line.set(false);
        self.rig.send("TX0;");
    }
}

fn command(rig: &Rig, port: &mut Port, status: &Mutex<LdgStatus>, cfg: &LdgConfig, stop: &AtomicBool, cmd: LdgCommand) -> Result<(), String> {
    let quick = Duration::from_secs(2);
    let io = |e: std::io::Error| e.to_string();
    let simple = |port: &mut Port, letter: u8| -> Result<u8, String> {
        send_letter(port, letter).map_err(io)?;
        answer(port, quick, stop).ok_or_else(|| format!("no answer to {}", letter as char))
    };
    match cmd {
        LdgCommand::ToggleAntenna => {
            let a = simple(port, b'A')?;
            status.lock().antenna = matches!(a, b'1' | b'2').then_some(a - b'0');
        }
        LdgCommand::Bypass => {
            if simple(port, b'P')? == b'P' {
                status.lock().mode = Some(TunerMode::Bypass);
            }
        }
        LdgCommand::Auto => {
            if simple(port, b'C')? == b'A' {
                status.lock().mode = Some(TunerMode::Auto);
            }
        }
        LdgCommand::Manual => {
            if simple(port, b'M')? == b'M' {
                status.lock().mode = Some(TunerMode::Manual);
            }
        }
        LdgCommand::Tune { full } => {
            let r = tune(rig, port, status, cfg, stop, full)?;
            let mut s = status.lock();
            s.last_tune = Some(r);
            // A tuning cycle puts it back in line.
            if s.mode == Some(TunerMode::Bypass) {
                s.mode = None;
            }
        }
    }
    Ok(())
}

/// A tuning cycle: the radio's own tuner out, low power, CW with break-in
/// and the key held down while the LDG tunes, then the radio as it was.
fn tune(rig: &Rig, port: &mut Port, status: &Mutex<LdgStatus>, cfg: &LdgConfig, stop: &AtomicBool, full: bool) -> Result<TuneResult, String> {
    let (power, mode, internal, break_in, caps, has_ac) = {
        if !rig.link().is_open() {
            return Err("can't tune: the radio isn't connected".into());
        }
        let s = rig.state();
        if s.transmitting() {
            return Err("can't tune: the radio is already transmitting".into());
        }
        if s.split() {
            return Err("can't tune while working split".into());
        }
        (s.power, s.mode_a, s.tuner, s.break_in, s.model.caps.power, s.model.has("AC"))
    };
    // First, so nothing about the radio has changed if the key can't be had.
    let line = LocalKeyLine::open(rig, Source::Gui).map_err(|e| format!("can't key the radio for tuning: {e} (is the CW key armed?)"))?;
    let watts = cfg.tune_watts.clamp(caps.0.max(1), caps.1);
    let read_power = || {
        let a = rig.read("PC;", Freshness::Radio, Source::Gui, Duration::from_secs(2)).ok()?;
        a.strip_prefix("PC")?.trim_end_matches(';').parse::<u8>().ok()
    };
    if has_ac && internal != 0 {
        rig.send("AC000;");
    }
    // Mode first: some radios (the FTDX3000) keep a power setting per mode
    // and bring it back with the mode.
    rig.send(ftdx3000_cat::set_mode_cmd(ftdx3000_cat::types::Mode::Cw));
    if !break_in {
        rig.send("BI1;");
    }
    std::thread::sleep(Duration::from_millis(300));
    let cw_power = read_power();
    let restore = || {
        if !break_in {
            rig.send("BI0;");
        }
        // CW's own power as it was, then the mode (and its power) as it was.
        if let Some(p) = cw_power {
            rig.send(format!("PC{p:03};"));
        }
        if let Some(m) = mode {
            rig.send(ftdx3000_cat::set_mode_cmd(m));
        }
        rig.send(format!("PC{power:03};"));
        if has_ac && internal != 0 {
            rig.send("AC001;");
        }
    };
    rig.send(format!("PC{watts:03};"));
    // Never key at more than the tune power.
    match read_power() {
        Some(p) if p <= watts => {}
        got => {
            drop(line);
            restore();
            return Err(match got {
                Some(p) => format!("can't tune: the radio is at {p} W, not the {watts} W asked for; nothing was sent"),
                None => "can't tune: couldn't read the radio's power back; nothing was sent".into(),
            });
        }
    }
    if let Err(e) = send_letter(port, if full { b'F' } else { b'T' }) {
        drop(line);
        restore();
        return Err(e.to_string());
    }
    // The tuner waits for RF once asked (as LDGControl does it).
    std::thread::sleep(SETTLE);
    let got = {
        let mut carrier = Carrier { rig, line };
        match carrier.line.set(true) {
            Ok(()) => metered_answer(rig, port, status, cfg, stop),
            Err(e) => {
                drop(carrier);
                restore();
                return Err(format!("can't key the radio: {e}"));
            }
        }
    };
    // Break-in hangs on a moment after the key lets go.
    std::thread::sleep(Duration::from_millis(500));
    restore();
    match got {
        Some(b'T') => Ok(TuneResult::Good),
        Some(b'M') => Ok(TuneResult::Usable),
        Some(b'F') => Ok(TuneResult::Failed),
        Some(b) => Err(format!("unexpected answer {:?} to tune", b as char)),
        None => Err(format!("no answer from the tuner within {} s; carrier stopped", MAX_CARRIER.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_resync_on_the_terminator() {
        let mut f = Frames::default();
        // Half a frame of junk first, then two frames split across reads.
        assert!(f.push(&[0x04, 0x82, b';', b';', 0x00, 0xC8]).is_empty());
        let got = f.push(&[0x00, 0x14, 0x04, 0x82, b';', b';', 0x00, 0x00, 0x00, 0x00, 0x04, 0x82, b';', b';']);
        assert_eq!(got, vec![(200, 20, 0x0482), (0, 0, 0x0482)]);
    }

    #[test]
    fn an_answer_between_frames_is_found() {
        let mut f = Frames::default();
        let frame = [0x00, 0x30, 0x00, 0x02, 0x04, 0x82, b';', b';'];
        f.push(&frame);
        assert_eq!(f.answer(b"TMF"), None);
        // "T" (good tune) arrives between two frames, and on its own at the end.
        let mut bytes = vec![b'T'];
        bytes.extend_from_slice(&frame);
        assert_eq!(f.push(&bytes).len(), 1);
        assert_eq!(f.answer(b"TMF"), Some(b'T'));
        f.push(b"M");
        assert_eq!(f.answer(b"TMF"), Some(b'M'));
    }

    #[test]
    fn watts_and_swr() {
        assert_eq!(meter_watts(0, METER_K), 0.0);
        // The calibration carriers, and an FT8 reading.
        for (raw, w, within) in [(77, 10.0, 0.2), (191, 50.0, 0.5), (223, 65.0, 2.0)] {
            assert!((meter_watts(raw, METER_K) - w).abs() < within, "{raw}: {}", meter_watts(raw, METER_K));
        }
        assert_eq!(swr(0.0, 0.0), None);
        assert!((swr(100.0, 0.0).unwrap() - 1.0).abs() < 1e-6);
        // 11 % reflected is 2:1.
        assert!((swr(100.0, 11.11).unwrap() - 2.0).abs() < 0.01);
        assert_eq!(swr(10.0, 10.0), Some(99.0));
    }
}
