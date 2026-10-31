//! Configuration file and the set of running services (engine, virtual ports,
//! rigctld server), shared by the GUI and the headless daemon.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::AudioDevice;
use crate::cw::{CwConfig, CwKeyer, KeyLine, LocalKeyLine, RemoteKeyLine};
use crate::engine::{EngineConfig, Rig};
use crate::remote::{AudioClient, Discovery, RemoteClientConfig, RemoteServer, RemoteServerConfig, RemoteTarget, ScopeClient};
use crate::rigctld::{RigctldConfig, RigctldServer};
use crate::scope::{CalibrationFix, Scope, ScopeConfig, ScopeStatus, ScopeTap};
use crate::txaudio::{self, MicBridge, MicStatus, TxAudioConfig};
use crate::vport::{VirtualPort, VirtualPortConfig, VirtualPortStatus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: EngineConfig,
    /// Connect automatically at start-up.
    pub auto_connect: bool,
    /// Directory for virtual port links (default `$XDG_RUNTIME_DIR/ftdx3000`).
    pub link_dir: Option<PathBuf>,
    pub virtual_ports: Vec<VirtualPortConfig>,
    pub rigctld: RigctldConfig,
    /// User-defined buttons that send a sequence of CAT commands.
    pub macros: Vec<Macro>,
    /// Let other computers on the LAN control this radio.
    pub remote_server: RemoteServerConfig,
    /// Audio settings used when controlling a remote radio.
    pub remote_client: RemoteClientConfig,
    /// Last remote radio used (address and password), for reconnecting.
    pub last_remote: Option<RemoteTarget>,
    /// Transmit audio: the radio's microphone or the computer's.
    pub tx_audio: TxAudioConfig,
    /// CW keying from this computer (never armed at start-up).
    pub cw: CwConfig,
    /// Band scope from an RTL-SDR dongle on the radio's IF OUT or RX OUT.
    pub scope: ScopeConfig,
    /// Display preferences for the desktop app.
    pub ui: UiPrefs,
}

/// The window's colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    /// Follow the system's light or dark setting.
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub theme: Theme,
    /// Show the rotator panel (for a Yaesu rotator on the radio's ROT jack).
    pub show_rotator: bool,
    pub show_macros: bool,
    /// Tuning step in Hz for the mouse wheel and arrow keys.
    pub tune_step: i64,
    pub show_waterfall: bool,
    /// Highest audio frequency shown on the waterfall.
    pub wf_max_hz: u32,
    /// 1 = fast, 2 = normal, 4 = slow.
    pub wf_speed: u32,
    /// Follow the noise floor automatically.
    pub wf_auto: bool,
    pub wf_floor_db: i32,
    pub wf_range_db: i32,
    pub wf_height: u32,
    /// The user confirmed the firewall lets LAN remote through.
    pub firewall_ok: bool,
    /// Hold the space bar to transmit (push-to-talk).
    pub space_ptt: bool,
    /// Show the band scope when an RTL-SDR dongle is found.
    pub show_scope: bool,
    /// Band scope span in Hz; 0 = everything the dongle covers.
    pub scope_span: u32,
    /// Band scope rows per second.
    pub scope_fps: u32,
    pub scope_auto: bool,
    pub scope_floor_db: i32,
    pub scope_range_db: i32,
    /// No longer used: the band scope fills the Operate page's height.
    pub scope_height: u32,
    /// Show the US band plan (CW/data and phone segments) and FT8/FT4 spots.
    pub scope_band_plan: bool,
    /// Offer the band scope's Band span, showing the whole band the dial is
    /// in. The other spans follow the dial either way.
    pub scope_band_span: bool,
    /// Widths of the Operate page's control columns (left and right of the
    /// scope), as last dragged.
    pub operate_left_w: u32,
    pub operate_right_w: u32,
    /// The band scope in a window of its own (to make it big, or put it on
    /// another monitor), and where that window was last.
    pub scope_window: bool,
    pub scope_window_size: [u32; 2],
    pub scope_window_pos: Option<[i32; 2]>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            show_rotator: false,
            show_macros: true,
            tune_step: 10,
            show_waterfall: true,
            wf_max_hz: 4000,
            wf_speed: 2,
            wf_auto: true,
            wf_floor_db: -110,
            wf_range_db: 60,
            wf_height: 180,
            firewall_ok: false,
            space_ptt: true,
            show_scope: true,
            scope_span: 200_000,
            scope_fps: 15,
            scope_auto: true,
            scope_floor_db: -100,
            scope_range_db: 50,
            scope_height: 220,
            scope_band_plan: true,
            scope_band_span: false,
            operate_left_w: 290,
            operate_right_w: 300,
            scope_window: false,
            scope_window_size: [1100, 620],
            scope_window_pos: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Macro {
    pub label: String,
    /// One or more CAT commands, e.g. `"FA14074000;MD0C;"`.
    pub commands: String,
}

fn default_macros() -> Vec<Macro> {
    let m = |l: &str, c: &str| Macro { label: l.into(), commands: c.into() };
    vec![
        m("FT8 40m", "FA07074000;MD0C;"),
        m("FT8 20m", "FA14074000;MD0C;"),
        m("FT8 15m", "FA21074000;MD0C;"),
        m("CW 20m", "FA14025000;MD03;"),
        m("SSB 20m", "FA14200000;MD02;"),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            auto_connect: true,
            link_dir: None,
            virtual_ports: vec![
                VirtualPortConfig { name: "vcom1".into(), label: "WSJT-X".into(), ..Default::default() },
                VirtualPortConfig { name: "vcom2".into(), label: "Logger".into(), ..Default::default() },
                VirtualPortConfig { name: "vcom3".into(), label: "CW Skimmer".into(), block_tx: true, ..Default::default() },
            ],
            rigctld: RigctldConfig::default(),
            macros: default_macros(),
            remote_server: RemoteServerConfig::default(),
            remote_client: RemoteClientConfig::default(),
            last_remote: None,
            tx_audio: TxAudioConfig::default(),
            cw: CwConfig::default(),
            scope: ScopeConfig::default(),
            ui: UiPrefs::default(),
        }
    }
}

impl Config {
    /// `$FTDX3000_CONFIG`, else `~/.config/ftdx3000/config.toml`
    /// (`%APPDATA%\ftdx3000\config.toml` on Windows, which has no `$HOME`).
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("FTDX3000_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| if cfg!(windows) { std::env::var_os("APPDATA").map(PathBuf::from) } else { None })
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("ftdx3000").join("config.toml")
    }

    pub fn load() -> Config {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                // Keep the user's file: the defaults will be saved over it.
                let bad = path.with_extension("toml.bad");
                let _ = std::fs::copy(&path, &bad);
                log::warn!("ignoring invalid config {} (copied to {}): {e}", path.display(), bad.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    /// Write the file atomically, so a crash mid-write can't truncate it.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn link_dir(&self) -> PathBuf {
        self.link_dir.clone().unwrap_or_else(default_link_dir)
    }

    /// Fill in the CAT port from auto-detection if none is configured or
    /// the configured one no longer exists.
    pub fn autodetect_port(&mut self) -> Option<crate::detect::DetectedRadio> {
        let radios = crate::detect::detect_radios();
        let first = radios.into_iter().next()?;
        let path = &self.engine.serial.path;
        if path.is_empty() || !std::path::Path::new(path).exists() {
            self.engine.serial.path = first.cat.stable_path().to_string();
        }
        Some(first)
    }
}

/// The user's home directory, for default file locations: `$HOME`, or on
/// Windows `%USERPROFILE%`.
pub fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| if cfg!(windows) { std::env::var("USERPROFILE") } else { Err(std::env::VarError::NotPresent) })
        .unwrap_or_else(|_| ".".into())
}

/// Default directory for the stable virtual-port links (Unix).
pub fn default_link_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("ftdx3000"),
        // macOS has no XDG runtime dir and its temp dir is a long random
        // path; keep the names short enough to type into WSJT-X.
        None if cfg!(unix) => PathBuf::from(format!("/tmp/ftdx3000-{}", std::env::var("USER").unwrap_or_else(|_| "user".into()))),
        None => std::env::temp_dir().join("ftdx3000"),
    }
}

/// Everything that runs while the program is up.
pub struct Services {
    pub rig: Rig,
    pub config: Config,
    ports: Vec<(VirtualPortConfig, Result<VirtualPort, String>)>,
    rigctld: Option<Result<RigctldServer, String>>,
    remote_server: Option<(RemoteServerConfig, Result<RemoteServer, String>)>,
    audio_client: Option<AudioClient>,
    pub discovery: Option<Discovery>,
    /// The radio's receive audio, local or from the remote server.
    pub rx_tap: crate::spectrum::AudioTap,
    local_rx: Option<crate::spectrum::LocalRxAudio>,
    /// The computer mic feeding the local radio (`tx_audio.computer_mic`).
    mic_bridge: Option<(AudioDevice, MicBridge)>,
    /// The MIC SEL menus were set for the computer mic on this connection.
    mic_applied: bool,
    last_mic_refresh: std::time::Instant,
    /// The armed CW key, if armed.
    cw: Option<CwKeyer>,
    /// Why the CW key was last disarmed by itself.
    pub cw_disarmed: Option<String>,
    /// Band scope rows and status: from the local dongle, or from the remote
    /// radio's computer.
    pub scope_tap: ScopeTap,
    scope: Option<Scope>,
    scope_client: Option<(RemoteTarget, ScopeClient)>,
    /// What the last band scope calibration found.
    pub scope_calibration: Option<String>,
}

/// The radio codec's capture and playback devices, if detected.
pub fn radio_audio_devices() -> (Option<AudioDevice>, Option<AudioDevice>) {
    crate::audio::radio_devices()
}

impl Services {
    pub fn start(config: Config) -> Services {
        let rig = Rig::start();
        let mut s = Services {
            rig,
            config,
            ports: Vec::new(),
            rigctld: None,
            remote_server: None,
            audio_client: None,
            discovery: None,
            rx_tap: Default::default(),
            local_rx: None,
            mic_bridge: None,
            mic_applied: false,
            last_mic_refresh: std::time::Instant::now(),
            cw: None,
            cw_disarmed: None,
            scope_tap: ScopeTap::default(),
            scope: None,
            scope_client: None,
            scope_calibration: None,
        };
        s.apply_scope();
        s.apply_local_rx();
        s.apply_tx_audio();
        s.discovery = Discovery::start().map_err(|e| log::warn!("LAN discovery unavailable: {e}")).ok();
        s.apply_sharing();
        let have_target = s.config.engine.remote.is_some() || !s.config.engine.serial.path.is_empty();
        if s.config.auto_connect && have_target {
            s.connect();
        }
        s
    }

    /// (Re)start virtual ports, rigctld and the remote server to match the config.
    pub fn apply_sharing(&mut self) {
        self.apply_remote_server();
        let dir = self.config.link_dir();
        // Keep ports whose config didn't change; restart the rest.
        let wanted: Vec<VirtualPortConfig> = self.config.virtual_ports.iter().filter(|p| p.enabled).cloned().collect();
        self.ports.retain(|(cfg, _)| wanted.contains(cfg));
        for cfg in wanted {
            if self.ports.iter().any(|(c, _)| *c == cfg) {
                continue;
            }
            let port = VirtualPort::start(self.rig.clone(), cfg.clone(), dir.clone()).map_err(|e| e.to_string());
            if let Err(e) = &port {
                self.rig.note(format!("virtual port {} failed: {e}", cfg.name));
            }
            self.ports.push((cfg, port));
        }
        let rc = &self.config.rigctld;
        let restart = match &self.rigctld {
            Some(Ok(s)) => !rc.enabled || s.addr.to_string() != rc.bind,
            Some(Err(_)) => true,
            None => rc.enabled,
        };
        if restart {
            self.rigctld = None;
            if rc.enabled {
                let r = RigctldServer::start(self.rig.clone(), rc.clone()).map_err(|e| e.to_string());
                if let Err(e) = &r {
                    self.rig.note(format!("rigctld server on {} failed: {e}", rc.bind));
                }
                self.rigctld = Some(r);
            }
        }
    }

    /// (Re)start the LAN remote server to match the config.
    pub fn apply_remote_server(&mut self) {
        let want = self.config.remote_server.clone();
        // A remote client can't also serve the radio it doesn't own.
        let enabled = want.enabled && self.config.engine.remote.is_none();
        let same = self.remote_server.as_ref().is_some_and(|(c, r)| *c == want && r.is_ok());
        if !enabled {
            self.remote_server = None;
            return;
        }
        if same {
            return;
        }
        self.remote_server = None;
        let r = RemoteServer::start(self.rig.clone(), want.clone(), self.scope_tap.clone()).map_err(|e| e.to_string());
        if let Err(e) = &r {
            self.rig.note(format!("remote server on {} failed: {e}", want.bind));
        }
        self.remote_server = Some((want, r));
    }

    pub fn remote_server_status(&self) -> Option<Result<(String, crate::remote::ServerStatus), String>> {
        self.remote_server.as_ref().map(|(_, r)| r.as_ref().map(|s| (s.addr.to_string(), s.status())).map_err(|e| e.clone()))
    }

    pub fn audio_client_status(&self) -> Option<crate::remote::AudioClientStatus> {
        self.audio_client.as_ref().map(|a| a.status())
    }

    /// The remote radio's computer announces that it won't play our
    /// microphone into the radio (its "Accept microphone audio" is off).
    pub fn remote_refuses_mic(&self) -> bool {
        let Some(target) = &self.config.engine.remote else { return false };
        self.discovery.as_ref().is_some_and(|d| d.servers().iter().any(|f| f.addr.to_string() == target.addr && !f.mic))
    }

    /// Record the local radio's audio (idle until something subscribes to
    /// `rx_tap`), unless we're controlling a remote radio.
    fn apply_local_rx(&mut self) {
        if self.config.engine.remote.is_some() {
            self.local_rx = None;
        } else if self.local_rx.is_none() {
            let device = || radio_audio_devices().0;
            self.local_rx = Some(crate::spectrum::LocalRxAudio::start(self.rx_tap.clone(), device, crate::remote::AUDIO_RATE));
        }
    }

    /// Run the local band scope, or show the remote radio's, to match the
    /// config. Call after changing `config.scope`.
    pub fn apply_scope(&mut self) {
        match self.config.engine.remote.clone() {
            Some(target) => {
                self.scope = None;
                if self.scope_client.as_ref().is_none_or(|(t, _)| *t != target) {
                    self.scope_client = None;
                    self.scope_client = Some((target.clone(), ScopeClient::start(target, self.scope_tap.clone())));
                }
            }
            None => {
                self.scope_client = None;
                match &self.scope {
                    Some(s) => s.update(self.config.scope.clone()),
                    None => self.scope = Some(Scope::start(self.rig.clone(), self.scope_tap.clone(), self.config.scope.clone())),
                }
            }
        }
    }

    /// Keep the result of a band scope calibration (local dongle only).
    fn apply_calibration(&mut self) {
        let Some(c) = self.scope_tap.take_calibration() else { return };
        if self.config.engine.remote.is_none() {
            let cfg = &mut self.config.scope;
            match c.fix {
                Some(CalibrationFix::Ppm(ppm, fine)) => (cfg.ppm, cfg.ppm_fine) = (ppm, fine),
                Some(CalibrationFix::IfHz(hz)) => cfg.if_hz = hz,
                None => {}
            }
            if c.fix.is_some() {
                cfg.calibrated = true;
                self.apply_scope();
                let _ = self.config.save();
            }
        }
        self.rig.note(format!("band scope calibration: {}", c.message));
        self.scope_calibration = Some(c.message);
    }

    pub fn scope_status(&self) -> ScopeStatus {
        self.scope_tap.status()
    }

    /// Where the waterfall's audio comes from, or why there is none.
    pub fn rx_audio_status(&self) -> Option<String> {
        match &self.local_rx {
            Some(l) => l.status.lock().clone(),
            None if self.config.engine.remote.is_some() => {
                Some(if self.config.remote_client.audio { "remote radio audio".into() } else { "turn on \"Listen to the radio\" in Remote (LAN)".into() })
            }
            None => None,
        }
    }

    /// Talk through the computer's microphone (`true`) or the radio's own.
    /// Sets the radio's AM/FM/SSB MIC SEL menus to USB, remembering what they
    /// were, and restores them when switching back.
    pub fn set_computer_mic(&mut self, on: bool) {
        self.config.tx_audio.computer_mic = on;
        if self.rig.link().is_open() {
            self.apply_mic_sel();
        } else {
            self.mic_applied = false;
        }
        self.apply_tx_audio();
        let _ = self.config.save();
    }

    /// Send the MIC SEL menu values the switch calls for.
    fn apply_mic_sel(&mut self) {
        let t = &mut self.config.tx_audio;
        let model = self.rig.model();
        let roles = model.roles;
        // Saved for another radio: those item numbers mean other things here.
        let saved_for = if t.saved_for.is_empty() { "ftdx3000" } else { t.saved_for.as_str() };
        if saved_for != model.key {
            t.saved_mic_sel.clear();
        }
        t.saved_for = model.key.to_string();
        for &menu in roles.mic_sel {
            let now = txaudio::mic_sel(&self.rig, menu);
            let want = if t.computer_mic {
                // Remember the radio's own setting to go back to.
                if let Some(v) = now.filter(|&v| v != roles.mic_usb) {
                    t.saved_mic_sel.retain(|(m, _)| *m != menu);
                    t.saved_mic_sel.push((menu, v));
                }
                roles.mic_usb
            } else {
                t.saved_mic_sel.iter().find(|(m, _)| *m == menu).map(|(_, v)| *v).filter(|&v| v != roles.mic_usb).unwrap_or(roles.mic_front)
            };
            if now != Some(want) {
                self.rig.send(txaudio::mic_sel_cmd(&self.rig, menu, want));
            }
        }
        if !t.computer_mic {
            t.saved_mic_sel.clear();
        }
        self.mic_applied = true;
    }

    /// Start, stop or adjust whatever carries the computer mic: the local
    /// bridge, or the audio stream to a remote radio.
    pub fn apply_tx_audio(&mut self) {
        let t = self.config.tx_audio.clone();
        let remote = self.config.engine.remote.is_some();
        let rc = &mut self.config.remote_client;
        (rc.send_mic, rc.mic, rc.mic_gain) = (t.computer_mic, t.mic.clone(), t.mic_gain);
        self.update_audio_client();
        if !t.computer_mic || remote {
            self.mic_bridge = None;
        } else if let Some((_, b)) = self.mic_bridge.as_ref().filter(|(dev, _)| *dev == t.mic) {
            b.set_gain(t.mic_gain);
        } else {
            self.mic_bridge = Some((t.mic.clone(), MicBridge::start(self.rig.clone(), t.mic.clone(), t.mic_gain)));
        }
    }

    /// The local computer-mic bridge's state, when it runs.
    pub fn mic_status(&self) -> Option<MicStatus> {
        self.mic_bridge.as_ref().map(|(_, b)| b.status())
    }

    /// Arm or disarm the CW key. Arming puts the radio in CW with break-in
    /// (so key-down transmits) and opens the keying line.
    pub fn cw_arm(&mut self, on: bool) {
        self.cw = None;
        if !on {
            return;
        }
        self.cw_disarmed = None;
        let (cw_mode, break_in, wpm) = {
            let s = self.rig.state();
            (s.tx_mode().is_some_and(|m| m.is_cw()), s.break_in, s.key_speed as u32)
        };
        if !cw_mode {
            self.rig.send("MD03;");
        }
        if !break_in {
            self.rig.send("BI1;");
        }
        let rig = self.rig.clone();
        let target = self.config.engine.remote.clone();
        let open = move || -> Result<Box<dyn KeyLine>, String> {
            Ok(match target {
                Some(t) => Box::new(RemoteKeyLine::open(&t)?),
                None => Box::new(LocalKeyLine::open(&rig, crate::Source::Gui)?),
            })
        };
        self.cw = Some(CwKeyer::start(open, self.config.cw.clone(), if wpm >= 4 { wpm } else { 20 }));
        self.rig.note("CW key armed");
    }

    pub fn cw(&self) -> Option<&CwKeyer> {
        self.cw.as_ref()
    }

    fn cw_disarm(&mut self, why: &str) {
        if self.cw.take().is_some() {
            self.cw_disarmed = Some(why.to_string());
            self.rig.note(format!("CW key disarmed: {why}"));
        }
    }

    /// Call regularly (every frame or so): after a (re)connect, set the
    /// radio's MIC SEL menus again if the computer mic is on; disarm the CW
    /// key when it should no longer be live; keep a band scope calibration.
    pub fn tick(&mut self) {
        self.apply_calibration();
        if let Some(k) = &self.cw {
            let st = k.status();
            let settled = k.armed_at.elapsed() > std::time::Duration::from_secs(3);
            let s = self.rig.state();
            let cw_mode = s.tx_mode().is_some_and(|m| m.is_cw());
            let wpm = s.key_speed as u32;
            drop(s);
            if wpm >= 4 {
                k.set_wpm(wpm);
            }
            if let Some(e) = st.error {
                self.cw = None;
                self.cw_disarmed = Some(e);
            } else if !self.rig.link().is_open() {
                self.cw_disarm("the radio disconnected");
            } else if settled && !cw_mode {
                self.cw_disarm("the radio left CW mode");
            } else if k.idle_for() > crate::cw::IDLE_DISARM {
                self.cw_disarm("not used for 10 minutes");
            }
        }
        if !matches!(self.rig.link(), crate::LinkState::Connected { .. }) {
            self.mic_applied = false;
            return;
        }
        if self.mic_applied || !self.config.tx_audio.computer_mic {
            return;
        }
        if self.rig.model().roles.mic_sel.iter().all(|&m| txaudio::mic_sel(&self.rig, m).is_some()) {
            self.apply_mic_sel();
            let _ = self.config.save();
        } else if self.last_mic_refresh.elapsed() > std::time::Duration::from_secs(3) {
            self.last_mic_refresh = std::time::Instant::now();
            let cmds: Vec<String> = self.rig.model().roles.mic_sel.iter().map(|&m| ftdx3000_cat::menu::read_cmd(m)).collect();
            self.rig.refresh(&cmds.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }

    /// Push changed client audio settings (volume, mic) to the running stream.
    pub fn update_audio_client(&mut self) {
        let rc = &self.config.remote_client;
        let want_audio = self.config.engine.remote.is_some() && (rc.audio || rc.send_mic);
        match (&self.audio_client, want_audio) {
            (Some(a), true) => a.update(self.config.remote_client.clone()),
            (None, true) => {
                if let Some(t) = self.config.engine.remote.clone() {
                    self.audio_client = Some(AudioClient::start(self.rig.clone(), t, self.config.remote_client.clone(), Some(self.rx_tap.clone())));
                }
            }
            (Some(_), false) => self.audio_client = None,
            (None, false) => {}
        }
    }

    /// Control a radio on another computer instead of a local one.
    pub fn connect_remote(&mut self, target: RemoteTarget) {
        self.cw_disarm("switched radios");
        self.config.engine.remote = Some(target.clone());
        self.config.last_remote = Some(target);
        self.audio_client = None;
        self.apply_remote_server();
        self.apply_local_rx();
        self.apply_scope();
        self.connect();
        self.apply_tx_audio();
    }

    /// Go back to the locally connected radio.
    pub fn use_local(&mut self) {
        self.cw_disarm("switched radios");
        self.config.engine.remote = None;
        self.audio_client = None;
        self.rig.disconnect();
        self.apply_remote_server();
        self.apply_local_rx();
        self.apply_scope();
        self.apply_tx_audio();
        if !self.config.engine.serial.path.is_empty() {
            self.connect();
        }
    }

    pub fn port_status(&self) -> Vec<(VirtualPortConfig, Result<VirtualPortStatus, String>)> {
        self.ports
            .iter()
            .map(|(c, p)| (c.clone(), p.as_ref().map(|p| p.status()).map_err(|e| e.clone())))
            .collect()
    }

    pub fn rigctld_status(&self) -> Option<Result<(String, usize), String>> {
        self.rigctld.as_ref().map(|r| r.as_ref().map(|s| (s.addr.to_string(), s.clients())).map_err(|e| e.clone()))
    }

    pub fn connect(&mut self) {
        self.rig.connect(self.config.engine.clone());
        self.update_audio_client();
    }

    pub fn shutdown(&mut self) {
        self.ports.clear();
        self.rigctld = None;
        self.remote_server = None;
        self.audio_client = None;
        self.discovery = None;
        self.local_rx = None;
        self.mic_bridge = None;
        self.cw = None;
        self.scope = None;
        self.scope_client = None;
        self.rig.shutdown();
        // Give the engine a moment to un-key and send AI0.
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}
