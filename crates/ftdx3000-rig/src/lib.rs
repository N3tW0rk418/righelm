//! FTDX3000 rig engine and port sharing.

pub mod detect;
pub mod engine;
pub mod sim;
pub mod transport;

pub use engine::{CatError, EngineConfig, Freshness, LinkState, Rig, Source};
#[cfg(windows)]
pub mod comport;
#[cfg(windows)]
mod winserial;
#[cfg(unix)]
pub mod pty;
#[cfg(unix)]
pub mod hamlib_check;
pub mod vport;
pub mod rigctld;
pub mod services;

pub use services::{Config, Macro, Services, UiPrefs};
pub mod audio;
pub mod cw;
pub mod logger;
pub mod remote;
pub mod scope;
pub mod spectrum;
pub mod txaudio;
pub mod firewall;
