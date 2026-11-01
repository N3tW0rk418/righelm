//! Remote operation over the LAN: one instance owns the radio (server), others
//! find it automatically and control it as if it were local (clients), with
//! receive audio streamed to the client and, optionally, the client's
//! microphone streamed back to the radio.
//!
//! Protocol
//! --------
//! * Discovery (UDP 4534): servers broadcast a beacon every 2 s and answer
//!   `FTDX3000-DISCOVER` probes; clients listen and probe.
//! * Sessions (TCP 4533): the server greets with a random nonce; the client
//!   proves it knows the password with HMAC-SHA256(password, nonce|mode).
//!   - `CAT` sessions then carry plain FTDX3000 CAT, served exactly like a
//!     virtual serial port (cache, per-session auto-information, TX block).
//!   - `AUDIO` sessions keep the TCP connection as a heartbeat and exchange
//!     20 ms PCM frames over UDP on the same port number.
//!   - `SCOPE` sessions carry the band scope: the client sends its view as
//!     text lines, the server answers with binary rows for that view (see
//!     [`scope_msg`]).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ftdx3000_cat::model::FTDX3000;
use ftdx3000_cat::{Framer, RadioModel};
use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::audio::{self, AudioDevice, Capture, Playback};
use crate::engine::{Rig, Source};
use crate::logger::{SharedSpot, SpotBook, SpotShare};
use crate::scope::{Detail, Feed, ScopeRow, ScopeStatus, ScopeSubscription, ScopeTap, ScopeView};
use crate::transport::Link;
use crate::vport::{self, SharedWriter, VirtualPortConfig};

pub const DEFAULT_PORT: u16 = 4533;
pub const DISCOVERY_PORT: u16 = 4534;
pub const AUDIO_RATE: u32 = 24_000;
const GREETING: &str = "FTDX3000-REMOTE";
const PROBE: &[u8] = b"FTDX3000-DISCOVER";
const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteServerConfig {
    pub enabled: bool,
    pub bind: String,
    /// Name shown to clients (defaults to the host name).
    pub name: String,
    pub password: String,
    /// Allow remote clients to transmit (PTT, TUNE, keyer...).
    pub allow_tx: bool,
    pub audio: bool,
    /// Accept microphone audio from clients while the radio transmits.
    pub tx_audio: bool,
    /// Radio receive audio source; `None` = the radio's codec, auto-detected.
    pub rx_device: Option<AudioDevice>,
    /// Radio transmit audio sink; `None` = the radio's codec, auto-detected.
    pub tx_device: Option<AudioDevice>,
}

impl Default for RemoteServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: format!("0.0.0.0:{DEFAULT_PORT}"),
            name: String::new(),
            password: String::new(),
            allow_tx: true,
            audio: true,
            tx_audio: false,
            rx_device: None,
            tx_device: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClientConfig {
    /// Receive audio from the server.
    pub audio: bool,
    pub speaker: AudioDevice,
    pub volume: f32,
    /// Send the microphone to the radio while transmitting.
    pub send_mic: bool,
    pub mic: AudioDevice,
    pub mic_gain: f32,
}

impl Default for RemoteClientConfig {
    fn default() -> Self {
        Self { audio: true, speaker: AudioDevice::Default, volume: 1.0, send_mic: false, mic: AudioDevice::Default, mic_gain: 1.0 }
    }
}

/// A remote radio to connect to instead of a serial port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemoteTarget {
    /// `host:port`
    pub addr: String,
    pub password: String,
}

/// Random per-process id so an instance doesn't list itself.
pub fn instance_id() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(random_u64)
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b = (t.as_nanos() as u64 ^ (std::process::id() as u64).rotate_left(32)).to_le_bytes();
    }
    u64::from_le_bytes(b)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn mac(password: &str, nonce: &str, mode: &str) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("HMAC takes any key length");
    m.update(format!("ftdx3000:{nonce}:{mode}").as_bytes());
    m
}

/// This machine's non-loopback IPv4 addresses.
pub fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = &i.addr {
            if !v4.ip.is_loopback() && !out.contains(&v4.ip.to_string()) {
                out.push(v4.ip.to_string());
            }
        }
    }
    out
}

pub fn hostname() -> String {
    gethostname::gethostname().into_string().ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "ftdx3000".into())
}

// ---------------------------------------------------------------- discovery

/// A server found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: u64,
    pub name: String,
    pub addr: SocketAddr,
    pub audio: bool,
    pub auth: bool,
    /// The server plays clients' microphones into the radio.
    pub mic: bool,
    /// The server's own radio link state ("connected", "no-response", ...).
    pub radio: String,
    /// Which radio it is (older servers don't say: an FTDX3000).
    pub model: &'static RadioModel,
    pub last_seen: Instant,
}

fn beacon_text(cfg: &RemoteServerConfig, port: u16, radio: &str, model: &RadioModel) -> String {
    let name = if cfg.name.is_empty() { hostname() } else { cfg.name.clone() };
    let name: String = name.chars().filter(|c| *c != ';' && *c != '=').collect();
    format!(
        "{GREETING} v{PROTOCOL};id={:016x};name={name};port={port};audio={};auth={};mic={};radio={radio};model={}",
        instance_id(),
        cfg.audio as u8,
        (!cfg.password.is_empty()) as u8,
        (cfg.audio && cfg.tx_audio && tx_allowed(cfg)) as u8,
        model.key
    )
}

fn parse_beacon(text: &str, from: SocketAddr) -> Option<Found> {
    let rest = text.strip_prefix(GREETING)?.trim_start();
    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut parts = rest.split(';');
    let _version = parts.next()?;
    for p in parts {
        if let Some((k, v)) = p.split_once('=') {
            fields.insert(k, v);
        }
    }
    let port: u16 = fields.get("port")?.parse().ok()?;
    Some(Found {
        id: u64::from_str_radix(fields.get("id")?, 16).ok()?,
        name: fields.get("name").unwrap_or(&"?").to_string(),
        addr: SocketAddr::new(from.ip(), port),
        audio: fields.get("audio") == Some(&"1"),
        auth: fields.get("auth") == Some(&"1"),
        mic: fields.get("mic") == Some(&"1"),
        radio: fields.get("radio").unwrap_or(&"?").to_string(),
        model: fields.get("model").and_then(|k| ftdx3000_cat::model::by_key(k)).unwrap_or(&FTDX3000),
        last_seen: Instant::now(),
    })
}

/// Broadcast addresses of all IPv4 interfaces, plus the limited broadcast.
fn broadcast_targets() -> Vec<SocketAddr> {
    let mut out = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT)];
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = &i.addr {
            if let Some(b) = v4.broadcast.filter(|_| !v4.ip.is_loopback()) {
                let a = SocketAddr::new(IpAddr::V4(b), DISCOVERY_PORT);
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

fn discovery_socket(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    // Lets two copies on one machine (and avahi etc.) share the port.
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port).into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s)
}

/// Background LAN discovery for clients.
pub struct Discovery {
    found: Arc<Mutex<HashMap<u64, Found>>>,
    stop: Arc<AtomicBool>,
}

impl Discovery {
    pub fn start() -> io::Result<Discovery> {
        let sock = discovery_socket(DISCOVERY_PORT).or_else(|_| discovery_socket(0))?;
        let found = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (found2, stop2) = (found.clone(), stop.clone());
        thread::Builder::new().name("discovery".into()).spawn(move || {
            let mut last_probe = Instant::now() - Duration::from_secs(60);
            let mut buf = [0u8; 512];
            while !stop2.load(Ordering::Relaxed) {
                if last_probe.elapsed() > Duration::from_secs(5) {
                    for t in broadcast_targets() {
                        let _ = sock.send_to(PROBE, t);
                    }
                    last_probe = Instant::now();
                }
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    if let Some(f) = parse_beacon(&String::from_utf8_lossy(&buf[..n]), from) {
                        if f.id != instance_id() {
                            found2.lock().insert(f.id, f);
                        }
                    }
                }
                found2.lock().retain(|_, f| f.last_seen.elapsed() < Duration::from_secs(8));
            }
        })?;
        Ok(Discovery { found, stop })
    }

    pub fn servers(&self) -> Vec<Found> {
        let mut v: Vec<Found> = self.found.lock().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------------- server

/// UDP packet: "FX" ver type session:u64 seq:u32 payload(s16le).
const PKT_HELLO: u8 = 1;
const PKT_RX: u8 = 2;
const PKT_TX: u8 = 3;
const HEADER: usize = 16;

fn packet(kind: u8, session: u64, seq: u32, samples: &[i16]) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER + samples.len() * 2);
    p.extend_from_slice(b"FX");
    p.push(1);
    p.push(kind);
    p.extend_from_slice(&session.to_le_bytes());
    p.extend_from_slice(&seq.to_le_bytes());
    for s in samples {
        p.extend_from_slice(&s.to_le_bytes());
    }
    p
}

fn parse_packet(p: &[u8]) -> Option<(u8, u64, u32, Vec<i16>)> {
    if p.len() < HEADER || &p[0..2] != b"FX" || p[2] != 1 {
        return None;
    }
    let session = u64::from_le_bytes(p[4..12].try_into().ok()?);
    let seq = u32::from_le_bytes(p[12..16].try_into().ok()?);
    let samples = p[HEADER..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    Some((p[3], session, seq, samples))
}

#[derive(Debug, Clone)]
struct AudioSession {
    peer: IpAddr,
    udp: Option<SocketAddr>,
    last_seen: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct ServerStatus {
    pub tx_audio_packets: u64,
    pub cat_clients: Vec<String>,
    pub audio_clients: Vec<String>,
    pub rx_audio: Option<String>,
    pub error: Option<String>,
}

pub struct RemoteServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    /// Held by every background thread; when only ours is left they're gone.
    alive: Arc<()>,
    status: Arc<Mutex<ServerStatus>>,
}

impl RemoteServer {
    /// `scope` is this computer's band scope, and `spots` its logger's DX
    /// spots, shared with clients that ask.
    pub fn start(rig: Rig, cfg: RemoteServerConfig, scope: ScopeTap, spots: SpotBook) -> io::Result<RemoteServer> {
        let listener = TcpListener::bind(&cfg.bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let udp = UdpSocket::bind(addr)?;
        udp.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(());
        let status = Arc::new(Mutex::new(ServerStatus::default()));
        let sessions: Arc<Mutex<HashMap<u64, AudioSession>>> = Arc::new(Mutex::new(HashMap::new()));
        if cfg.password.is_empty() {
            rig.note("remote server has no password: remote clients can listen and tune but not transmit");
        }
        rig.note(format!("remote server listening on {addr} (TCP control, UDP audio)"));

        // Discovery: beacon + probe responder.
        {
            let (rig, cfg, stop) = (rig.clone(), cfg.clone(), stop.clone());
            let sock = discovery_socket(DISCOVERY_PORT).or_else(|_| discovery_socket(0))?;
            let alive_guard = alive.clone();
            thread::Builder::new().name("remote-beacon".into()).spawn(move || {
                let _alive_guard = alive_guard;
                let mut last = Instant::now() - Duration::from_secs(10);
                let mut buf = [0u8; 256];
                while !stop.load(Ordering::Relaxed) {
                    let radio = match rig.link() {
                        crate::LinkState::Connected { .. } => "connected",
                        crate::LinkState::NoResponse { .. } => "not-answering",
                        _ => "disconnected",
                    };
                    let text = beacon_text(&cfg, addr.port(), radio, rig.model());
                    if last.elapsed() > Duration::from_secs(2) {
                        for t in broadcast_targets() {
                            let _ = sock.send_to(text.as_bytes(), t);
                        }
                        last = Instant::now();
                    }
                    if let Ok((n, from)) = sock.recv_from(&mut buf) {
                        if &buf[..n] == PROBE {
                            let _ = sock.send_to(text.as_bytes(), from);
                        }
                    }
                }
            })?;
        }

        // Control connections.
        {
            let gate = Arc::new(Gate::default());
            let (rig, cfg, stop, status, sessions) = (rig.clone(), cfg.clone(), stop.clone(), status.clone(), sessions.clone());
            let alive_guard = alive.clone();
            let (scope, spots) = (scope.clone(), spots.clone());
            thread::Builder::new().name("remote-accept".into()).spawn(move || {
                let _alive_guard = alive_guard;
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let _ = stream.set_nonblocking(false);
                            let (rig, cfg, stop, status, sessions, gate, scope, spots) =
                                (rig.clone(), cfg.clone(), stop.clone(), status.clone(), sessions.clone(), gate.clone(), scope.clone(), spots.clone());
                            let _ = thread::Builder::new().name(format!("remote-{peer}")).spawn(move || {
                                if let Err(e) = serve_session(&rig, &cfg, stream, peer, &stop, &status, &sessions, &gate, &scope, &spots) {
                                    log::debug!("remote session {peer}: {e}");
                                }
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                }
            })?;
        }

        // Audio: receive hellos and TX audio; stream RX audio to sessions.
        // The radio's sound card is looked up whenever a stream starts (it may
        // be switched on after us), and never replaced by the system default:
        // that would be this computer's own microphone and speakers.
        if cfg.audio {
            let udp = Arc::new(udp);
            {
                let (udp, stop, sessions, rig, cfg, status) = (udp.clone(), stop.clone(), sessions.clone(), rig.clone(), cfg.clone(), status.clone());
                let alive_guard = alive.clone();
                thread::Builder::new().name("remote-udp-in".into()).spawn(move || {
                    let _alive_guard = alive_guard;
                    let mut buf = [0u8; 4096];
                    let mut tx_play: Option<(Playback, Instant)> = None;
                    let mut next_open = Instant::now();
                    // One client's microphone at a time: never interleave two.
                    let mut talker: Option<(u64, Instant)> = None;
                    while !stop.load(Ordering::Relaxed) {
                        if let Some((_, at)) = &tx_play {
                            if at.elapsed() > Duration::from_secs(1) {
                                tx_play = None;
                            }
                        }
                        let Ok((n, from)) = udp.recv_from(&mut buf) else { continue };
                        let Some((kind, session, _seq, samples)) = parse_packet(&buf[..n]) else { continue };
                        let mut s = sessions.lock();
                        let Some(sess) = s.get_mut(&session) else { continue };
                        if from.ip() != sess.peer {
                            continue;
                        }
                        sess.udp = Some(from);
                        sess.last_seen = Instant::now();
                        drop(s);
                        if kind == PKT_TX && cfg.tx_audio && tx_allowed(&cfg) && crate::txaudio::mic_tx_allowed(&rig, true) {
                            if talker.is_some_and(|(id, at)| id != session && at.elapsed() < Duration::from_millis(500)) {
                                continue;
                            }
                            talker = Some((session, Instant::now()));
                            if tx_play.is_none() && Instant::now() >= next_open {
                                next_open = Instant::now() + Duration::from_secs(2);
                                match cfg.tx_device.clone().or_else(|| audio::radio_devices().1) {
                                    Some(dev) => match Playback::start(&dev, AUDIO_RATE, Duration::from_millis(120)) {
                                        Ok(p) => tx_play = Some((p, Instant::now())),
                                        Err(e) => status.lock().error = Some(format!("TX audio {dev}: {e}")),
                                    },
                                    None => status.lock().error = Some("TX audio: the radio's sound card was not found".into()),
                                }
                            }
                            if let Some((p, at)) = &mut tx_play {
                                p.push(samples);
                                *at = Instant::now();
                                status.lock().tx_audio_packets += 1;
                            }
                        }
                    }
                })?;
            }
            {
                let (stop, sessions, status, cfg) = (stop.clone(), sessions.clone(), status.clone(), cfg.clone());
                let alive_guard = alive.clone();
                thread::Builder::new().name("remote-udp-out".into()).spawn(move || {
                    let _alive_guard = alive_guard;
                    let mut capture: Option<Capture> = None;
                    let mut seq: u32 = 0;
                    // Never send faster than real time, whatever the device does.
                    let mut pacer = audio::Pacer::new(AUDIO_RATE, Duration::from_millis(60));
                    let mut idle_since = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let targets: Vec<(u64, SocketAddr)> = {
                            let mut s = sessions.lock();
                            s.retain(|_, a| a.last_seen.elapsed() < Duration::from_secs(10));
                            s.iter().filter_map(|(id, a)| a.udp.map(|u| (*id, u))).collect()
                        };
                        if targets.is_empty() {
                            if capture.is_some() && idle_since.elapsed() > Duration::from_secs(5) {
                                capture = None;
                                status.lock().rx_audio = None;
                            }
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        idle_since = Instant::now();
                        if capture.as_mut().is_none_or(|c| !c.alive()) {
                            let Some(rx_dev) = cfg.rx_device.clone().or_else(|| audio::radio_devices().0) else {
                                status.lock().error = Some("RX audio: the radio's sound card was not found (is the radio on?)".into());
                                thread::sleep(Duration::from_secs(2));
                                continue;
                            };
                            match Capture::start(&rx_dev, AUDIO_RATE) {
                                Ok(c) => {
                                    let mut st = status.lock();
                                    st.rx_audio = Some(rx_dev.to_string());
                                    st.error = None;
                                    capture = Some(c);
                                }
                                Err(e) => {
                                    status.lock().error = Some(format!("RX audio: {e}"));
                                    thread::sleep(Duration::from_secs(2));
                                    continue;
                                }
                            }
                        }
                        let Some(c) = &capture else { continue };
                        if let Ok(frame) = c.frames.recv_timeout(Duration::from_millis(200)) {
                            if !pacer.admit(frame.len()) {
                                // Ahead of real time: drop this frame instead of
                                // flooding, but count it so clients see a gap
                                // and fade across it rather than splice.
                                seq = seq.wrapping_add(1);
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            seq = seq.wrapping_add(1);
                            for (id, to) in &targets {
                                let _ = udp.send_to(&packet(PKT_RX, *id, seq, &frame), to);
                            }
                        }
                    }
                })?;
            }
        }
        Ok(RemoteServer { addr, stop, alive, status })
    }

    pub fn status(&self) -> ServerStatus {
        self.status.lock().clone()
    }
}

impl Drop for RemoteServer {
    /// Stops the server and waits for its threads to let go of the ports,
    /// so it can be restarted on the same port straight away.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let t0 = Instant::now();
        while Arc::strong_count(&self.alive) > 1 && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Shared protection for the control port.
#[derive(Default)]
struct Gate {
    active: std::sync::atomic::AtomicUsize,
    /// Wrong-password count and time of the last failure, per address.
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

const MAX_SESSIONS: usize = 16;
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// The client engine polls several times a second; silence means it's gone.
const CAT_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

impl Gate {
    fn locked_out(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock();
        f.retain(|_, (_, at)| at.elapsed() < LOCKOUT);
        f.get(&ip).is_some_and(|(n, _)| *n >= MAX_FAILURES)
    }

    fn failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock();
        let e = f.entry(ip).or_insert((0, Instant::now()));
        e.0 += 1;
        e.1 = Instant::now();
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_session(
    rig: &Rig,
    cfg: &RemoteServerConfig,
    mut stream: TcpStream,
    peer: SocketAddr,
    stop: &Arc<AtomicBool>,
    status: &Arc<Mutex<ServerStatus>>,
    sessions: &Arc<Mutex<HashMap<u64, AudioSession>>>,
    gate: &Arc<Gate>,
    scope: &ScopeTap,
    spots: &SpotBook,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket2::SockRef::from(&stream).set_keepalive(true)?;
    if gate.active.load(Ordering::Relaxed) >= MAX_SESSIONS {
        stream.write_all(b"DENIED too many sessions\n")?;
        return Ok(());
    }
    if gate.locked_out(peer.ip()) {
        stream.write_all(b"DENIED too many wrong passwords, try again later\n")?;
        return Ok(());
    }
    gate.active.fetch_add(1, Ordering::Relaxed);
    struct Active<'a>(&'a Gate);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _active = Active(gate);

    let nonce = hex(&random_u64().to_le_bytes());
    let name = if cfg.name.is_empty() { hostname() } else { cfg.name.clone() };
    stream.write_all(format!("{GREETING} {PROTOCOL} {nonce} {name}\n").as_bytes())?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    // Bounded: an unauthenticated peer can't make us buffer more than this.
    (&mut reader).take(256).read_line(&mut line)?;
    let mut it = line.split_whitespace();
    let (Some("HELLO"), Some(mode), Some(proof)) = (it.next(), it.next(), it.next()) else {
        stream.write_all(b"DENIED bad hello\n")?;
        return Ok(());
    };
    let mode = mode.to_string();
    if !cfg.password.is_empty() {
        let ok = unhex(proof).is_some_and(|p| mac(&cfg.password, &nonce, &mode).verify_slice(&p).is_ok());
        if !ok {
            gate.failed(peer.ip());
            rig.note(format!("remote: {peer} gave a wrong password"));
            thread::sleep(Duration::from_millis(500));
            stream.write_all(b"DENIED wrong password\n")?;
            return Ok(());
        }
    }
    match mode.as_str() {
        "CAT" => {}
        "KEY" if tx_allowed(cfg) => {}
        "KEY" => {
            stream.write_all(b"DENIED this server does not allow transmit\n")?;
            return Ok(());
        }
        "AUDIO" if cfg.audio => {}
        "AUDIO" => {
            stream.write_all(b"DENIED audio is turned off on this server\n")?;
            return Ok(());
        }
        "SCOPE" => {}
        _ => {
            stream.write_all(b"DENIED unsupported mode\n")?;
            return Ok(());
        }
    }
    let session = random_u64();
    stream.write_all(format!("OK {session:016x} {} {AUDIO_RATE} {}\n", stream.local_addr()?.port(), rig.model().key).as_bytes())?;
    let who = format!("remote {}", peer.ip());
    match mode.as_str() {
        "CAT" => {
            rig.note(format!("remote control session from {peer}"));
            status.lock().cat_clients.push(peer.to_string());
            let r = serve_cat(rig, cfg, stream, reader, &who, stop);
            status.lock().cat_clients.retain(|c| *c != peer.to_string());
            rig.note(format!("remote control session from {peer} ended"));
            r
        }
        "KEY" => {
            // Key the radio for a remote CW operator.
            match crate::cw::LocalKeyLine::open(rig, Source::Client(who.clone())) {
                Ok(mut line) => {
                    stream.write_all(b"READY\n")?;
                    rig.note(format!("remote CW key from {peer}"));
                    let r = crate::cw::serve_key(&mut line, stream, stop);
                    rig.note(format!("remote CW key from {peer} ended"));
                    r
                }
                Err(e) => {
                    stream.write_all(format!("ERR {e}\n").as_bytes())?;
                    Ok(())
                }
            }
        }
        "SCOPE" => {
            rig.note(format!("remote band scope session from {peer}"));
            let r = serve_scope(scope, spots, stream, reader, stop);
            rig.note(format!("remote band scope session from {peer} ended"));
            r
        }
        "AUDIO" if cfg.audio => {
            rig.note(format!("remote audio session from {peer}"));
            sessions.lock().insert(session, AudioSession { peer: peer.ip(), udp: None, last_seen: Instant::now() });
            status.lock().audio_clients.push(peer.to_string());
            // Heartbeat: the session lives while the TCP connection does.
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut buf = [0u8; 64];
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) && last.elapsed() < Duration::from_secs(10) {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        last = Instant::now();
                        if let Some(s) = sessions.lock().get_mut(&session) {
                            s.last_seen = Instant::now();
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
            sessions.lock().remove(&session);
            status.lock().audio_clients.retain(|c| *c != peer.to_string());
            rig.note(format!("remote audio session from {peer} ended"));
            Ok(())
        }
        _ => {
            stream.write_all(b"DENIED unsupported mode\n")?;
            Ok(())
        }
    }
}

fn serve_cat(rig: &Rig, cfg: &RemoteServerConfig, stream: TcpStream, mut reader: BufReader<TcpStream>, who: &str, stop: &Arc<AtomicBool>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(stream.try_clone()?)));
    let client = Arc::new(vport::ClientCtx::default());
    let connected = Arc::new(AtomicBool::new(true));
    let session_stop = Arc::new(AtomicBool::new(false));
    // The client is a RigHelm engine: push it the meters so they don't
    // wait on its polls.
    let _pusher = vport::ai_pusher(rig.clone(), writer.clone(), client.clone(), connected.clone(), session_stop.clone(), true, || {})?;
    let vcfg = VirtualPortConfig { name: who.into(), label: who.into(), enabled: true, block_tx: !tx_allowed(cfg), com_port: String::new(), emulate: "ftdx3000".into() };
    let source = Source::Client(who.into());
    let mut framer = Framer::new();
    let mut buf = [0u8; 1024];
    let mut last_rx = Instant::now();
    let model = rig.model();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // The client was told which radio this is; if another one has been
        // connected since, it must reconnect to find out.
        if rig.model() != model {
            break Ok(());
        }
        if last_rx.elapsed() > CAT_IDLE_TIMEOUT {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "client went silent"));
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
            Err(e) => break Err(e),
        };
        last_rx = Instant::now();
        let mut failed = None;
        for f in framer.push(&buf[..n]) {
            if let Some(reply) = vport::handle(rig, &vcfg, &source, &client, &f) {
                if let Err(e) = writer.lock().write_all(reply.as_bytes()) {
                    failed = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
    };
    // Every exit path: stop pushing and release the transmitter if this
    // client was holding it.
    session_stop.store(true, Ordering::Relaxed);
    connected.store(false, Ordering::Relaxed);
    client.disconnected(rig, &source);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result
}

/// Transmitting needs both the setting and a password: an open server is
/// receive-only.
fn tx_allowed(cfg: &RemoteServerConfig) -> bool {
    cfg.allow_tx && !cfg.password.is_empty()
}

// ------------------------------------------------------------------- client

/// Connect, greet and authenticate. Returns the stream positioned after `OK`,
/// the session id, server UDP port and audio rate.
fn handshake(addr: &str, password: &str, mode: &str) -> io::Result<(TcpStream, String, u64, u16, u32)> {
    handshake_model(addr, password, mode).map(|(h, _)| h)
}

/// [`handshake`], and the radio model the server has.
fn handshake_model(addr: &str, password: &str, mode: &str) -> io::Result<((TcpStream, String, u64, u16, u32), &'static RadioModel)> {
    let mut addrs: Vec<SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(addr)?.collect();
    // "localhost" often resolves to ::1 first; servers usually listen on IPv4.
    addrs.sort_by_key(|a| a.is_ipv6());
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {addr}"));
    let mut connected = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(s) => {
                connected = Some((s, a));
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let (stream, sock_addr) = connected.ok_or(last_err)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut it = line.split_whitespace();
    let (Some(GREETING), Some(_ver), Some(nonce)) = (it.next(), it.next(), it.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an FTDX3000 rig control server"));
    };
    let server_name = it.collect::<Vec<_>>().join(" ");
    let proof = hex(&mac(password, nonce, mode).finalize().into_bytes());
    (&stream).write_all(format!("HELLO {mode} {proof}\n").as_bytes())?;
    line.clear();
    reader.read_line(&mut line)?;
    let mut it = line.split_whitespace();
    match it.next() {
        Some("OK") => {
            let session = it.next().and_then(|s| u64::from_str_radix(s, 16).ok()).unwrap_or(0);
            let udp_port = it.next().and_then(|s| s.parse().ok()).unwrap_or(sock_addr.port());
            let rate = it.next().and_then(|s| s.parse().ok()).unwrap_or(AUDIO_RATE);
            if !(8_000..=48_000).contains(&rate) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported audio rate {rate}")));
            }
            // Servers before multi-radio support say nothing: an FTDX3000.
            let model = it.next().and_then(ftdx3000_cat::model::by_key).unwrap_or(&FTDX3000);
            // Anything the BufReader read ahead would be lost; the server
            // sends nothing until we do, so there is none.
            Ok(((stream, server_name, session, udp_port, rate), model))
        }
        Some("DENIED") => Err(io::Error::new(io::ErrorKind::PermissionDenied, line.trim().trim_start_matches("DENIED ").to_string())),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected reply: {}", line.trim()))),
    }
}

/// Open a CW key session: the server arms its keying port, then answers.
pub fn open_key_session(target: &RemoteTarget) -> io::Result<TcpStream> {
    let (stream, _, _, _, _) = handshake(&target.addr, &target.password, "KEY")?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let reply = crate::cw::read_reply(&stream, Duration::from_secs(6))?;
    match reply.split_once(' ').map_or((reply.as_str(), ""), |(a, b)| (a, b)) {
        ("READY", _) => Ok(stream),
        ("ERR", e) => Err(io::Error::other(e.to_string())),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected reply: {reply}"))),
    }
}

/// TCP reads that report end-of-stream as an error, so the engine notices.
struct TcpReader(TcpStream);

impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf)? {
            0 => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed the connection")),
            n => Ok(n),
        }
    }
}

/// Open a remote CAT link for the engine, and say which radio is behind it.
/// The link speaks RigHelm's language: the server translates for its radio.
pub fn connect_cat(target: &RemoteTarget) -> io::Result<(Link, &'static RadioModel)> {
    let ((stream, name, _, _, _), model) = handshake_model(&target.addr, &target.password, "CAT")?;
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    let link = Link {
        reader: Box::new(TcpReader(stream.try_clone()?)),
        writer: Box::new(stream),
        description: format!("remote {} \"{name}\" at {}", model.name, target.addr),
    };
    Ok((link, model))
}

#[derive(Debug, Clone, Default)]
pub struct AudioClientStatus {
    pub connected: bool,
    pub server: String,
    pub rx_level_db: f32,
    pub packets: u64,
    pub lost: u64,
    pub sending_mic: bool,
    pub mic_level_db: f32,
    pub error: Option<String>,
}

/// Receives the remote radio's audio (and sends the microphone back).
pub struct AudioClient {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<AudioClientStatus>>,
    config: Arc<Mutex<RemoteClientConfig>>,
    generation: Arc<AtomicU64>,
}

impl AudioClient {
    /// `tap` also receives the incoming radio audio (for the waterfall).
    pub fn start(rig: Rig, target: RemoteTarget, config: RemoteClientConfig, tap: Option<crate::spectrum::AudioTap>) -> AudioClient {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(AudioClientStatus::default()));
        let config = Arc::new(Mutex::new(config));
        let generation = Arc::new(AtomicU64::new(0));
        let (stop2, status2, config2, gen2, tap2) = (stop.clone(), status.clone(), config.clone(), generation.clone(), tap);
        thread::Builder::new()
            .name("remote-audio".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    if let Err(e) = audio_session(&rig, &target, &config2, &status2, &stop2, &gen2, tap2.as_ref()) {
                        let mut s = status2.lock();
                        s.connected = false;
                        s.error = Some(e.to_string());
                    }
                    for _ in 0..30 {
                        if stop2.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn audio client");
        AudioClient { stop, status, config, generation }
    }

    pub fn status(&self) -> AudioClientStatus {
        self.status.lock().clone()
    }

    /// Change volume/mic settings; device changes restart the streams.
    pub fn update(&self, cfg: RemoteClientConfig) {
        let mut cur = self.config.lock();
        if cur.speaker != cfg.speaker || cur.mic != cfg.mic || cur.send_mic != cfg.send_mic || cur.audio != cfg.audio {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        *cur = cfg;
    }
}

impl Drop for AudioClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn audio_session(
    rig: &Rig,
    target: &RemoteTarget,
    config: &Arc<Mutex<RemoteClientConfig>>,
    status: &Arc<Mutex<AudioClientStatus>>,
    stop: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
    tap: Option<&crate::spectrum::AudioTap>,
) -> io::Result<()> {
    if !config.lock().audio && !config.lock().send_mic {
        thread::sleep(Duration::from_millis(500));
        return Ok(());
    }
    let my_gen = generation.load(Ordering::Relaxed);
    let (mut control, name, session, udp_port, rate) = handshake(&target.addr, &target.password, "AUDIO")?;
    let server_ip = control.peer_addr()?.ip();
    let server_udp = SocketAddr::new(server_ip, udp_port);
    let any: IpAddr = if server_ip.is_ipv6() { Ipv6Addr::UNSPECIFIED.into() } else { Ipv4Addr::UNSPECIFIED.into() };
    let udp = UdpSocket::bind(SocketAddr::new(any, 0))?;
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;
    let cfg = config.lock().clone();
    // Room for Wi-Fi that delivers the packets in clumps 150 ms apart.
    let mut speaker = Playback::start(&cfg.speaker, rate, Duration::from_millis(300))?;
    // A microphone that won't open must not cost the receive audio.
    let (mut mic, mic_error) = if cfg.send_mic {
        match audio::computer_mic(&cfg.mic).and_then(|d| Capture::start(&d, rate).map_err(|e| format!("microphone {d}: {e}"))) {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    {
        let mut s = status.lock();
        *s = AudioClientStatus { connected: true, server: name.clone(), error: mic_error, mic_level_db: -100.0, ..Default::default() };
    }
    rig.note(format!("receiving audio from \"{name}\""));
    let mut last_hello = Instant::now() - Duration::from_secs(5);
    let mut last_seq: Option<u32> = None;
    let mut buf = [0u8; 4096];
    let mut tx_seq: u32 = 0;
    let frame = audio::frame_len(rate);
    let result = loop {
        if stop.load(Ordering::Relaxed) || generation.load(Ordering::Relaxed) != my_gen {
            break Ok(());
        }
        if last_hello.elapsed() > Duration::from_secs(1) {
            udp.send_to(&packet(PKT_HELLO, session, 0, &[]), server_udp)?;
            if let Err(e) = control.write_all(b"\n") {
                break Err(e);
            }
            last_hello = Instant::now();
        }
        if !speaker.alive() {
            break Err(io::Error::other("speaker playback stopped"));
        }
        // Microphone: only while this operator transmits in a phone mode.
        if let Some(m) = &mic {
            let transmitting = crate::txaudio::mic_tx_allowed(rig, false);
            let gain = config.lock().mic_gain;
            let mut level = None;
            while let Ok(mut f) = m.frames.try_recv() {
                audio::apply_gain(&mut f, gain);
                level = Some(audio::level_dbfs(&f));
                if transmitting {
                    tx_seq = tx_seq.wrapping_add(1);
                    let _ = udp.send_to(&packet(PKT_TX, session, tx_seq, &f), server_udp);
                }
            }
            let mut s = status.lock();
            s.sending_mic = transmitting;
            if let Some(l) = level {
                s.mic_level_db = l;
            }
        }
        match udp.recv_from(&mut buf) {
            Ok((n, from)) if from.ip() == server_ip => {
                let Some((PKT_RX, sid, seq, mut samples)) = parse_packet(&buf[..n]) else { continue };
                if sid != session {
                    continue;
                }
                let mut lost = 0;
                match last_seq {
                    Some(l) if seq <= l && l.wrapping_sub(seq) < 1000 => continue, // late duplicate
                    Some(l) => {
                        let gap = seq.wrapping_sub(l).saturating_sub(1);
                        if (1..=5).contains(&gap) {
                            // Conceal short losses with silence to keep timing.
                            for _ in 0..gap {
                                speaker.push(vec![0; frame]);
                            }
                        }
                        lost = gap as u64;
                    }
                    None => {}
                }
                last_seq = Some(seq);
                if let Some(t) = tap {
                    t.publish(rate, &samples);
                }
                // Sending the mic without listening: keep quiet.
                let vol = if config.lock().audio { config.lock().volume } else { 0.0 };
                let level = audio::level_dbfs(&samples);
                audio::apply_gain(&mut samples, vol);
                speaker.push(samples);
                let mut s = status.lock();
                s.packets += 1;
                s.lost += lost;
                s.rx_level_db = level;
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => break Err(e),
        }
        if mic.as_mut().is_some_and(|m| !m.alive()) {
            mic = None;
            status.lock().error = Some("microphone capture stopped".into());
        }
    };
    status.lock().connected = false;
    result
}

// --------------------------------------------------------------- band scope

/// Band scope messages, server to client: a type byte, a little-endian u32
/// payload length, then the payload.
/// * `R` row: start_hz f64, col_hz f64, flags u8 (1 = transmitting), levels.
/// * `S` status: flags u8 (1 = present, 2 = running), feed u8 (0 none,
///   1 IF OUT, 2 RX OUT), covers lo f64, hi f64 (0 = unknown), text.
/// * `D` detail: id u32, start_hz f64, bin_hz f64, levels f32 (dB).
/// * `L` logger spots: flags u8 (1 = first part, 2 = last part, 4 = clicking
///   sets the mode, 8 = clicking fills in the logger), then a JSON array of
///   spots. The parts between a first and a last are the whole list.
///
/// Client to server, text lines: `VIEW <centre Hz> <span Hz> <cols> <fps>`
/// starts rows for that view, `IDLE` stops them, `DETAIL <id> <lo> <hi>`
/// asks for a full-resolution look at `lo..hi`, `SPOTS` asks for the logger's
/// spots from now on, `SPOTCALL <Hz> <call>` fills in the logger on the
/// server's computer with a spot it sent; a bare newline is a heartbeat. The
/// server sends nothing until the client's first line.
pub mod scope_msg {
    use super::*;

    pub const ROW: u8 = b'R';
    pub const STATUS: u8 = b'S';
    pub const DETAIL: u8 = b'D';
    pub const SPOTS: u8 = b'L';
    /// Largest payload a client accepts.
    pub const MAX_LEN: usize = 16_384;

    fn frame(kind: u8, payload: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    pub fn row(r: &ScopeRow) -> Vec<u8> {
        let mut p = Vec::with_capacity(17 + r.cols.len());
        p.extend_from_slice(&r.start_hz.to_le_bytes());
        p.extend_from_slice(&r.col_hz.to_le_bytes());
        p.push(r.transmitting as u8);
        p.extend_from_slice(&r.cols);
        frame(ROW, p)
    }

    pub fn status(s: &ScopeStatus) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(s.present as u8 | (s.running as u8) << 1);
        p.push(match s.feed {
            None => 0,
            Some(Feed::IfOut) => 1,
            Some(Feed::RxOut) => 2,
        });
        let (lo, hi) = s.covers.unwrap_or((0.0, 0.0));
        p.extend_from_slice(&lo.to_le_bytes());
        p.extend_from_slice(&hi.to_le_bytes());
        p.extend_from_slice(s.text.as_bytes());
        frame(STATUS, p)
    }

    fn f64_at(p: &[u8], at: usize) -> Option<f64> {
        Some(f64::from_le_bytes(p.get(at..at + 8)?.try_into().ok()?))
    }

    pub fn parse_row(p: &[u8]) -> Option<ScopeRow> {
        let (start_hz, col_hz) = (f64_at(p, 0)?, f64_at(p, 8)?);
        let flags = *p.get(16)?;
        if !start_hz.is_finite() || !col_hz.is_finite() || col_hz <= 0.0 {
            return None;
        }
        Some(ScopeRow { start_hz, col_hz, cols: p[17..].to_vec(), transmitting: flags & 1 != 0 })
    }

    pub fn parse_status(p: &[u8]) -> Option<ScopeStatus> {
        let flags = *p.first()?;
        let feed = match p.get(1)? {
            1 => Some(Feed::IfOut),
            2 => Some(Feed::RxOut),
            _ => None,
        };
        let (lo, hi) = (f64_at(p, 2)?, f64_at(p, 10)?);
        let covers = (lo.is_finite() && hi.is_finite() && hi > lo).then_some((lo, hi));
        let text = String::from_utf8_lossy(p.get(18..)?).into_owned();
        Some(ScopeStatus { present: flags & 1 != 0, running: flags & 2 != 0, text, covers, feed, dongles: Vec::new() })
    }

    pub fn detail(id: u32, d: &Detail) -> Vec<u8> {
        let mut p = Vec::with_capacity(20 + d.db.len() * 4);
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&d.start_hz.to_le_bytes());
        p.extend_from_slice(&d.bin_hz.to_le_bytes());
        for v in d.db.iter().take((MAX_LEN - 20) / 4) {
            p.extend_from_slice(&v.to_le_bytes());
        }
        frame(DETAIL, p)
    }

    pub fn parse_detail(p: &[u8]) -> Option<(u32, Detail)> {
        let id = u32::from_le_bytes(p.get(0..4)?.try_into().ok()?);
        let (start_hz, bin_hz) = (f64_at(p, 4)?, f64_at(p, 12)?);
        if !start_hz.is_finite() || !bin_hz.is_finite() || bin_hz <= 0.0 {
            return None;
        }
        let db = p[20..].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        Some((id, Detail { start_hz, bin_hz, db }))
    }

    /// The spots in messages small enough for any client's `MAX_LEN`. There
    /// is always one, so that an empty list gets through too.
    pub fn spots(share: &SpotShare) -> Vec<Vec<u8>> {
        let mut parts: Vec<String> = vec![String::new()];
        for s in &share.spots {
            let Ok(text) = serde_json::to_string(s) else { continue };
            // A flags byte, two brackets, a comma.
            if text.len() + 4 > MAX_LEN {
                continue;
            }
            if parts.last().is_some_and(|p| p.len() + text.len() + 4 > MAX_LEN) {
                parts.push(String::new());
            }
            let p = parts.last_mut().expect("never empty");
            if !p.is_empty() {
                p.push(',');
            }
            p.push_str(&text);
        }
        let last = parts.len() - 1;
        let settings = (share.click_sets_mode as u8) << 2 | (share.send_call as u8) << 3;
        let framed = parts.iter().enumerate().map(|(i, part)| {
            let mut p = vec![(i == 0) as u8 | ((i == last) as u8) << 1 | settings, b'['];
            p.extend_from_slice(part.as_bytes());
            p.push(b']');
            frame(SPOTS, p)
        });
        framed.collect()
    }

    /// One part: is it the first, is it the last, and what it says.
    pub fn parse_spots(p: &[u8]) -> Option<(bool, bool, SpotShare)> {
        let flags = *p.first()?;
        let spots: Vec<SharedSpot> = serde_json::from_slice(p.get(1..)?).ok()?;
        Some((flags & 1 != 0, flags & 2 != 0, SpotShare { click_sets_mode: flags & 4 != 0, send_call: flags & 8 != 0, spots }))
    }

    pub fn call_line(hz: u64, call: &str) -> String {
        format!("SPOTCALL {hz} {call}\n")
    }

    pub fn parse_call_line(line: &str) -> Option<(u64, &str)> {
        let mut it = line.split_whitespace();
        if it.next()? != "SPOTCALL" {
            return None;
        }
        Some((it.next()?.parse().ok()?, it.next()?))
    }

    pub fn detail_line(id: u32, lo: f64, hi: f64) -> String {
        format!("DETAIL {id} {lo:.0} {hi:.0}\n")
    }

    pub fn parse_detail_line(line: &str) -> Option<(u32, f64, f64)> {
        let mut it = line.split_whitespace();
        if it.next()? != "DETAIL" {
            return None;
        }
        let (id, lo, hi): (u32, f64, f64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
        (lo.is_finite() && hi.is_finite()).then_some((id, lo, hi))
    }

    pub fn view_line(v: &ScopeView) -> String {
        format!("VIEW {:.0} {:.0} {} {}\n", v.center_hz, v.span_hz, v.cols, v.fps)
    }

    pub fn parse_view(line: &str) -> Option<ScopeView> {
        let mut it = line.split_whitespace();
        if it.next()? != "VIEW" {
            return None;
        }
        let view = ScopeView { center_hz: it.next()?.parse().ok()?, span_hz: it.next()?.parse().ok()?, cols: it.next()?.parse().ok()?, fps: it.next()?.parse().ok()? };
        Some(view.sanitized())
    }
}

/// Serve this computer's band scope to a remote client: rows for the view it
/// asks for, the scope's status, and the logger's spots if it wants them,
/// until it goes away.
fn serve_scope(scope: &ScopeTap, spots: &SpotBook, stream: TcpStream, mut reader: BufReader<TcpStream>, stop: &Arc<AtomicBool>) -> io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    // Lines from the client on their own thread; rows go out on this one.
    // (view, last heard, has spoken)
    let wanted: Arc<Mutex<(Option<ScopeView>, Instant, bool)>> = Arc::new(Mutex::new((None, Instant::now(), false)));
    let done = Arc::new(AtomicBool::new(false));
    let wants_spots = Arc::new(AtomicBool::new(false));
    let (ask_tx, ask_rx) = crossbeam_channel::bounded::<(u32, f64, f64)>(4);
    {
        let (wanted, done, wants_spots, spots) = (wanted.clone(), done.clone(), wants_spots.clone(), spots.clone());
        reader.get_ref().set_read_timeout(Some(Duration::from_millis(500)))?;
        thread::Builder::new().name("remote-scope-in".into()).spawn(move || {
            let mut line = String::new();
            while !done.load(Ordering::Relaxed) {
                line.clear();
                // Bounded, like the hello: a peer can't make us buffer more.
                match (&mut reader).take(256).read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut w = wanted.lock();
                        w.1 = Instant::now();
                        w.2 = true;
                        if let Some(v) = scope_msg::parse_view(&line) {
                            w.0 = Some(v);
                        } else if let Some(ask) = scope_msg::parse_detail_line(&line) {
                            let _ = ask_tx.try_send(ask);
                        } else if let Some((hz, call)) = scope_msg::parse_call_line(&line) {
                            spots.send_shared_call(hz, call);
                        } else if line.trim() == "SPOTS" {
                            wants_spots.store(true, Ordering::Relaxed);
                        } else if line.trim() == "IDLE" {
                            w.0 = None;
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
            done.store(true, Ordering::Relaxed);
        })?;
    }
    let mut out = stream.try_clone()?;
    let mut sub: Option<ScopeSubscription> = None;
    let mut sent_status: Option<(ScopeStatus, Instant)> = None;
    let mut spots_seen = None;
    let mut spots_sent: Option<Instant> = None;
    // Detail asks waiting for the scope's answer.
    let mut asked: Vec<(u32, crossbeam_channel::Receiver<Detail>, Instant)> = Vec::new();
    let result = loop {
        if stop.load(Ordering::Relaxed) || done.load(Ordering::Relaxed) {
            break Ok(());
        }
        let (view, heard, spoken) = *wanted.lock();
        if heard.elapsed() > Duration::from_secs(10) {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "client went silent"));
        }
        // The client reads the handshake through a buffer: anything sent
        // before it speaks could be swallowed by it.
        if !spoken {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        // Only watch the dongle while the client wants rows.
        match (view, &sub) {
            (Some(v), Some(s)) => s.set_view(v),
            (Some(v), None) => sub = Some(scope.subscribe(v)),
            (None, Some(_)) => sub = None,
            (None, None) => {}
        }
        let status = scope.status();
        if sent_status.as_ref().is_none_or(|(s, at)| *s != status || at.elapsed() > Duration::from_secs(2)) {
            if let Err(e) = out.write_all(&scope_msg::status(&status)) {
                break Err(e);
            }
            sent_status = Some((status, Instant::now()));
        }
        // A cluster feed changes all the time: every 2 s is soon enough.
        if wants_spots.load(Ordering::Relaxed) && spots_sent.is_none_or(|at| at.elapsed() > Duration::from_secs(2)) {
            if let Some(share) = spots.share(&mut spots_seen) {
                spots_sent = Some(Instant::now());
                if let Err(e) = scope_msg::spots(&share).iter().try_for_each(|m| out.write_all(m)) {
                    break Err(e);
                }
            }
        }
        let row = match &sub {
            Some(s) => s.rows.recv_timeout(Duration::from_millis(100)).ok(),
            None => {
                thread::sleep(Duration::from_millis(100));
                None
            }
        };
        if let Some(r) = row {
            if let Err(e) = out.write_all(&scope_msg::row(&r)) {
                break Err(e);
            }
        }
        while let Ok((id, lo, hi)) = ask_rx.try_recv() {
            asked.push((id, scope.ask_detail(lo, hi), Instant::now()));
        }
        let mut failed = None;
        asked.retain(|(id, rx, at)| match rx.try_recv() {
            Ok(d) => {
                if let Err(e) = out.write_all(&scope_msg::detail(*id, &d)) {
                    failed = Some(e);
                }
                false
            }
            Err(_) => at.elapsed() < Duration::from_secs(2),
        });
        if let Some(e) = failed {
            break Err(e);
        }
    };
    done.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result
}

/// Shows a remote radio's band scope: keeps a `SCOPE` session to the server
/// open, asks for the view of whoever watches `tap`, and passes the rows and
/// the server's scope status on to `tap`, and the server's logger spots on to
/// `spots`.
pub struct ScopeClient {
    stop: Arc<AtomicBool>,
}

impl ScopeClient {
    pub fn start(target: RemoteTarget, tap: ScopeTap, spots: SpotBook) -> ScopeClient {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        thread::Builder::new()
            .name("remote-scope".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    let wait = match scope_session(&target, &tap, &spots, &stop2) {
                        Ok(()) => Duration::from_secs(1),
                        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                            tap.set_status(ScopeStatus { text: "the remote radio's RigHelm is too old for the band scope".into(), ..Default::default() });
                            Duration::from_secs(30)
                        }
                        Err(e) => {
                            tap.set_status(ScopeStatus { text: format!("remote band scope: {e}"), ..Default::default() });
                            Duration::from_secs(3)
                        }
                    };
                    // Out of touch: they would go stale.
                    if !stop2.load(Ordering::Relaxed) {
                        spots.clear();
                    }
                    let t0 = Instant::now();
                    while t0.elapsed() < wait && !stop2.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                tap.set_status(ScopeStatus::default());
            })
            .expect("spawn scope client");
        ScopeClient { stop }
    }
}

impl Drop for ScopeClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn scope_session(target: &RemoteTarget, tap: &ScopeTap, spots: &SpotBook, stop: &AtomicBool) -> io::Result<()> {
    let (stream, _name, _, _, _) = match handshake(&target.addr, &target.password, "SCOPE") {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && e.to_string().contains("unsupported mode") => {
            return Err(io::Error::new(io::ErrorKind::Unsupported, e.to_string()));
        }
        r => r?,
    };
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    writer.write_all(b"SPOTS\n")?;
    let mut incoming = SpotShare::default();
    let mut sent_view: Option<ScopeView> = None;
    let mut asked: HashMap<u32, crossbeam_channel::Sender<Detail>> = HashMap::new();
    let mut next_id: u32 = 1;
    let mut last_beat = Instant::now() - Duration::from_secs(5);
    let mut last_heard = Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let view = tap.first_view();
        if view != sent_view {
            let line = match &view {
                Some(v) => scope_msg::view_line(v),
                None => "IDLE\n".to_string(),
            };
            writer.write_all(line.as_bytes())?;
            sent_view = view;
            last_beat = Instant::now();
        } else if last_beat.elapsed() > Duration::from_secs(1) {
            writer.write_all(b"\n")?;
            last_beat = Instant::now();
        }
        for ask in tap.take_asks() {
            next_id = next_id.wrapping_add(1);
            writer.write_all(scope_msg::detail_line(next_id, ask.lo, ask.hi).as_bytes())?;
            if asked.len() > 16 {
                asked.clear();
            }
            asked.insert(next_id, ask.reply);
        }
        for (hz, call) in spots.take_calls() {
            writer.write_all(scope_msg::call_line(hz, &call).as_bytes())?;
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed the connection")),
            Ok(n) => {
                last_heard = Instant::now();
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
        // The server sends its status every 2 s at least.
        if last_heard.elapsed() > Duration::from_secs(6) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "server went silent"));
        }
        while buf.len() >= 5 {
            let len = u32::from_le_bytes(buf[1..5].try_into().expect("4 bytes")) as usize;
            if len > scope_msg::MAX_LEN {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized band scope message"));
            }
            if buf.len() < 5 + len {
                break;
            }
            let payload = &buf[5..5 + len];
            match buf[0] {
                scope_msg::ROW => {
                    if let Some(row) = scope_msg::parse_row(payload) {
                        tap.publish(&row);
                    }
                }
                scope_msg::STATUS => {
                    if let Some(s) = scope_msg::parse_status(payload) {
                        tap.set_status(s);
                    }
                }
                scope_msg::DETAIL => {
                    if let Some((id, d)) = scope_msg::parse_detail(payload) {
                        if let Some(reply) = asked.remove(&id) {
                            let _ = reply.try_send(d);
                        }
                    }
                }
                scope_msg::SPOTS => {
                    if let Some((first, last, part)) = scope_msg::parse_spots(payload) {
                        if first {
                            incoming.spots.clear();
                        }
                        incoming.spots.extend(part.spots);
                        if last {
                            incoming.click_sets_mode = part.click_sets_mode;
                            incoming.send_call = part.send_call;
                            spots.mirror(std::mem::take(&mut incoming));
                        }
                    }
                }
                _ => {}
            }
            buf.drain(..5 + len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::sim::SimRadio;

    #[test]
    fn restarts_on_the_same_port_immediately() {
        let rig = Rig::start();
        let cfg = RemoteServerConfig { bind: "127.0.0.1:0".into(), audio: true, rx_device: Some(AudioDevice::Alsa("null".into())), ..Default::default() };
        let first = RemoteServer::start(rig.clone(), cfg.clone(), ScopeTap::default(), SpotBook::default()).unwrap();
        let addr = first.addr;
        drop(first);
        let again = RemoteServer::start(rig.clone(), RemoteServerConfig { bind: addr.to_string(), ..cfg }, ScopeTap::default(), SpotBook::default());
        assert!(again.is_ok(), "{:?}", again.err());
        rig.shutdown();
    }

    #[test]
    fn beacon_round_trip() {
        let cfg = RemoteServerConfig { name: "shack".into(), password: "x".into(), ..Default::default() };
        let text = beacon_text(&cfg, 4533, "connected", &FTDX3000);
        let f = parse_beacon(&text, "192.168.1.5:4534".parse().unwrap()).unwrap();
        assert_eq!(f.name, "shack");
        assert_eq!(f.addr, "192.168.1.5:4533".parse().unwrap());
        assert!(f.auth);
        assert!(f.audio);
        assert_eq!(f.radio, "connected");
        assert_eq!(f.id, instance_id());
    }

    #[test]
    fn packets() {
        let p = packet(PKT_RX, 42, 7, &[1, -2, 3]);
        assert_eq!(parse_packet(&p), Some((PKT_RX, 42, 7, vec![1, -2, 3])));
        assert_eq!(parse_packet(b"nope"), None);
    }

    fn sim_rig() -> Rig {
        let sim = SimRadio::new();
        let rig = Rig::start();
        rig.connect_link(sim.link(), EngineConfig { read_menu_on_connect: false, ..Default::default() });
        rig
    }

    #[test]
    fn remote_control_end_to_end() {
        let server_rig = sim_rig();
        let cfg = RemoteServerConfig { bind: "127.0.0.1:0".into(), password: "secret".into(), audio: false, ..Default::default() };
        let server = RemoteServer::start(server_rig.clone(), cfg, ScopeTap::default(), SpotBook::default()).unwrap();
        let addr = server.addr.to_string();

        // Wrong password is refused.
        let bad = connect_cat(&RemoteTarget { addr: addr.clone(), password: "nope".into() });
        assert_eq!(bad.err().map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));

        // A client engine drives the server's radio.
        let client = Rig::start();
        let link = connect_cat(&RemoteTarget { addr: addr.clone(), password: "secret".into() }).unwrap().0;
        client.connect_link(link, EngineConfig { read_menu_on_connect: false, ..Default::default() });
        let fa = client.read("FA;", crate::Freshness::Radio, Source::Gui, Duration::from_secs(3)).unwrap();
        assert_eq!(fa, "FA14074000;");
        client.send("FA07030000;");
        let t0 = Instant::now();
        while server_rig.state().freq_a != 7_030_000 && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(server_rig.state().freq_a, 7_030_000);
        // Front-panel changes on the server reach the client (auto information).
        server_rig.send("MD03;");
        let t0 = Instant::now();
        while client.state().mode_a != Some(ftdx3000_cat::Mode::Cw) && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(client.state().mode_a, Some(ftdx3000_cat::Mode::Cw));
        client.shutdown();
        server_rig.shutdown();
    }

    #[test]
    fn memory_channels_reach_remote_clients() {
        let server_rig = sim_rig();
        let cfg = RemoteServerConfig { bind: "127.0.0.1:0".into(), password: "secret".into(), audio: false, ..Default::default() };
        let server = RemoteServer::start(server_rig.clone(), cfg, ScopeTap::default(), SpotBook::default()).unwrap();
        let client = Rig::start();
        let link = connect_cat(&RemoteTarget { addr: server.addr.to_string(), password: "secret".into() }).unwrap().0;
        client.connect_link(link, EngineConfig { read_menu_on_connect: false, ..Default::default() });
        let t0 = Instant::now();
        while !client.link().is_open() && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        let cmds: Vec<String> = client.model().caps.memory.channels().map(|c| format!("MR{c:03};")).collect();
        let refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        client.refresh(&refs);
        let t0 = Instant::now();
        while client.state().memories.len() < 2 && t0.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(client.state().memories.get(&1).map(|m| m.freq), Some(7_074_000));
        assert_eq!(client.state().memories.get(&2).map(|m| m.freq), Some(14_074_000));
        client.shutdown();
        server_rig.shutdown();
    }

    #[test]
    fn meters_are_pushed_to_remote_clients() {
        let server_rig = sim_rig();
        let cfg = RemoteServerConfig { bind: "127.0.0.1:0".into(), password: "secret".into(), audio: false, ..Default::default() };
        let server = RemoteServer::start(server_rig.clone(), cfg, ScopeTap::default(), SpotBook::default()).unwrap();
        let (mut link, _) = connect_cat(&RemoteTarget { addr: server.addr.to_string(), password: "secret".into() }).unwrap();
        link.writer.write_all(b"AI1;").unwrap();
        server_rig.send("TX1;");
        // Without asking for them, power-out readings arrive.
        let mut framer = ftdx3000_cat::Framer::new();
        let mut buf = [0u8; 512];
        let t0 = Instant::now();
        let mut po = 0;
        while po < 3 && t0.elapsed() < Duration::from_secs(5) {
            if let Ok(n) = link.reader.read(&mut buf) {
                po += framer.push(&buf[..n]).iter().filter(|f| f.starts_with("RM5")).count();
            }
        }
        server_rig.send("TX0;");
        assert!(po >= 3, "only {po} PO readings pushed");
        server_rig.shutdown();
    }

    #[test]
    fn audio_streams_end_to_end() {
        // ALSA's "null" device captures silence and swallows playback, so the
        // whole pipeline runs without touching real sound hardware.
        if !std::path::Path::new("/usr/bin/arecord").exists() {
            return;
        }
        let server_rig = sim_rig();
        let null = AudioDevice::Alsa("null".into());
        let cfg = RemoteServerConfig {
            bind: "127.0.0.1:0".into(),
            audio: true,
            rx_device: Some(null.clone()),
            tx_device: Some(null.clone()),
            ..Default::default()
        };
        // Transmit (including mic audio) needs a password on the server.
        let cfg = RemoteServerConfig { tx_audio: true, password: "pw".into(), ..cfg };
        let server = RemoteServer::start(server_rig.clone(), cfg, ScopeTap::default(), SpotBook::default()).unwrap();
        let target = RemoteTarget { addr: server.addr.to_string(), password: "pw".into() };
        // The client mirrors the remote radio's state over its own CAT session.
        let client_rig = Rig::start();
        client_rig.connect_link(connect_cat(&target).unwrap().0, EngineConfig { read_menu_on_connect: false, ..Default::default() });
        let client = AudioClient::start(
            client_rig.clone(),
            target,
            RemoteClientConfig { speaker: null.clone(), mic: null, send_mic: true, ..Default::default() },
            None,
        );
        let t0 = Instant::now();
        while client.status().packets < 25 && t0.elapsed() < Duration::from_secs(6) {
            thread::sleep(Duration::from_millis(50));
        }
        let st = client.status();
        assert!(st.connected, "{st:?}");
        assert!(st.packets >= 25, "only {} audio packets in 6 s: {st:?}", st.packets);
        // Paced to real time: ~50 packets/s, not a flood.
        let rate = st.packets as f64 / t0.elapsed().as_secs_f64();
        assert!(rate < 80.0, "{rate:.0} packets/s");
        assert_eq!(server.status().audio_clients.len(), 1);

        // Microphone audio reaches the radio only while transmitting.
        thread::sleep(Duration::from_millis(300));
        assert_eq!(server.status().tx_audio_packets, 0, "mic must not be sent while receiving");
        client_rig.send("TX1;");
        let t0 = Instant::now();
        while server.status().tx_audio_packets < 10 && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(server.status().tx_audio_packets >= 10, "{:?}", server.status());
        client_rig.send("TX0;");
        drop(client);
        server_rig.shutdown();
        client_rig.shutdown();
    }

    #[test]
    fn open_server_is_receive_only_and_unkeys_lost_clients() {
        let sim = SimRadio::new();
        let server_rig = Rig::start();
        server_rig.connect_link(sim.link(), EngineConfig { read_menu_on_connect: false, ..Default::default() });
        // No password: tuning works, transmitting doesn't.
        let open = RemoteServer::start(server_rig.clone(), RemoteServerConfig { bind: "127.0.0.1:0".into(), audio: false, ..Default::default() }, ScopeTap::default(), SpotBook::default()).unwrap();
        let mut c = TcpStream::connect(open.addr).unwrap();
        let mut line = String::new();
        BufReader::new(c.try_clone().unwrap()).read_line(&mut line).unwrap();
        c.write_all(b"HELLO CAT -\nFA07020000;TX1;AC002;").unwrap();
        thread::sleep(Duration::from_millis(400));
        assert_eq!(sim.core().freq_a, 7_020_000);
        assert_eq!(sim.core().tx, 0);
        drop(c);

        // With a password, a client that keys and then vanishes is un-keyed.
        let secured = RemoteServer::start(
            server_rig.clone(),
            RemoteServerConfig { bind: "127.0.0.1:0".into(), audio: false, password: "pw".into(), ..Default::default() },
            ScopeTap::default(),
            SpotBook::default(),
        )
        .unwrap();
        let link = connect_cat(&RemoteTarget { addr: secured.addr.to_string(), password: "pw".into() }).unwrap().0;
        let mut w = link.writer;
        w.write_all(b"TX1;").unwrap();
        let t0 = Instant::now();
        while sim.core().tx != 1 && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(sim.core().tx, 1);
        drop(w);
        drop(link.reader);
        let t0 = Instant::now();
        while sim.core().tx != 0 && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(sim.core().tx, 0, "radio left transmitting after the remote client vanished");
        server_rig.shutdown();
    }

    #[test]
    fn scope_messages_round_trip() {
        let row = ScopeRow { start_hz: 14_000_000.0, col_hz: 97.5, cols: vec![0, 1, 200, 255], transmitting: true };
        let m = scope_msg::row(&row);
        assert_eq!(m[0], scope_msg::ROW);
        assert_eq!(scope_msg::parse_row(&m[5..]), Some(row));
        let st = ScopeStatus { present: true, running: true, text: "RTL-SDR on IF OUT".into(), covers: Some((13.0e6, 15.0e6)), feed: Some(Feed::IfOut), dongles: vec![] };
        let m = scope_msg::status(&st);
        assert_eq!(scope_msg::parse_status(&m[5..]), Some(st));
        let v = ScopeView { center_hz: 7_100_000.0, span_hz: 50_000.0, cols: 800, fps: 10 };
        assert_eq!(scope_msg::parse_view(&scope_msg::view_line(&v)), Some(v));
        assert_eq!(scope_msg::parse_view("VIEW nan"), None);
        let d = Detail { start_hz: 7_000_000.0, bin_hz: 31.25, db: vec![-80.0, -20.5] };
        let m = scope_msg::detail(9, &d);
        assert_eq!(m[0], scope_msg::DETAIL);
        assert_eq!(scope_msg::parse_detail(&m[5..]), Some((9, d)));
        assert_eq!(scope_msg::parse_detail_line(&scope_msg::detail_line(3, 7.0e6, 7.01e6)), Some((3, 7.0e6, 7.01e6)));
        assert_eq!(scope_msg::parse_row(&[1, 2, 3]), None);
    }

    fn test_spot(call: String, hz: u64) -> SharedSpot {
        SharedSpot {
            call,
            hz,
            mode: crate::logger::SpotMode::Cw,
            mode_text: "CW".into(),
            status: crate::logger::SpotStatus::Other,
            comment: "x".repeat(200),
            spotter: "K5KRK".into(),
            country: String::new(),
            rules: Vec::new(),
            source: "QLog".into(),
            age_s: 90,
        }
    }

    #[test]
    fn a_long_spot_list_splits_into_small_messages() {
        let share = SpotShare { click_sets_mode: true, send_call: false, spots: (0..2000).map(|i| test_spot(format!("K{i}"), 14_000_000 + i)).collect() };
        let parts = scope_msg::spots(&share);
        assert!(parts.len() > 1);
        let mut got = Vec::new();
        for (i, m) in parts.iter().enumerate() {
            assert_eq!(m[0], scope_msg::SPOTS);
            assert!(m.len() - 5 <= scope_msg::MAX_LEN);
            let (first, last, part) = scope_msg::parse_spots(&m[5..]).unwrap();
            assert_eq!((first, last), (i == 0, i == parts.len() - 1));
            assert!(part.click_sets_mode && !part.send_call);
            got.extend(part.spots);
        }
        assert_eq!(got, share.spots);
        // An empty list still says so.
        let empty = scope_msg::spots(&SpotShare::default());
        assert_eq!(empty.len(), 1);
        assert_eq!(scope_msg::parse_spots(&empty[0][5..]), Some((true, true, SpotShare::default())));
    }

    #[test]
    fn call_lines_round_trip() {
        assert_eq!(scope_msg::parse_call_line(&scope_msg::call_line(14_074_000, "K1ABC/P")), Some((14_074_000, "K1ABC/P")));
        assert_eq!(scope_msg::parse_call_line("SPOTCALL nope K1ABC\n"), None);
    }

    #[test]
    fn a_remote_client_shows_the_servers_spots() {
        let server_rig = sim_rig();
        let (server_spots, client_spots) = (SpotBook::default(), SpotBook::default());
        let cfg = crate::logger::LoggerConfig { enabled: true, ..Default::default() };
        server_spots.set_config(&cfg);
        let listener = crate::logger::SpotListener::start(server_spots.clone(), crate::logger::LoggerConfig { port: 0, ..cfg.clone() }).unwrap();
        let logger = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        logger.send_to(br#"<spot><app>N1MM</app><dxcall>W1AW</dxcall><frequency>14074.0</frequency><action>add</action></spot>"#, listener.addr).unwrap();
        let t0 = Instant::now();
        while server_spots.stats().held == 0 && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        let server = RemoteServer::start(
            server_rig.clone(),
            RemoteServerConfig { bind: "127.0.0.1:0".into(), audio: false, password: "pw".into(), ..Default::default() },
            ScopeTap::default(),
            server_spots.clone(),
        )
        .unwrap();
        client_spots.set_mirror(true);
        let _client = ScopeClient::start(RemoteTarget { addr: server.addr.to_string(), password: "pw".into() }, ScopeTap::default(), client_spots.clone());
        let off = crate::logger::LoggerConfig::default();
        let t0 = Instant::now();
        while client_spots.on_scope(&off, 14e6, 14.35e6).is_empty() && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(client_spots.on_scope(&off, 14e6, 14.35e6)[0].call, "W1AW");
        // Clearing them on the radio's computer clears them everywhere.
        server_spots.clear();
        let t0 = Instant::now();
        while !client_spots.on_scope(&off, 14e6, 14.35e6).is_empty() && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(client_spots.on_scope(&off, 14e6, 14.35e6).is_empty());
    }

    #[test]
    fn band_scope_streams_to_a_remote_client() {
        let server_rig = sim_rig();
        let server_tap = ScopeTap::default();
        let server = RemoteServer::start(
            server_rig.clone(),
            RemoteServerConfig { bind: "127.0.0.1:0".into(), audio: false, password: "pw".into(), ..Default::default() },
            server_tap.clone(),
            SpotBook::default(),
        )
        .unwrap();
        // Stand-in for the dongle: rows for whoever watches.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (tap, stop) = (server_tap.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    tap.set_status(ScopeStatus { present: true, running: true, text: "test dongle".into(), ..Default::default() });
                    if let Some(v) = tap.first_view() {
                        tap.publish(&ScopeRow { start_hz: v.start_hz(), col_hz: v.span_hz / v.cols as f64, cols: vec![100; v.cols as usize], transmitting: false });
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            });
        }
        let client_tap = ScopeTap::default();
        let _client = ScopeClient::start(RemoteTarget { addr: server.addr.to_string(), password: "pw".into() }, client_tap.clone(), SpotBook::default());
        let view = ScopeView { center_hz: 7_100_000.0, span_hz: 50_000.0, cols: 500, fps: 20 };
        let sub = client_tap.subscribe(view);
        let row = sub.rows.recv_timeout(Duration::from_secs(5)).expect("a row from the server");
        assert_eq!(row.cols.len(), 500);
        assert_eq!(row.start_hz, 7_075_000.0);
        assert_eq!(server_tap.first_view(), Some(view));
        // A detail ask goes to the server's scope and the answer comes back.
        let answer = client_tap.ask_detail(7_090_000.0, 7_092_000.0);
        let t0 = Instant::now();
        let mut answered = false;
        while t0.elapsed() < Duration::from_secs(3) && !answered {
            for ask in server_tap.take_asks() {
                assert_eq!((ask.lo, ask.hi), (7_090_000.0, 7_092_000.0));
                let _ = ask.reply.try_send(Detail { start_hz: ask.lo, bin_hz: 31.25, db: vec![-50.0; 64] });
                answered = true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let d = answer.recv_timeout(Duration::from_secs(3)).expect("detail from the server");
        assert_eq!((d.start_hz, d.db.len()), (7_090_000.0, 64));
        assert_eq!(client_tap.status().text, "test dongle", "{:?}", client_tap.status());
        // Nobody watching on the client: the server stops watching too.
        drop(sub);
        let t0 = Instant::now();
        while server_tap.has_watchers() && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!server_tap.has_watchers());
        // A wrong password gets nothing.
        let other = ScopeTap::default();
        let _bad = ScopeClient::start(RemoteTarget { addr: server.addr.to_string(), password: "nope".into() }, other.clone(), SpotBook::default());
        let _sub = other.subscribe(view);
        thread::sleep(Duration::from_millis(800));
        assert!(!other.status().present);
        stop.store(true, Ordering::Relaxed);
    }

}
