//! A Hamlib `rigctld`-compatible TCP server ("Hamlib NET rigctl", model 2).
//!
//! Lets any Hamlib-based program (WSJT-X, JTDX, fldigi, GridTracker, many
//! loggers) share the radio over `localhost:4532` without a serial port at all.
//! Implements the commands those programs use; anything else answers
//! `RPRT -4` (not implemented) like a real rigctld would for an unsupported
//! feature.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use ftdx3000_cat::types::{self, CTCSS_DHZ, Meter, WidthClass, interpolate};
use ftdx3000_cat::{Mode, RadioModel, RadioState};
use serde::{Deserialize, Serialize};

use crate::engine::{CatError, Freshness, Rig, Source};
use crate::vport::ClientCtx;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RigctldConfig {
    pub enabled: bool,
    pub bind: String,
    pub block_tx: bool,
}

impl Default for RigctldConfig {
    fn default() -> Self {
        Self { enabled: true, bind: "127.0.0.1:4532".into(), block_tx: false }
    }
}

/// Hamlib error codes (negated in `RPRT` lines).
const EINVAL: i32 = 1;
const ENIMPL: i32 = 4;
const ETIMEOUT: i32 = 5;
const EPROTO: i32 = 8;
const ERJCTED: i32 = 9;

// Hamlib bit values (rig.h).
const MODE_AM: u64 = 1 << 0;
const MODE_CW: u64 = 1 << 1;
const MODE_USB: u64 = 1 << 2;
const MODE_LSB: u64 = 1 << 3;
const MODE_RTTY: u64 = 1 << 4;
const MODE_FM: u64 = 1 << 5;
const MODE_CWR: u64 = 1 << 7;
const MODE_RTTYR: u64 = 1 << 8;
const MODE_PKTLSB: u64 = 1 << 10;
const MODE_PKTUSB: u64 = 1 << 11;
const MODE_PKTFM: u64 = 1 << 12;
const MODE_FMN: u64 = 1 << 21;
const MODE_WFM: u64 = 1 << 6;
const SSB_MODES: u64 = MODE_USB | MODE_LSB | MODE_PKTLSB | MODE_PKTUSB;
const CW_MODES: u64 = MODE_CW | MODE_CWR | MODE_RTTY | MODE_RTTYR;
const VFO_AB: u32 = 0b11;

/// Levels we support: (name, hamlib bit, settable).
const LEVELS: &[(&str, u32, bool)] = &[
    ("PREAMP", 0, true),
    ("ATT", 1, true),
    ("VOXDELAY", 2, true),
    ("AF", 3, true),
    ("RF", 4, true),
    ("SQL", 5, true),
    ("IF", 6, true),
    ("NR", 8, true),
    ("CWPITCH", 11, true),
    ("RFPOWER", 12, true),
    ("MICGAIN", 13, true),
    ("KEYSPD", 14, true),
    ("NOTCHF", 15, true),
    ("COMP", 16, true),
    ("AGC", 17, true),
    ("VOXGAIN", 21, true),
    ("RAWSTR", 26, false),
    ("SWR", 28, false),
    ("ALC", 29, false),
    ("STRENGTH", 30, false),
    ("RFPOWER_METER", 32, false),
    ("COMP_METER", 33, false),
    ("VD_METER", 34, false),
    ("ID_METER", 35, false),
    ("MONITOR_GAIN", 37, true),
    ("NB", 38, true),
    ("RFPOWER_METER_WATTS", 39, false),
];

/// Functions we support: (name, hamlib bit).
const FUNCS: &[(&str, u32)] = &[
    ("NB", 1),
    ("COMP", 2),
    ("VOX", 3),
    ("FBKIN", 7),
    ("ANF", 8),
    ("NR", 9),
    ("APF", 11),
    ("MON", 12),
    ("MN", 13),
    ("LOCK", 16),
    ("RIT", 24),
    ("TUNER", 30),
    ("XIT", 31),
];

pub struct RigctldServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    clients: Arc<AtomicUsize>,
}

impl RigctldServer {
    pub fn start(rig: Rig, config: RigctldConfig) -> std::io::Result<RigctldServer> {
        let listener = TcpListener::bind(&config.bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let clients = Arc::new(AtomicUsize::new(0));
        rig.note(format!("rigctld server listening on {addr}"));
        let (stop2, clients2) = (stop.clone(), clients.clone());
        thread::Builder::new().name("rigctld".into()).spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        let _ = stream.set_nonblocking(false);
                        let (rig, stop, clients, cfg) = (rig.clone(), stop2.clone(), clients2.clone(), config.clone());
                        clients.fetch_add(1, Ordering::Relaxed);
                        let _ = thread::Builder::new().name(format!("rigctld-{peer}")).spawn(move || {
                            rig.note(format!("rigctld client {peer} connected"));
                            let _ = serve_client(&rig, &cfg, stream, peer, &stop);
                            rig.note(format!("rigctld client {peer} disconnected"));
                            clients.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        })?;
        Ok(RigctldServer { addr, stop, clients })
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }
}

impl Drop for RigctldServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Notice a peer that vanished without closing (Wi-Fi drop, closed laptop)
/// within about 10 s, so a keyed radio is released.
fn fast_keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(5)).with_interval(Duration::from_secs(1));
    #[cfg(unix)]
    let ka = ka.with_retries(4);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&ka) {
        log::debug!("TCP keepalive: {e}");
    }
}

fn serve_client(rig: &Rig, cfg: &RigctldConfig, stream: TcpStream, peer: SocketAddr, stop: &AtomicBool) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    fast_keepalive(&stream);
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    writer.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut session = Session {
        rig: rig.clone(),
        cfg: cfg.clone(),
        source: Source::Client(format!("rigctld {peer}")),
        chk_vfo: false,
        client: ClientCtx::default(),
    };
    let result = session_loop(&mut session, &mut reader, &mut writer, stop);
    // However the client left, don't leave the radio keyed on its behalf.
    session.client.disconnected(rig, &session.source);
    result
}

fn session_loop(session: &mut Session, reader: &mut BufReader<TcpStream>, writer: &mut TcpStream, stop: &AtomicBool) -> std::io::Result<()> {
    let mut line = String::new();
    while !stop.load(Ordering::Relaxed) {
        // On a timeout read_line keeps what it got so far in `line`; only
        // act on (and clear) complete lines.
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        }
        if !line.ends_with('\n') {
            if line.len() > 4096 {
                return Ok(()); // not a rigctl client
            }
            continue;
        }
        let text = line.trim().to_string();
        line.clear();
        if text.is_empty() {
            continue;
        }
        match session.execute(&text) {
            Some(reply) => writer.write_all(reply.as_bytes())?,
            None => {
                // Hamlib's netrigctl_close waits for a reply to "q".
                let _ = writer.write_all(b"RPRT 0\n");
                return Ok(());
            }
        }
    }
    Ok(())
}

struct Session {
    rig: Rig,
    cfg: RigctldConfig,
    source: Source,
    chk_vfo: bool,
    client: ClientCtx,
}

/// Result of one command: output values with their labels, or an error code.
enum Out {
    Ok,
    Values(Vec<(&'static str, String)>),
    /// Pre-formatted block (dump_state).
    Raw(String),
    Err(i32),
    Quit,
}

fn is_vfo_name(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "VFOA" | "VFOB" | "CURRVFO" | "MAIN" | "SUB" | "TX" | "RX" | "VFO" | "MEM" | "MAINA" | "SUBA" | "NONE"
    )
}

impl Session {
    /// Execute one line; `None` means close the connection.
    fn execute(&mut self, line: &str) -> Option<String> {
        // Extended response: leading '+', ';', '|' or ','.
        let (ext, sep, body) = match line.chars().next() {
            Some('+') => (true, '\n', &line[1..]),
            Some(c @ (';' | '|' | ',')) => (true, c, &line[1..]),
            _ => (false, '\n', line),
        };
        let body = body.trim();
        let (name, rest) = match body.strip_prefix('\\') {
            Some(long) => {
                let mut it = long.splitn(2, char::is_whitespace);
                (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").trim().to_string())
            }
            None => {
                let mut chars = body.chars();
                let Some(c) = chars.next() else { return Some(format!("RPRT {EINVAL}\n")) };
                (short_name(c).unwrap_or("?").to_string(), chars.as_str().trim().to_string())
            }
        };
        let mut args: Vec<&str> = rest.split_whitespace().collect();
        // We report vfo mode 0, but tolerate clients that send a VFO anyway.
        let keeps_vfo = matches!(name.as_str(), "set_vfo" | "get_vfo_info" | "send_cmd" | "w")
            // "S 1 VFOB" has a VFO as its own argument; "S VFOA 1 VFOB" has an extra one.
            || (name == "set_split_vfo" && args.len() < 3);
        if !keeps_vfo && args.first().is_some_and(|a| is_vfo_name(a)) {
            args.remove(0);
        }
        let out = self.dispatch(&name, &args, &rest);
        Some(match out {
            Out::Quit => return None,
            Out::Raw(s) => s,
            Out::Ok => {
                if ext {
                    format!("{name}: {rest}{sep}RPRT 0\n")
                } else {
                    "RPRT 0\n".into()
                }
            }
            Out::Err(code) => {
                if ext {
                    format!("{name}: {rest}{sep}RPRT -{code}\n")
                } else {
                    format!("RPRT -{code}\n")
                }
            }
            Out::Values(vals) => {
                let mut s = String::new();
                if ext {
                    s.push_str(&format!("{name}:{sep}"));
                    for (label, v) in &vals {
                        s.push_str(&format!("{label}: {v}{sep}"));
                    }
                    s.push_str("RPRT 0\n");
                } else {
                    for (_, v) in &vals {
                        s.push_str(v);
                        s.push('\n');
                    }
                    // rigctld ends answers to commands not flagged ARG_OUT
                    // with "RPRT 0", and Hamlib 4.6+ waits for that line (5 s
                    // without it, before every mode change).
                    if name == "get_lock_mode" {
                        s.push_str("RPRT 0\n");
                    }
                }
                s
            }
        })
    }

    fn state(&self) -> RadioState {
        self.rig.state().clone()
    }

    fn connected(&self) -> bool {
        self.rig.link().is_open()
    }

    /// Fresh read through the engine cache.
    fn read(&self, cmd: &str) -> Result<String, i32> {
        self.rig.read(cmd, Freshness::Auto, self.source.clone(), Duration::from_millis(2000)).map_err(|e| match e {
            CatError::Rejected => ERJCTED,
            CatError::Timeout | CatError::NotConnected => ETIMEOUT,
            CatError::Invalid => EINVAL,
        })
    }

    /// Every command to the radio goes through here, so "block TX" covers
    /// PTT, tuner start, keyer/voice playback, VOX and raw `w` commands alike.
    fn send(&self, cmd: &str) -> Out {
        if !self.connected() {
            return Out::Err(ETIMEOUT);
        }
        let mut f = ftdx3000_cat::Framer::new();
        let frames = f.push(cmd.as_bytes());
        for frame in &frames {
            if ftdx3000_cat::command::is_tx_command(frame) && self.cfg.block_tx {
                self.rig.note(format!("{}: blocked {frame} (transmit not allowed)", self.source));
                return Out::Err(ERJCTED);
            }
            self.client.observe_set(&ftdx3000_cat::command::normalize(frame));
        }
        for frame in frames {
            self.rig.send_from(&frame, self.source.clone());
        }
        Out::Ok
    }

    fn refresh(&self, cmds: &[&str]) -> Result<RadioState, i32> {
        if !self.connected() {
            return Err(ETIMEOUT);
        }
        for c in cmds {
            self.read(c)?;
        }
        Ok(self.state())
    }

    fn dispatch(&mut self, name: &str, args: &[&str], rest: &str) -> Out {
        let num = |i: usize| -> Option<f64> { args.get(i).and_then(|a| a.parse::<f64>().ok()) };
        match name {
            "get_freq" => match self.refresh(&["FA;", "FB;", "FR;"]) {
                Ok(s) => Out::Values(vec![("Frequency", s.rx_freq().to_string())]),
                Err(e) => Out::Err(e),
            },
            "set_freq" => {
                let Some(f) = num(0) else { return Out::Err(EINVAL) };
                let s = self.state();
                let f = f.round() as u64;
                if !s.model.in_range(f) {
                    return Out::Err(EINVAL);
                }
                self.send(&ftdx3000_cat::set_freq_cmd_for(s.model, s.rx_on_b(), f))
            }
            "get_mode" => match self.refresh(&["MD0;", "NA0;", "SH0;"]) {
                Ok(s) => {
                    let m = s.mode_a.unwrap_or(Mode::Usb);
                    Out::Values(vec![("Mode", m.hamlib().to_string()), ("Passband", passband(&s, m).to_string())])
                }
                Err(e) => Out::Err(e),
            },
            "set_mode" => {
                let Some(mode) = args.first().and_then(|m| Mode::from_hamlib(m)) else { return Out::Err(EINVAL) };
                let width = num(1).map(|w| w as i64).unwrap_or(-1);
                let s = self.state();
                if s.mode_a != Some(mode) {
                    // The radio may move VFO-A by itself when the mode changes
                    // (an FTDX3000 moved it 1240 Hz going USB <-> LSB). Clients
                    // usually set the frequency first, so put it back after.
                    // The read includes a frequency the client just set.
                    let keep = self.read("FA;").ok().and_then(|a| a.get(2..a.len() - 1)?.parse::<u64>().ok());
                    let r = self.send(&ftdx3000_cat::set_mode_cmd(mode));
                    if !matches!(r, Out::Ok) {
                        return r;
                    }
                    if let Some(hz) = keep.filter(|hz| s.model.in_range(*hz)) {
                        self.send(&ftdx3000_cat::set_freq_cmd_for(s.model, false, hz));
                    }
                }
                // Mode change may change the width table; use the target mode's.
                for c in width_cmds_for(s.model, s.width_class(mode), width) {
                    self.send(&c);
                }
                Out::Ok
            }
            "get_vfo" => match self.refresh(&["FR;"]) {
                Ok(s) => Out::Values(vec![("VFO", if s.rx_on_b() { "VFOB" } else { "VFOA" }.into())]),
                Err(e) => Out::Err(e),
            },
            "set_vfo" => match args.first().map(|v| v.to_ascii_uppercase()).as_deref() {
                Some("VFOA" | "MAIN" | "CURRVFO" | "RX") => self.send("VS0;"),
                Some("VFOB" | "SUB") => self.send("VS1;"),
                Some("TX") => Out::Ok,
                _ => Out::Err(EINVAL),
            },
            "get_ptt" => match self.refresh(&["TX;"]) {
                Ok(s) => Out::Values(vec![("PTT", (s.transmitting() as u8).to_string())]),
                Err(e) => Out::Err(e),
            },
            "set_ptt" => match args.first() {
                Some(&"0") => self.send("TX0;"),
                Some(&("1" | "2" | "3")) => self.send("TX1;"),
                _ => Out::Err(EINVAL),
            },
            "get_split_vfo" => match self.refresh(&["FT;", "FR;"]) {
                Ok(s) => Out::Values(vec![
                    ("Split", (s.split() as u8).to_string()),
                    ("TX VFO", if s.tx_vfo == 1 { "VFOB" } else { "VFOA" }.into()),
                ]),
                Err(e) => Out::Err(e),
            },
            "set_split_vfo" => {
                let on = args.first().is_some_and(|a| *a == "1");
                let s = self.state();
                // Transmit on the VFO we are not receiving on for split.
                let tx_b = if on { !s.rx_on_b() } else { s.rx_on_b() };
                self.send(if tx_b { "FT3;" } else { "FT2;" })
            }
            "get_split_freq" => match self.refresh(&["FA;", "FB;", "FR;"]) {
                Ok(s) => Out::Values(vec![("TX Frequency", if s.rx_on_b() { s.freq_a } else { s.freq_b }.to_string())]),
                Err(e) => Out::Err(e),
            },
            "set_split_freq" => {
                let Some(f) = num(0) else { return Out::Err(EINVAL) };
                let s = self.state();
                self.send(&ftdx3000_cat::set_freq_cmd(!s.rx_on_b(), f.round() as u64))
            }
            "get_split_mode" => match self.refresh(&["OI;"]) {
                Ok(s) => {
                    let m = s.mode_b.unwrap_or(Mode::Usb);
                    Out::Values(vec![("TX Mode", m.hamlib().into()), ("TX Passband", passband(&s, m).to_string())])
                }
                Err(e) => Out::Err(e),
            },
            "set_split_mode" => {
                // The FTDX3000 has no direct "mode of VFO-B" command; programs
                // almost always ask for the RX mode here, which is already set.
                Out::Ok
            }
            "get_level" => {
                let Some(level) = args.first() else { return Out::Err(EINVAL) };
                if *level == "?" {
                    return Out::Values(vec![("Level", LEVELS.iter().map(|l| l.0).collect::<Vec<_>>().join(" "))]);
                }
                match self.get_level(&level.to_ascii_uppercase()) {
                    Ok(v) => Out::Values(vec![("Level Value", v)]),
                    Err(e) => Out::Err(e),
                }
            }
            "set_level" => {
                let (Some(level), Some(v)) = (args.first(), num(1)) else {
                    if args.first() == Some(&"?") {
                        let names: Vec<&str> = LEVELS.iter().filter(|l| l.2).map(|l| l.0).collect();
                        return Out::Values(vec![("Level", names.join(" "))]);
                    }
                    return Out::Err(EINVAL);
                };
                match self.level_cmd(&level.to_ascii_uppercase(), v) {
                    Some(cmd) => self.send(&cmd),
                    None => Out::Err(EINVAL),
                }
            }
            "get_func" => {
                let Some(f) = args.first() else { return Out::Err(EINVAL) };
                if *f == "?" {
                    return Out::Values(vec![("Func", FUNCS.iter().map(|f| f.0).collect::<Vec<_>>().join(" "))]);
                }
                match self.get_func(&f.to_ascii_uppercase()) {
                    Ok(v) => Out::Values(vec![("Func Status", (v as u8).to_string())]),
                    Err(e) => Out::Err(e),
                }
            }
            "set_func" => {
                let (Some(f), Some(v)) = (args.first(), args.get(1)) else { return Out::Err(EINVAL) };
                match func_cmd(&f.to_ascii_uppercase(), *v == "1") {
                    Some(cmd) => self.send(&cmd),
                    None => Out::Err(EINVAL),
                }
            }
            "get_rit" | "get_xit" => match self.refresh(&["IF;"]) {
                Ok(s) => Out::Values(vec![(if name == "get_rit" { "RIT" } else { "XIT" }, s.info_a.clar_offset.to_string())]),
                Err(e) => Out::Err(e),
            },
            "set_rit" | "set_xit" => {
                let Some(v) = num(0).filter(|v| v.is_finite()) else { return Out::Err(EINVAL) };
                let v = v.clamp(-9999.0, 9999.0) as i32;
                let on = if name == "set_rit" { "RT" } else { "XT" };
                let mut cmds = vec!["RC;".to_string()];
                if v > 0 {
                    cmds.push(format!("RU{:04};", v.min(9999)));
                } else if v < 0 {
                    cmds.push(format!("RD{:04};", (-v).min(9999)));
                }
                cmds.push(format!("{on}{};", (v != 0) as u8));
                for c in cmds {
                    self.send(&c);
                }
                Out::Ok
            }
            "get_powerstat" => {
                let on = self.connected() && self.read("PS;").is_ok_and(|a| a == "PS1;");
                Out::Values(vec![("Power Status", (on as u8).to_string())])
            }
            "set_powerstat" => match args.first() {
                Some(&"0") => self.send("PS0;"),
                Some(&"1") => {
                    self.rig.send_from("PS1;", self.source.clone());
                    Out::Ok
                }
                _ => Out::Err(EINVAL),
            },
            "get_ctcss_tone" => match self.refresh(&["CN0;"]) {
                Ok(s) => Out::Values(vec![("CTCSS Tone", CTCSS_DHZ.get(s.ctcss_tone as usize).copied().unwrap_or(0).to_string())]),
                Err(e) => Out::Err(e),
            },
            "set_ctcss_tone" => {
                let Some(t) = num(0) else { return Out::Err(EINVAL) };
                match CTCSS_DHZ.iter().position(|d| *d as f64 == t) {
                    Some(i) => self.send(&format!("CN0{i:02};")),
                    None => Out::Err(EINVAL),
                }
            }
            "vfo_op" => {
                let cmd = match args.first().map(|a| a.to_ascii_uppercase()).as_deref() {
                    Some("CPY") => "AB;",
                    Some("XCHG") => "SV;",
                    Some("FROM_VFO") => "AM;",
                    Some("TO_VFO") => "MA;",
                    Some("UP") => "UP;",
                    Some("DOWN") => "DN;",
                    Some("BAND_UP") => "BU0;",
                    Some("BAND_DOWN") => "BD0;",
                    Some("TUNE") => "AC002;",
                    Some("TOGGLE") => {
                        let s = self.state();
                        if s.vfo_select == 0 { "VS1;" } else { "VS0;" }
                    }
                    _ => return Out::Err(EINVAL),
                };
                self.send(cmd)
            }
            "get_vfo_info" => match self.refresh(&["FA;", "FB;", "MD0;", "OI;", "FT;", "FR;", "SH0;", "NA0;"]) {
                Ok(s) => {
                    let b = args.first().is_some_and(|v| v.eq_ignore_ascii_case("VFOB") || v.eq_ignore_ascii_case("SUB"));
                    let (f, m) = if b { (s.freq_b, s.mode_b) } else { (s.freq_a, s.mode_a) };
                    let m = m.unwrap_or(Mode::Usb);
                    Out::Values(vec![
                        ("Freq", f.to_string()),
                        ("Mode", m.hamlib().into()),
                        ("Width", passband(&s, m).to_string()),
                        ("Split", (s.split() as u8).to_string()),
                        ("SatMode", "0".into()),
                    ])
                }
                Err(e) => Out::Err(e),
            },
            "get_rig_info" => match self.refresh(&["FA;", "FB;", "MD0;", "OI;", "FT;", "FR;", "TX;"]) {
                Ok(s) => {
                    let ma = s.mode_a.unwrap_or(Mode::Usb);
                    let mb = s.mode_b.unwrap_or(Mode::Usb);
                    let info = format!(
                        "VFO=VFOA Freq={} Mode={} Width={} RX={} TX={}\nVFO=VFOB Freq={} Mode={} Width={} RX={} TX={}\nSplit={} SatMode=0\nRig=FTDX-3000\nApp=ftdx3000\nVersion={}\nCRC=0x00000000",
                        s.freq_a, ma.hamlib(), passband(&s, ma), (!s.rx_on_b()) as u8, (s.tx_vfo == 0) as u8,
                        s.freq_b, mb.hamlib(), passband(&s, mb), s.rx_on_b() as u8, (s.tx_vfo == 1) as u8,
                        s.split() as u8, env!("CARGO_PKG_VERSION")
                    );
                    Out::Values(vec![("RigInfo", info)])
                }
                Err(e) => Out::Err(e),
            },
            "send_cmd" | "w" => {
                // Raw CAT passthrough: "w FA;" -> "FA14074000;"
                if rest.is_empty() {
                    return Out::Err(EINVAL);
                }
                let cmd = ftdx3000_cat::command::normalize(rest);
                match ftdx3000_cat::classify(&cmd) {
                    ftdx3000_cat::Request::Read { .. } => match self.read(&cmd) {
                        Ok(a) => Out::Values(vec![("Reply", a)]),
                        Err(e) => Out::Err(e),
                    },
                    ftdx3000_cat::Request::Set => self.send(&cmd),
                    ftdx3000_cat::Request::Unknown => Out::Err(EINVAL),
                }
            }
            "chk_vfo" => {
                self.chk_vfo = true;
                Out::Raw("0\n".into())
            }
            "set_vfo_opt" => Out::Ok,
            "get_lock_mode" => Out::Values(vec![("Locked", "0".into())]),
            "set_lock_mode" => Out::Ok,
            "dump_state" => Out::Raw(dump_state(self.rig.model(), self.chk_vfo)),
            "get_info" => Out::Values(vec![("Info", "Yaesu FTDX-3000 via ftdx3000 rig control".into())]),
            "send_morse" => Out::Err(ENIMPL),
            "quit" | "q" | "Q" => Out::Quit,
            _ => Out::Err(ENIMPL),
        }
    }

    fn get_level(&self, level: &str) -> Result<String, i32> {
        let f = |v: f32| format!("{v:.6}");
        let read = |cmd: &str| -> Result<RadioState, i32> { self.refresh(&[cmd]) };
        Ok(match level {
            "AF" => f(read("AG0;")?.af_gain as f32 / 255.0),
            "RF" => f(read("RG0;")?.rf_gain as f32 / 255.0),
            "SQL" => f(read("SQ0;")?.squelch as f32 / 255.0),
            "RFPOWER" => f(read("PC;")?.power as f32 / 100.0),
            "MICGAIN" => f(read("MG;")?.mic_gain as f32 / 100.0),
            "COMP" => f(read("PL;")?.proc_level as f32 / 100.0),
            "NR" => f(read("RL0;")?.nr_level as f32 / 15.0),
            "NB" => f(read("NL0;")?.nb_level as f32 / 100.0),
            "MONITOR_GAIN" => f(read("ML1;")?.monitor_level as f32 / 100.0),
            "VOXGAIN" => f(read("VG;")?.vox_gain as f32 / 100.0),
            "VOXDELAY" => (read("VD;")?.vox_delay / 100).to_string(),
            "KEYSPD" => read("KS;")?.key_speed.to_string(),
            "CWPITCH" => (300 + 10 * read("KP;")?.key_pitch as u32).to_string(),
            "PREAMP" => (read("PA0;")?.preamp as u32 * 10).to_string(),
            "ATT" => {
                let s = read("RA0;")?;
                s.model.caps.att.iter().find(|a| a.0 == s.att).map_or(0, |a| label_db(a.1)).to_string()
            }
            "AGC" => match read("GT0;")?.agc {
                0 => 0,
                1 => 2,
                2 => 5,
                3 => 3,
                _ => 6,
            }
            .to_string(),
            "IF" => read("IS0;")?.if_shift.to_string(),
            "NOTCHF" => (read("BP01;")?.notch_freq as u32 * 10).to_string(),
            "RAWSTR" => read("SM0;")?.s_meter.to_string(),
            "STRENGTH" => {
                let s = read("SM0;")?;
                format!("{:.0}", interpolate(&types::S_METER_CAL, s.s_meter))
            }
            "SWR" => f(interpolate(&types::SWR_CAL, self.meter(Meter::Swr)?)),
            "ALC" => f(self.meter(Meter::Alc)? as f32 / 255.0),
            "COMP_METER" => f(self.meter(Meter::Comp)? as f32 / 255.0 * 30.0),
            "RFPOWER_METER" => f(interpolate(&types::PO_CAL, self.meter(Meter::Po)?) / 100.0),
            "RFPOWER_METER_WATTS" => f(interpolate(&types::PO_CAL, self.meter(Meter::Po)?)),
            "ID_METER" => f(interpolate(&types::ID_CAL, self.meter(Meter::Id)?)),
            "VD_METER" => f(interpolate(&types::VDD_CAL, self.meter(Meter::Vdd)?)),
            _ => return Err(EINVAL),
        })
    }

    fn meter(&self, m: Meter) -> Result<u8, i32> {
        let s = self.refresh(&[&format!("RM{};", m.rm_code())])?;
        s.meter(m).ok_or(EPROTO)
    }

    fn level_cmd(&self, level: &str, v: f64) -> Option<String> {
        if !v.is_finite() {
            return None;
        }
        let pct = |max: f64| (v.clamp(0.0, 1.0) * max).round() as u32;
        Some(match level {
            "AF" => format!("AG0{:03};", pct(255.0)),
            "RF" => format!("RG0{:03};", pct(255.0)),
            "SQL" => format!("SQ0{:03};", pct(255.0)),
            "RFPOWER" => format!("PC{:03};", pct(100.0).clamp(5, 100)),
            "MICGAIN" => format!("MG{:03};", pct(100.0)),
            "COMP" => format!("PL{:03};", pct(100.0)),
            "NR" => format!("RL0{:02};", pct(15.0).max(1)),
            "NB" => format!("NL0{:03};", pct(100.0)),
            "MONITOR_GAIN" => format!("ML1{:03};", pct(100.0).max(1)),
            "VOXGAIN" => format!("VG{:03};", pct(100.0)),
            "VOXDELAY" => format!("VD{:04};", ((v * 100.0) as u32).clamp(30, 3000)),
            "KEYSPD" => format!("KS{:03};", (v as u32).clamp(4, 60)),
            "CWPITCH" => format!("KP{:02};", ((v as u32).clamp(300, 1050) - 300) / 10),
            "PREAMP" => {
                let top = self.rig.model().caps.preamp.iter().map(|p| p.0).max().unwrap_or(0) as u32;
                format!("PA0{};", match v as u32 { 0 => 0, 1..=15 => 1, _ => 2 }.min(top))
            }
            "ATT" => {
                // The nearest step the radio has.
                let att = self.rig.model().caps.att;
                let code = att.iter().min_by_key(|a| (label_db(a.1) as i64 - v.round() as i64).abs()).map_or(0, |a| a.0);
                format!("RA0{code};")
            }
            "AGC" => format!("GT0{};", match v as u32 { 0 => 0, 1 | 2 => 1, 5 => 2, 3 => 3, _ => 4 }),
            "IF" => {
                let hz = (v as i32).clamp(-1000, 1000);
                format!("IS0{}{:04};", if hz < 0 { '-' } else { '+' }, hz.abs())
            }
            "NOTCHF" => format!("BP01{:03};", ((v as u32) / 10).clamp(1, 400)),
            _ => return None,
        })
    }

    fn get_func(&self, func: &str) -> Result<bool, i32> {
        let read = |cmd: &str| self.refresh(&[cmd]);
        Ok(match func {
            "NB" => read("NB0;")?.nb != 0,
            "NR" => read("NR0;")?.nr,
            "ANF" => read("BC0;")?.auto_notch,
            "COMP" => read("PR0;")?.processor,
            "VOX" => read("VX;")?.vox,
            "MON" => read("ML0;")?.monitor,
            "LOCK" => read("LK;")?.lock & 1 == 1,
            "TUNER" => read("AC;")?.tuner != 0,
            "RIT" => read("RT;")?.rx_clar,
            "XIT" => read("XT;")?.tx_clar,
            "FBKIN" => read("BI;")?.break_in,
            "MN" => read("BP00;")?.manual_notch,
            "APF" => read("CO00;")?.contour_mode == 2,
            _ => return Err(EINVAL),
        })
    }
}

/// Hamlib mode bits of the modes a radio has.
fn model_modes(model: &RadioModel) -> u64 {
    model
        .modes
        .iter()
        .map(|m| match m {
            Mode::Am | Mode::AmN => MODE_AM,
            Mode::Cw => MODE_CW,
            Mode::Usb => MODE_USB,
            Mode::Lsb => MODE_LSB,
            Mode::RttyLsb => MODE_RTTY,
            Mode::Fm => MODE_FM,
            Mode::CwR => MODE_CWR,
            Mode::RttyUsb => MODE_RTTYR,
            Mode::DataLsb => MODE_PKTLSB,
            Mode::DataUsb => MODE_PKTUSB,
            Mode::DataFm | Mode::DataFmN => MODE_PKTFM,
            Mode::FmN => MODE_FMN,
            Mode::Wfm => MODE_WFM,
            _ => 0,
        })
        .fold(0, |a, b| a | b)
}

fn func_cmd(func: &str, on: bool) -> Option<String> {
    let b = on as u8;
    Some(match func {
        "NB" => format!("NB0{b};"),
        "NR" => format!("NR0{b};"),
        "ANF" => format!("BC0{b};"),
        "COMP" => format!("PR0{b};"),
        "VOX" => format!("VX{b};"),
        "MON" => format!("ML0{:03};", b),
        "LOCK" => format!("LK{b};"),
        "TUNER" => format!("AC00{b};"),
        "RIT" => format!("RT{b};"),
        "XIT" => format!("XT{b};"),
        "FBKIN" => format!("BI{b};"),
        "MN" => format!("BP00{:03};", b),
        "APF" => format!("CO00{:02};", if on { 2 } else { 0 }),
        _ => return None,
    })
}

/// Passband for a mode from the radio's width settings.
fn passband(s: &RadioState, mode: Mode) -> u32 {
    match s.width_class(mode) {
        WidthClass::Fixed => match mode {
            Mode::Am => if s.narrow { 6000 } else { 9000 },
            Mode::AmN => 6000,
            Mode::FmN => 9000,
            _ => if s.narrow { 9000 } else { 16000 },
        },
        class => s.model.width_hz(class, s.narrow, s.width_index).unwrap_or(if class == WidthClass::Ssb { 2400 } else { 500 }),
    }
}

/// The decibels in an attenuator label ("12", "20 dB", "OFF" = 0).
fn label_db(label: &str) -> u32 {
    label.split(|c: char| !c.is_ascii_digit()).find(|t| !t.is_empty()).and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// [`width_cmds`] for any radio: its own width table, and the narrow
/// switch only where it has one.
pub fn width_cmds_for(model: &RadioModel, class: WidthClass, width: i64) -> Vec<String> {
    if model.is_ftdx3000() {
        return width_cmds(class, width);
    }
    if width < 0 || class == WidthClass::Fixed || !model.has("SH") {
        return Vec::new();
    }
    let narrow_switch = model.caps.narrow && model.has("NA");
    let narrow_max = if class == WidthClass::Ssb { 1800 } else { 500 };
    let narrow = narrow_switch && width > 0 && width as u32 <= narrow_max;
    let mut choices: Vec<(u8, u32)> = (model.caps.widths)(class, narrow).into_iter().filter(|(i, w)| *w != 0 && (!narrow_switch || *i != 0)).collect();
    choices.sort_by_key(|(_, w)| *w);
    let want = if width == 0 { if class == WidthClass::Ssb { 2400 } else { 500 } } else { width };
    let Some(best) = choices.iter().find(|(_, w)| *w as i64 >= want).or(choices.last()).map(|(i, _)| *i) else { return Vec::new() };
    let mut out = Vec::new();
    if narrow_switch {
        out.push(format!("NA0{};", narrow as u8));
    }
    out.push(format!("SH0{best:02};"));
    out
}

/// Commands to select the closest width (Hamlib semantics:
/// `RIG_PASSBAND_NOCHANGE` = -1 keeps it, `RIG_PASSBAND_NORMAL` = 0 is the default).
pub fn width_cmds(class: WidthClass, width: i64) -> Vec<String> {
    if width < 0 || class == WidthClass::Fixed {
        return Vec::new();
    }
    if width == 0 {
        return vec!["NA00;".into(), "SH000;".into()];
    }
    let narrow_max = if class == WidthClass::Ssb { 1800 } else { 500 };
    let narrow = width as u32 <= narrow_max;
    // The narrowest filter at least as wide as requested, else the widest.
    let mut choices: Vec<(u8, u32)> = types::width_choices(class, narrow).into_iter().filter(|(i, _)| *i != 0).collect();
    choices.sort_by_key(|(_, w)| *w);
    let best = choices
        .iter()
        .find(|(_, w)| *w as i64 >= width)
        .or(choices.last())
        .map(|(i, _)| *i)
        .unwrap_or(0);
    vec![format!("NA0{};", narrow as u8), format!("SH0{best:02};")]
}

fn dump_state(model: &RadioModel, protocol1: bool) -> String {
    let mut s = String::new();
    let mut l = |t: &str| {
        s.push_str(t);
        s.push('\n');
    };
    let all_modes = model_modes(model);
    l("1"); // protocol version
    l(&model.hamlib.to_string()); // the radio's Hamlib model (1037: FTDX3000)
    l("0"); // ITU region (deprecated)
    l(&format!("{} {} 0x{all_modes:x} -1 -1 0x{VFO_AB:x} 0x0", model.freq_min, model.freq_max));
    l("0 0 0 0 0 0 0");
    let (lo_w, hi_w) = (model.caps.power.0 as u32 * 1000, model.caps.power.1 as u32 * 1000);
    for (lo, hi) in model.bands.iter().filter(|b| b.label != "GEN").map(|b| (b.lo, b.hi)) {
        l(&format!("{lo} {hi} 0x{:x} {lo_w} {hi_w} 0x{VFO_AB:x} 0x0", all_modes & !MODE_AM));
        l(&format!("{lo} {hi} 0x{:x} {lo_w} {} 0x{VFO_AB:x} 0x0", all_modes & MODE_AM, hi_w / 4));
    }
    l("0 0 0 0 0 0 0");
    l(&format!("0x{:x} 1", SSB_MODES | CW_MODES));
    l(&format!("0x{all_modes:x} 10"));
    l(&format!("0x{all_modes:x} 100"));
    l(&format!("0x{all_modes:x} 1000"));
    l("0 0");
    for (m, w) in [(SSB_MODES, 2400), (SSB_MODES, 1800), (SSB_MODES, 3000), (CW_MODES, 500), (CW_MODES, 2400),
        (CW_MODES, 250), (MODE_AM, 9000), (MODE_AM, 6000), (MODE_FM | MODE_PKTFM, 16000), (MODE_FM | MODE_PKTFM | MODE_FMN, 9000)]
    {
        l(&format!("0x{m:x} {w}"));
    }
    l("0 0");
    l("9999");
    l("9999");
    l("1000");
    l("0");
    let preamps: Vec<String> = model.caps.preamp.iter().filter(|p| p.0 > 0).map(|p| format!("{}", p.0 as u32 * 10)).collect();
    let atts: Vec<String> = model.caps.att.iter().filter(|a| a.0 > 0).map(|a| label_db(a.1).to_string()).collect();
    l(&format!("{} ", preamps.join(" ")));
    l(&format!("{} ", atts.join(" ")));
    let funcs: u64 = FUNCS.iter().fold(0, |a, f| a | (1u64 << f.1));
    let get_levels: u64 = LEVELS.iter().fold(0, |a, f| a | (1u64 << f.1));
    let set_levels: u64 = LEVELS.iter().filter(|f| f.2).fold(0, |a, f| a | (1u64 << f.1));
    l(&format!("0x{funcs:x}"));
    l(&format!("0x{funcs:x}"));
    l(&format!("0x{get_levels:x}"));
    l(&format!("0x{set_levels:x}"));
    l("0x0");
    l("0x0");
    if protocol1 {
        l("vfo_ops=0x1dff");
        l("ptt_type=0x1");
        l("targetable_vfo=0x0");
        l("has_set_vfo=1");
        l("has_get_vfo=1");
        l("has_set_freq=1");
        l("has_get_freq=1");
        l("has_set_conf=0");
        l("has_get_conf=0");
        l("has_power2mW=0");
        l("has_mW2power=0");
        l("has_get_ant=0");
        l("has_set_ant=0");
        l("timeout=2000");
        l("rig_model=1037");
        l(&format!("rigctld_version=ftdx3000 {}", env!("CARGO_PKG_VERSION")));
        l("agc_levels=0=OFF 2=FAST 5=MEDIUM 3=SLOW 6=AUTO");
        let ctcss: Vec<String> = CTCSS_DHZ.iter().map(|t| format!("{}.{}", t / 10, t % 10)).collect();
        l(&format!("ctcss_list= {}", ctcss.join(" ")));
        l("rig_model=1037");
        l(&format!("hamlib_version=ftdx3000 {}", env!("CARGO_PKG_VERSION")));
        l("done");
    }
    s
}

fn short_name(c: char) -> Option<&'static str> {
    Some(match c {
        'f' => "get_freq",
        'F' => "set_freq",
        'm' => "get_mode",
        'M' => "set_mode",
        'v' => "get_vfo",
        'V' => "set_vfo",
        't' => "get_ptt",
        'T' => "set_ptt",
        's' => "get_split_vfo",
        'S' => "set_split_vfo",
        'i' => "get_split_freq",
        'I' => "set_split_freq",
        'x' => "get_split_mode",
        'X' => "set_split_mode",
        'l' => "get_level",
        'L' => "set_level",
        'u' => "get_func",
        'U' => "set_func",
        'j' => "get_rit",
        'J' => "set_rit",
        'z' => "get_xit",
        'Z' => "set_xit",
        'c' => "get_ctcss_tone",
        'C' => "set_ctcss_tone",
        'G' => "vfo_op",
        'w' => "send_cmd",
        'b' => "send_morse",
        '_' => "get_info",
        'q' | 'Q' => "quit",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::sim::SimRadio;
    use std::io::Read;

    fn session() -> (Session, SimRadio) {
        let sim = SimRadio::new();
        let rig = Rig::start();
        rig.connect_link(sim.link(), EngineConfig { read_menu_on_connect: false, ..Default::default() });
        let t0 = std::time::Instant::now();
        while !rig.link().is_open() && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        (Session { rig, cfg: RigctldConfig::default(), source: Source::Gui, chk_vfo: false, client: ClientCtx::default() }, sim)
    }

    #[test]
    fn basic_commands() {
        let (mut s, sim) = session();
        assert_eq!(s.execute("f").unwrap(), "14074000\n");
        assert_eq!(s.execute("F 7074000").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("f").unwrap(), "7074000\n");
        assert_eq!(s.execute("+f").unwrap(), "get_freq:\nFrequency: 7074000\nRPRT 0\n");
        assert_eq!(s.execute("M USB 3000").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(400));
        assert_eq!(s.execute("m").unwrap(), "USB\n3000\n");
        assert_eq!(s.execute("M PKTUSB 500").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(400));
        assert_eq!(s.execute("m").unwrap(), "PKTUSB\n500\n");
        assert_eq!(s.execute("t").unwrap(), "0\n");
        assert_eq!(s.execute("T 1").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(sim.core().tx, 1);
        assert_eq!(s.execute("T 0").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("L RFPOWER 0.5").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("l RFPOWER").unwrap(), "0.500000\n");
        assert_eq!(s.execute("\\chk_vfo").unwrap(), "0\n");
        assert!(s.execute("\\dump_state").unwrap().ends_with("done\n"));
        assert_eq!(s.execute("w ID;").unwrap(), "ID0462;\n");
        assert_eq!(s.execute("\\get_powerstat").unwrap(), "1\n");
        assert_eq!(s.execute("\\get_lock_mode").unwrap(), "0\nRPRT 0\n");
        assert_eq!(s.execute("+\\get_lock_mode").unwrap(), "get_lock_mode:\nLocked: 0\nRPRT 0\n");
        assert_eq!(s.execute("Y 1").unwrap(), "RPRT -4\n");
        // Hostile numbers don't panic.
        assert_eq!(s.execute("J -1e12").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("L ATT 1e10").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("L AF nan").unwrap(), "RPRT -1\n");
        // Split with a VFO argument (VFO mode clients).
        assert_eq!(s.execute("S VFOA 1 VFOB").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(150));
        assert_eq!(s.execute("s").unwrap(), "1\nVFOB\n");
        assert_eq!(s.execute("S 0 VFOA").unwrap(), "RPRT 0\n");
        // Block TX covers every route to the transmitter.
        s.cfg.block_tx = true;
        for cmd in ["T 1", "w TX1;", "w MX1;", "G TUNE", "U VOX 1", "w KY1;"] {
            assert_eq!(s.execute(cmd).unwrap(), "RPRT -9\n", "{cmd}");
        }
        thread::sleep(Duration::from_millis(150));
        assert_eq!(sim.core().tx, 0);
        assert!(!sim.core().mox);
        s.cfg.block_tx = false;
        assert!(s.execute("q").is_none());
        s.rig.shutdown();
    }

    #[test]
    fn mode_change_keeps_the_frequency() {
        let (mut s, sim) = session();
        sim.core().sideband_shift = 1240;
        // Frequency first, then the mode, as QLog does for a spot.
        assert_eq!(s.execute("F 7267000").unwrap(), "RPRT 0\n");
        assert_eq!(s.execute("M LSB -1").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(400));
        assert_eq!(sim.core().mode_a, Mode::Lsb);
        assert_eq!(sim.core().freq_a, 7_267_000);
        assert_eq!(s.execute("M USB -1").unwrap(), "RPRT 0\n");
        thread::sleep(Duration::from_millis(400));
        assert_eq!(sim.core().freq_a, 7_267_000);
        assert_eq!(s.execute("f").unwrap(), "7267000\n");
        s.rig.shutdown();
    }

    #[test]
    fn width_selection() {
        assert_eq!(width_cmds(WidthClass::Ssb, 2400), vec!["NA00;", "SH014;"]);
        assert_eq!(width_cmds(WidthClass::Ssb, 500), vec!["NA01;", "SH003;"]);
        assert_eq!(width_cmds(WidthClass::Cw, 500), vec!["NA01;", "SH010;"]);
        assert_eq!(width_cmds(WidthClass::Rtty, 3000), vec!["NA00;", "SH016;"]);
        assert_eq!(width_cmds(WidthClass::Cw, -1), Vec::<String>::new());
        assert_eq!(width_cmds(WidthClass::Cw, 0), vec!["NA00;", "SH000;"]);
        assert_eq!(width_cmds(WidthClass::Fixed, 9000), Vec::<String>::new());
    }

    #[test]
    fn tcp_server_round_trip() {
        let (s, _sim) = session();
        let server = RigctldServer::start(s.rig.clone(), RigctldConfig { bind: "127.0.0.1:0".into(), ..Default::default() }).unwrap();
        let mut c = TcpStream::connect(server.addr).unwrap();
        c.write_all(b"f\n").unwrap();
        let mut buf = [0u8; 64];
        let n = c.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"14074000\n");
        s.rig.shutdown();
    }
}
