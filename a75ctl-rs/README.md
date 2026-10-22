# A75 Control (Rust)

Fast, native Linux controller for the **VGN A75 Hall-effect** keyboard
(ROYUAN `3151:4010 / 4011 / 4015`).

This is the optimized replacement for the Python prototype in `../a75ctl`.
It keeps all HID work on a background thread, so the UI stays at 60 fps while
the keyboard is being configured.

## Features

- **Wootility-style workspace** — persistent keyboard map at the top, a
  four-tab navigation sidebar (Actuation, Advanced Keys, Performance, Profiles)
  and a context-sensitive settings area below. Existing lighting, remap, key
  test and device tools remain under Tools.
- **Multi-select keyboard map** — click one key, Ctrl+Click to toggle keys,
  drag a marquee over empty space, or use Select All / Deselect All / WASD /
  Invert Selection. Selected keys get a bright border.
- **Per-key depth on every keycap** — each key shows its current actuation
  depth (for example `1.5mm`); SOCD-bound keys also get a colour-coded badge.
- **Responsive egui UI** — real sliders with drag + numeric entry, no
  blocking on HID reads.
- **Actuation** — press/release point, bottom deadzone, rapid trigger with
  separate press/release sensitivity, presets, travel diagram.
- **Per-key and batch tuning** — edit all selected keys at once. The
  Actuation slider shows `Mixed` when selected keys differ; moving it resolves
  the mixed state and overwrites the whole selection. When every physical key
  is selected, Apply uses one global `0x1A` packet instead of 82 per-key
  writes. Quick presets include WASD at 0.1 mm and Caps Lock at 3.0 mm.
- **Profiles** — save/load local presets containing per-key actuation, rapid
  trigger, deadzone, polling, debounce, Tachyon state and SOCD pairs.
- **Advanced Keys** — SOCD pair builder with Last Input Priority, Deeper
  Press (Rappy Snappy) and Neutral options, a pair list with Delete, and
  badges on the keyboard map. **The A75 firmware has no SOCD command**, so
  the Rust app now includes an optional Linux **evdev/uinput interceptor**:
  it grabs `/dev/input/event*` for the A75, applies Last Input Priority or
  Neutral, and forwards everything else to a virtual keyboard. Enable it
  with the checkbox in Advanced Keys. Rappy Snappy still needs live per-key
  analog depth, which the firmware does not expose, so that mode is stored
  but passed through.
- **Advanced key modes** — toggle hold / toggle dots for the selected keys,
  plus a Normal mode button to clear those flags.
- **Key remap** — choose any physical key and any standard HID target
  (for example Caps Lock → Home). The complete matrix is written to the
  keyboard, saved to `~/.config/a75ctl/remap.bin`, and re-applied on startup.
  A factory matrix is embedded so individual keys or the whole board can be
  restored.
- **Performance** — Tachyon toggle (1000 Hz + simplified solid lighting),
  polling rate, bottom deadzone and debounce. The travel diagram uses a 4.0 mm
  scale and shows a live cyan marker while Analog Test is running. The A75 hardware maximum is
  1000 Hz; the UI explicitly says it is not a true sub-1 ms scan.
- **RGB** — instant global effects (solid, breath, neon, wave, ripple,
  raindrop, snake, press action, coverage, sine, kaleidoscope, laser,
  circle wave, dazzling, rain down, meteor, user colour, …), colour picker,
  brightness and speed.
- **Per-key colour** — click a key on the live layout and paint it, or paint
  the whole board (slower; progress bar in the status bar).
- **Live key test** — the actual A75 75% layout lights up and fills from the
  bottom on each keypress, with last key, event age, press count, per-key
  press heatmap, rollover and an event log.
- **Analog test** — enables the vendor `SET_MAGNETISM_REPORT` stream and
  plots the raw 16-bit telemetry from the keyboard's vendor input endpoint.
  The keyboard stops typing while this mode is active.
- **Device panel** — firmware, MLED firmware, device id, profile, report
  rate, debounce, key matrix, calibration, reconnect.
- **CLI probes** — `--status`, `--selftest`.

## Build

```bash
cd a75ctl-rs
cargo build --release
./target/release/a75ctl
```

Dependencies are only `eframe`/`egui` and `libc`; HID is done directly with
`ioctl`/`read` on `/dev/hidraw*` (no hidapi, no vendor daemon, no Wine).

## Permissions

Install the udev rule from the Python folder (or copy it here):

```bash
sudo rm -f /etc/udev/rules.d/99-vgn-a75.rules   # remove the old late rule if present
sudo cp ../a75ctl/70-vgn-a75.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
# unplug/re-plug the keyboard
```

The rule must be installed as **70**-vgn-a75.rules: the `uaccess` tag has to
exist before systemd's `71-seat.rules` runs. A `99-vgn-a75.rules` input rule is
too late and gives `event2` a `uaccess` tag without an ACL. The rule grants
access to the A75 evdev keyboard and `/dev/uinput` for the SOCD interceptor.

For a temporary test without replugging:

```bash
sudo setfacl -m u:$USER:rw /dev/input/event2
```

## CLI

```bash
./target/release/a75ctl --status
./target/release/a75ctl --nodes
./target/release/a75ctl --selftest      # RGB red -> green -> orange, then actuation
./target/release/a75ctl --rgb-test      # colour steps + readback
./target/release/a75ctl --perkey-test   # A at 0.1 mm, then restore
./target/release/a75ctl --wasd-test     # global 2.0 mm + WASD at 0.1 mm
./target/release/a75ctl --toggle-test   # F12 toggle-dots, then restore
./target/release/a75ctl --paint-test    # 12 per-key colour writes
./target/release/a75ctl --dump-matrix   # active 86-key matrix records
./target/release/a75ctl --remap 0x39 0x4a   # Caps -> Home
./target/release/a75ctl --restore-matrix    # write the embedded factory matrix
```

Only one process can run at a time (a `flock` lock is used); close the GUI
before running a CLI probe.

## Wootility-inspired look

The Wootility 5.4.2 AppImage was unpacked (`resources/app.asar`) and its CSS
theme was used as the visual reference: navy `#10151e` / `#1a2331` surfaces,
`#5b51ff` selection, `#ffd45c` action colour, gold keycaps, a left navigation
rail and a "Save to keyboard" button. The Rust implementation is original
egui code; no Wootility assets or code are copied.

## Official Windows VHUB protocol analysis

The official `VGN_VHUB_4.0.9.zip` Windows build was unpacked and analysed with
REA. The detailed packet tables are in
[`../a75ctl/docs/WINDOWS_VHUB_PROTOCOL.md`](../a75ctl/docs/WINDOWS_VHUB_PROTOCOL.md).

Key findings:

- The shipped Windows app has **no A75 USB match entry**. Its Rongyuan path
  expects usage page `0xFFFF` / usage **1** and 520-byte feature reports; the
  A75 exposes usage **2** and 64-byte feature reports.
- The Windows build's newer SDK-v2 path (`flash75` / `flashextreme`, usage page
  `0xFFA0` / `0xFFB0`) does contain exact SOCD, DKS, MPT, MT, TGL, END, RS and
  rapid-trigger builders. These are **not A75 firmware features** and must not
  be sent to the A75. The A75 command map is different: its raw command byte
  `0x2C` is flash erase, and SDK-v2 group `4`/`6` packets mean different A75
  commands.
- The A75's validated per-key actuation command remains `0x1A`; its polling
  command remains `0x04`. The A75 has no SOCD or Tachyon command.

A standalone Linux implementation of both packet families, including the
safety classification gate and hidraw transport, is in
[`src/vgn_hid.rs`](src/vgn_hid.rs). It includes exact A75 builders and the
official SDK-v2 builders, plus byte-layout unit tests.

## Protocol notes

The A75 uses the **shifted v2 command map** for report/profile/option/LED:

| Function | Set | Get |
|---|---|---|
| report rate | `0x04` | `0x84` |
| profile | `0x05` | `0x85` |
| keyboard option | `0x06` | `0x86` |
| **main RGB** | **`0x07`** | **`0x87`** |
| side/logo light | `0x08` | `0x88` |
| key matrix | `0x09` | `0x89` |
| per-key colour | `0x14` | `0x8C` |
| actuation/RT | `0x1A` | — |
| key magnetism mode | `0x1D` | `0x9D` |
| debounce | `0x11` | `0x91` |

RGB packet (`0x07`, checksum at byte **8**):

```
[1] effect mode
[2] speed field = 5 - speed
[3] brightness (0..4)
[4] (option << 4) | 0x07   // 0x07 = NORMAL/use RGB; 0x00 = COMMONCOLOR[0] = red
[5] R  [6] G  [7] B
[8] checksum
```

Leaving `[4]` at 0 makes every solid colour red; this was the earlier bug.
"Lights off" sends solid black first, then mode 0, to clear per-key
overrides.

### Key matrix / remap

`GET_KEYMATRIX 0x89` returns 8 pages; the first **504 bytes** are the
keymap (126 four-byte records), followed by an 8-byte footer. Each record
is `[flags, ?, HID usage, ?]`; a normal key is `00 00 <usage> 00`.

`SET_KEYMATRIX 0x09` is a 9-page upload. Each 64-byte report is:

```
[0]=0x09  [1]=profile  [2]=0xF8  [3]=0x01  [4]=page  [7]=checksum
[8..63] = 56 bytes of matrix data
```

The firmware stages the data and applies it after the last page. A readback
immediately after the write can still return the old matrix; the app keeps a
local shadow and treats the saved remap list as authoritative. The A75 has
no PrtSc key and uses **Insert** where many 75% boards put Home; both the
on-screen layout and the factory matrix reflect that.

Verified effects on v1.12: solid, breath, wave, ripple, etc. `GET 0x87`
returns the stored state after ~1 s.

Live keyboard input is read from `/dev/hidraw0` (8-byte boot keyboard
reports). Analog telemetry is read from the vendor input node as
`05 1B <hi> <lo>` (16-bit value).

Full firmware analysis: [`../a75ctl/docs/FIRMWARE.md`](../a75ctl/docs/FIRMWARE.md).

## Feature coverage vs. Wootility

Implemented on the A75 protocol:

- Per-key actuation and per-key rapid trigger (press/release sensitivity)
- Global actuation / rapid trigger / bottom deadzone
- Toggle hold and toggle dots
- OS-level SOCD interceptor: Last Input Priority and Neutral (evdev/uinput)
- Report-rate (Tachyon = 1000 Hz on this board), debounce, profiles 0–3
- Main RGB effects and per-key colour table
- Live digital key test + heatmap + raw analog telemetry
- Simple key remapping (one physical key to one standard HID key)

Not exposed by A75 VHUB 4.0.9 / not implemented here:

- **SOCD firmware support** — the A75 firmware has no SOCD packet. The
  optional evdev/uinput interceptor in this app provides Last Input Priority
  and Neutral at the OS level; Rappy Snappy (deeper-press priority)
  additionally needs live per-key analog depth, which the A75 does not
  stream to us.
- DKS / Mod-Tap action remapping — simple one-to-one remaps are implemented,
  but assigning the four DKS/Mod-Tap actions is not.
- Analog thumbstick/controller emulation and custom response curves — need a
  virtual gamepad; the raw analog value mapping is not fully decoded. The
  travel diagram's live marker maps the raw u16 value linearly to 0–4 mm as an
  approximate visual aid.
- Multi-layer remapping, macros, share codes
- Third-party Hall-switch profiles and top deadband (only the bottom deadzone
  is known in the `0x1A` packet)
- 8 kHz polling (the A75 firmware uses 1000 Hz maximum)

Only one instance may run at a time; a `flock` lock prevents two controllers
from fighting over the feature interface.

## Notes

Only one process should own the vendor feature interface at a time.
Close the GUI before running `--status`, `--selftest`, or the legacy Python
tool; concurrent feature-report users can time out and stall the control pipe.

## Safety

- No firmware flashing.
- If the control pipe stalls, unplug/re-plug. This can happen if an
  undefined command is sent (e.g. `0xA3` on this firmware).
- Per-key colour writing is paced because the LED controller NAKs if driven
  too fast; global effects are instant.
