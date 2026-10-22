# VGN A75 firmware and protocol notes

Analysis performed against the shipped VGN VHUB 4.0.9 package and the
firmware it downloads for the A75 (`dev_id 1478`,
`yc3121_vgna75_dm_hall`).

## 1. Artifacts

| Artifact | SHA-256 |
|---|---|
| `VGN_VHUB_4.0.9.zip` | `29a10487c901cb9052641498db9e9064d8bab3b39d09c3fe0e84874eaa8c9a40` |
| `VGN_VHUB_installer.exe` (NSIS) | `c4719e9fa97d3a950ec6d6159b15470363701f364b55b4afe226acde86563cee` |
| `resources/app.asar` | `18fd179a35d185a669073f66e65a8bcabfd446f1d86492b7da9dd3eb36fc1995` |
| `firmwareFile.bin` | `f60dbb71d4f0c687d8cee4d4272e1e37683c2862f9501b923c65acd1500ba06f` |
| `firmwareMledFile.bin` | `c67d912e56a1a1d02f37ba2e24425830434f0c2654c7ab7af4b102ac9693eca7` |
| `cmhidra.dll` (HID transport) | `8d29b88a715dcd2b4f2273fe404879c6cdc1febe56097a784e1c78e507049daf` |
| `Wootility-5.4.2.AppImage` | `2e0bc9f21684d414ea44f7e8fb9a15c1c0d40d54afe8e83e79027198324d7368` |

### Packaging chain

```
VGN_VHUB_4.0.9.zip
└── VGN_VHUB_installer.exe              NSIS 3 self-extracting installer
    └── $PLUGINSDIR/app-64.7z           Electron application payload
        └── resources/app.asar
            ├── dist/                   React renderer (the VHUB UI)
            ├── dist-electron/          Electron main/preload
            ├── node_modules/@vgnlab/vgn-serve/
            │   ├── vgn-serve.win32-x64-msvc.node   Rust N-API service
            │   └── deps/cmhidra.dll                HID transport DLL
            └── node_modules/@vgnlab/vgn-lighting-engine/
```

The renderer contains the entire device database and the FEA command
constructors. The Rust service only moves 64-byte buffers through
`CmSetFeatureReport` / `CmGetFeatureReport` (`cmhidra.dll`, which wraps
Windows `HidD_SetFeature` / `HidD_GetFeature`).

For the A75 the device database entries are:

```js
// yc3121_vgna75_hall       pid 0x4011, 0x4015
// yc3121_vgna75_dm_hall    pid 0x4015
// yc500_s75a_dm            pid 0x4010
{ vid: 0x3151, usage: 2, usagePage: 0xFFFF,
  featureReportByteLength: 65, magnetism: true }
```

## 2. Firmware image layout

`firmwareFile.bin` is 184,608 bytes (`0x2D120`). It is a **two-slot flash
image**: a 64 KiB bootloader slot followed by a 64 KiB-aligned application
slot. The unused parts of each slot are zero-filled.

| File offset | Virtual address | Size | Contents |
|---|---|---|---|
| `0x00000` | `0x01000000` | `0x200` | boot slot header / jump table |
| `0x00200` | `0x01000200` | `0x31C5` | bootloader code (reset stub, libc, YC SDK, USB DFU) |
| `0x033C5` | `0x010033C5` | `0xCC3B` | erased flash (all zero) |
| `0x10000` | `0x01010000` | `0xE968` | application code (USB HID, keyboard, Hall/LED logic) |
| `0x1E968` | `0x0101E968` | `0x949A` | erased flash (all zero) |
| `0x27E02` | `0x01027E02` | `0x531E` | application data: keymaps, timing, LED and calibration tables |

Important addresses:

| Address | Meaning |
|---|---|
| `0x01000200` | bootloader reset stub (`SP = 0x03000000`, calls `0x01002ACC`, jumps to `0x01000898`) |
| `0x01002718` | bootloader USB product string `"Gaming Keyboard"` |
| `0x01002728` | bootloader USB string `"Joystick in HS mode"` |
| `0x01002764` | bootloader `"HID Config"` / `"HID Interface"` strings |
| `0x01010000` | application slot start |
| `0x01010200` | application reset stub (`SP = 0x03000000`, calls `0x0101CF40`, jumps to `0x01010502`) |
| `0x01017B00` | application USB strings `"VGN A75"`, `"ROYUAN"`, `"HID Config"` |
| `0x0101E82E` | application keyboard report descriptor region |
| `0x0101E869` | application consumer-control report descriptor region |
| `0x0101E8E5` | application vendor report descriptor region |
| `0x010168AE` | **read-side feature command dispatcher** |
| `0x0101686E` | **frame checksum validator** |
| `0x01016B36` | continuation of the dispatcher (commands `0xC0`/`0xC1`, `0x0D`/`0x0E`) |
| `0x000229F4` | RAM command/report buffer used by the dispatcher |
| `0x000243D8` | RAM transfer/state structure |

The bootloader and application are both Thumb-2 ARM. The SDK paths embedded
in the image are:

```
..\..\..\..\..\Librarier\sdk\yc_gpio.c
..\..\..\..\..\Librarier\sdk\yc_uart.c
..\..\..\..\..\Librarier\sdk\yc_timer.c
..\..\..\..\..\Librarier\sdk\yc_systick.c
..\..\..\..\..\Librarier\sdk\yc_sysctrl.c
..\..\..\..\..\Librarier\sdk\yc_adc.c
..\..\..\..\..\Librarier\sdk\yc_wdt.c
..\..\..\..\..\Librarier\sdk\yc_ssc.c
```

This is the Yichip (YC) SDK family used by the YC3121 keyboard MCU.

### MLED firmware

`firmwareMledFile.bin` is 97,468 bytes (`0x17CBC`) and has the same
two-slot structure:

| File offset | Size | Contents |
|---|---|---|
| `0x00000` | `0x2708` | bootloader code |
| `0x02708` | `0x10000` | erased flash |
| `0x10000` | `0x790A` | LED-controller application |
| `0x1790A` | `0x3B2` | small data/parameter tail |

It is a separate MCU image for the RGB LED controller. VHUB flashes it
after the main keyboard firmware (`v112_mledv105` = main v1.12, MLED v1.05).

### How REA was used

Raw firmware is not an ELF, so a minimal ELF32 container was generated with
the image mapped at `0x01000000` and `.text` marked ARM/Thumb. REA then
analysed it with Hopper:

- `binary_overview`: 1 segment `0x01000000..0x0102D120`, **823 procedures**,
  31 strings.
- Evidence IDs:
  - `ev_fb8a83bb04c39823a8232e719875ad67d11ed7f7bb17467a5fb3b8c292f993e2`
    (firmware overview)
  - `ev_13ab8200b9625bb15c2d8653c833990ecc97cccd09da049682a18f2d03fabb80`
    (command dispatcher assembly)
  - `ev_b027ab552de3f8ae9b59e8969a3ca9a4112d5fd1fc7faffa368d3d6371d4e4d3`
    (checksum validator assembly)
  - `ev_f37990cb28f349369c5673a201ae4c2494e52c9fccb61f00342c8a63ab525c82`
    (MLED firmware overview)
  - `ev_46608183bc84b2bd299cefef19db989af0d2c0c65ee4f4c602dd269883b5f292`
    (Wootility AppImage overview)

The generated ELF changes file offsets to virtual addresses; it does not
modify the firmware.

## 3. HID transport

The vendor interface has this report descriptor:

```
06 FF FF        Usage Page (Vendor 0xFFFF)
09 02           Usage (2)
A1 01           Collection (Application)
09 02             Usage (2)
15 80             Logical Minimum (-128)
25 7F             Logical Maximum (127)
95 40             Report Count (64)
75 08             Report Size (8)
B1 02             Feature (Data,Var,Abs)
C0              End Collection
```

On Linux this is a 64-byte feature report. `hidraw` requires the report ID
as the first byte, so the buffer is 65 bytes:

```
[0x00][64-byte packet]
```

### Checksum

At `0x0101686E` the firmware sums the command bytes and compares them with
the checksum byte:

```
cmd != 0x07 and cmd != 0x08:
    sum = packet[0] + ... + packet[6]
    valid if (sum + packet[7]) & 0xFF == 0xFF

cmd == 0x07 or cmd == 0x08:
    sum = packet[0] + ... + packet[7]
    valid if (sum + packet[8]) & 0xFF == 0xFF
```

So the checksum is the two's complement of the sum, stored at byte **7**
(or byte **8** for commands `0x07`/`0x08`). In VHUB this is
`CheckSumType.BIT7` / `BIT8` (`NONE` = 2).

### Response format

A read command returns a 64-byte packet with:

```
[0]     command echo
[1..]   little-endian fields
[7/8]   checksum
```

Example on firmware v1.12:

```
GET_REV (0x80)  -> 80 12 01 00 00 00 00 7F ...
                    ^^ version 0x0112 = v1.12
GET_KEY_MAGNETISM_MODE (0x9D, profile 0)
                -> 9D 02 00 ...   mode 2 = gaming
```

SET commands are not acknowledged; VHUB sends them and then reads a status
command after a short delay.

## 4. Command map

The A75 uses a **shifted v2 command map** for report/profile/option/LED
commands.  Other Rongyuan devices use the older map (`SET_LEDPARAM = 0x04`),
which is why probing with `0x04` appeared to be ignored on this keyboard.

### SET

| Cmd | Name | Payload |
|---|---|---|
| `0x04` | `SET_REPORT` | `[1]=profile`, `[2]=rate index` (`1`=1000, `2`=500, `4`=250, `8`=125) |
| `0x05` | `SET_PROFILE` | `[1]=profile` |
| `0x06` | `SET_KBOPTION` | option bitfield |
| `0x07` | `SET_LEDPARAM` | **main RGB**, checksum at byte 8 (below) |
| `0x08` | `SET_SLEDPARAM` | side/logo light, checksum at byte 8 |
| `0x09` | `SET_KEYMATRIX` | 9-page keymap upload, see below |
| `0x11` | `SET_DEBOUNCE` | `[2]=value` |
| `0x14` | `SET_USERPIC_SIMPLE` | `[1]=profile`, `[2]=key index`, `[8..10]=R,G,B` |
| `0x1A` | `SET_MAGNETISM_MODE` | per-key/global actuation and rapid trigger (below) |
| `0x1B` | `SET_MAGNETISM_REPORT` | `[1]=1` start analog stream, `0` stop |
| `0x1C` | `SET_MAGNETISM_CAL` | `[1]=1` start travel calibration |
| `0x1D` | `SET_KEY_MAGNETISM_MODE` | `[1]=0` comfort, `1` sensitive, `2` gaming, `3` custom; `[2]=keyboard disable` |
| `0x1E` | `SET_MAGNETISM_MAXIMUM_CALIBRATION` | `[1]=1` |
| `0x1F` | `SET_MAGNETISM_FAST` | 4 quick-release keys |
| `0x7F` | `SET_BOOTLOATER` | firmware-update reboot; magic `55 AA 55 AA` |
| `0x2C` | `SET_FLASHCHIPERASSE` | flash erase |

### GET

| Cmd | Name | Response |
|---|---|---|
| `0x80` | `GET_REV` | `[1..2]` u16 firmware version |
| `0x84` | `GET_REPORT` | `[1]` profile, `[2]` rate index |
| `0x85` | `GET_PROFILE` | `[1]` profile |
| `0x86` | `GET_KBOPTION` | option bytes at `[2..4]` |
| `0x87` | `GET_LEDPARAM` | `[1]` effect, `[2]` speed, `[3]` brightness, `[4]` option, `[5..7]` RGB |
| `0x88` | `GET_SLEDPARAM` | side/logo light |
| `0x89` | `GET_KEYMATRIX` | `[1]=profile`, `[2]=page 0..7`; 64 bytes of 4-byte key records per page (504-byte keymap + 8-byte footer) |
| `0x8C` | `GET_USERPIC` | stored per-key colour bytes (`R,G,B` at `[0..2]`) |
| `0x8F` | `GET_INFOR` | device info; `[1..2]` = dev id (1478 / `0x05C6`) |
| `0x91` | `GET_DEBOUNCE` | `[2]` debounce |
| `0x92` | `GET_SLEEPTIME` | four u16 values at `[1,3,5,7]` |
| `0x9D` | `GET_KEY_MAGNETISM_MODE` | `[1]` mode, `[2]` keyboard-disable flag |
| `0x9F` | `GET_MAGNETISM_FAST` | 4 x (enable, key index) at `[8..15]` |
| `0xAE` | `GETMLED_VERSION` | `[1..2]` u16 LED firmware version |

The read dispatcher at `0x010168AE` handles:
`0x40`, `0x41`, `0x42`, `0x7E`, `0x7F`, `0x80`, `0x82`, `0x83`, `0x84`,
`0x85`–`0x8C`, `0x8D`, `0x8F`, `0x90`, `0x91`, `0x92`, `0x97`, `0x9D`,
`0x9F`, `0xA5`, `0xAD`, `0xAE`, `0xB0`, `0xB1`, `0xC0`, `0xC1`.

Some defined commands are **not** implemented on this firmware. Sending
`0xA3` (`GET_KEYSTROKE`) stalls the vendor control pipe; only a physical
unplug/re-plug recovers it.

The A75 matrix read returned 86 non-zero 4-byte records across pages 0..5
(indices 11, 29, 35 and 75 are gaps).  Writing a per-key command to a gap
index stalls the control pipe, so the matrix is read first when colour data
is written.

### RGB (`0x07` / `0x87`) — verified

`SET_LEDPARAM` uses the special checksum position (byte **8**), so the RGB
bytes at `[5..7]` are not overwritten by the checksum:

```
[0] 0x07
[1] effect mode
[2] speed field  = 5 - speed (A75 v2 class)
[3] brightness   0..4
[4] (option_nibble << 4) | colour_mode
    colour_mode 0x7 = NORMAL (use RGB bytes)
    colour_mode 0x6 = DAZZLE
    colour_mode 0x0 = COMMONCOLOR[0] = RED (overrides RGB)
[5] R  [6] G  [7] B
[8] checksum     = 0xFF - sum(bytes 0..7)
```

**Important:** leaving `[4]` at `0` makes every solid colour red because
the firmware selects `COMMONCOLOR[0]` instead of the RGB bytes.  Use
`0x07` for normal effects (`[4] = (option << 4) | 0x07`).

Verified effect mode numbers on v1.12 include:

| Mode | Effect |
|---|---|
| 0 | off |
| 1 | solid |
| 2 | breath |
| 3 | neon |
| 4 | wave |
| 5 | ripple |
| 6 | raindrop |
| 7 | snake |
| 8 | press action |
| 9 | coverage |
| 10 | sine wave |
| 11 | kaleidoscope |
| 12 | line wave |
| 13 | user picture |
| 14 | laser |
| 15 | circle wave |
| 16 | dazzling |
| 17 | rain down |
| 18 | meteor |
| 19 | press action off |
| 20 | music follow 3 |
| 21 | screen colour |
| 22 | music follow 2 |
| 23 | train |
| 24 | fireworks |
| 66 | user colour |

Example: solid orange `#ff5a3c` at maximum brightness/speed:

```
07 01 04 04 00 ff 5a 3c <checksum>
```

`GET_LEDPARAM 0x87` returns the stored state after roughly one second.  The
controller accepts the command immediately; the readback delay is a
firmware/LED-controller timing detail.

**Full lights-off:** send a solid-black frame (`mode 1`, RGB `00 00 00`)
before `mode 0`.  Mode 0 alone leaves per-key overrides (for example one
key in the user-picture table) lit.  The controller does both in
`set_off()`.

Per-key colour is a separate command (`0x14`), verified persistent via
`GET_USERPIC 0x8C`:

```
SET_USERPIC_SIMPLE 0x14: [1]=profile [2]=matrix index [8]=R [9]=G [10]=B
```

Writing all 86 keys needs pacing (~140 ms per key, with a pause every 8);
driving it faster makes the LED controller NAK the control pipe.

### Input reports

- `/dev/hidraw0` (keyboard interface) — standard 8-byte boot report:
  `[modifiers, reserved, k1..k6]`.  Modifier bits are `0x01` LCtrl,
  `0x02` LShift, `0x04` LAlt, `0x08` LGui, `0x10` RCtrl, `0x20` RShift,
  `0x40` RAlt, `0x80` RGui.
- `/dev/hidraw1` (vendor input, report ID 5) — analog telemetry:
  `05 1B <hi> <lo>`, a 16-bit big-endian value.  The stream only runs while
  `SET_MAGNETISM_REPORT` is enabled; the keyboard does not type in this mode.

## 5. Actuation / rapid trigger packet (`0x1A`)

VHUB's `setMagnetismMode` builds this packet:

| Byte | Meaning |
|---|---|
| `0` | `0x1A` |
| `1` | profile |
| `2` | `1` (settings present) |
| `3` | mode flags: `0` normal, `2` DKS, `3` MT, `4` toggle-hold, `5` toggle-dots, `6` gamepad; `0x80` = rapid trigger |
| `4` | actuation point |
| `5` | `1` = all keys, `0` = single key / gamepad |
| `6` | matrix key index (0 when all) |
| `7` | checksum |
| `8` | release/lift point |
| `9` | rapid-trigger press sensitivity |
| `10` | rapid-trigger release sensitivity |
| `11` | dynamic travel (`value - 2`) |
| `12..15` | trigger mode bytes |
| `16` | MT timer (default 20) |
| `17..24` | gamepad depth/analog pairs |
| `25` | bottom deadzone |

Value encodings:

```
actuation / release raw = round(10 * (mm - 0.1))      # 0.1 mm units, min 0.1
rapid-trigger raw       = round(10 * mm)              # 0.1 mm units
deadzone raw            = round(10 * mm - 1)          # 0 disables
```

Examples (VHUB defaults):

| Setting | mm | raw |
|---|---|---|
| Comfort press/release | 2.0 | 19 |
| Sensitive press | 0.5 | 4 |
| RT press sensitivity | 0.2 | 2 |
| RT release sensitivity | 0.3 | 3 |
| Bottom deadzone | 3.7 | 36 |

**Not exposed by this firmware:** there is no SOCD pair command and no
Tachyon command. The app's Performance tab maps Tachyon to the A75's maximum
1000 Hz plus solid lighting. SOCD pair configuration is stored/badged by the
app, but OS-level suppression would require a separate evdev/uinput layer.
Rappy Snappy also needs per-key analog depth, which the A75 does not stream
to the controller.

Presets (`0x1D`) are firmware-side profiles:

- `0` comfort: 2.0 mm, no rapid trigger
- `1` sensitive: 0.5 mm
- `2` gaming: 0.5 mm + rapid trigger
- `3` custom: per-key table

After a custom `0x1A` write the reported mode becomes `3` (custom).

Per-key writes use the same packet with `[5]=0` and `[6]=` the matrix index
from `GET_KEYMATRIX 0x89` (e.g. A = index 9 on the tested unit).  Toggle
modes are `[3]=4` (toggle hold) and `[3]=5` (toggle dots).  The packet also
carries `mt_time`, trigger-mode bytes and gamepad depth/analog pairs, but
assigning the extra DKS/Mod-Tap actions is not implemented.

### Key remap (verified)

`SET_KEYMATRIX 0x09` writes the keymap in nine 64-byte reports:

```
[0] 0x09        [1] profile (0)
[2] 0xF8        [3] 0x01       (total length 504, little-endian)
[4] page 0..8   [7] checksum   (BIT7, 0xFF - sum(bytes 0..6))
[8..63]         56 bytes of matrix data
```

The firmware stages the data at RAM `0x22A34`; the last page sets a commit
flag.  A `GET_KEYMATRIX` immediately afterwards may still return the old
matrix.  The A75 accepts a complete shadow matrix, so the controller keeps a
factory matrix and a current matrix locally, writes all nine pages, and
saves remaps in `~/.config/a75ctl/remap.bin`.

Matrix records are 4 bytes.  Normal keys are `00 00 <HID usage> 00`; e.g.
Caps = `00 00 39 00`, Home = `00 00 4A 00`, Insert = `00 00 49 00`.
Remapping Caps → Home changes slot 3 to `00 00 4A 00`.  The physical matrix
was verified to contain **86 records**, no PrtSc, Del at the far right, and
Insert in the Home position.  Simple key remaps work; DKS/Mod-Tap action
assignment is separate.

## 6. Firmware update path (not implemented in this utility)

VHUB resolves firmware through the Rongyuan API:

```
POST https://api2.rongyuan.tech:3816/api/v2/get_fw_version
     {"dev_id": 1478}
 -> {"version_str":"v112_mledv105",
     "file_path":"fw_upgrade_file/1478_v112_mledv105", ...}

GET  https://api2.rongyuan.tech:3816/download/fw_upgrade_file/1478_v112_mledv105
 -> ZIP (deflate) with firmwareFile.bin + firmwareMledFile.bin
```

The updater then:

1. Sends `SET_BOOTLOATER` (`0x7F`) with `55 AA 55 AA` to reboot into the
   bootloader.
2. The bootloader re-enumerates as `"Gaming Keyboard"` /
   `"Joystick in HS mode"`.
3. Writes the image in 56/64-byte chunks with the bootloader's flash
   commands and verifies checksums.
4. Reboots into the application (`"VGN A75"`).

The main image includes both slots, so a failed application update can be
recovered through the bootloader as long as the bootloader slot is intact.

## 7. Open questions

- The exact decompression/encoding of the report-descriptor block at
  `0x0101E82E` was not recovered; the runtime descriptors were read directly
  from the live HID interface instead.
- The semantics of the 21 KiB data block at `0x01027E02` are only partially
  mapped (timing defaults, key/LED tables, calibration data).
- The analog stream is decoded as `05 1B <hi> <lo>` (16-bit big-endian),
  but mapping the raw value to a specific key and to millimetres is not
  fully understood; the controller plots it as raw telemetry.
- The `0x1A` packet supports per-key DKS/MT/gamepad modes; the controller
  writes the global "normal + rapid trigger" subset.
