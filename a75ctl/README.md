> **Legacy prototype.** The optimized Rust application is in `../a75ctl-rs`. This Python version is kept for the protocol notes and as a reference implementation.

# A75 Control

A small, Wootility-style Linux controller for the **VGN A75 Hall-effect**
keyboard (ROYUAN `3151:4010 / 3151:4011 / 3151:4015`).

It speaks the keyboard's native 64-byte HID feature-report protocol directly;
it does **not** need the Windows VGN VHUB driver, Wine, or a vendor daemon.

## Features

- Global actuation point (press / release) for every key
- Rapid trigger on/off with separate press and release sensitivity
- Bottom deadzone
- Comfort / sensitive / gaming presets
- Device status: firmware, LED-controller firmware, profile, mode,
  report rate, debounce
- Report-rate and debounce settings
- Travel calibration (with confirmation)
- Both a Qt GUI and a scriptable CLI

## Requirements

- Linux (hidraw)
- Python 3.10+
- GUI: `PySide6` (`pip install PySide6` or your distro package)
- CLI: Python standard library only

The keyboard exposes three HID interfaces. The vendor interface is the one
with `Usage Page 0xFFFF / Usage 0x0002`; the utility finds it automatically.

## Permissions

The `hidraw` nodes are usually root-only. Install the included udev rule:

```bash
sudo cp 70-vgn-a75.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Then unplug/re-plug the keyboard. Check with:

```bash
ls -l /dev/hidraw*
python3 -c 'import vgn_a75; print(vgn_a75.find_candidate_nodes())'
```

## Relationship to Wootility

Wootility is Wooting's official configurator and speaks Wooting's own
protocol. It cannot configure a VGN/ROYUAN keyboard. This project borrows
the *idea* (analog actuation, rapid trigger, live device status) and applies
it to the VGN A75 protocol. If you also own a Wooting board, keep using the
Wootility AppImage for it; the two tools do not conflict.

## GUI

```bash
cd a75ctl
./a75ctl
```

## CLI

```bash
# show everything
./a75ctl status

# actuation 2.0 mm down / 2.0 mm up, no rapid trigger
./a75ctl apply --travel 2.0 --lift 2.0

# gaming: 0.5 mm actuation + rapid trigger 0.3/0.3 mm
./a75ctl apply --travel 0.5 --lift 2.0 --rt --rt-press 0.3 --rt-lift 0.3

# firmware presets
./a75ctl preset comfort
./a75ctl preset sensitive
./a75ctl preset gaming


# polling / debounce
./a75ctl rate 1000
./a75ctl debounce 5

# analog value stream on/off (the keyboard becomes unusable while on)
./a75ctl analog on
./a75ctl analog off

# travel calibration
./a75ctl calibrate
```

Use `./a75ctl --device /dev/hidraw2 status` if auto-detection picks the
wrong node.

## How it works

The keyboard's vendor interface (HID report descriptor
`06 FF FF 09 02 ... B1 02 C0`) accepts 64-byte feature reports. Each packet
starts with a command byte and carries a checksum:

```
cmd != 0x07 && cmd != 0x08:
    packet[7]  = 0xFF - (sum(packet[0:7])  & 0xFF)
cmd == 0x07 || cmd == 0x08:
    packet[8]  = 0xFF - (sum(packet[0:8])  & 0xFF)
```

On Linux the feature report is 65 bytes including report ID `0x00`:

```python
buf = bytes([0x00]) + packet          # 65 bytes
ioctl(fd, HIDIOCSFEATURE(65), buf)
ioctl(fd, HIDIOCGFEATURE(65), buf)    # response echoes the command
```

The same layout is used by VHUB's `writeFeatureCmd` / `readFeatureCmd`,
which forward the packet over a local gRPC service to `cmhidra.dll` ->
`HidD_SetFeature` / `HidD_GetFeature`.

The complete command map and firmware layout are documented in
[`docs/FIRMWARE.md`](docs/FIRMWARE.md).

## Lighting

The A75's effect-engine command was not recovered for this firmware.  The
per-key colour table can be written (`SET_USERPIC_SIMPLE`), but the
LED controller stalls if it is driven immediately after a status read, and
the effect is not activated by any of the commands that VHUB exposes in the
tested build.  Lighting is therefore intentionally left out of this utility;
use the keyboard's own Fn shortcuts.

## Safety

- This is an unofficial tool. It writes the same settings VHUB writes, but
  there is no warranty.
- Firmware updating is intentionally **not** implemented.
- If a command stalls the control pipe (`OSError: [Errno 71] Protocol error`),
  unplug and re-plug the keyboard. This can happen if an undefined command
  (for example `GET_KEYSTROKE 0xA3` on this firmware) is sent.
- The analog-report mode intentionally makes the keyboard stop typing until
  it is turned off.

## Files

```
a75ctl/
  a75ctl             # GUI + CLI entry point
  vgn_a75.py         # protocol library (stdlib only)
  70-vgn-a75.rules   # udev rule
  a75ctl.desktop     # desktop launcher
  README.md
  docs/FIRMWARE.md   # firmware + protocol analysis
```
