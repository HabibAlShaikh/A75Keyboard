#!/usr/bin/env python3
"""
Minimal Linux control library for the VGN A75 (ROYUAN 3151:401x)
Hall-effect keyboard.

The protocol was recovered from:
  * the VGN VHUB 4.0.9 web/Electron driver
  * the A75 firmware image (FEA command dispatcher at 0x10168ae)
  * live HID feature-report probing

Transport
---------
The keyboard exposes a vendor HID interface:

    Usage Page : 0xFFFF
    Usage      : 0x0002
    Feature    : 64 bytes (65 bytes including report ID 0)

Every command is a 64-byte packet.  For all commands except 0x07/0x08 the
checksum lives in byte 7 and is the two's-complement of the sum of bytes
0..6.  For commands 0x07/0x08 the checksum lives in byte 8.

No third-party packages are required.
"""

from __future__ import annotations

import errno
import fcntl
import glob
import os
import struct
import time
from dataclasses import dataclass
from typing import Iterable

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

VENDOR_ID = 0x3151
# All A75 variants in VHUB 4.0.9: 0x4010, 0x4011, 0x4015.
A75_PRODUCT_IDS = {0x4010, 0x4011, 0x4015}

REPORT_ID = 0x00
PACKET_LEN = 64

# Feature commands (from VHUB's FEA_CMD_* enum)
CMD_SET_REPORT = 0x04
CMD_SET_PROFILE = 0x05
CMD_SET_KBOPTION = 0x06
CMD_SET_LEDPARAM = 0x07
CMD_SET_SLEDPARAM = 0x08
CMD_SET_DEBOUNCE = 0x11
CMD_SET_USERPIC_SIMPLE = 0x14
CMD_SET_MAGNETISM_MODE = 0x1A       # 26: per-key/global actuation + rapid trigger
CMD_SET_MAGNETISM_REPORT = 0x1B     # 27: analog value stream on/off
CMD_SET_MAGNETISM_CAL = 0x1C        # 28
CMD_SET_KEY_MAGNETISM_MODE = 0x1D   # 29: comfort/sensitive/gaming/custom
CMD_SET_MAGNETISM_MAX_CAL = 0x1E    # 30

CMD_GET_REV = 0x80
CMD_GET_REPORT = 0x84
CMD_GET_PROFILE = 0x85
CMD_GET_KBOPTION = 0x86
CMD_GET_LEDPARAM = 0x87
CMD_GET_SLEDPARAM = 0x88
CMD_GET_INFOR = 0x8F
CMD_GET_DEBOUNCE = 0x91
CMD_GET_SLEEPTIME = 0x92
CMD_GET_KEY_MAGNETISM_MODE = 0x9D   # 157
CMD_GET_MAGNETISM_FAST = 0x9F       # 159
CMD_GET_MLED_VERSION = 0xAE         # 174

REPORT_RATES = {1: 1000, 2: 500, 4: 250, 8: 125, 0: 1000}
REPORT_RATE_INDEX = {1000: 1, 500: 2, 250: 4, 125: 8}

MODE_NAMES = {0: "comfort", 1: "sensitive", 2: "gaming", 3: "custom"}
MODE_VALUES = {v: k for k, v in MODE_NAMES.items()}

# A75 travel range used by VHUB (0.1 .. 4.0 mm, 0.05 mm steps).
TRAVEL_MIN_MM = 0.1
TRAVEL_MAX_MM = 3.3
FIRE_TRAVEL_MIN_MM = 0.1
FIRE_TRAVEL_MAX_MM = 4.0
DEADZONE_DEFAULT_MM = 3.7

# HIDIOCSFEATURE / HIDIOCGFEATURE for a 65-byte report.
_IOC_WRITE_READ = 3
_HIDIOCSFEATURE = (_IOC_WRITE_READ << 30) | ((PACKET_LEN + 1) << 16) | (ord("H") << 8) | 0x06
_HIDIOCGFEATURE = (_IOC_WRITE_READ << 30) | ((PACKET_LEN + 1) << 16) | (ord("H") << 8) | 0x07


class VgnError(RuntimeError):
    pass


class DeviceNotConnected(VgnError):
    pass


class DeviceStalled(VgnError):
    """The control pipe STALLed.  Physically re-plug the keyboard."""


class ProtocolError(VgnError):
    pass


class DeviceTimeout(VgnError):
    """The device did not answer in time (usually still applying a setting)."""


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def travel_to_raw(mm: float) -> int:
    """VHUB encoding: raw = 10*(mm - 0.1) for the main/lift actuation point."""
    return max(0, min(255, int(round(10.0 * (mm - TRAVEL_MIN_MM)))))


def raw_to_travel(raw: int) -> float:
    return raw / 10.0 + TRAVEL_MIN_MM


def fire_to_raw(mm: float) -> int:
    """Rapid-trigger sensitivity is stored in 0.1 mm units."""
    return max(0, min(255, int(round(10.0 * mm))))


def raw_to_fire(raw: int) -> float:
    return raw / 10.0


def deadzone_to_raw(mm: float) -> int:
    if mm <= 0:
        return 0
    return max(0, min(255, int(round(10.0 * mm - 1.0))))


def raw_to_deadzone(raw: int) -> float:
    return (raw + 1.0) / 10.0 if raw else 0.0


def _hid_id(path: str) -> tuple[int, int] | None:
    uevent = os.path.join(path, "device", "uevent")
    try:
        with open(uevent, "r", encoding="ascii", errors="ignore") as fh:
            for line in fh:
                if line.startswith("HID_ID="):
                    # 0003:00003151:00004015
                    _, vid, pid = line.strip().split("=", 1)[1].split(":")
                    return int(vid, 16), int(pid, 16)
    except OSError:
        pass
    return None


def _report_descriptor(node: str) -> bytes:
    sys_path = f"/sys/class/hidraw/{os.path.basename(node)}"
    try:
        with open(os.path.join(sys_path, "device", "report_descriptor"), "rb") as fh:
            return fh.read()
    except OSError:
        return b""


def find_candidate_nodes() -> list[str]:
    """Return only the vendor 0xFFFF/0x0002 interface.

    The keyboard exposes three hidraw nodes (keyboard, consumer, vendor).
    Probing the wrong one with a feature report can STALL endpoint 0, so the
    report descriptor is checked before anything is opened.
    """
    nodes = []
    fallback = []
    for node in sorted(glob.glob("/dev/hidraw*")):
        sys_path = f"/sys/class/hidraw/{os.path.basename(node)}"
        ids = _hid_id(sys_path)
        if not ids or ids[0] != VENDOR_ID or ids[1] not in A75_PRODUCT_IDS:
            continue
        desc = _report_descriptor(node)
        if b"\x06\xff\xff\x09\x02" in desc:
            nodes.append(node)
        elif not desc:
            fallback.append(node)
    return nodes or fallback


# ---------------------------------------------------------------------------
# Device
# ---------------------------------------------------------------------------

@dataclass
class DeviceStatus:
    path: str
    firmware: str
    mled: str
    mode: str
    keyboard_disabled: bool
    profile: int
    report_rate: int
    debounce: int
    led_mode: int
    led_speed: int
    led_brightness: int
    led_option: int
    info: str


class A75:
    def __init__(self, path: str | None = None, *, timeout: float = 0.15):
        self.path = path
        self.fd: int | None = None
        self.timeout = timeout
        self._last_query = 0.0
        self._profile = 0

    # -- lifecycle ---------------------------------------------------------
    def open(self) -> "A75":
        if self.fd is not None:
            return self

        candidates = [self.path] if self.path else find_candidate_nodes()
        if not candidates:
            raise DeviceNotConnected(
                "VGN A75 not found. Check `lsusb -d 3151:` and permissions on /dev/hidraw*."
            )

        errors = []
        for node in candidates:
            try:
                fd = os.open(node, os.O_RDWR | os.O_NONBLOCK)
            except OSError as exc:
                errors.append(f"{node}: {exc}")
                continue
            self.fd = fd
            self.path = node
            try:
                self.get_version()
                return self
            except Exception as exc:  # wrong interface or stalled pipe
                errors.append(f"{node}: {exc}")
                os.close(fd)
                self.fd = None
        raise DeviceNotConnected("Could not open the vendor HID interface:\n  " + "\n  ".join(errors))

    def close(self) -> None:
        if self.fd is not None:
            try:
                os.close(self.fd)
            finally:
                self.fd = None

    def __enter__(self) -> "A75":
        return self.open()

    def __exit__(self, *exc) -> None:
        self.close()

    # -- low level ---------------------------------------------------------
    def _ensure_open(self) -> None:
        if self.fd is None:
            self.open()

    def _ioctl(self, request: int, buf: bytearray) -> None:
        try:
            fcntl.ioctl(self.fd, request, buf, True)
        except OSError as exc:
            if exc.errno in (errno.EPROTO, errno.ESHUTDOWN, errno.ENODEV):
                raise DeviceStalled(
                    "The keyboard's control pipe stalled. Unplug and re-plug the "
                    "keyboard (or power-cycle the USB port), then reconnect."
                ) from exc
            if exc.errno == errno.EOVERFLOW:
                raise ProtocolError("Feature report length mismatch (expected 65 bytes).") from exc
            raise

    @staticmethod
    def _checksum_index(cmd: int) -> int:
        return 8 if cmd in (0x07, 0x08) else 7

    def _frame(self, cmd: int, fields: dict[int, int] | None = None) -> bytearray:
        packet = bytearray(PACKET_LEN)
        packet[0] = cmd
        if fields:
            for index, value in fields.items():
                packet[index] = value & 0xFF
        idx = self._checksum_index(cmd)
        packet[idx] = (0xFF - (sum(packet[:idx]) & 0xFF)) & 0xFF
        return packet

    def send(self, cmd: int, fields: dict[int, int] | None = None, *, settle: float = 0.03) -> None:
        """Send a SET command.  SET commands are not acknowledged."""
        self._ensure_open()
        # Keep a gap between a read and the next write; the firmware can NAK
        # (and the kernel can time out) if it is still finishing the previous
        # GET.
        gap = 0.75 - (time.monotonic() - self._last_query)
        if gap > 0:
            time.sleep(gap)
        time.sleep(0.06)
        packet = self._frame(cmd, fields)
        last: OSError | None = None
        for attempt in range(3):
            try:
                buf = bytearray([REPORT_ID]) + packet
                self._ioctl(_HIDIOCSFEATURE, buf)
                if settle:
                    time.sleep(settle)
                return
            except OSError as exc:
                if exc.errno in (errno.ETIMEDOUT, errno.EAGAIN):
                    last = exc
                    time.sleep(0.25 + 0.15 * attempt)
                    continue
                raise
        raise DeviceTimeout(f"Device did not accept command 0x{cmd:02X}: {last}")

    def query(self, cmd: int, fields: dict[int, int] | None = None,
              *, retries: int = 3, delay: float = 0.04) -> bytes:
        """Send a GET command and return the 64-byte response packet."""
        self._ensure_open()
        packet = self._frame(cmd, fields)
        last = b""
        for attempt in range(retries):
            try:
                buf = bytearray([REPORT_ID]) + packet
                self._ioctl(_HIDIOCSFEATURE, buf)
                time.sleep(delay)
                resp = bytearray(PACKET_LEN + 1)
                self._ioctl(_HIDIOCGFEATURE, resp)
            except OSError as exc:
                # ETIMEDOUT happens while the firmware is still applying a
                # previous SET (actuation/lighting writes take a few hundred
                # milliseconds).  Back off and try again.
                if exc.errno in (errno.ETIMEDOUT, errno.EAGAIN):
                    time.sleep(0.25 + 0.15 * attempt)
                    continue
                raise
            data = bytes(resp[1:])
            last = data
            if data[0] == cmd:
                # The firmware's response checksum is not consistently
                # populated (some GET handlers leave byte 7 at zero), so the
                # command echo is the reliable acceptance test.
                self._last_query = time.monotonic()
                time.sleep(0.05)
                return data
            time.sleep(delay)
        raise ProtocolError(
            f"Unexpected response for command 0x{cmd:02X}: {last[:16].hex(' ')}"
        )

    def query_raw(self, cmd: int, fields: dict[int, int] | None = None,
                  *, retries: int = 3, delay: float = 0.05) -> bytes:
        """Send a GET command whose response does not echo the command byte."""
        self._ensure_open()
        time.sleep(0.06)
        packet = self._frame(cmd, fields)
        for attempt in range(retries):
            try:
                buf = bytearray([REPORT_ID]) + packet
                self._ioctl(_HIDIOCSFEATURE, buf)
                time.sleep(delay)
                resp = bytearray(PACKET_LEN + 1)
                self._ioctl(_HIDIOCGFEATURE, resp)
                self._last_query = time.monotonic()
                time.sleep(0.05)
                return bytes(resp[1:])
            except OSError as exc:
                if exc.errno in (errno.ETIMEDOUT, errno.EAGAIN):
                    time.sleep(0.25 + 0.15 * attempt)
                    continue
                raise
        raise DeviceTimeout(f"Device did not answer command 0x{cmd:02X}")

    # -- GET commands ------------------------------------------------------
    def get_version(self) -> str:
        data = self.query(CMD_GET_REV)
        raw = struct.unpack_from("<H", data, 1)[0]
        return f"{raw >> 8}.{raw & 0xFF:02X}" if raw >= 0x100 else str(raw)

    def get_version_raw(self) -> int:
        return struct.unpack_from("<H", self.query(CMD_GET_REV), 1)[0]

    def get_mled_version(self) -> str:
        data = self.query(CMD_GET_MLED_VERSION)
        raw = struct.unpack_from("<H", data, 1)[0]
        return f"{raw >> 8}.{raw & 0xFF:02X}"

    def get_info(self) -> str:
        data = self.query(CMD_GET_INFOR)
        return data[1:].hex(" ")

    def get_key_matrix(self, profile: int = 0, pages: int = 8) -> bytes:
        """Read the 4-byte key-matrix records (GET_KEYMATRIX 0x89)."""
        out = bytearray()
        for page in range(pages):
            out += self.query_raw(0x89, {1: profile, 2: page})
        return bytes(out)

    def valid_key_indices(self, profile: int = 0) -> list[int]:
        """Return matrix indices that contain a real key (non-zero record)."""
        matrix = self.get_key_matrix(profile)
        return [i for i in range(len(matrix) // 4)
                if any(matrix[i * 4:(i + 1) * 4])]

    def get_profile(self) -> int:
        value = self.query(CMD_GET_PROFILE)[1]
        self._profile = value
        return value

    def get_report_rate(self) -> int:
        idx = self.query(CMD_GET_REPORT, {1: 0})[2]
        return REPORT_RATES.get(idx, 0)

    def get_debounce(self) -> int:
        return self.query(CMD_GET_DEBOUNCE)[2]

    def get_magnetism_mode(self) -> tuple[str, bool]:
        data = self.query(CMD_GET_KEY_MAGNETISM_MODE, {1: 0})
        return MODE_NAMES.get(data[1], f"unknown({data[1]})"), bool(data[2])

    def get_led(self) -> tuple[int, int, int, int]:
        data = self.query(CMD_GET_LEDPARAM)
        return data[1], data[2], data[3], data[4]

    def get_sleep_times(self) -> tuple[int, int, int, int]:
        data = self.query(CMD_GET_SLEEPTIME)
        return struct.unpack_from("<4H", data, 1)

    def status(self) -> DeviceStatus:
        self.open()

        def safe(fn, default):
            try:
                return fn()
            except ProtocolError:
                return default

        mode, disabled = safe(self.get_magnetism_mode, ("unknown", False))
        led = safe(self.get_led, (0, 0, 0, 0))
        return DeviceStatus(
            path=self.path or "?",
            firmware=self.get_version(),
            mled=safe(self.get_mled_version, "?"),
            mode=mode,
            keyboard_disabled=disabled,
            profile=safe(self.get_profile, 0),
            report_rate=safe(self.get_report_rate, 0),
            debounce=safe(self.get_debounce, 0),
            led_mode=led[0],
            led_speed=led[1],
            led_brightness=led[2],
            led_option=led[3],
            info=safe(self.get_info, ""),
        )

    # -- SET commands ------------------------------------------------------
    def set_profile(self, profile: int) -> None:
        self.send(CMD_SET_PROFILE, {1: profile}, settle=0.5)
        self._profile = profile

    def set_report_rate(self, rate: int) -> None:
        if rate not in REPORT_RATE_INDEX:
            raise ValueError(f"Unsupported report rate: {rate} (use 125/250/500/1000)")
        profile = self.get_profile()
        self.send(CMD_SET_REPORT, {1: profile, 2: REPORT_RATE_INDEX[rate]}, settle=0.2)

    def set_debounce(self, value: int) -> None:
        self.send(CMD_SET_DEBOUNCE, {2: max(0, min(255, value))}, settle=0.5)

    def set_preset(self, preset: str) -> None:
        """Apply a firmware preset (comfort / sensitive / gaming / custom)."""
        if preset not in MODE_VALUES:
            raise ValueError(f"Unknown preset: {preset}")
        self.send(CMD_SET_KEY_MAGNETISM_MODE,
                  {1: MODE_VALUES[preset], 2: 0}, settle=0.5)

    def set_global_magnetism(
        self,
        travel_mm: float,
        lift_mm: float,
        *,
        rapid_trigger: bool = False,
        rt_press_mm: float = 0.2,
        rt_lift_mm: float = 0.3,
        deadzone_mm: float = DEADZONE_DEFAULT_MM,
        profile: int | None = None,
    ) -> None:
        """Apply one actuation/rapid-trigger configuration to all keys.

        This is the same 'custom' mode packet VHUB sends when you press
        'select all' + confirm on its rapid-trigger page.
        """
        if profile is None:
            profile = self.get_profile()
        travel_mm = max(TRAVEL_MIN_MM, min(TRAVEL_MAX_MM, travel_mm))
        lift_mm = max(TRAVEL_MIN_MM, min(TRAVEL_MAX_MM, lift_mm))
        fields = {
            1: profile,
            2: 1,                       # settings present
            3: 0x80 if rapid_trigger else 0,
            4: travel_to_raw(travel_mm),
            5: 1,                       # all keys
            6: 0,
            8: travel_to_raw(lift_mm),
            9: fire_to_raw(rt_press_mm),
            10: fire_to_raw(rt_lift_mm),
            11: 0,                      # dynamic travel off
            16: 20,                     # MT timer default
            25: deadzone_to_raw(deadzone_mm),
        }
        self.send(CMD_SET_MAGNETISM_MODE, fields, settle=0.5)

    def set_rapid_trigger(self, enabled: bool) -> None:
        """Toggle rapid trigger on the current custom profile."""
        # VHUB re-sends the whole custom block.  Read nothing; use current defaults.
        # The GUI/CLI should prefer set_global_magnetism() so values are explicit.
        self.send(CMD_SET_MAGNETISM_REPORT, {1: 1 if enabled else 0}, settle=0.05)

    def set_led(self, effect: int, *, brightness: int = 180, speed: int = 3,
                option: int = 0) -> None:
        """Set the lighting effect.

        effect: 0 off, 1 always-on, 2 breath, 3 wave, 4 ripple, 5 raindrop,
                6 snake, 7 press-action, 8 coverage, 9 user-picture
        speed : 1..5 (VHUB stores 5-speed)
        """
        speed = max(1, min(5, speed))
        self.send(CMD_SET_LEDPARAM, {
            1: effect,
            2: 5 - speed,
            3: max(0, min(255, brightness)),
            4: option,
        }, settle=0.5)

    def set_led_off(self) -> None:
        self.set_led(0, brightness=0)

    def set_rgb_all(self, rgb: int, *, profile: int | None = None,
                    indices: list[int] | None = None) -> None:
        """Write a solid colour to every valid key slot.

        Only matrix indices that hold a real key are written; the firmware
        STALLs the control pipe on gap indices.  The valid list is read from
        GET_KEYMATRIX (0x89) and cached.
        """
        if profile is None:
            profile = getattr(self, "_profile", 0)
        if indices is None:
            indices = getattr(self, "_valid_indices", None)
            if not indices:
                indices = self.valid_key_indices(profile)
                self._valid_indices = indices
                # The matrix read leaves the firmware busy; let it settle
                # before starting the colour stream.
                time.sleep(1.0)
        # A colour burst immediately after a status/matrix read can stall the
        # control pipe on this firmware, so always leave a generous gap.
        time.sleep(1.5)
        r = (rgb >> 16) & 0xFF
        g = (rgb >> 8) & 0xFF
        b = rgb & 0xFF
        for n, index in enumerate(indices):
            self.send(CMD_SET_USERPIC_SIMPLE, {
                1: profile,
                2: index,
                8: r,
                9: g,
                10: b,
            }, settle=0.08)
            if (n + 1) % 8 == 0:
                # Give the LED controller time to drain the colour queue.
                time.sleep(0.35)

    def set_magnetism_report(self, enabled: bool) -> None:
        self.send(CMD_SET_MAGNETISM_REPORT, {1: 1 if enabled else 0}, settle=0.1)

    def start_travel_calibration(self) -> None:
        self.send(CMD_SET_MAGNETISM_CAL, {1: 1}, settle=0.2)

    def start_max_calibration(self) -> None:
        self.send(CMD_SET_MAGNETISM_MAX_CAL, {1: 1}, settle=0.2)

    # -- presets -----------------------------------------------------------
    PRESETS = {
        "comfort":   dict(travel_mm=2.0, lift_mm=2.0, rapid_trigger=False,
                          rt_press_mm=0.2, rt_lift_mm=0.3),
        "sensitive": dict(travel_mm=0.5, lift_mm=2.0, rapid_trigger=False,
                          rt_press_mm=0.2, rt_lift_mm=0.3),
        "gaming":    dict(travel_mm=0.5, lift_mm=2.0, rapid_trigger=True,
                          rt_press_mm=0.3, rt_lift_mm=0.3),
    }

    def apply_preset(self, name: str) -> None:
        if name not in self.PRESETS:
            raise ValueError(f"Unknown preset: {name}")
        self.set_global_magnetism(**self.PRESETS[name])
