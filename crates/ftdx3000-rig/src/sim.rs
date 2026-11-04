//! A software FTDX3000 for development and tests — or any other supported
//! radio.
//!
//! It answers every command in the CAT reference with the same frame formats
//! as the radio, rejects invalid commands with `?;`, supports auto-information
//! and powering off. It is not a model of the radio's DSP — meters wander
//! pseudo-randomly.
//!
//! As another radio ([`SimRadio::for_model`]) the same core keeps that
//! radio's menus, bands and modes, and the radio's own translator turns
//! what is said on the line into that radio's protocol, so a program (or
//! Hamlib) can be pointed at it as at the real one.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ftdx3000_cat::command::{self, Request};
use ftdx3000_cat::dialect::Emulator;
use ftdx3000_cat::menu::{self, Kind, Value};
use ftdx3000_cat::model::FTDX3000;
use ftdx3000_cat::{ChannelInfo, Framer, Mode, RadioModel, frame};
use parking_lot::Mutex;

use crate::transport::{Link, pipe};

pub struct SimCore {
    pub model: &'static RadioModel,
    /// Stored answers for simple "set == answer" commands, by answer key.
    answers: BTreeMap<String, String>,
    pub freq_a: u64,
    pub freq_b: u64,
    pub mode_a: Mode,
    pub mode_b: Mode,
    /// Hz VFO-A moves by itself going from USB to LSB (and back the other
    /// way), as a real FTDX3000 was seen to do. 0 = the dial stays put.
    pub sideband_shift: i64,
    pub power_on: bool,
    pub ai: bool,
    pub tx: u8,
    pub mox: bool,
    tx_vfo: u8,
    rx_function: u8,
    clar_offset: i32,
    rx_clar: bool,
    tx_clar: bool,
    lock: u8,
    fast: u8,
    antenna: u8,
    roofing: char,
    tuner: u8,
    memory_channel: u16,
    memories: BTreeMap<u16, ChannelInfo>,
    rng: u32,
    s_meter: u8,
    /// Frames to send unsolicited (auto-information).
    pub outbox: Vec<String>,
}

fn default_menu_raw(model: &RadioModel, item: &menu::MenuItem) -> String {
    let v = match item.kind {
        Kind::Text { max } => return format!("{:<max$}", model.name.chars().take(max).collect::<String>()),
        Kind::Num { min, max, signed, .. } => {
            if signed { 0 } else { min.max(0).min(max) }
        }
        Kind::Coded(p) => p[0].0,
        Kind::MicGain => 1000,
        _ => 0,
    };
    let v = match item.num {
        _ if !model.is_ftdx3000() => v,
        38 => 1, // CAT RATE 9600
        39 => 0,
        40 => 1,
        62 | 79 | 182 => 300,
        _ => v,
    };
    item.encode(&Value::Int(v), None)
}

impl Default for SimCore {
    fn default() -> Self {
        Self::new()
    }
}

impl SimCore {
    pub fn new() -> Self {
        Self::for_model(&FTDX3000)
    }

    /// A simulated `model` (in RigHelm's language; see [`SimRadio`] for one
    /// that speaks the radio's own).
    pub fn for_model(model: &'static RadioModel) -> Self {
        let mut answers = BTreeMap::new();
        for a in [
            "AG0100;", "BC00;", "BI0;", "BP00000;", "BP01150;", "BY00;", "CN000;", "CO0000;", "CO0110;",
            "CO0225;", "CS0;", "CT00;", "DA101010;", "GT03;", "ID0462;", "IS0+0000;", "KM1CQ CQ DE FTDX3000 K};",
            "KM2TU 5NN};", "KM3};", "KM4};", "KM5};", "KP40;", "KR1;", "KS025;", "LM00;", "MG050;", "ML0000;",
            "ML1050;", "MS1;", "NA00;", "NB00;", "NL0010;", "NR00;", "OS00;", "PA00;", "PB00;", "PC100;",
            "PL050;", "PR00;", "PR10;", "RA00;", "RG0255;", "RI00;", "RL008;", "RO0000050;", "RS0;", "SC0;",
            "SD0200;", "SF1;", "SH000;", "SQ0000;", "TS0;", "UL0;", "VD0300;", "VF001281;", "VG050;", "VS0;",
            "VX0;", "EM11 ;",
        ] {
            if model.has(&a[..2]) {
                answers.insert(command::answer_key(a).unwrap(), a.to_string());
            }
        }
        // The model's own starting values, where the FTDX3000's don't fit.
        for a in model.sim {
            if let Some(k) = command::answer_key(a) {
                answers.insert(k, a.to_string());
            }
        }
        answers.insert("ID".into(), format!("ID{};", model.id));
        for item in model.menu {
            answers.insert(format!("EX{:03}", item.num), format!("EX{:03}{};", item.num, default_menu_raw(model, item)));
        }
        let mut memories = BTreeMap::new();
        memories.insert(
            1,
            ChannelInfo { channel: 1, freq: 7_074_000, mode: Some(Mode::DataUsb), mem_mode: 1, ..Default::default() },
        );
        memories.insert(
            2,
            ChannelInfo { channel: 2, freq: 14_074_000, mode: Some(Mode::DataUsb), mem_mode: 1, ..Default::default() },
        );
        // A radio without 20 m (VHF/UHF only) starts on its first band.
        let (freq_a, freq_b) = match model.bands.iter().find(|b| b.label != "GEN") {
            Some(b) if !model.in_range(14_074_000) => (b.lo + 100_000, b.lo + 102_000),
            _ => (14_074_000, 14_076_000),
        };
        Self {
            model,
            answers,
            freq_a,
            freq_b,
            mode_a: Mode::Usb,
            mode_b: Mode::Usb,
            sideband_shift: 0,
            power_on: true,
            ai: false,
            tx: 0,
            mox: false,
            tx_vfo: 0,
            rx_function: 0,
            clar_offset: 0,
            rx_clar: false,
            tx_clar: false,
            lock: 4,
            fast: 4,
            antenna: 1,
            roofing: '6',
            tuner: 0,
            memory_channel: 1,
            memories,
            rng: 0x1234_5678,
            s_meter: 60,
            outbox: Vec::new(),
        }
    }

    fn rand(&mut self) -> u32 {
        // xorshift32
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    fn info(&self, b: bool) -> ChannelInfo {
        ChannelInfo {
            channel: self.memory_channel,
            freq: if b { self.freq_b } else { self.freq_a },
            clar_offset: self.clar_offset,
            rx_clar: self.rx_clar,
            tx_clar: self.tx_clar,
            mode: Some(if b { self.mode_b } else { self.mode_a }),
            ..Default::default()
        }
    }

    fn transmitting(&self) -> bool {
        self.tx != 0 || self.mox
    }

    fn get(&self, key: &str) -> Option<String> {
        self.answers.get(key).cloned()
    }

    fn power(&self) -> u32 {
        self.get("PC").and_then(|a| a[2..5].parse().ok()).unwrap_or(100)
    }

    /// Answer a Read command.
    fn read(&mut self, key: &str) -> Option<String> {
        Some(match key {
            k if !self.model.has(&k[..2]) => return None,
            "AC" => format!("AC00{};", self.tuner),
            "AI" => format!("AI{};", self.ai as u8),
            "AN0" => format!("AN0{}0;", self.antenna),
            "FA" => format!("FA{:08};", self.freq_a),
            "FB" => format!("FB{:08};", self.freq_b),
            "FR" => format!("FR{};", self.rx_function),
            "FS" => format!("FS{};", self.fast),
            "FT" => format!("FT{};", self.tx_vfo),
            "IF" => self.info(false).to_answer("IF"),
            "OI" => self.info(true).to_answer("OI"),
            "LK" => format!("LK{};", self.lock),
            "MC" => format!("MC{:03};", self.memory_channel),
            "MD0" => format!("MD0{};", self.mode_a.code()),
            "MX" => format!("MX{};", self.mox as u8),
            "PS" => "PS1;".into(),
            "RF0" => format!("RF0{};", self.roofing),
            "RT" => format!("RT{};", self.rx_clar as u8),
            "XT" => format!("XT{};", self.tx_clar as u8),
            "TX" => format!("TX{};", if self.mox { 2 } else { self.tx }),
            "SM0" => {
                let step = (self.rand() % 21) as i32 - 10;
                self.s_meter = (self.s_meter as i32 + step).clamp(20, 200) as u8;
                format!("SM0{:03};", if self.transmitting() { 0 } else { self.s_meter })
            }
            k if k.starts_with("RM") => {
                let tx = self.transmitting();
                let jitter = (self.rand() % 6) as u32;
                let v: u32 = match &k[2..] {
                    "1" => self.s_meter as u32,
                    "3" if tx => 40 + jitter,
                    "4" if tx => 30 + jitter,
                    "5" if tx => (self.power() * 161 / 100).min(255) + jitter,
                    "6" if tx => 20 + jitter / 2,
                    "7" if tx => 110 + jitter,
                    "8" => 190 + jitter / 3,
                    "0" | "2" | "3" | "4" | "5" | "6" | "7" => 0,
                    _ => return None,
                };
                format!("{k}{:03};", v.min(255))
            }
            k if k.starts_with("MR") => {
                let ch: u16 = k[2..].parse().ok()?;
                // Like the radio: P2 is the *current* channel, not `ch`.
                let info = ChannelInfo { channel: self.memory_channel, ..self.memories.get(&ch)?.clone() };
                info.to_answer("MR")
            }
            k => self.get(k)?,
        })
    }

    /// The model's amateur bands (not GEN).
    fn bands(&self) -> Vec<&'static ftdx3000_cat::types::Band> {
        self.model.bands.iter().filter(|b| b.label != "GEN").collect()
    }

    fn set_band(&mut self, idx: usize) {
        let bands = self.bands();
        let b = bands[idx.min(bands.len() - 1)];
        self.freq_a = b.lo + match b.label {
            "160" => 30_000,
            "80" => 73_000,
            _ if b.hi - b.lo < 200_000 => (b.hi - b.lo) / 2,
            _ => 74_000,
        };
    }

    fn band_index(&self) -> usize {
        self.bands().iter().position(|b| (b.lo..=b.hi).contains(&self.freq_a)).unwrap_or(0)
    }

    /// Apply a Set command. Returns false if the radio would answer `?;`.
    fn set(&mut self, code: &str, p: &str) -> bool {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        match code {
            _ if !self.model.has(code) => return false,
            "FA" | "FB" => {
                // 8 digits; 9 or 10 on radios that go that high.
                if !(p.len() == 8 || p.len() == 9 && self.model.freq_max > 99_999_999 || p.len() == 10 && self.model.freq_max > 999_999_999) || !digits(p) {
                    return false;
                }
                let f: u64 = p.parse().unwrap();
                if !self.model.in_range(f) {
                    return false;
                }
                if self.transmitting() {
                    return true; // silently ignored, like the radio
                }
                if code == "FA" { self.freq_a = f } else { self.freq_b = f }
            }
            "MD" => {
                let Some(m) = p.get(1..2).and_then(|c| Mode::from_code(c.chars().next()?)) else { return false };
                if p.len() != 2 || !self.model.modes.contains(&m) {
                    return false;
                }
                match (self.mode_a, m) {
                    (Mode::Usb, Mode::Lsb) => self.freq_a = (self.freq_a as i64 + self.sideband_shift) as u64,
                    (Mode::Lsb, Mode::Usb) => self.freq_a = (self.freq_a as i64 - self.sideband_shift) as u64,
                    _ => {}
                }
                self.mode_a = m;
            }
            "AB" => {
                self.freq_b = self.freq_a;
                self.mode_b = self.mode_a;
            }
            "BA" => {
                self.freq_a = self.freq_b;
                self.mode_a = self.mode_b;
            }
            "SV" => {
                std::mem::swap(&mut self.freq_a, &mut self.freq_b);
                std::mem::swap(&mut self.mode_a, &mut self.mode_b);
            }
            "BS" => {
                let Ok(code) = p.parse::<u8>() else { return false };
                let Some(b) = self.model.bands.iter().find(|b| b.code == code) else { return false };
                if let Some(i) = self.bands().iter().position(|x| x.code == b.code) {
                    self.set_band(i);
                }
            }
            "BU" => {
                let (i, n) = (self.band_index(), self.bands().len());
                self.set_band((i + 1) % n);
            }
            "BD" => {
                let (i, n) = (self.band_index(), self.bands().len());
                self.set_band((i + n - 1) % n);
            }
            "UP" | "DN" => {
                let step = 100;
                self.freq_a = self.model.clamp_freq(if code == "UP" { self.freq_a + step } else { self.freq_a.saturating_sub(step) });
            }
            "TX" => match p {
                "0" | "1" => {
                    self.tx = p.parse().unwrap();
                    self.mox = false;
                }
                _ => return false,
            },
            "MX" => match p {
                "0" | "1" => self.mox = p == "1",
                _ => return false,
            },
            "FT" => match p {
                "0" | "2" => self.tx_vfo = 0,
                "1" | "3" => self.tx_vfo = 1,
                _ => return false,
            },
            "FR" => match p {
                "0" | "1" | "4" | "5" => self.rx_function = p.parse().unwrap(),
                _ => return false,
            },
            "AN" => match p.get(1..2).and_then(|a| a.parse::<u8>().ok()) {
                Some(a) if p.len() == 2 && (1..=self.model.caps.antennas).contains(&a) => self.antenna = a,
                _ => return false,
            },
            "AC" => match p {
                "000" => self.tuner = 0,
                "001" | "002" => self.tuner = 1,
                _ => return false,
            },
            "RF" if !self.model.is_ftdx3000() => {
                let Some(c) = p.get(1..2).and_then(|c| c.chars().next()) else { return false };
                if p.len() != 2 || !self.model.caps.roofing.iter().any(|(x, _)| *x == c) {
                    return false;
                }
                self.roofing = c;
            }
            "RF" => {
                self.roofing = match p {
                    "00" => '6',
                    "01" => '1',
                    "02" => '2',
                    "03" => '3',
                    "04" => '7',
                    "05" => '8',
                    _ => return false,
                }
            }
            "GT" => {
                let Ok(v) = p.parse::<u8>() else { return false };
                if v > 4 {
                    return false;
                }
                let ans = if v == 4 { 5 } else { v }; // AUTO answers AUTO-MID here
                self.answers.insert("GT0".into(), format!("GT0{ans};"));
            }
            "LK" | "FS" => {
                let Ok(v) = p.parse::<u8>() else { return false };
                let cur = if code == "LK" { &mut self.lock } else { &mut self.fast };
                let (mut a, mut b) = (*cur & 1, (*cur >> 1) & 1);
                match v {
                    0 | 1 => a = v,
                    2 | 3 => b = v - 2,
                    4..=7 => {
                        a = v & 1;
                        b = (v >> 1) & 1;
                    }
                    _ => return false,
                }
                *cur = 4 | a | (b << 1);
            }
            "RC" => self.clar_offset = 0,
            "RU" | "RD" => {
                let Ok(v) = p.parse::<i32>() else { return false };
                if p.len() != 4 || v < 0 {
                    return false;
                }
                self.clar_offset = (self.clar_offset + if code == "RU" { v } else { -v }).clamp(-9999, 9999);
            }
            "RT" => self.rx_clar = p == "1",
            "XT" => self.tx_clar = p == "1",
            "MC" => {
                let Ok(ch) = p.parse::<u16>() else { return false };
                if !self.model.caps.memory.channels().contains(&ch) {
                    return false;
                }
                self.memory_channel = ch;
                if let Some(m) = self.memories.get(&ch) {
                    self.freq_a = m.freq;
                    self.mode_a = m.mode.unwrap_or(self.mode_a);
                }
            }
            "MA" => {
                if let Some(m) = self.memories.get(&self.memory_channel) {
                    self.freq_a = m.freq;
                    self.mode_a = m.mode.unwrap_or(self.mode_a);
                }
            }
            "AM" | "VM" => {
                let mut info = self.info(false);
                info.channel = self.memory_channel;
                info.mem_mode = 1;
                self.memories.insert(self.memory_channel, info);
            }
            "MW" => {
                if p.len() < 24 {
                    return false;
                }
                let Some(mut info) = ChannelInfo::parse(p) else { return false };
                info.mem_mode = 1;
                self.memories.insert(info.channel, info);
            }
            "AI" => match p {
                "0" | "1" => self.ai = p == "1",
                _ => return false,
            },
            "PS" => self.power_on = p == "1",
            "EX" => {
                let Some(num) = p.get(..3).and_then(|n| n.parse::<u16>().ok()) else { return false };
                let Some(item) = self.model.menu_item(num) else { return false };
                let raw = &p[3..];
                if item.decode(raw).is_none() {
                    return false;
                }
                self.answers.insert(format!("EX{num:03}"), format!("EX{p};"));
            }
            "PC" => {
                let Ok(v) = p.parse::<u32>() else { return false };
                let (lo, hi) = self.model.caps.power;
                if p.len() != 3 || !(lo as u32..=hi as u32).contains(&v) {
                    return false;
                }
                self.answers.insert("PC".into(), format!("PC{p};"));
            }
            "AG" | "RG" | "SQ" => {
                if p.len() != 4 || !digits(p) || p[1..].parse::<u32>().unwrap() > 255 {
                    return false;
                }
                self.answers.insert(format!("{code}0"), format!("{code}{p};"));
            }
            "KM" => {
                let Some(ch) = p.get(..1).and_then(|c| c.parse::<u8>().ok()).filter(|c| (1..=self.model.caps.keyer_memories).contains(c)) else { return false };
                let text = p[1..].trim_end_matches('}');
                if text.len() > self.model.caps.keyer_memory_len as usize {
                    return false;
                }
                self.answers.insert(format!("KM{ch}"), format!("KM{ch}{text}}};"));
            }
            "KY" | "EN" | "QS" | "QI" | "QR" | "CH" | "ED" | "EU" | "EK" | "SC" | "PB" | "LM" | "RO" => {}
            _ => {
                let Some(info) = command::lookup(code) else { return false };
                if !info.set {
                    return false;
                }
                if info.set_is_answer && info.read {
                    if p.len() <= info.read_params {
                        return false;
                    }
                    let key = format!("{}{}", info.code, &p[..info.read_params]);
                    // Only accept values with the same shape as the stored answer.
                    if let Some(existing) = self.answers.get(&key) {
                        if existing.len() != p.len() + 3 && !matches!(code, "KM" | "EM") {
                            return false;
                        }
                    }
                    self.answers.insert(key, format!("{code}{p};"));
                }
            }
        }
        true
    }

    /// Process one frame from the computer; returns the frames to send back.
    pub fn handle(&mut self, frame_text: &str) -> Vec<String> {
        let frame_text = command::normalize(frame_text);
        if !self.power_on {
            // Only a power-on command does anything while off.
            if frame_text == "PS1;" {
                self.power_on = true;
            }
            return Vec::new();
        }
        let Some((code, params)) = frame::split(&frame_text) else { return vec!["?;".into()] };
        match command::classify(&frame_text) {
            Request::Read { key } => vec![self.read(&key).unwrap_or_else(|| "?;".into())],
            Request::Set => {
                if self.set(&code, params) {
                    Vec::new()
                } else {
                    vec!["?;".into()]
                }
            }
            Request::Unknown => vec!["?;".into()],
        }
    }

    /// Simulate a front-panel change: apply a Set and report it via AI.
    pub fn front_panel(&mut self, frame_text: &str) {
        let frame_text = command::normalize(frame_text);
        if let Some((code, params)) = frame::split(&frame_text) {
            if self.set(&code, params) && self.ai {
                if let Request::Read { key } = command::classify(&format!("{};", command::answer_key(&frame_text).unwrap_or_default())) {
                    if let Some(a) = self.read(&key) {
                        self.outbox.push(a);
                    }
                }
            }
        }
    }
}

/// A simulator running on its own thread, reachable through a [`Link`].
#[derive(Clone)]
pub struct SimRadio {
    core: Arc<Mutex<SimCore>>,
}

impl Default for SimRadio {
    fn default() -> Self {
        Self::new()
    }
}

/// One simulated radio's end of the line: RigHelm's language for the
/// FTDX3000, the radio's own (through its emulator) for the others.
pub struct SimLine {
    core: Arc<Mutex<SimCore>>,
    framer: Framer,
    emulator: Option<Box<dyn Emulator>>,
    /// The program turned the radio's auto-information on.
    ai: bool,
}

impl SimLine {
    pub fn new(core: Arc<Mutex<SimCore>>) -> Self {
        let model = core.lock().model;
        Self { core, framer: Framer::new(), emulator: ftdx3000_cat::dialect::emulator_for(model), ai: false }
    }

    /// Bytes from the program in, bytes for it out.
    pub fn handle(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut c = self.core.lock();
        let Some(em) = self.emulator.as_mut() else {
            let mut out = String::new();
            for f in self.framer.push(bytes) {
                for a in c.handle(&f) {
                    out.push_str(&a);
                }
            }
            for a in c.outbox.drain(..) {
                out.push_str(&a);
            }
            return out.into_bytes();
        };
        let mut out = Vec::new();
        for f in em.split(bytes) {
            let req = em.request(&f);
            if let Some(on) = req.auto_info {
                self.ai = on;
                c.handle(if on { "AI1;" } else { "AI0;" });
            }
            let mut rejected = false;
            for set in &req.sets {
                if c.handle(set).iter().any(|a| a == "?;") {
                    rejected = true;
                }
            }
            if let Some(now) = &req.immediate {
                out.extend_from_slice(now);
            } else if rejected {
                out.extend_from_slice(&em.respond(&req, &[None]));
            } else if !req.reads.is_empty() {
                let answers: Vec<Option<String>> = req.reads.iter().map(|r| c.handle(r).into_iter().next().filter(|a| a != "?;")).collect();
                out.extend_from_slice(&em.respond(&req, &answers));
            } else {
                out.extend_from_slice(&em.respond(&req, &[]));
            }
            for set in em.after() {
                c.handle(&set);
            }
        }
        let pushed: Vec<String> = c.outbox.drain(..).collect();
        if self.ai {
            for a in pushed {
                if let Some(b) = em.unsolicited(&a) {
                    out.extend_from_slice(&b);
                }
            }
        }
        out
    }

    /// Auto-information the core queued by itself (front-panel changes).
    pub fn pending(&mut self) -> Vec<u8> {
        self.handle(&[])
    }
}

impl SimRadio {
    pub fn new() -> Self {
        Self::for_model(&FTDX3000)
    }

    /// A simulated `model`, speaking that radio's own protocol.
    pub fn for_model(model: &'static RadioModel) -> Self {
        Self { core: Arc::new(Mutex::new(SimCore::for_model(model))) }
    }

    pub fn core(&self) -> parking_lot::MutexGuard<'_, SimCore> {
        self.core.lock()
    }

    pub fn auto_info(&self) -> bool {
        self.core.lock().ai
    }

    pub fn front_panel(&self, frame: &str) {
        self.core.lock().front_panel(frame);
    }

    /// Serve the simulator over any byte stream (PTY, pipe).
    pub fn serve(&self, mut input: impl Read + Send + 'static, mut output: impl Write + Send + 'static) {
        let mut line = SimLine::new(self.core.clone());
        thread::Builder::new()
            .name("sim".into())
            .spawn(move || {
                let mut buf = [0u8; 256];
                loop {
                    let n = match input.read(&mut buf) {
                        Ok(0) => {
                            thread::sleep(Duration::from_millis(5));
                            0
                        }
                        Ok(n) => n,
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => 0,
                        Err(_) => return,
                    };
                    let out = line.handle(&buf[..n]);
                    // A write fails while nobody has the PTY open; the radio
                    // doesn't care either, so drop the output and carry on.
                    if !out.is_empty() {
                        let _ = output.write_all(&out).and_then(|_| output.flush());
                    }
                }
            })
            .expect("spawn sim");
    }

    /// An in-process link to this simulator.
    pub fn link(&self) -> Link {
        let (to_sim, sim_in) = pipe::pipe();
        let (sim_out, from_sim) = pipe::pipe();
        self.serve(sim_in, sim_out);
        let name = self.core.lock().model.name;
        Link { reader: Box::new(from_sim), writer: Box::new(to_sim), description: format!("built-in {name} simulator") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_like_the_radio() {
        let mut s = SimCore::new();
        assert_eq!(s.handle("ID;"), vec!["ID0462;"]);
        assert_eq!(s.handle("FA;"), vec!["FA14074000;"]);
        assert!(s.handle("FA07074000;").is_empty());
        assert_eq!(s.handle("FA;"), vec!["FA07074000;"]);
        assert_eq!(s.handle("FA7074000;"), vec!["?;"]);
        assert_eq!(s.handle("ZZ;"), vec!["?;"]);
        assert!(s.handle("MD03;").is_empty());
        assert_eq!(s.handle("IF;"), vec!["IF00107074000+000000300000;"]);
        assert_eq!(s.handle("EX038;"), vec!["EX0381;"]);
        assert!(s.handle("EX0383;").is_empty());
        assert_eq!(s.handle("EX038;"), vec!["EX0383;"]);
        assert_eq!(s.handle("EX0389;"), vec!["?;"]);
        assert!(s.handle("PS0;").is_empty());
        assert!(s.handle("FA;").is_empty());
        assert!(s.handle("PS1;").is_empty());
        assert_eq!(s.handle("PS;"), vec!["PS1;"]);
    }

    #[test]
    fn split_and_lock() {
        let mut s = SimCore::new();
        s.handle("FT3;");
        assert_eq!(s.handle("FT;"), vec!["FT1;"]);
        s.handle("LK1;");
        assert_eq!(s.handle("LK;"), vec!["LK5;"]);
        s.handle("RF04;");
        assert_eq!(s.handle("RF0;"), vec!["RF07;"]);
    }
}
