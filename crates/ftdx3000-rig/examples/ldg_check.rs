//! Hardware check of an LDG tuner on its meter jack: connects, prints the
//! meter readings each second, and optionally sends commands. Never tunes
//! (that would need the radio to transmit).
//!
//!     cargo run --release --example ldg_check -- <port> [ant|bypass|auto|manual]...

use std::time::{Duration, Instant};

use ftdx3000_rig::Rig;
use ftdx3000_rig::ldg::{LdgCommand, LdgConfig, LdgTuner};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(port) = args.first().cloned() else {
        eprintln!("usage: ldg_check <port> [ant|bypass|auto|manual]...");
        std::process::exit(2);
    };
    let tuner = LdgTuner::start(Rig::start(), LdgConfig { enabled: true, port, ..Default::default() }, Default::default());
    let watch = |secs: u64| {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_secs(1));
            let s = tuner.status();
            println!(
                "{:<40} fwd {:6.1} W  ref {:5.1} W  swr {:>5}  word {:#06x}  ant {:?}  mode {:?}  last {:?}  busy {:?}",
                s.text,
                s.forward_w,
                s.reflected_w,
                s.swr.map_or("—".into(), |v| format!("{v:.2}")),
                s.status_word,
                s.antenna,
                s.mode.map(|m| m.label()),
                s.last_tune.map(|t| t.label()),
                s.busy
            );
        }
    };
    watch(args.iter().any(|a| a == "--long").then_some(14).unwrap_or(3));
    for a in &args[1..] {
        let cmd = match a.as_str() {
            "ant" => LdgCommand::ToggleAntenna,
            "bypass" => LdgCommand::Bypass,
            "auto" => LdgCommand::Auto,
            "manual" => LdgCommand::Manual,
            "--long" => continue,
            other => {
                eprintln!("unknown command {other}");
                continue;
            }
        };
        println!("--> {a}");
        tuner.send(cmd);
        watch(2);
    }
}
