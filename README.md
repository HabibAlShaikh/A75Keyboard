# VGN A75 — firmware analysis + Linux control app

[![Latest release](https://img.shields.io/github/v/release/HabibAlShaikh/A75Keyboard?display_name=tag)](https://github.com/HabibAlShaikh/A75Keyboard/releases/latest)

This workspace analyses the **VGN A75 eSports Hall-effect gasket-mount
keyboard** (ROYUAN `3151:4010 / 4011 / 4015`) and provides a fast native
Linux controller in the spirit of Wootility.

## What is here

```
A75Keyboard/
├── a75ctl-rs/                      Rust app
│   ├── src/protocol.rs             HID protocol, discovery, RGB, matrix
│   ├── src/input.rs                live key + analog readers, 75% layout
│   ├── src/socd.rs                 evdev/uinput SOCD interceptor
│   ├── src/app.rs                  egui UI
│   └── README.md
├── docs/
│   ├── FIRMWARE.md                 full firmware + protocol write-up
│   └── WINDOWS_VHUB_PROTOCOL.md    official Windows VHUB analysis
├── packaging/
│   ├── 70-vgn-a75.rules            udev permissions (hidraw + evdev + uinput)
│   └── install.sh                  release tarball installer
├── firmware/                       main + MLED firmware, update ZIP, ELF views
├── tools/make_elf.py               reproduce the REA ELF wrapper
├── .github/workflows/release.yml   tag-driven release build
└── README.md
```

The large vendor/reference binaries are **not committed**:

- `VGN_VHUB_4.0.9.zip` — official Windows VHUB package used for the REA
  analysis
- `Wootility-5.4.2.AppImage` — UX reference only

Place them in this directory if you want to reproduce the extraction steps;
their hashes are recorded in [`docs/FIRMWARE.md`](docs/FIRMWARE.md).

## Quick start (Rust)

```bash
cd a75ctl-rs
cargo build --release
./target/release/a75ctl
```

One-time permissions:

```bash
sudo rm -f /etc/udev/rules.d/99-vgn-a75.rules   # old late rule, if present
sudo cp ../packaging/70-vgn-a75.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
# unplug/re-plug the keyboard
```

The rule must be named **70**-vgn-a75.rules so it runs before
`71-seat.rules`; a `99` input rule is too late to get the `uaccess` ACL. It
also grants access to the A75 evdev node and `/dev/uinput`, which the optional
Advanced Keys SOCD interceptor needs.

Probes:

```bash
./target/release/a75ctl --status
./target/release/a75ctl --selftest
```

## Releases

Tagged builds (`v*`) are built by GitHub Actions and published on the
[Releases page](https://github.com/HabibAlShaikh/A75Keyboard/releases). The
release assets include:

- `a75ctl-v*-x86_64-linux.tar.gz` — portable tarball with binary, udev rule
  and `install.sh`
- `a75ctl_*_amd64.deb` — Debian/Ubuntu package

Install from the tarball:

```bash
tar -xzf a75ctl-v*-x86_64-linux.tar.gz
cd a75ctl-v*-x86_64-linux
sudo ./install.sh
# replug the keyboard
a75ctl
```

Install the Debian package:

```bash
sudo apt install ./a75ctl_*_amd64.deb
# replug the keyboard
a75ctl
```

Use a different destination prefix with `PREFIX` for the tarball installer:

```bash
sudo PREFIX="$HOME/.local" ./install.sh
```

Windows `.exe` builds are not available yet. The current controller talks to
`/dev/hidraw*` and uses Linux `evdev`/`uinput` for SOCD, so a Windows binary
needs a separate HID backend and key-suppression layer.

## What the app can do

- Persistent Wootility-style keyboard map with per-key depth labels
- Multi-select (click, Ctrl+Click, marquee, Select All/Deselect All/WASD/Invert)
- Batch per-key actuation with Mixed-value handling
- Actuation point, release point, bottom deadzone
- Rapid trigger with press/release sensitivity
- Performance tab: Tachyon (1000 Hz + solid lighting), polling rate, debounce
- Profiles tab: save/load local tuning presets
- Advanced Keys tab: SOCD pair builder, resolution modes, map badges, and the optional evdev/uinput interceptor
- **Per-key** actuation/RT for a key selected on the live layout
- Live analog depth marker on the 4.0 mm travel diagram while Analog Test is running
- Quick presets: WASD 0.1 mm, Caps Lock 3.0 mm, Tachyon/max polling
- Toggle hold / toggle dots per key
- **Key remap** — any physical key to any standard HID key (e.g. Caps -> Home),
  with a saved remap file and factory-matrix restore
- **RGB**: instant global effects, colour picker, brightness, speed,
  per-key colour painting
- **Live key test**: the A75 layout lights up and fills on each keystroke,
  with last key, event age, press count, heatmap, rollover and event log.
  The on-screen layout has no PrtSc key and uses Insert where a 75% board
  would normally put Home, matching the physical A75.
- **Analog test**: raw Hall telemetry graph from the vendor input endpoint
- Firmware/MLED version, profile, report rate, debounce, calibration

SOCD pairs can be configured and are shown on the map. The A75 firmware does
not implement SOCD, so the app now includes an optional evdev/uinput
interceptor for Last Input Priority and Neutral; enable it in Advanced Keys.
Rappy Snappy additionally needs per-key analog depth that the firmware does
not expose. Also not implemented: DKS/Mod-Tap actions (simple one-to-one
remaps are supported), analog controller emulation/custom curves,
multi-layer remapping/macros/share codes.

The Rust app is multi-threaded: HID commands run on a worker thread and the
input readers run on their own thread, so sliders and the keyboard view stay
responsive.

## Key protocol findings

- Firmware is a dual-slot Thumb-2 ARM image: 64 KiB bootloader + 64 KiB
  aligned application (`0x01010000`). Firmware v1.12, MLED v1.05.
- Vendor HID interface (`Usage Page 0xFFFF / Usage 0x0002`) takes 64-byte
  feature reports; Linux buffer is 65 bytes with report ID 0.
- Checksum: two's complement of bytes `0..6` in byte **7**; for commands
  `0x07`/`0x08` the checksum is in byte **8**.
- A75 uses the **shifted v2 command map**:
  - main RGB `SET_LEDPARAM = 0x07`, `GET_LEDPARAM = 0x87`
  - report `0x04/0x84`, profile `0x05/0x85`, option `0x06/0x86`
  - key matrix `0x89`, per-key colour `0x14/0x8C`
- RGB packet: `[1]=mode [2]=speed [3]=brightness [4]=option
  [5..7]=RGB [8]=checksum`.
- Live keyboard reports: 8-byte boot keyboard on `/dev/hidraw0`.
- Analog stream: vendor input `/dev/hidraw1`, `05 1B <hi> <lo>`.
- `0xA3` (`GET_KEYSTROKE`) is not implemented and stalls the control pipe;
  the app does not send it.

Full details and REA evidence IDs: [`docs/FIRMWARE.md`](docs/FIRMWARE.md).

### Official Windows VHUB 4.0.9

The official Windows package was unpacked and analysed with REA. The shipped
`app.asar` contains **no A75 USB match entry**; its Rongyuan path expects
usage page `0xFFFF` / usage 1 and 520-byte feature reports, while the A75 uses
usage 2 and 64-byte feature reports. The newer SDK-v2 path contains exact
SOCD/DKS/MT/MPT/TGL/END/RS and rapid-trigger packet builders for other VGN
keyboards, but those are **not A75 firmware features** and must not be sent to
the A75.

The packet tables, source offsets, safety notes and a Linux implementation
module are in [`docs/WINDOWS_VHUB_PROTOCOL.md`](docs/WINDOWS_VHUB_PROTOCOL.md)
and [`a75ctl-rs/src/vgn_hid.rs`](a75ctl-rs/src/vgn_hid.rs).

## Notes

Only one process should own the vendor feature interface at a time. Close the GUI
before running `--status` or `--selftest`; concurrent feature-report users can
time out and stall the control pipe.

## Wootility note

Wootility is Wooting's own configurator and cannot drive a VGN/ROYUAN
keyboard. This project reuses the *idea* (analog actuation, rapid trigger,
live test, RGB) against the A75 protocol. If you also own a Wooting board,
keep using the Wootility AppImage for it.

## License

MIT for this repository's original code and documentation. See [`LICENSE`](LICENSE).
Vendor firmware and vendor applications retain their own terms and are not
relicensed by this project.
