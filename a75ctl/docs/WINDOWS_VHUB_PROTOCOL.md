# VGN VHUB 4.0.9 Windows build — USB HID protocol notes

This document records the USB/HID protocol extracted from the official VGN
VHUB 4.0.9 Windows package and cross-checks it against the **A75 firmware
v1.12** that is actually attached to this machine.

> **Safety first:** the official VHUB build contains several different
> protocols for different keyboard generations. They are not interchangeable.
> **Do not send the newer SDK-v2 / KB2 packets to an A75.** The A75 command
> map is different: an A75 feature packet whose byte 0 is `0x2C` is
> `SET_FLASHCHIPERASSE`, while SDK-v2 group `4`/`6` packets are interpreted
> as A75 SET_REPORT/SET_KBOPTION. The A75 firmware has no SOCD command. The
> A75 implementation in this repository deliberately does not send one.

## 0. Artifact and evidence

The package `VGN_VHUB_4.0.9.zip` contains a single NSIS installer. Extracting
it yields an Electron application:

| Item | Value |
|---|---|
| Installer | `VGN_VHUB_installer.exe` (NSIS 3, x86) |
| Application payload | `app/resources/app.asar` |
| `app.asar` SHA-256 | `18fd179a35d185a669073f66e65a8bcabfd446f1d86492b7da9dd3eb36fc1995` |
| SDK analysed | `node_modules/@sparklinkplayjoy/sdk-keyboard-v2/dist/esm/index.js` |
| SDK SHA-256 | `415c0ee51e62690300a222a2018793236bb2042427cc0dcdcda33b69d9c87093` |
| REA application evidence | `ev_e4e201c2e63592779bf0852ebcdafe7b1291798433c736fd5af04313c94a08f8` |

REA reconstructed the selected JavaScript sources without executing them.
Coverage is static, not a live capture. The official application is an
Electron/WebHID client; the Linux implementation below uses `hidraw`.

## 1. Device initialization

### 1.1 The A75 on this machine (observed)

The live descriptor and sysfs data are unambiguous:

| Property | Value |
|---|---|
| VID | `0x3151` (12625, ROYUAN) |
| PID | `0x4015` (16405) |
| Product | `ROYUAN VGN A75` |
| HID interfaces | interface 0 keyboard, interface 1 vendor input, **interface 2 vendor feature** |
| Target usage page | `0xFFFF` |
| Target usage | `0x0002` |
| Report type | **feature**, no report ID, 64 bytes |
| Feature report descriptor | `06 FF FF 09 02 A1 01 09 02 15 80 25 7F 95 40 75 08 B1 02 C0` |
| Firmware | v1.12, MLED v1.05 |

Linux nodes on the tested unit:

```
/dev/hidraw0  interface 0  keyboard input
/dev/hidraw1  interface 1  vendor analog input
/dev/hidraw2  interface 2  usage 0xFFFF / 0x0002, 64-byte feature
```

The A75 feature transport is `HIDIOCSFEATURE` / `HIDIOCGFEATURE` with a
65-byte ioctl buffer: one leading report ID byte (0) followed by the 64-byte
packet.

### 1.2 What the official Windows build actually matches

The shipped `app.asar` contains a built-in USB table. Its only `vid: 12625`
entries are Neon75 variants:

| Product ID | Product |
|---|---|
| 20525, 20527 | VGN Neon75 |
| 20536 | VGN Neon75 Dongle |
| 20564 | VGN Neon75 Extreme Tri-mode |
| 20565 | VGN Neon75 Extreme 2.4G Dongle |

There is **no `productId: 16405` (0x4015)** entry. The official VHUB 4.0.9
build therefore does not contain a shipped A75 USB match record.

The app then has two different `rongyuan` paths:

1. **RyServe** — selected for `customDeviceType == "rongyuan"`. It requires
   usage page `65535` (`0xFFFF`) **and usage 1**, and its `sendCmd()` pads
   every command to **520 bytes** before calling
   `device.sendFeatureReport(reportId, data)`.
2. **SDK v2** — selected for `customDeviceType == "flash75"` /
   `"flashextreme"`. It requires usage page `65440` (`0xFFA0`) or `65456`
   (`0xFFB0`), usage 1, and uses 64-byte **output** reports through
   `device.sendReport(0, report)`.

The A75 exposes page `0xFFFF`, **usage 2**, and a **64-byte** feature report.
It matches neither path. Consequently the A75 protocol is the older VGN web
VHUB protocol documented in `a75ctl/docs/FIRMWARE.md`, not the newer RyServe
or SDK-v2 packet format.

Source locations:

* `dist/renderer/assets/index-BQEzNOIS.js` lines 35389–35419 — Neon75 USB
  table, VID 12625, no 0x4015.
* `dist/renderer/assets/home-eFhDeR83.js` line 40693 — Rongyuan feature
  interface requires `usage === 1 && usagePage === 65535`.
* `dist/renderer/assets/home-eFhDeR83.js` lines 15259–15286 — RyServe pads to
  520 and calls `sendFeatureReport`/`receiveFeatureReport`.
* `dist/renderer/assets/home-eFhDeR83.js` lines 40724–40768 — `flash75`
  (`0xFFA0`) and `flashextreme` (`0xFFB0`) use SDK v2.

## 2. Transport framing

| Protocol | HID report | Report ID | Length | Linux call |
|---|---|---|---|---|
| A75 legacy | **feature** | 0 (no report ID in descriptor) | 64 | `HIDIOCSFEATURE`, buffer 65 |
| SDK v2 (`flash75`/`extreme`) | **output** | 0 | 64 | `write()` on hidraw, buffer 65 |
| RyServe (`rongyuan` Neon75) | **feature** | 0 | 520 | `HIDIOCSFEATURE`, buffer 521 |

The A75 does not expose an output-report path on its vendor interface. Do not
use `write()` for `0x1A`; it will not work. The SDK-v2 code builds 64-byte
arrays with `fe()` (a `Uint8Array(64).fill(0)` plus byte copies) and calls
`this.device.sendReport(0, e)`, not `sendFeatureReport`.

## 3. A75 legacy protocol (validated on firmware v1.12)

### 3.1 Per-key actuation / rapid trigger — SET_MAGNETISM_MODE `0x1A`

This is the exact packet already used by `a75ctl-rs/src/protocol.rs` and
verified on the A75. It is a 64-byte feature report.

| Byte | Meaning |
|---|---|
| 0 | `0x1A` |
| 1 | profile (0..3) |
| 2 | `1` = settings present |
| 3 | mode flags: `0` normal, `2` DKS, `3` MT, `4` toggle-hold, `5` toggle-dots, `6` gamepad; `0x80` = rapid trigger |
| 4 | actuation point, raw |
| 5 | `1` = all keys, `0` = per-key / gamepad |
| 6 | A75 matrix index (0 when all) |
| 7 | checksum = `0xFF - sum(bytes 0..6)` |
| 8 | release/lift point, raw |
| 9 | rapid-trigger press sensitivity, raw |
| 10 | rapid-trigger release sensitivity, raw |
| 11 | dynamic travel (`value - 2`) |
| 12..15 | trigger-mode bytes |
| 16 | MT timer (default 20) |
| 17..24 | gamepad depth/analog pairs |
| 25 | bottom deadzone, raw |
| 26..63 | zero |

Encoding:

```
actuation / release raw = round(10 * (mm - 0.1))   # 0.1 mm units
rapid-trigger raw       = round(10 * mm)           # 0.1 mm units
deadzone raw            = round(10 * mm - 1)       # 0 disables
```

| Value | Raw |
|---|---:|
| 0.1 mm actuation | `0x00` |
| 1.5 mm actuation | `0x0E` |
| 2.0 mm actuation | `0x13` |
| 4.0 mm actuation | `0x27` |
| 0.2 mm RT press | `0x02` |
| 0.3 mm RT release | `0x03` |
| 3.7 mm deadzone | `0x24` |

The matrix index is the A75 slot from `GET_KEYMATRIX 0x89`; on the tested
unit A = index 9. It is not a HID usage code. The 504-byte matrix is an array
of 4-byte records; the key's HID usage is stored in `record[2]`, and the
index is `record_offset / 4`. The A75 populates 86 records; unused records are
zero. `a75ctl-rs/factory_matrix.bin` is the embedded factory copy, and the
controller resolves a selected HID usage to its current matrix index with
`usage_index()`.

The firmware also accepts DKS/MT/gamepad **mode flags** in byte 3 and has
slots for their parameters at bytes 12..24, but the action-assignment
semantics of those slots are not fully recovered. The controller writes the
normal + rapid-trigger subset only. No SOCD command exists in this firmware.

### 3.2 Report rate — SET_REPORT `0x04`

| Byte | Meaning |
|---|---|
| 0 | `0x04` |
| 1 | profile |
| 2 | rate index: `1`=1000 Hz, `2`=500 Hz, `4`=250 Hz, `8`=125 Hz |
| 7 | checksum |

Read back with `0x84`. The A75 maximum is 1000 Hz. There is no Tachyon
command in the A75 firmware.

### 3.3 Checksum

For most A75 commands, including `0x1A`, `0x04`, and `0x09`, the checksum is
at byte 7:

```
p[7] = 0xFF - sum(p[0..7])   (wrapping u8)
```

Only RGB commands `0x07` and `0x08` use byte 8. `protocol.rs` implements this
with `checksum_index()`.

### 3.4 Absent on the A75

* **SOCD** — no command. An A75 feature packet with byte 0 = `0x2C` is the
  flash-erase command; never send a raw A75-framed `0x2C`.
* **Tachyon / sub-1 ms** — no command; hardware tops out at 1000 Hz.
* **8 kHz polling** — not supported.
* **Rappy Snappy / Deeper Press Priority** — requires per-key analog depth
  telemetry that the A75 does not stream to the controller.

## 4. Official SDK v2 (newer VGN keyboards; NOT the A75)

The SDK uses command groups. Relevant constants from
`@sparklinkplayjoy/sdk-keyboard-v2`:

| Name | Value | Meaning |
|---|---:|---|
| `Performance` | 4 | performance group (`oi`) |
| `HigherKey` | 6 | higher-key group (`Ar`) |
| `Global` | 2 | global settings (`Ce`) |
| `READ` | 1 | read subcommand |
| `WRITE` | 2 | write subcommand |

Higher-key modes (`Q`):

| Mode | Value |
|---|---:|
| NONE | 0 |
| DKS | 1 |
| MPT | 2 |
| MT | 3 |
| TGL | 4 |
| END | 5 |
| SOCD | 6 |
| RS | 7 |

SOCD resolutions (`$`):

| Name | Value | Packets' byte 13 pair |
|---|---:|---|
| Cover | 0 | `[0, 0]` |
| PriorityA | 1 | `[1, 2]` |
| PriorityB | 2 | `[2, 1]` |
| Neutral | 3 | `[3, 3]` |

### 4.1 PERFORMANCE / SET_PERFORMANCE

64-byte output report, report ID 0.

| Byte | Meaning |
|---|---|
| 0 | `4` (Performance) |
| 1 | `2` (SetPerformance) |
| 2 | row |
| 3 | col |
| 4 | mode |
| 5..6 | normal press, mm × 1000, u16 LE |
| 7..8 | normal release, mm × 1000, u16 LE |
| 9..10 | RT first touch, mm × 1000, u16 LE |
| 11..12 | RT press, mm × 1000, u16 LE |
| 13..14 | RT release, mm × 1000, u16 LE |
| 15..16 | press dead stroke, mm × 1000, u16 LE |
| 17..18 | release dead stroke, mm × 1000, u16 LE |
| 19 | axis |
| 20 | calibrate |
| 21..22 | axis v2 id, u16 LE |
| 23..24 | axis range max, u16 LE |
| 25..26 | axis coefficient, u16 LE |
| 27..63 | zero |

Encoding is **micrometres**, not tenths of a millimetre:

```
u16 = round(mm * 1000)
0.1 mm -> 100  = 0x0064
4.0 mm -> 4000 = 0x0FA0
```

The read-side builder divides each u16 by 1000, confirming the unit.

### 4.2 HIGHER_KEY / WRITE packets

All are 64-byte output reports. Header is
`[6, 2, row, col, mode]`; the two-key variants add `[row2, col2]` at bytes
5–6.

#### DKS (mode 1)

| Bytes | Meaning |
|---|---|
| 0..4 | `6, 2, row, col, 1` |
| 5..6 | kc1 u16 LE |
| 7..8 | kc2 u16 LE |
| 9..10 | kc3 u16 LE |
| 11..12 | kc4 u16 LE |
| 13..16 | trp1..trp4 (one byte each) |
| 17..18 | deadband 1: ms × 1000, u16 LE |
| 19..20 | deadband 2: ms × 1000, u16 LE |
| 21..63 | zero |

#### MPT (mode 2)

`6, 2, row, col, 2`, then three kcs (bytes 5..10, u16 LE) and three
deadbands (bytes 11..16, ms × 1000, u16 LE).

#### MT (mode 3)

`6, 2, row, col, 3`, kc1 at 5..6, kc2 at 7..8, time at 9..10 (u16 LE).

#### TGL (mode 4)

`6, 2, row, col, 4`, kc at 5..6, time at 7..8.

#### END (mode 5)

`6, 2, row, col, 5`, kc1 at 5..6, kc2 at 7..8, delay at 9..10.

#### SOCD (mode 6) — two packets

The official code sends **two** reports, one for each key:

Packet 1:

| Byte | Meaning |
|---|---|
| 0 | `6` |
| 1 | `2` |
| 2 | row1 |
| 3 | col1 |
| 4 | `6` |
| 5 | row2 |
| 6 | col2 |
| 7..8 | keycode1 u16 LE |
| 9..10 | keycode2 u16 LE |
| 11..12 | delay u16 LE |
| 13 | resolution pair[0] |
| 14..63 | zero |

Packet 2 swaps the two keys:

`6, 2, row2, col2, 6, row1, col1, kc2, kc1, delay, resolution[1]`.

The bytes at 7..12 are 16-bit HID usages/keycodes, not A75 matrix indices.
The official UI supplies `row`/`col` from the device key table.

#### RS (mode 7) — two packets

Same layout as SOCD but byte 4 = `7` and byte 13 is left zero; packet 2 swaps
the keys.

### 4.3 Polling rate — GLOBAL / ReportRateSetting

`[2, 5, 2, rate_code]` in a 64-byte output report.

| Rate | Code |
|---|---:|
| 8000 Hz | 0 |
| 4000 Hz | 1 |
| 2000 Hz | 2 |
| 1000 Hz | 3 |
| 500 Hz | 4 |
| 250 Hz | 5 |
| 125 Hz | 6 |

`GFSHighPollingRateReset` is `[2, 15, 1]`; it is a high-polling-rate reset,
not a Tachyon command. The extracted SDK has no literal `Tachyon` command.
For newer devices the closest feature is selecting an 8 kHz rate.

## 5. RyServe 520-byte protocol (Neon75, not A75)

For completeness, the `rongyuan` branch uses a different 520-byte feature
protocol. Its command IDs include:

| Command | Value |
|---|---:|
| SetReturnRate | 3 |
| GetReturnRate | 131 |
| SetKey | 10 |
| SetKeyOption | 9 |
| SetMagneticAxisTravel | 101 |
| GetMagneticAxisTravel | 229 |

The higher-key helper in that package is KB2: its frame starts with
`0x5C` (head), then length, command, CRC and payload, and it allocates 64
bytes. `KB2_CMD_SOCD = 0x2C` is therefore the **third byte** of a KB2 frame,
not the first byte of an A75 packet. It is still wrong for the A75 because the
frame, report size (520 feature for RyServe) and command map are different.

The A75's 64-byte `0x1A` command is **not** this protocol. Do not reuse these
IDs on the A75 merely because the vendor ID is the same.

## 6. Linux implementation module

`a75ctl-rs/src/vgn_hid.rs` contains a standalone, compiled implementation:

* `A75_VID`, `A75_PID`, `A75_USAGE_PAGE`, `A75_USAGE`,
  `A75_FEATURE_INTERFACE`, `A75_FEATURE_REPORT_LEN`.
* `classify()` — distinguishes A75, SDK-v2 and RyServe devices from
  PID/usage/report length.
* `a75_per_key_actuation()`, `a75_report_rate()`, `a75_matrix_page()` —
  validated A75 builders.
* `sdk_performance()`, `sdk_higher_dks()`, `sdk_higher_mpt()`,
  `sdk_higher_mt()`, `sdk_higher_tgl()`, `sdk_higher_end()`,
  `sdk_higher_socd()`, `sdk_higher_rs()`, `sdk_report_rate()`,
  `sdk_high_polling_reset()` — exact SDK-v2 builders.
* `HidrawDevice` — `write_feature()` / `read_feature()` for feature reports,
  and `write_output()` for SDK-v2 output reports.
* Unit tests cover the A75 0.1/1.5/2.0/4.0 mm encodings, the A75 checksum,
  SDK micrometre encoding, the two SOCD packets and the rate packet.

The module deliberately exposes both protocols but **does not auto-select
SDK v2 for an A75**. Use `classify()` before sending anything:

```rust
let family = vgn_hid::classify(pid, usage_page, usage, 64);
match family {
    vgn_hid::Family::A75Legacy => { /* 0x1A / 0x04 feature packets */ }
    vgn_hid::Family::VgnSdkV2 => { /* SDK-v2 output packets */ }
    _ => { /* unsupported for this feature set */ }
}
```

`cargo test --release` currently passes 19 active tests (plus two ignored
hardware smoke tests), including packet layout, SOCD state-machine and
analog-depth tests.

## 7. Direct source references

* SDK mode/rate constants: SDK file byte offsets `0x1c00`–`0x2200`.
* `fe()` 64-byte padding helper: SDK file byte offset `0x3080`.
* `_r` higher-key builder: SDK byte offset `0xb5ad`.
* Performance builder `zi`: SDK byte offset `0xd3fb`.
* `GFSRateOfReturn`: SDK byte offset `0xe6b8`.
* `GFSHighPollingRateReset`: SDK byte offset `0xf6bb`.
* Higher-key read-back layouts: SDK byte offset `0xf760`.
* A75 packet tables: `a75ctl/docs/FIRMWARE.md` §§5–6.
* A75 implementation: `a75ctl-rs/src/protocol.rs` (`set_actuation_key`,
  `set_report_rate`, `set_key_matrix`).

## 8. Bottom line

* The A75 is a **64-byte feature-report, vendor-page 0xFFFF/usage 2** device.
* Its working per-key actuation command is `0x1A`; polling is `0x04`.
* The official VHUB 4.0.9 Windows build does **not** contain an A75 match
  entry and uses a 520-byte feature protocol for the Rongyuan devices it does
  list.
* The official build **does** contain full SOCD/DKS/MT/MPT/TGL/END/RS and
  rapid-trigger packet builders for newer SDK-v2 keyboards. Those packets are
  documented and implemented here, but they are not A75 firmware features.
* A75 SOCD/Tachyon support therefore remains impossible at the firmware
  level; the app now provides an optional evdev/uinput interceptor for
  Last Input Priority and Neutral.
