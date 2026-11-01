# RigHelm

![RigHelm](assets/righelm-128.png)

**RigHelm** — the Linux alternative to Win4Yaesu for the Yaesu FTDX3000. Rig control,
sharing and LAN remote, written in Rust. Runs on Linux, macOS and Windows: as the station
the radio is plugged into, or as a remote control for another RigHelm on your network.

It owns the radio's CAT
port and shares it with any number of other programs, on this computer or across your LAN.

Besides the FTDX3000 it controls 31 other Yaesu, Icom, Kenwood and Elecraft radios (see
[Supported radios](#supported-radios)), with their own menus, bands, modes and controls.

- **Full radio control.** VFO A/B, split, bands, modes, filters and DSP, receiver and
  transmitter settings, clarifier, CW keyer and keyer memories, voice memories, tuner,
  antennas, rotator, live meters, the radio's whole menu (196 items on the FTDX3000, with
  backup and restore), memory channels, macros, and a raw CAT console. On other radios the
  controls it doesn't have are hidden.
- **Audio waterfall.** A scrolling spectrogram of the radio's receive audio (up to 6 kHz),
  with the filter passband and CW pitch overlaid. Hover shows a signal's radio frequency;
  in CW, click a trace to zero-beat it. It shares the radio's sound card with WSJT-X, and
  works from a remote computer too.
- **Band scope.** Plug an RTL-SDR dongle into the radio's IF OUT or RX OUT jack and a
  wide spectrum and waterfall of the band (up to 1.6 MHz) appears by itself, follows the
  VFO, and tunes the radio with a click. Nothing else to install, and remote computers
  see it too (see below).
- **Logger Connect.** DX spots from QLog, N1MM+ and others as markers on the band scope.
  Click one to tune to it and fill in the logger.
- **Auto-detect.** Finds the radio's USB bridge (Yaesu's CP2105: Enhanced = CAT, Standard =
  keying; Icom's and Kenwood's CP2102; the IC-705's own ports; FTDI for Elecraft) and the
  radio's own USB audio codec. The two are paired because they sit on the same internal USB
  hub of the radio. On connect the baud rate, handshake and the radio model are probed.
- **Port sharing.** Linux has the same one-program-per-port problem as Windows, so this
  program opens the radio exclusively and offers **virtual serial ports** and a
  **rigctld-compatible server** to everything else.
- **Remote over the LAN.** Run a second copy on another computer: it finds the radio
  automatically, controls everything, plays the radio's audio, and can send your
  microphone back.
- **Computer microphone.** One click switches between the radio's own mic and this
  computer's (see below), here or on a remote radio.
- **CW from the computer.** A RADIO KEY / COMPUTER KEY switch: key the radio from the
  touchpad, mouse or keyboard (straight key or iambic paddles) or send typed text.
- **Space bar push-to-talk.** Hold the space bar to transmit, let go to stop.

## Supported radios

| Maker | Radios |
|-------|--------|
| Yaesu | FTDX3000, FTDX101D, FTDX101MP, FTDX10, FT-710, FT-991A, FT-991, FT-891, FTDX5000, FTDX1200 (and with FFT-1), FT-950, FT-2000, FT-2000D, FT-450D, FT-450 |
| Icom | IC-7300, IC-7300MK2, IC-7610, IC-705, IC-9700 |
| Kenwood | TS-590S, TS-590SG, TS-890S, TS-990S, TS-2000, TS-480SAT, TS-480HX |
| Elecraft | K3 / K3S, KX3, KX2, K4 |

RigHelm finds out which radio it is when it connects (Settings → *Ports & sharing* → *Radio*
can also name it, for the few that answer alike: the TS-480SAT/HX, FT-2000/2000D). Each
model's menus, bands, modes, filter widths, preamp, attenuator and power range come from the
manufacturer's CAT/CI-V reference manual.

How it works: inside RigHelm everything speaks the FTDX3000's CAT language, whatever the
radio. For any other radio a translator on the serial line turns that into the radio's own
commands (Yaesu and Kenwood text commands, Icom's binary CI-V) and the radio's answers back.
The FTDX3000 itself goes through no translator at all. The virtual ports run the same
translator the other way round, so programs see the real radio.

Only the FTDX3000 has been used with the real radio. The others are checked against
simulators (see [Testing without the radio](#testing-without-the-radio)) and against
Hamlib's own drivers for them, but not yet on the air: meter scales in particular are
estimates for most of them. Reports from owners are welcome.

Radio settings for the other makes:

- **Icom**: CI-V USB port *Unlink from [REMOTE]*, *CI-V Transceive* ON, *CI-V USB Echo Back*
  OFF. RigHelm opens the port with RTS and DTR off, so *USB SEND* / *USB Keying* set to RTS or
  DTR can't key the radio.
- **Kenwood**: the PC port's speed as you like (it's probed); RTS/CTS handshake.
- **Elecraft**: RigHelm turns on the extended (K31) command mode itself.

## Build and install

```bash
cargo build --release
```

To install the binaries, icon and a desktop launcher for your user:

```bash
./packaging/install.sh
```

Binaries in `target/release/`:

| binary          | what                                                         |
|-----------------|--------------------------------------------------------------|
| `righelm`       | the desktop app                                              |
| `righelmd`      | headless server (same config, no window), `--detect` lists hardware, `--discover` lists RigHelm servers on the LAN |
| `righelm-sim`   | a simulated FTDX3000 on a PTY for testing without the radio  |

Your user must be in the `dialout` group to open the radio's serial port.

### macOS

RigHelm also builds on macOS (Apple Silicon and Intel). Audio uses CoreAudio there instead
of PipeWire.

1. Install the Xcode command-line tools and Rust:

   ```bash
   xcode-select --install
   ```

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Build and run from the repository:

   ```bash
   cargo build --release
   ```

   ```bash
   ./target/release/righelm
   ```

Notes for the Mac:

- **Microphone permission.** The first time RigHelm records audio (the radio's receive audio
  for the waterfall or remote server, or your microphone for remote transmit), macOS asks
  to allow microphone access for the app you started it from (Terminal). Allow it, or audio
  stays silent. You can change it later in System Settings → Privacy & Security →
  Microphone.
- **Network permission.** For LAN remote, macOS may ask whether RigHelm may accept incoming
  network connections. Allow it so it can find, and be found by, other copies of RigHelm.
- **Radio connected to the Mac.** The radio's ports appear as `/dev/cu.*` devices, using
  either macOS's built-in driver or Silicon Labs' CP210x driver, and are auto-detected. The
  radio's audio is found by name ("USB Audio CODEC").
- **Virtual ports** are created in `/tmp/ftdx3000-<your user name>/` (e.g.
  `/tmp/ftdx3000-jane/vcom1`).
- **Controlling a radio on your Linux machine** needs no serial port on the Mac at all.
  Open Remote (LAN) and connect.

### Windows

1. Install [Rust](https://rustup.rs) (it also installs the Visual Studio C++ build tools
   it needs).
2. Install the radio's USB driver: the Silicon Labs CP210x driver from Yaesu's or Silicon
   Labs' site. The radio then shows up as two COM ports.
3. Build and run from the repository:

   ```bash
   cargo build --release
   ```

   ```bash
   target\release\righelm.exe
   ```

   To install it for your user (in `%LOCALAPPDATA%\Programs\RigHelm`, with Start menu and
   desktop shortcuts; no administrator rights needed):

   ```bash
   powershell -ExecutionPolicy Bypass -File packaging\install-windows.ps1
   ```

   The Windows release download is a zip with the same script in it.

Notes for Windows:

- **Virtual ports need [com0com](https://com0com.sourceforge.net/)**, the same free virtual
  null-modem driver Win4Yaesu uses. Create a pair (for example COM10 ⇄ COM11). In Ports &
  sharing, enter COM10 for a virtual port, then point WSJT-X or your logger at COM11.
- On Windows a virtual port can't tell when the program on the other end closes, so the
  automatic un-key when a client disappears covers rigctld and remote clients but not
  com0com ports. Where a program supports it, prefer **Hamlib NET rigctl** (127.0.0.1:4532),
  which has no such gap and needs no com0com.
- Audio uses Windows' own audio system; the radio's sound card is found by name
  ("USB Audio CODEC").
- Allow RigHelm through Windows Firewall when asked if you use LAN remote.
- `righelm-sim` (the simulator) needs a Unix pseudo-terminal and is Linux/macOS only.
- The band scope's RTL-SDR dongle needs the WinUSB driver on Windows: install it for
  "Bulk-In, Interface (Interface 0)" with [Zadig](https://zadig.akeo.ie/).

## Radio settings

| menu | setting           | value |
|------|-------------------|-------|
| 037  | CAT SELECT        | USB |
| 038  | CAT RATE          | 38400 (any rate works; it's auto-detected) |
| 039  | CAT TIME OUT TIMER| set to 100 ms automatically on connect |

For WSJT-X and other data programs, set **075 DATA IN SELECT** to **USB**. The microphone
switch and the CW key set the other menus they need themselves (below).

## Talking through the computer's microphone

The **MIC** switch on the Operate page (also at the top of *Settings*) chooses where your
voice comes from:

- **RADIO MIC**: the microphone plugged into the radio. The radio's AM, FM and SSB MIC SEL
  menus (053, 085, 103) are what they were before you switched.
- **COMPUTER MIC**: this computer's microphone. Those menus are set to **USB**, and the mic
  is played into the radio's USB audio input while you transmit. Clicking back restores the
  menus.

To talk, **hold the space bar** (push-to-talk) or click **MOX / PTT**. Space never keys while
you are typing in a text box or with the computer CW key on, and it lets go if RigHelm's
window loses focus. *Settings → Transmit audio* can turn it off.

The computer mic only reaches the radio while you transmit in SSB, AM or FM, keyed by MOX
or the space bar in RigHelm, the radio's own PTT or footswitch, or a remote operator. It is never mixed into
a transmission that WSJT-X or another program on a virtual port or rigctld keyed. *Settings
→ Transmit audio* picks the microphone and its gain and shows its level. "System default"
never picks the radio's own audio input, even when PipeWire makes that the default.

When controlling a remote radio, the same switch sends this computer's mic over the
network. The radio's computer must allow it: *Remote (LAN) → Accept microphone audio for
transmit*.

## Sending CW from the computer

The **KEY** switch on the Operate page (also in *Settings → CW key*) chooses where CW
comes from:

- **RADIO KEY**: the key or paddle plugged into the radio. This is the default, and RigHelm
  always starts with it.
- **COMPUTER KEY**: this computer. The switch turns red, a red **COMPUTER KEY** badge appears
  at the top right, the radio goes to CW with break-in on, and a key pad opens.

Ways to key with the computer key:

- **Key pad**: press and hold with the touchpad or mouse. Iambic modes: left button = dit,
  right button (two-finger press on a touchpad) = dah, and the DIT/DAH halves light up.
  Holding both gives a squeeze. On a ThinkPad, the buttons above the touchpad make good
  paddles. Straight key mode: hold = key down.
- **Keyboard**: `[` = dit, `]` = dah (or the straight key).
- **Send text**: type and press Enter. Touching the paddles, **Stop** or Esc stops it.
- The speed is the radio's keyer speed (CW group → Speed). *Sidetone here* plays a tone on
  this computer, which helps on a remote radio.

The radio is keyed on its Standard USB serial port's RTS line, with menu 065 PC KEYING
set to RTS. RigHelm goes back to the radio's key by itself when the radio disconnects or
leaves CW, and after 10 minutes unused. A key held down longer than 6
seconds is released. On a remote radio, the timing is made on your computer and the
server keys its own port. It lets go if your connection goes quiet.

## Sharing the radio with other programs

Only one program at a time can sensibly talk to a serial port. Linux will even let two open
it, and then they steal each other's answers. Most programs also lock it exclusively
(`TIOCEXCL`). This app keeps the real port and shares it in two ways.

### 1. Virtual serial ports

Each one behaves exactly like the radio on a serial cable (default `vcom1..3` in
`$XDG_RUNTIME_DIR/ftdx3000/`, e.g. `/run/user/1000/ftdx3000/vcom1`): the connected radio
itself, so you choose your radio in other programs. A port can also be set to show an
FTDX3000 whatever the radio (*Programs see* in *Ports & sharing*), for programs already set up
for one.

- **WSJT-X / JTDX**: Rig = your radio (e.g. *Yaesu FTDX-3000*, *Icom IC-7300*), Serial port
  `/run/user/1000/ftdx3000/vcom1` (type it in), Handshake *None*, PTT *CAT*.
- **fldigi**: Hamlib, your radio, device as above.
- **flrig**: your radio, port as above.
- **Wine programs** (N1MM, Log4OM, CW Skimmer...): *Ports & sharing → Wine COM port
  mapping* links `COMn` in a Wine prefix to a virtual port.

How the sharing works:

- **Reads are cached.** Five programs polling `FA;` cost the radio one read.
- **Sets go out in order**, one at a time, never interleaved.
- **Auto-information is emulated per program.** One program turning `AI1;` on doesn't flood
  the others.
- **Block TX** per port, for a skimmer that should never key the radio. This blocks every way
  to transmit: PTT/MOX, tuner start, keyer and voice playback, and VOX.
- Virtual ports carry CAT only, so use CAT PTT in other programs (RTS/DTR on a PTY does
  nothing).

### 2. rigctld-compatible server

On `127.0.0.1:4532`. Anything with Hamlib can use rig **Hamlib NET rigctl** with no serial
port at all, for every supported radio. Tested against Hamlib 4.6's own `rigctl -m 2`.

## Remote operation over the LAN

On the computer with the radio, open **Remote (LAN) → Let other computers use this
radio**, set a password, and enable it. On any other computer, start the app and open
**Remote (LAN)**. The radio appears under *Radios found on your network*. Click
*Connect*.

- Everything in the remote app controls the remote radio, and the remote app's own virtual
  ports and rigctld server work too. You can run WSJT-X on the remote computer against
  its local `vcom1`.
- **Receive audio** streams to the remote computer's speakers (24 kHz, 16-bit PCM over UDP,
  about 400 kbit/s, with latency held to roughly 150 ms).
- **The MIC switch** (COMPUTER MIC) streams your microphone to the radio while you
  transmit with MOX, if the server accepts microphone audio. The **CW key** works too, if
  the server allows transmit.
- The server uses TCP+UDP 4533 for control and audio and UDP 4534 for discovery. If you run
  a firewall:

  ```bash
  sudo ufw allow 4533
  ```

  ```bash
  sudo ufw allow 4534/udp
  ```

- **Transmit needs a password.** A server without one is receive-only: remote users can
  listen and tune, but every transmit command (PTT, MOX, TUNE, keyer/voice playback, VOX)
  is refused.
- The password is checked with an HMAC-SHA256 challenge-response and never sent in clear.
  Five wrong guesses lock that address out for a minute.
- Traffic is not encrypted, and a fake server on your LAN could collect a response to try
  passwords against offline. Use it on your home network, not across the internet.
- **Un-keying.** If a remote client, rigctld client, or program on a virtual port keyed the
  radio and then disappears (crash, closed laptop, Wi-Fi drop), the radio is un-keyed. A
  silently dropped network connection is detected within 5 seconds.
- **No stale commands.** Commands sent while the radio is disconnected are discarded, not
  replayed later.

## Logger Connect (DX spots on the band scope)

RigHelm can show DX spots from your logging program on the band scope: a marker at each
spot's frequency, with its callsign as far as there's room. It only listens, and spots
change nothing by themselves. Turn it on in *Settings → Logger Connect*, then point your
logger at the address shown there:

- **QLog**: *Settings → Network → Notifications*, set **DX Spots** to `127.0.0.1:12060`,
  and **Spot Alerts** too if you use alert rules.
- **N1MM+**, and **DXLog** and **Logger32**, which send the same format: turn on their
  spot broadcast and send it to the same address. From another computer, choose *This
  computer and the network* and use this computer's address.

**Send a test spot** shows whether it all works, and the panel lists what has been heard
from each program.

- **Colours** show what your logger says: alert, new one, new band or mode, already
  worked. Hover over a marker for the details.
- **Click a marker to tune to it**, and to set the mode unless you turn that off.
- **Show** picks what appears: a spot shows if anything about it is ticked, in a mode
  that's ticked. For Parks on the Air only, click **None** and tick **POTA**. QLog sends
  every cluster spot, ignoring its own filters, so this is where to narrow them down.
- **Clicking a spot fills in the callsign in the logger**, when turned on, so QLog looks
  up the station ready to log it. This replaces what's in QLog's new contact form. It
  uses the port QLog listens to WSJT-X on, `127.0.0.1:2237` unless you changed it.

Spots only reach RigHelm while it runs, so the scope fills up over a few minutes.

## Band scope (RTL-SDR dongle or HackRF)

The FTDX3000 doesn't send its own scope over CAT. An inexpensive RTL-SDR dongle on one of
the radio's rear jacks gives RigHelm a wide band scope instead:

1. Connect the dongle's antenna input to the radio's **IF OUT** jack (RCA). If your radio
   has no IF OUT, use **RX OUT**. Never connect it to an antenna jack or anything that
   carries transmit power: 100 W destroys the dongle.
2. Plug the dongle into the computer's USB. That's all. RigHelm finds it, also when it is
   plugged in later, and a **BAND SCOPE** panel appears on the Operate page.
3. In *Settings → Band scope*, say which jack it is on (IF OUT is the default).

On screen:

- The amber line is the receive dial, with the filter passband shaded; a red line marks
  the transmit frequency when you work split. Outside the amateur bands is dimmed, and a
  thin strip under the spectrum shows the US band plan (blue CW/data, green phone) with
  the FT8/FT4 calling frequencies in purple (*Settings → Display* turns it off).
- The bright line is the spectrum, the faint one above it the recent peaks.
- *Settings → Display* can add a **Band** span, which shows the whole band whichever part
  you are tuned to. The other spans follow the dial as usual.
- **Click a signal to go straight to it.** Hover first and the scope marks the signal it
  found and shows where a click will tune. It looks at the signal at full resolution and
  tunes the way the mode needs:
  - **CW**: zero-beat (the signal heard at your CW pitch), then a final touch from the
    audio once the radio has settled.
  - **SSB**: onto the suppressed carrier, just past the edge of the voice.
  - **AM/FM**: onto the carrier.
  - **DATA**: inside an FT8/FT4 window, to its calling frequency; elsewhere the signal is
    centred in the passband.

  **Shift+click** tunes exactly where you click. **Scroll** tunes by the tuning step,
  **Ctrl+scroll** zooms. *Span → Full* shows everything the dongle covers (about 1.6 MHz).
- The view moves by itself to keep the dial in sight.
- While the radio transmits, the scope pauses (the picture would mean nothing).

The two jacks behave differently:

| jack | what the dongle sees | how the scope follows you |
|------|----------------------|---------------------------|
| IF OUT | the radio's 9 MHz first IF, before the roofing filter | the radio tunes it: the picture always sits around the dial, filtered by the radio's own front end |
| RX OUT | the receive antenna signal | the dongle retunes itself: the whole band when it fits (160–12 m), otherwise around the dial |

**Calibration** is automatic. Shortwave broadcast stations sit exactly on 5 kHz channels
and time signals (WWV, CHU) on exact frequencies, so the first time some are in view
(40 m at night next to the 41 m broadcast band, 20 m by day next to 22 m), RigHelm
measures them and corrects itself: the dongle's crystal on RX OUT (typically a couple of
ppm, which is 15–20 Hz on 40 m), the IF frequency on IF OUT. Everything is in
*Settings → Band scope*:

- **Calibrate now** measures again (a few seconds), and the result is shown beneath.
- **Reset** forgets the calibration; it calibrates itself again.
- **Dongle correction by hand** (ppm), if you'd rather set it yourself.
- IF OUT only: **IF frequency** and **Flipped**. If signals move the wrong way when you
  tune, change *Flipped*.

To check it, hover over a broadcast station's carrier: it should read on a 5 kHz step
(say 7.335.000), or WWV exactly 10.000.000.

Dongles:

- **RTL-SDR Blog V4** covers HF with its built-in upconverter.
- **RTL-SDR Blog V3** and other R820T dongles sample HF directly (the "Q branch"
  input). A no-name TV stick without that wiring shows only noise below 28.8 MHz.
- No driver or other program is needed: RigHelm talks to the dongle itself, on Linux and
  macOS alike, and takes it from the Linux TV driver if that grabbed it. With two dongles,
  pick one in Settings.
- **Linux permissions:** desktop distributions with systemd let the logged-in user open
  SDR dongles. If Settings says the dongle can't be opened, add a udev rule:

  ```bash
  echo 'SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0660", TAG+="uaccess"' | sudo tee /etc/udev/rules.d/60-rtl-sdr.rules
  ```

  then unplug and replug the dongle.
- **HackRF One** works too, the same way (pick it in Settings if a dongle is also plugged
  in). It has no gain control of its own: set its RF and baseband gain in *Settings → Band
  scope*. RigHelm only ever receives with it and keeps its antenna power off. On Linux, the
  `hackrf` package installs the udev rule it needs.

**Remote:** a computer controlling the radio over the LAN shows the radio computer's band
scope, with no dongle of its own. It asks for just the view it shows, so the stream is
about 130 kbit/s at the normal speed (250 at fast), over the same TCP port 4533. The dongle only runs
while somebody is looking at the scope.

## LDG tuner (AT-1000ProII, AT-600ProII)

These LDG tuners have a rear **Ext. Meter** jack (4-pin mini-DIN) for LDG's external meter.
It is also a serial port, and LDG published its protocol. With a USB-TTL serial adapter on
it, RigHelm shows the tuner's own power and SWR meter and its buttons on the Operate page:

- **TUNE** (tune from memory, or search) and **FULL** (a full search). Both transmit:
  RigHelm turns the radio's own tuner off, switches to CW at a low power (*Settings → LDG
  tuner*, 15 W unless you change it), holds the CW key down until the tuner says it's done
  (20 s at most), then puts the power, mode and break-in back. It keys the way the CW key
  does (menu 065 PC KEYING = RTS), so disarm the CW key first. Put an amplifier in
  standby first.
- **AUTO**, **MAN** (semi-automatic) and **BYP** (bypass), and **ANT 1/2**.
- The power meter switches to 1 kW full scale when an amplifier is in line. Its
  calibration (*Settings → LDG tuner → Meter calibration*) was measured on one
  AT-1000ProII with 10 W and 50 W carriers; above about 100 W it is approximate. SWR
  doesn't depend on it.

To set it up:

1. Wire a USB-TTL (5 V) serial adapter to the jack: pin 2 ground, pin 3 the adapter's TX
   (into the tuner), pin 4 the adapter's RX. Pin 1 carries 12 V: leave it unconnected, and
   don't use an S-Video cable (it joins pins). An RS-232 adapter needs a level converter.
2. In *Settings → LDG tuner*, choose the adapter's serial port.

Good to know:

- The tuner reads power and SWR only while you transmit. Between transmissions RigHelm
  lets the tuner's processor sleep, as LDG designed it, so it adds no digital noise to the
  receiver.
- Switched off at its power button, the tuner doesn't answer at all, so it can't be
  switched on from the computer. RigHelm shows it as off and finds it again when you
  switch it on.
- The tuner can't be asked its antenna or mode, so RigHelm shows what the tuner last
  reported to it, remembered across restarts. If you change them on the tuner's front
  panel, RigHelm shows the change after your next press of that button in RigHelm.
- RigHelm never takes the tuner's adapter for the radio when it looks for the radio.

## Testing without the radio

```bash
cargo run --release --bin righelm-sim
```

Then choose `/run/user/1000/ftdx3000/sim` as the CAT port. The simulator answers every
command in the CAT reference, supports auto-information, and rejects bad commands with
`?;`. It can be any other supported radio too, speaking that radio's own protocol (so any
program, or Hamlib, can be pointed at it as at the real one):

```bash
cargo run --release --bin righelm-sim -- --model ic7300
```

`--list` shows the models.

```bash
cargo test
```

Runs the protocol, engine, virtual port (with real PTYs), rigctld, remote control/audio and
CW keyer tests, and for every supported radio:

- the radio through the real engine and its translator against its simulator: every command
  the model says it has, every band and mode, reading and writing every menu item;
- Hamlib's own driver for the radio (`rigctl -m <model>`) against the simulator, and through
  a RigHelm virtual port (skipped if Hamlib isn't installed). Hamlib's drivers were written
  by people with the radios, so this is a check independent of RigHelm's own reading of the
  manuals;
- connecting to the simulator recognises the right model.

`RIGHELM_MODELS=ic7300,ft991a cargo test` checks just those radios.

A check against the real radio that never transmits: it detects the radio, runs RigHelm's
start-up, reads the whole menu and everything the screens use, writes back values the radio
already has, and reads it through the rigctld server. Every transmit command is dropped
before it reaches the radio. Close RigHelm first, because it needs the CAT port:

```bash
cargo run --release --example radio_check
```

With the radio connected to a dummy load, two hardware checks key it briefly at 5 W. Close
RigHelm first, because they need the CAT port:

```bash
cargo run --release --example cw_check
```

```bash
cargo run --release --example mic_check
```

Two checks for the band scope never send anything to the radio. The first opens the
dongle and prints what it sees (`if` or `rx` for the jack it is on, then the dial
frequency). The second shows simulated band activity instead of a dongle:

```bash
cargo run --release --example scope_check -- if 14200000
```

```bash
RIGHELM_SCOPE_SIM=1 cargo run --release --bin righelm
```

## Files

- Config: `~/.config/ftdx3000/config.toml` (override with `FTDX3000_CONFIG`).
- Menu backups default to `~/<radio>-menu-backup.txt` (`~/ftdx3000-menu-backup.txt` for the
  FTDX3000) and remember which radio they are from. Restore skips the CAT settings (037-040
  on the FTDX3000) unless you tick the box.

## Notes and limits

- Meter calibrations (SWR, ID, VDD) are approximate until checked against the radio; on the
  radios other than the FTDX3000 all meter scales are estimates.
- The CAT console and macros use RigHelm's language (FTDX3000 commands) whatever the radio.
- The computer CW key keys the radio on RTS of its second USB serial port (the Standard COM
  port of Yaesu's CP2105). Radios with a single CAT port (most Icom and Kenwood radios) can't
  be keyed that way yet; their own keyer and keyer memories still work.
- The FTDX3000 does not send its band scope over CAT; the band scope needs an RTL-SDR
  dongle on IF OUT or RX OUT.
- DATA-mode filter widths follow menu 067: PSK widths when DATA MODE = PSK, SSB widths
  when OTHER.

## License

RigHelm is free software under the [GNU General Public License v3 or later](LICENSE).
