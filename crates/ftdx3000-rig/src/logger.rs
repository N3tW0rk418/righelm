//! Logger Connect: DX spots from logging programs, for the band scope.
//!
//! Listens on one UDP port and takes both formats loggers send, told apart by
//! their first character:
//!   - QLog's network notifications: JSON, `dxspot` and `spotalert` messages,
//!     frequency in MHz. They come before QLog's own filters, so this is the
//!     whole cluster feed; `SpotBook` dedupes and expires it.
//!   - The N1MM+ `<spot>` broadcast, also sent by DXLog and Logger32: XML,
//!     frequency in kHz, `action` add or delete.
//!
//! Spots only ever draw markers: nothing here touches the radio. The way back
//! to the logger is `SpotBook::send_call`: a spot clicked on the band scope can
//! fill in the logger's contact form, with the status packet WSJT-X sends when
//! you pick a station, which QLog and others listen for.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ftdx3000_cat::types::Mode;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The port N1MM+ recommends for its broadcasts.
pub const DEFAULT_PORT: u16 = 12060;
/// Where loggers listen for WSJT-X.
pub const DEFAULT_SEND_TO: &str = "127.0.0.1:2237";
/// Spots kept at most; the oldest go first (a contest weekend's cluster).
const MAX_SPOTS: usize = 2000;
/// The same call closer than this is the same spot (it moved within the
/// band). Wider than any band, narrower than the gap between any two.
const SAME_BAND_HZ: u64 = 500_000;
/// An alert stays on its spot when the call is spotted again this close.
const SAME_SIGNAL_HZ: u64 = 1_000;
const MAX_CALL: usize = 20;
const MAX_TEXT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggerConfig {
    pub enabled: bool,
    /// Listen to other computers on the LAN too, not only this one.
    pub lan: bool,
    pub port: u16,
    pub accept_qlog: bool,
    pub accept_n1mm: bool,
    pub show_on_scope: bool,
    /// Callsign labels; off leaves only the tick marks.
    pub show_labels: bool,
    /// Rows of labels above the spectrum at most.
    pub label_rows: u32,
    /// Forget a spot this long after it was last heard of.
    pub max_age_min: u32,
    pub show_cw: bool,
    pub show_phone: bool,
    pub show_digital: bool,
    /// The Show list: a spot appears if anything about it is ticked, so
    /// ticking more can only add spots.
    pub show_alerts: bool,
    pub show_pota: bool,
    pub show_new: bool,
    pub show_new_slot: bool,
    pub show_worked: bool,
    pub show_other: bool,
    /// Clicking a spot sets the radio's mode as well as its frequency.
    pub click_sets_mode: bool,
    /// Clicking a spot also sends its call to the logger's contact form,
    /// replacing what's there.
    pub send_call: bool,
    /// The logger's WSJT-X port, `address:port`.
    pub send_to: String,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lan: false,
            port: DEFAULT_PORT,
            accept_qlog: true,
            accept_n1mm: true,
            show_on_scope: true,
            show_labels: true,
            label_rows: 3,
            max_age_min: 30,
            show_cw: true,
            show_phone: true,
            show_digital: true,
            show_alerts: true,
            show_pota: true,
            show_new: true,
            show_new_slot: true,
            show_worked: true,
            show_other: true,
            click_sets_mode: true,
            send_call: false,
            send_to: DEFAULT_SEND_TO.into(),
        }
    }
}

impl LoggerConfig {
    /// Whether the filters let a spot through: its mode, and the Show list.
    /// A mode nobody named always passes: there's nothing to judge it by.
    pub fn shows(&self, s: &Spot) -> bool {
        let mode = match s.mode {
            SpotMode::Cw => self.show_cw,
            SpotMode::Phone => self.show_phone,
            SpotMode::Digital => self.show_digital,
            SpotMode::Unknown => true,
        };
        let status = match s.status {
            SpotStatus::NewEntity => self.show_new,
            SpotStatus::NewSlot => self.show_new_slot,
            SpotStatus::Worked => self.show_worked,
            SpotStatus::Other => self.show_other,
        };
        mode && (status || (self.show_alerts && s.alert()) || (self.show_pota && s.pota()))
    }

    /// Set the whole Show list, for its All and None buttons.
    pub fn show_all(&mut self, on: bool) {
        (self.show_alerts, self.show_pota, self.show_new, self.show_new_slot, self.show_worked, self.show_other) = (on, on, on, on, on, on);
    }

    /// What the listener depends on: a change to this restarts it.
    fn listener(&self) -> (bool, bool, u16, bool, bool) {
        (self.enabled, self.lan, self.port, self.accept_qlog, self.accept_n1mm)
    }

    fn addr(&self) -> SocketAddr {
        let ip = if self.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
        SocketAddr::from((ip, self.port))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotMode {
    Cw,
    Phone,
    Digital,
    Unknown,
}

impl SpotMode {
    fn parse(text: &str) -> SpotMode {
        match text.to_ascii_uppercase().as_str() {
            "" | "UNKNOWN" | "NONE" => SpotMode::Unknown,
            "CW" => SpotMode::Cw,
            "PHONE" | "SSB" | "USB" | "LSB" | "AM" | "FM" => SpotMode::Phone,
            _ => SpotMode::Digital,
        }
    }
}

/// What the logger thinks of a spot, as one scale for both formats: QLog
/// judges it against your log, N1MM against the contest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpotStatus {
    Worked,
    Other,
    NewSlot,
    NewEntity,
}

impl SpotStatus {
    fn parse(text: &str) -> SpotStatus {
        match text.to_ascii_lowercase().as_str() {
            "newentity" | "mult" | "single mult" | "double mult" => SpotStatus::NewEntity,
            "newbandmode" | "newband" | "newmode" | "newslot" | "new qso" | "new" => SpotStatus::NewSlot,
            "worked" | "confirmed" | "dupe" => SpotStatus::Worked,
            _ => SpotStatus::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spot {
    pub call: String,
    pub hz: u64,
    pub mode: SpotMode,
    /// The mode as the logger gave it.
    pub mode_text: String,
    pub status: SpotStatus,
    pub comment: String,
    pub spotter: String,
    pub country: String,
    /// The logger's alert rules this spot matched.
    pub rules: Vec<String>,
    /// The program it came from.
    pub source: String,
    /// When it was last heard of.
    pub seen: Instant,
}

impl Spot {
    pub fn alert(&self) -> bool {
        !self.rules.is_empty()
    }

    /// A Parks on the Air activator: the comment says POTA or gives a park.
    pub fn pota(&self) -> bool {
        self.comment.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case("POTA")) || self.park().is_some()
    }

    /// The park reference in the comment, like `US-2303`. Four or five
    /// digits: islands (NA-001) have three, and WWFF prefixes (KFF-1234) are
    /// longer than a country's two characters.
    pub fn park(&self) -> Option<&str> {
        self.comment.split(|c: char| !(c.is_alphanumeric() || c == '-')).find(|w| {
            w.split_once('-').is_some_and(|(prefix, num)| {
                (1..=2).contains(&prefix.len())
                    && prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                    && (4..=5).contains(&num.len())
                    && num.chars().all(|c| c.is_ascii_digit())
            })
        })
    }

    /// The radio mode to work this spot in, if the logger's mode says.
    pub fn radio_mode(&self) -> Option<Mode> {
        let text = self.mode_text.to_ascii_uppercase();
        Some(match self.mode {
            SpotMode::Unknown => return None,
            SpotMode::Cw => Mode::Cw,
            SpotMode::Phone if text == "AM" => Mode::Am,
            SpotMode::Phone if text == "FM" => Mode::Fm,
            // LSB below 10 MHz, except 60 m.
            SpotMode::Phone if self.hz < 10_000_000 && !(5_000_000..5_500_000).contains(&self.hz) => Mode::Lsb,
            SpotMode::Phone => Mode::Usb,
            SpotMode::Digital if text.starts_with("RTTY") => Mode::RttyLsb,
            SpotMode::Digital => Mode::DataUsb,
        })
    }
}

/// One packet, understood.
#[derive(Debug, Clone, PartialEq)]
enum Packet {
    Spot(Spot),
    Delete { call: String, hz: u64 },
    /// From a logger, but nothing about spots (QSOs, solar data, radio info).
    Other { source: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Qlog,
    N1mm,
}

impl Format {
    fn of(data: &[u8]) -> Option<Format> {
        match data.iter().find(|b| !b.is_ascii_whitespace())? {
            b'{' => Some(Format::Qlog),
            b'<' => Some(Format::N1mm),
            _ => None,
        }
    }
}

fn parse(format: Format, data: &[u8]) -> Result<Packet, String> {
    let text = std::str::from_utf8(data).map_err(|_| "not text".to_string())?;
    match format {
        Format::Qlog => parse_qlog(text),
        Format::N1mm => parse_n1mm(text),
    }
}

fn clipped(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

fn call_of(text: &str) -> Result<String, String> {
    let call = clipped(text, MAX_CALL).to_ascii_uppercase();
    if call.is_empty() { Err("spot without a callsign".into()) } else { Ok(call) }
}

fn hz_of(value: f64, scale: f64) -> Result<u64, String> {
    let hz = value * scale;
    // Round to the Hz: 7.188 MHz in floating point isn't quite 7188000.
    if (30_000.0..=300_000_000_000.0).contains(&hz) { Ok(hz.round() as u64) } else { Err(format!("frequency {value} out of range")) }
}

fn parse_qlog(text: &str) -> Result<Packet, String> {
    use serde_json::Value;
    let msg: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    let source = msg["appid"].as_str().unwrap_or("QLog");
    let kind = msg["msgtype"].as_str().ok_or("no msgtype")?;
    if !matches!(kind, "dxspot" | "spotalert") {
        return Ok(Packet::Other { source: clipped(source, MAX_CALL) });
    }
    let d = &msg["data"];
    let s = |v: &Value| clipped(v.as_str().unwrap_or(""), MAX_TEXT);
    // A number now; older QLogs wrote it as a string.
    let mhz = d["freq"].as_f64().or_else(|| d["freq"].as_str().and_then(|f| f.trim().parse().ok())).ok_or("spot without a frequency")?;
    let mode_text = s(&d["mode"]);
    let rules = d["rules"].as_array().map_or(Vec::new(), |r| r.iter().filter_map(Value::as_str).map(|r| clipped(r, MAX_TEXT)).collect());
    Ok(Packet::Spot(Spot {
        call: call_of(d["dx"]["call"].as_str().unwrap_or(""))?,
        hz: hz_of(mhz, 1e6)?,
        mode: SpotMode::parse(&mode_text),
        mode_text,
        status: SpotStatus::parse(d["status"].as_str().unwrap_or("")),
        comment: s(&d["comment"]),
        spotter: s(&d["spotter"]["call"]),
        country: s(&d["dx"]["country"]),
        rules,
        source: clipped(source, MAX_CALL),
        seen: Instant::now(),
    }))
}

/// The text of `<name>…</name>`. Spot packets are flat, so this is all the
/// XML they need.
fn tag(xml: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let len = xml[start..].find(&format!("</{name}>"))?;
    let text = &xml[start..start + len];
    Some(text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"))
}

fn parse_n1mm(text: &str) -> Result<Packet, String> {
    let source = clipped(&tag(text, "app").unwrap_or_default(), MAX_CALL);
    let source = if source.is_empty() { "N1MM format".to_string() } else { source };
    if !text.contains("<spot>") {
        return Ok(Packet::Other { source });
    }
    let call = call_of(&tag(text, "dxcall").unwrap_or_default())?;
    let khz: f64 = tag(text, "frequency").and_then(|f| f.trim().parse().ok()).ok_or("spot without a frequency")?;
    let hz = hz_of(khz, 1e3)?;
    if tag(text, "action").is_some_and(|a| a.trim().eq_ignore_ascii_case("delete")) {
        return Ok(Packet::Delete { call, hz });
    }
    let field = |name: &str| clipped(&tag(text, name).unwrap_or_default(), MAX_TEXT);
    let mode_text = field("mode");
    // Named `status` in N1MM's description, `statusFlag` by some senders.
    let status = tag(text, "status").or_else(|| tag(text, "statusFlag")).unwrap_or_default();
    Ok(Packet::Spot(Spot {
        call,
        hz,
        mode: SpotMode::parse(&mode_text),
        mode_text,
        status: SpotStatus::parse(status.trim()),
        comment: field("comment"),
        spotter: field("spottercall"),
        country: String::new(),
        rules: Vec::new(),
        source,
        seen: Instant::now(),
    }))
}

/// A program spots have come from.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub name: String,
    pub from: SocketAddr,
    pub spots: u64,
    pub last: Instant,
}

/// What has reached the listener, for the Settings page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpotStats {
    pub held: usize,
    pub sources: Vec<Source>,
    /// Understood but not spots.
    pub other: u64,
    /// In a format that's turned off.
    pub refused: u64,
    /// Not understood.
    pub bad: u64,
    pub last_bad: Option<String>,
    /// What became of the last call sent to the logger.
    pub last_sent: Option<Result<String, String>>,
}

#[derive(Default)]
struct Book {
    spots: Vec<Spot>,
    stats: SpotStats,
}

impl Book {
    fn add(&mut self, mut spot: Spot) {
        if let Some(i) = self.spots.iter().position(|s| s.call == spot.call && s.hz.abs_diff(spot.hz) < SAME_BAND_HZ) {
            // The plain spot follows the alert for the same signal, or the
            // other way round: it is still the signal the alert was about.
            let old = self.spots.remove(i);
            if spot.rules.is_empty() && old.hz.abs_diff(spot.hz) < SAME_SIGNAL_HZ {
                spot.rules = old.rules;
            }
        }
        self.spots.push(spot);
        if self.spots.len() > MAX_SPOTS {
            self.spots.remove(0);
        }
    }

    fn delete(&mut self, call: &str, hz: u64) {
        self.spots.retain(|s| !(s.call == call && s.hz.abs_diff(hz) < SAME_BAND_HZ));
    }

    fn count(&mut self, name: &str, from: SocketAddr, spots: u64) {
        let now = Instant::now();
        match self.stats.sources.iter_mut().find(|s| s.name == name) {
            Some(s) => (s.from, s.spots, s.last) = (from, s.spots + spots, now),
            None => self.stats.sources.push(Source { name: name.to_string(), from, spots, last: now }),
        }
    }

    fn receive(&mut self, cfg: &LoggerConfig, data: &[u8], from: SocketAddr) {
        let bad = |b: &mut Book, why: String| {
            b.stats.bad += 1;
            b.stats.last_bad = Some(why);
        };
        let Some(format) = Format::of(data) else { return bad(self, "neither JSON nor XML".into()) };
        if !match format {
            Format::Qlog => cfg.accept_qlog,
            Format::N1mm => cfg.accept_n1mm,
        } {
            self.stats.refused += 1;
            return;
        }
        match parse(format, data) {
            Ok(Packet::Spot(s)) => {
                self.count(&s.source.clone(), from, 1);
                self.add(s);
            }
            Ok(Packet::Delete { call, hz }) => self.delete(&call, hz),
            Ok(Packet::Other { source }) => {
                self.stats.other += 1;
                self.count(&source, from, 0);
            }
            Err(e) => bad(self, e),
        }
    }

    fn expire(&mut self, max_age: Duration) {
        self.spots.retain(|s| s.seen.elapsed() < max_age);
    }
}

/// The spots received so far, shared with the band scope.
#[derive(Clone, Default)]
pub struct SpotBook(Arc<Mutex<Book>>);

impl SpotBook {
    /// The spots between two frequencies that the filters let through.
    pub fn in_view(&self, cfg: &LoggerConfig, lo: f64, hi: f64) -> Vec<Spot> {
        let mut b = self.0.lock();
        b.expire(Duration::from_secs(cfg.max_age_min.max(1) as u64 * 60));
        b.spots.iter().filter(|s| (lo..hi).contains(&(s.hz as f64)) && cfg.shows(s)).cloned().collect()
    }

    pub fn stats(&self) -> SpotStats {
        let b = self.0.lock();
        SpotStats { held: b.spots.len(), ..b.stats.clone() }
    }

    pub fn clear(&self) {
        self.0.lock().spots.clear();
    }

    /// Fill in the logger's contact form with a clicked spot, if set to.
    pub fn send_call(&self, cfg: &LoggerConfig, spot: &Spot) {
        if cfg.send_call {
            self.0.lock().stats.last_sent = Some(send_status(&cfg.send_to, spot).map(|()| spot.call.clone()));
        }
    }
}

/// The name we go by in status packets. A logger's lookup of the call needs a
/// grid, which spots don't have; QLog skips that need for WRITELOG, by name.
const STATUS_ID: &str = "RigHelm-WRITELOG";

/// A WSJT-X status packet (schema 3) saying `call` is the station being worked.
fn status_packet(call: &str, hz: u64, mode: &str) -> Vec<u8> {
    fn text(out: &mut Vec<u8>, s: &str) {
        out.extend((s.len() as u32).to_be_bytes());
        out.extend(s.as_bytes());
    }
    let mut p = Vec::new();
    for word in [0xadbc_cbda_u32, 3, 1] {
        p.extend(word.to_be_bytes());
    }
    text(&mut p, STATUS_ID);
    p.extend(hz.to_be_bytes());
    text(&mut p, mode);
    text(&mut p, call);
    text(&mut p, ""); // report
    text(&mut p, mode); // TX mode
    p.extend([0, 0, 0]); // TX enabled, transmitting, decoding
    p.extend([0; 8]); // RX and TX audio offsets
    for s in ["", "", ""] {
        text(&mut p, s); // our call, our grid, DX grid
    }
    p.push(0); // TX watchdog
    text(&mut p, ""); // submode
    p.extend([0, 0]); // fast mode, special operation
    p.extend([0; 8]); // frequency tolerance, T/R period
    text(&mut p, ""); // configuration
    text(&mut p, ""); // TX message
    p
}

/// First no call, then the call: QLog only takes a changed call from a sender
/// it has heard before, and clicking the same spot twice has to work too.
fn send_status(to: &str, spot: &Spot) -> Result<(), String> {
    let addr: SocketAddr = to.trim().parse().map_err(|_| format!("{to} isn't address:port"))?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    let mode = match spot.mode {
        SpotMode::Phone => "SSB",
        _ => spot.mode_text.as_str(),
    };
    for call in ["", spot.call.as_str()] {
        socket.send_to(&status_packet(call, spot.hz, mode), addr).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The running listener; it stops when dropped.
pub struct SpotListener {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SpotListener {
    pub fn start(book: SpotBook, cfg: LoggerConfig) -> std::io::Result<SpotListener> {
        let socket = UdpSocket::bind(cfg.addr())?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = thread::Builder::new().name("logger-connect".into()).spawn(move || {
            let mut buf = vec![0u8; 65536];
            while !stop2.load(Ordering::Relaxed) {
                // Timeouts come back as errors, and there's nothing else to
                // do about one but listen again.
                if let Ok((n, from)) = socket.recv_from(&mut buf) {
                    book.0.lock().receive(&cfg, &buf[..n], from);
                }
            }
        })?;
        Ok(SpotListener { addr, stop, thread: Some(thread) })
    }
}

impl Drop for SpotListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wait, so a restart can bind the same port.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Listener, and the settings it was started with.
#[derive(Default)]
pub struct LoggerConnect {
    running: Option<(LoggerConfig, Result<SpotListener, String>)>,
}

impl LoggerConnect {
    /// Start, stop or restart the listener to match the config.
    pub fn apply(&mut self, book: &SpotBook, cfg: &LoggerConfig) -> Option<&Result<SpotListener, String>> {
        let same = self.running.as_ref().is_some_and(|(c, r)| c.listener() == cfg.listener() && r.is_ok());
        if !same {
            self.running = None;
            if cfg.enabled {
                let r = SpotListener::start(book.clone(), cfg.clone()).map_err(|e| e.to_string());
                self.running = Some((cfg.clone(), r));
                return self.status();
            }
        }
        None
    }

    pub fn status(&self) -> Option<&Result<SpotListener, String>> {
        self.running.as_ref().map(|(_, r)| r)
    }
}

/// Send a test spot to the listener through its own port, at `hz`, so the
/// whole path shows working. It's in a format the settings accept.
pub fn send_test(cfg: &LoggerConfig, hz: u64) -> std::io::Result<()> {
    let text = if cfg.accept_qlog {
        format!(
            r#"{{"appid":"RigHelm test","msgtype":"spotalert","data":{{"freq":{},"mode":"","status":"unknown","comment":"Test spot from Settings (POTA)","rules":["Test"],"dx":{{"call":"TEST"}},"spotter":{{"call":"RIGHELM"}}}}}}"#,
            hz as f64 / 1e6
        )
    } else {
        format!("<spot><app>RigHelm test</app><dxcall>TEST</dxcall><frequency>{}</frequency><spottercall>RIGHELM</spottercall><action>add</action><comment>Test spot from Settings (POTA)</comment></spot>", hz as f64 / 1e3)
    };
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    socket.send_to(text.as_bytes(), (Ipv4Addr::LOCALHOST, cfg.port)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const QLOG: &str = r#"{"appid":"QLog","data":{"band":"40m","comment":"tnx qso","dx":{"call":"yb0ar","cont":"OC","country":"Indonesia","cqz":28,"dxcc":327,"ituz":54,"member":["LoTW"],"pfx":"YB","utcoffset":-7},"freq":7.188,"mode":"PHONE","rcvtime":"20220316 20:04:30","spotter":{"call":"G0DEF"},"status":"newentity"},"logid":"{x}","msgtype":"dxspot","time":1647461070000}"#;
    const N1MM: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<spot>\n\t<app>N1MM</app>\n\t<StationName>K1ABC</StationName>\n\t<dxcall>W1AW</dxcall>\n\t<frequency>7061.2</frequency>\n\t<spottercall>N2XYZ</spottercall>\n\t<timestamp>2026-10-31 12:00:00</timestamp>\n\t<action>add</action>\n\t<mode>CW</mode>\n\t<comment>up 2 &amp; loud</comment>\n\t<status>single mult</status>\n\t<statuslist></statuslist>\n</spot>";

    fn peer() -> SocketAddr {
        "127.0.0.1:50000".parse().unwrap()
    }

    fn spot(call: &str, hz: u64) -> Spot {
        Spot {
            call: call.into(),
            hz,
            mode: SpotMode::Cw,
            mode_text: "CW".into(),
            status: SpotStatus::Other,
            comment: String::new(),
            spotter: String::new(),
            country: String::new(),
            rules: Vec::new(),
            source: "QLog".into(),
            seen: Instant::now(),
        }
    }

    fn only(b: &Book) -> &Spot {
        assert_eq!(b.spots.len(), 1);
        &b.spots[0]
    }

    #[test]
    fn reads_a_qlog_spot() {
        let Ok(Packet::Spot(s)) = parse(Format::Qlog, QLOG.as_bytes()) else { panic!() };
        assert_eq!((s.call.as_str(), s.hz), ("YB0AR", 7_188_000));
        assert_eq!((s.mode, s.status), (SpotMode::Phone, SpotStatus::NewEntity));
        assert_eq!((s.spotter.as_str(), s.country.as_str(), s.comment.as_str()), ("G0DEF", "Indonesia", "tnx qso"));
        assert_eq!(s.source, "QLog");
        assert!(!s.alert());
    }

    #[test]
    fn reads_qlog_frequency_as_a_string_too() {
        let text = QLOG.replace("\"freq\":7.188", "\"freq\":\"7.1880\"");
        let Ok(Packet::Spot(s)) = parse(Format::Qlog, text.as_bytes()) else { panic!() };
        assert_eq!(s.hz, 7_188_000);
    }

    #[test]
    fn reads_a_qlog_alert_with_its_rules() {
        let text = QLOG.replace("\"dxspot\"", "\"spotalert\"").replace("\"status\":", "\"rules\":[\"Pacific\"],\"status\":");
        let Ok(Packet::Spot(s)) = parse(Format::Qlog, text.as_bytes()) else { panic!() };
        assert_eq!(s.rules, ["Pacific"]);
    }

    #[test]
    fn other_qlog_messages_are_not_spots() {
        for kind in ["wcyspot", "wwvspot", "toallspot", "qso", "rigstatus", "wsjtxcqspot"] {
            let text = format!(r#"{{"appid":"QLog","msgtype":"{kind}","data":{{}}}}"#);
            assert_eq!(parse(Format::Qlog, text.as_bytes()), Ok(Packet::Other { source: "QLog".into() }), "{kind}");
        }
    }

    #[test]
    fn reads_an_n1mm_spot() {
        let Ok(Packet::Spot(s)) = parse(Format::N1mm, N1MM.as_bytes()) else { panic!() };
        assert_eq!((s.call.as_str(), s.hz), ("W1AW", 7_061_200));
        assert_eq!((s.mode, s.status), (SpotMode::Cw, SpotStatus::NewEntity));
        assert_eq!((s.spotter.as_str(), s.comment.as_str(), s.source.as_str()), ("N2XYZ", "up 2 & loud", "N1MM"));
    }

    #[test]
    fn n1mm_status_flag_name_works_too() {
        let text = N1MM.replace("<status>single mult</status>", "<statusFlag>dupe</statusFlag>");
        let Ok(Packet::Spot(s)) = parse(Format::N1mm, text.as_bytes()) else { panic!() };
        assert_eq!(s.status, SpotStatus::Worked);
    }

    #[test]
    fn n1mm_delete_removes_the_spot() {
        let mut b = Book::default();
        let cfg = LoggerConfig::default();
        b.receive(&cfg, N1MM.as_bytes(), peer());
        assert_eq!(b.spots.len(), 1);
        b.receive(&cfg, N1MM.replace(">add<", ">delete<").as_bytes(), peer());
        assert!(b.spots.is_empty());
    }

    #[test]
    fn other_n1mm_broadcasts_are_not_spots() {
        let text = "<?xml version=\"1.0\"?><RadioInfo><app>N1MM</app><Freq>1407400</Freq></RadioInfo>";
        assert_eq!(parse(Format::N1mm, text.as_bytes()), Ok(Packet::Other { source: "N1MM".into() }));
    }

    #[test]
    fn bad_packets_are_counted_and_harmless() {
        let mut b = Book::default();
        let cfg = LoggerConfig::default();
        let huge = format!(r#"{{"msgtype":"dxspot","data":{{"freq":1e300,"dx":{{"call":"X"}}}}}}"#);
        for data in ["", "hello", "{not json", "<spot>", r#"{"msgtype":"dxspot","data":{"dx":{"call":"K1A"}}}"#, huge.as_str()] {
            b.receive(&cfg, data.as_bytes(), peer());
        }
        b.receive(&cfg, &[0xff, 0xfe, b'{'], peer());
        assert!(b.spots.is_empty());
        assert_eq!(b.stats.bad, 7);
        assert!(b.stats.last_bad.is_some());
    }

    #[test]
    fn long_fields_are_clipped() {
        let text = QLOG.replace("tnx qso", &"x".repeat(5000));
        let Ok(Packet::Spot(s)) = parse(Format::Qlog, text.as_bytes()) else { panic!() };
        assert_eq!(s.comment.len(), MAX_TEXT);
    }

    #[test]
    fn a_format_turned_off_is_refused() {
        let mut b = Book::default();
        let cfg = LoggerConfig { accept_n1mm: false, ..Default::default() };
        b.receive(&cfg, N1MM.as_bytes(), peer());
        b.receive(&cfg, QLOG.as_bytes(), peer());
        assert_eq!(only(&b).call, "YB0AR");
        assert_eq!(b.stats.refused, 1);
    }

    #[test]
    fn the_same_call_moves_within_a_band() {
        let mut b = Book::default();
        b.add(spot("K1A", 14_010_000));
        b.add(spot("K1A", 14_020_000));
        assert_eq!(only(&b).hz, 14_020_000);
    }

    #[test]
    fn the_same_call_can_be_on_two_bands() {
        let mut b = Book::default();
        b.add(spot("K1A", 7_010_000));
        b.add(spot("K1A", 14_010_000));
        b.add(spot("K1A", 24_900_000));
        b.add(spot("K1A", 28_000_000));
        assert_eq!(b.spots.len(), 4);
    }

    #[test]
    fn an_alert_survives_the_plain_spot_after_it() {
        let mut b = Book::default();
        b.add(Spot { rules: vec!["DX".into()], ..spot("K1A", 14_010_000) });
        b.add(spot("K1A", 14_010_100));
        assert!(only(&b).alert());
        // But not a move to another signal.
        b.add(spot("K1A", 14_030_000));
        assert!(!only(&b).alert());
    }

    #[test]
    fn the_oldest_spot_goes_when_full() {
        let mut b = Book::default();
        for i in 0..MAX_SPOTS + 1 {
            b.add(spot(&format!("K{i}"), 14_000_000));
        }
        assert_eq!(b.spots.len(), MAX_SPOTS);
        assert_eq!(b.spots[0].call, "K1");
    }

    #[test]
    fn old_spots_expire() {
        let book = SpotBook::default();
        let cfg = LoggerConfig { max_age_min: 5, ..Default::default() };
        let Some(old) = Instant::now().checked_sub(Duration::from_secs(301)) else { return };
        book.0.lock().add(Spot { seen: old, ..spot("OLD", 14_010_000) });
        book.0.lock().add(spot("NEW", 14_020_000));
        let shown = book.in_view(&cfg, 14e6, 14.35e6);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].call, "NEW");
        assert_eq!(book.stats().held, 1);
    }

    #[test]
    fn only_spots_in_view_are_shown() {
        let book = SpotBook::default();
        book.0.lock().add(spot("K1A", 7_010_000));
        book.0.lock().add(spot("K2B", 14_010_000));
        assert_eq!(book.in_view(&LoggerConfig::default(), 14e6, 14.35e6).len(), 1);
    }

    #[test]
    fn filters() {
        let on = LoggerConfig::default();
        let s = spot("K1A", 14_010_000);
        assert!(on.shows(&s));
        assert!(!LoggerConfig { show_cw: false, ..on.clone() }.shows(&s));
        assert!(!LoggerConfig { show_other: false, ..on.clone() }.shows(&s));
        let worked = Spot { status: SpotStatus::Worked, ..s.clone() };
        assert!(!LoggerConfig { show_worked: false, ..on.clone() }.shows(&worked));
        assert!(LoggerConfig { show_worked: false, ..on.clone() }.shows(&s));
        // A mode nobody named can't be filtered on.
        assert!(LoggerConfig { show_cw: false, show_phone: false, show_digital: false, ..on }.shows(&Spot { mode: SpotMode::Unknown, ..s }));
    }

    #[test]
    fn pota_spots_are_found_from_the_comment() {
        let s = |comment: &str| Spot { comment: comment.into(), ..spot("K1A", 14_010_000) };
        for c in ["LSB [+] POTA US-2303", "POTA", "pota activation", "#POTA", "US-6854", "Park VE-0123 @ 15z", "K-1234", "4X-0012,"] {
            assert!(s(c).pota(), "{c}");
        }
        assert_eq!(s("LSB [+] POTA US-2303").park(), Some("US-2303"));
        assert_eq!(s("POTA").park(), None);
        for c in ["", "tnx qso", "SOTA W4C/CM-001", "IOTA NA-001", "KFF-1234", "potato", "CQ UP 2", "TNX-1234", "US-12"] {
            assert!(!s(c).pota(), "{c}");
        }
    }

    #[test]
    fn the_show_list_adds_up() {
        let mut none = LoggerConfig::default();
        none.show_all(false);
        let plain = spot("K1A", 14_010_000);
        let alert = Spot { rules: vec!["a".into()], ..plain.clone() };
        let park = Spot { comment: "POTA US-2303".into(), ..plain.clone() };
        let worked_park = Spot { status: SpotStatus::Worked, ..park.clone() };
        assert!(!none.shows(&plain) && !none.shows(&alert) && !none.shows(&park));
        // POTA alone: only parks, worked country or not.
        let pota = LoggerConfig { show_pota: true, ..none.clone() };
        assert!(pota.shows(&park) && pota.shows(&worked_park));
        assert!(!pota.shows(&plain) && !pota.shows(&alert));
        // Two ticks: either kind, not only spots that are both.
        let both = LoggerConfig { show_alerts: true, ..pota.clone() };
        assert!(both.shows(&park) && both.shows(&alert));
        assert!(!both.shows(&plain));
        // The modes still apply.
        assert!(!LoggerConfig { show_cw: false, ..pota }.shows(&park));
    }

    #[test]
    fn status_packets_are_what_wsjtx_sends() {
        let p = status_packet("K1A", 14_074_000, "FT8");
        let mut want: Vec<u8> = vec![0xad, 0xbc, 0xcb, 0xda, 0, 0, 0, 3, 0, 0, 0, 1];
        let text = |w: &mut Vec<u8>, s: &str| {
            w.extend((s.len() as u32).to_be_bytes());
            w.extend(s.as_bytes());
        };
        text(&mut want, "RigHelm-WRITELOG");
        want.extend(14_074_000u64.to_be_bytes());
        for s in ["FT8", "K1A", "", "FT8"] {
            text(&mut want, s);
        }
        want.extend([0u8; 3 + 8]);
        for _ in 0..3 {
            text(&mut want, "");
        }
        want.push(0);
        text(&mut want, "");
        want.extend([0u8; 2 + 8]);
        text(&mut want, "");
        text(&mut want, "");
        assert_eq!(p, want);
    }

    #[test]
    fn a_click_sends_no_call_then_the_call() {
        let logger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        logger.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let book = SpotBook::default();
        let s = spot("K1A", 14_010_000);
        let to = logger.local_addr().unwrap().to_string();
        // Off by default: nothing goes out.
        book.send_call(&LoggerConfig { send_to: to.clone(), ..Default::default() }, &s);
        assert_eq!(book.stats().last_sent, None);
        book.send_call(&LoggerConfig { send_to: to, send_call: true, ..Default::default() }, &s);
        let mut buf = [0u8; 512];
        for call in ["", "K1A"] {
            let n = logger.recv(&mut buf).unwrap();
            assert_eq!(&buf[..n], status_packet(call, 14_010_000, "CW").as_slice());
        }
        assert_eq!(book.stats().last_sent, Some(Ok("K1A".to_string())));
    }

    #[test]
    fn a_bad_address_is_reported() {
        let book = SpotBook::default();
        book.send_call(&LoggerConfig { send_to: "qlog".into(), send_call: true, ..Default::default() }, &spot("K1A", 14_010_000));
        assert!(matches!(book.stats().last_sent, Some(Err(_))));
    }

    #[test]
    fn radio_modes() {
        let m = |text: &str, hz| {
            let s = Spot { mode: SpotMode::parse(text), mode_text: text.into(), ..spot("K1A", hz) };
            s.radio_mode()
        };
        assert_eq!(m("CW", 7_010_000), Some(Mode::Cw));
        assert_eq!(m("PHONE", 7_188_000), Some(Mode::Lsb));
        assert_eq!(m("PHONE", 14_200_000), Some(Mode::Usb));
        assert_eq!(m("PHONE", 5_357_000), Some(Mode::Usb));
        assert_eq!(m("AM", 3_885_000), Some(Mode::Am));
        assert_eq!(m("FT8", 14_074_000), Some(Mode::DataUsb));
        assert_eq!(m("DIGITAL", 14_074_000), Some(Mode::DataUsb));
        assert_eq!(m("RTTY", 14_080_000), Some(Mode::RttyLsb));
        assert_eq!(m("", 14_080_000), None);
    }

    #[test]
    fn listener_receives_over_udp_and_restarts_on_the_same_port() {
        let book = SpotBook::default();
        let free = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let cfg = LoggerConfig { enabled: true, port: free, ..Default::default() };
        let mut lc = LoggerConnect::default();
        assert!(matches!(lc.apply(&book, &cfg), Some(Ok(_))));
        send_test(&cfg, 14_100_000).unwrap();
        send_test(&LoggerConfig { accept_qlog: false, port: free, ..Default::default() }, 14_200_000).unwrap();
        let wait = Instant::now();
        while book.stats().held < 1 && wait.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(50));
        let shown = book.in_view(&cfg, 14e6, 14.35e6);
        assert_eq!(shown.len(), 1, "same call on a band: the later one");
        assert_eq!(shown[0].call, "TEST");
        assert_eq!(book.stats().sources[0].name, "RigHelm test");
        // A changed setting restarts it on the same port.
        let cfg = LoggerConfig { accept_n1mm: false, ..cfg };
        assert!(matches!(lc.apply(&book, &cfg), Some(Ok(_))));
        assert!(lc.apply(&book, &LoggerConfig { enabled: false, ..cfg }).is_none());
        assert!(lc.status().is_none());
    }

    #[test]
    fn a_port_in_use_is_reported() {
        let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let cfg = LoggerConfig { enabled: true, port: held.local_addr().unwrap().port(), ..Default::default() };
        let mut lc = LoggerConnect::default();
        assert!(matches!(lc.apply(&SpotBook::default(), &cfg), Some(Err(_))));
    }
}
