//! A receive-only driver for the HackRF One (and its Jawbreaker and rad1o
//! relatives), in pure Rust over `nusb` like the RTL-SDR's: no libhackrf.
//!
//! It only ever puts the HackRF in receive or off, never transmit, and
//! keeps the antenna port's bias power off: the input may be the radio's
//! IF OUT or RX OUT jack.

use std::collections::VecDeque;
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Recipient};
use rustfft::num_complex::Complex32;

const VID: u16 = 0x1d50;
/// (product id, name)
const PRODUCTS: &[(u16, &str)] = &[(0x6089, "HackRF One"), (0x604b, "HackRF Jawbreaker"), (0xcc15, "rad1o")];

const RX_ENDPOINT: u8 = 0x81;
/// 16 ms of samples at 8.192 MS/s, 8 in flight.
const TRANSFER_BYTES: usize = 262_144;
const TRANSFERS: usize = 8;
const TIMEOUT: Duration = Duration::from_millis(500);

// Vendor requests (libhackrf's numbering).
const SET_TRANSCEIVER_MODE: u8 = 1;
const SAMPLE_RATE_SET: u8 = 6;
const BASEBAND_FILTER_BANDWIDTH_SET: u8 = 7;
const BOARD_ID_READ: u8 = 14;
const SET_FREQ: u8 = 16;
const AMP_ENABLE: u8 = 17;
const SET_LNA_GAIN: u8 = 19;
const SET_VGA_GAIN: u8 = 20;
const ANTENNA_ENABLE: u8 = 23;

const MODE_OFF: u16 = 0;
const MODE_RECEIVE: u16 = 1;

/// The MAX2837's baseband filter widths.
const BASEBAND_FILTERS_HZ: &[u32] = &[
    1_750_000, 2_500_000, 3_500_000, 5_000_000, 5_500_000, 6_000_000, 7_000_000, 8_000_000, 9_000_000, 10_000_000, 12_000_000, 14_000_000, 15_000_000,
    20_000_000, 24_000_000, 28_000_000,
];

/// The baseband filter for a sample rate, as libhackrf picks it: the widest
/// within three quarters of the rate.
fn baseband_filter(sample_rate: u32) -> u32 {
    let max = sample_rate as u64 * 3 / 4;
    BASEBAND_FILTERS_HZ.iter().copied().take_while(|&f| f as u64 <= max).last().unwrap_or(BASEBAND_FILTERS_HZ[0])
}

/// A HackRF plugged into this computer.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub product: &'static str,
    pub serial: String,
    id: nusb::DeviceId,
}

impl Info {
    pub fn describe(&self) -> String {
        // Serials are 32 hex digits, mostly leading zeros.
        let short = self.serial.trim_start_matches('0');
        if short.is_empty() { self.product.to_string() } else { format!("{} (serial {short})", self.product) }
    }
}

/// Plugged-in HackRFs.
pub fn list() -> Vec<Info> {
    let Ok(devices) = nusb::list_devices().wait() else { return Vec::new() };
    devices
        .filter(|d| d.vendor_id() == VID)
        .filter_map(|d| {
            let product = PRODUCTS.iter().find(|(pid, _)| *pid == d.product_id())?.1;
            Some(Info { product, serial: d.serial_number().unwrap_or_default().to_string(), id: d.id() })
        })
        .collect()
}

/// Receive gains. The HackRF has no gain control of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gains {
    /// RF amplifier, 0-40 dB in 8 dB steps.
    pub lna_db: u8,
    /// Baseband amplifier, 0-62 dB in 2 dB steps.
    pub vga_db: u8,
    /// The extra 14 dB front-end amplifier.
    pub amp: bool,
}

/// An open HackRF, receiving.
pub struct HackRf {
    _device: nusb::Device,
    interface: nusb::Interface,
    endpoint: nusb::Endpoint<Bulk, In>,
    /// Samples received but not yet read.
    queued: VecDeque<Complex32>,
    /// Transfers to throw away after a retune (they hold the old frequency).
    stale: usize,
}

fn usb(e: impl std::fmt::Display) -> String {
    let text = e.to_string();
    if cfg!(target_os = "linux") && (text.contains("ermission") || text.contains("EACCES") || text.contains("denied")) {
        format!("{text} — this user may not open the HackRF; install the hackrf package's udev rules, or see the README (Band scope)")
    } else {
        text
    }
}

impl HackRf {
    pub fn open(info: &Info, hz: u64, sample_rate: u32, gains: Gains) -> Result<HackRf, String> {
        let device = nusb::list_devices()
            .wait()
            .map_err(usb)?
            .find(|d| d.id() == info.id)
            .ok_or("the HackRF was unplugged")?
            .open()
            .wait()
            .map_err(usb)?;
        if !device.active_configuration().is_ok_and(|c| c.configuration_value() == 1) {
            device.set_configuration(1).wait().map_err(usb)?;
        }
        let interface = device.detach_and_claim_interface(0).wait().map_err(usb)?;
        let endpoint = interface.endpoint::<Bulk, In>(RX_ENDPOINT).map_err(usb)?;
        let mut h = HackRf { _device: device, interface, endpoint, queued: VecDeque::new(), stale: 0 };
        // A previous user may have left it streaming.
        h.out(SET_TRANSCEIVER_MODE, MODE_OFF, 0, &[])?;
        h.read_board_id()?;
        h.out(ANTENNA_ENABLE, 0, 0, &[])?;
        let mut rate = [0u8; 8];
        rate[..4].copy_from_slice(&sample_rate.to_le_bytes());
        rate[4..].copy_from_slice(&1u32.to_le_bytes());
        h.out(SAMPLE_RATE_SET, 0, 0, &rate)?;
        let filter = baseband_filter(sample_rate);
        h.out(BASEBAND_FILTER_BANDWIDTH_SET, (filter & 0xffff) as u16, (filter >> 16) as u16, &[])?;
        h.set_gains(gains)?;
        h.set_frequency(hz)?;
        h.out(SET_TRANSCEIVER_MODE, MODE_RECEIVE, 0, &[])?;
        for _ in 0..TRANSFERS {
            let buf = h.endpoint.allocate(TRANSFER_BYTES);
            h.endpoint.submit(buf);
        }
        h.stale = 0;
        Ok(h)
    }

    fn out(&self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), String> {
        let c = ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, data };
        self.interface.control_out(c, TIMEOUT).wait().map_err(usb)
    }

    fn input(&self, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, String> {
        let c = ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, length };
        self.interface.control_in(c, TIMEOUT).wait().map_err(usb)
    }

    fn read_board_id(&self) -> Result<u8, String> {
        self.input(BOARD_ID_READ, 0, 0, 1)?.first().copied().ok_or_else(|| "no answer from the HackRF".into())
    }

    /// Tune; samples already received are thrown away.
    pub fn set_frequency(&mut self, hz: u64) -> Result<(), String> {
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&((hz / 1_000_000) as u32).to_le_bytes());
        data[4..].copy_from_slice(&((hz % 1_000_000) as u32).to_le_bytes());
        self.out(SET_FREQ, 0, 0, &data)?;
        self.queued.clear();
        self.stale = self.endpoint.pending();
        Ok(())
    }

    pub fn set_gains(&mut self, g: Gains) -> Result<(), String> {
        // Out of range or between steps is refused, so round down to a step.
        let lna = g.lna_db.min(40) & !7;
        let vga = g.vga_db.min(62) & !1;
        let ok = |v: Vec<u8>| v.first().is_some_and(|b| *b != 0);
        if !ok(self.input(SET_LNA_GAIN, 0, lna as u16, 1)?) {
            return Err(format!("the HackRF refused LNA gain {lna} dB"));
        }
        if !ok(self.input(SET_VGA_GAIN, 0, vga as u16, 1)?) {
            return Err(format!("the HackRF refused VGA gain {vga} dB"));
        }
        self.out(AMP_ENABLE, g.amp as u16, 0, &[])
    }

    /// Fill `buf` with samples (full scale = 1.0); returns how many.
    pub fn read(&mut self, buf: &mut [Complex32]) -> Result<usize, String> {
        while self.queued.len() < buf.len() {
            if self.endpoint.pending() == 0 {
                return Err("stream closed".into());
            }
            let c = self.endpoint.wait_next_complete(TIMEOUT).ok_or("the HackRF stopped sending samples")?;
            let mut data: Buffer = c.buffer;
            if self.stale > 0 {
                self.stale -= 1;
            } else if c.status.is_ok() {
                // Signed 8-bit I, Q pairs.
                self.queued.extend(data[..c.actual_len].chunks_exact(2).map(|iq| Complex32::new(iq[0] as i8 as f32 / 128.0, iq[1] as i8 as f32 / 128.0)));
            }
            data.clear();
            self.endpoint.submit(data);
            c.status.map_err(usb)?;
        }
        let n = buf.len();
        for (b, s) in buf.iter_mut().zip(self.queued.drain(..n)) {
            *b = s;
        }
        Ok(n)
    }
}

impl Drop for HackRf {
    fn drop(&mut self) {
        let _ = self.out(SET_TRANSCEIVER_MODE, MODE_OFF, 0, &[]);
        self.endpoint.cancel_all();
        while self.endpoint.pending() > 0 && self.endpoint.wait_next_complete(TIMEOUT).is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseband_filter_as_libhackrf() {
        assert_eq!(baseband_filter(2_048_000), 1_750_000);
        assert_eq!(baseband_filter(8_192_000), 6_000_000);
        assert_eq!(baseband_filter(20_000_000), 15_000_000);
    }
}
