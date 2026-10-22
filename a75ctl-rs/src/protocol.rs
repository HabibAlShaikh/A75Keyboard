//! VGN A75 HID protocol (reverse engineered from VHUB 4.0.9 + firmware v1.12).
//!
//! Vendor interface: HID usage page 0xFFFF / usage 0x0002, 64-byte feature
//! reports.  On Linux the hidraw buffer is 65 bytes: report id 0 + 64 bytes.
//!
//! Checksum: two's complement of the sum of bytes 0..7 in byte 7, except for
//! commands 0x07/0x08, where the checksum lives in byte 8.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use libc::{c_int, c_ulong, c_void};

pub const VENDOR_ID: u16 = 0x3151;
pub const A75_PIDS: [u16; 3] = [0x4010, 0x4011, 0x4015];

const REPORT_LEN: usize = 64;
const BUF_LEN: usize = REPORT_LEN + 1;

const fn iowr(nr: u64, len: usize) -> u64 {
    (3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}
const HIDIOCSFEATURE: u64 = iowr(0x06, BUF_LEN);
const HIDIOCGFEATURE: u64 = iowr(0x07, BUF_LEN);

#[derive(Debug, Clone)]
pub enum HidError {
    NotFound(String),
    Io(String),
    Stalled,
    Timeout,
    Protocol(String),
}

impl fmt::Display for HidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HidError::NotFound(s) => write!(f, "{s}"),
            HidError::Io(s) => write!(f, "{s}"),
            HidError::Stalled => write!(
                f,
                "The keyboard's control pipe stalled. Unplug and re-plug it (or power-cycle the USB port)."
            ),
            HidError::Timeout => write!(f, "The keyboard did not answer in time."),
            HidError::Protocol(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for HidError {}

pub type Result<T> = std::result::Result<T, HidError>;

fn map_io(e: io::Error) -> HidError {
    match e.raw_os_error() {
        Some(libc::EPROTO) | Some(libc::ESHUTDOWN) | Some(libc::ENODEV) => HidError::Stalled,
        Some(libc::ETIMEDOUT) | Some(libc::EAGAIN) => HidError::Timeout,
        _ => HidError::Io(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Nodes {
    pub feature: Option<String>,
    pub keyboard: Option<String>,
    pub vendor_input: Option<String>,
}

fn read_uevent(dir: &Path) -> Option<(u16, u16)> {
    let text = fs::read_to_string(dir.join("device/uevent")).ok()?;
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("HID_ID=") {
            let mut it = id.split(':');
            let _bus = it.next()?;
            let vid = u16::from_str_radix(it.next()?, 16).ok()?;
            let pid = u16::from_str_radix(it.next()?, 16).ok()?;
            return Some((vid, pid));
        }
    }
    None
}

fn read_descriptor(dir: &Path) -> Vec<u8> {
    fs::read(dir.join("device/report_descriptor")).unwrap_or_default()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

pub fn discover() -> Nodes {
    let mut nodes = Nodes::default();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return nodes;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("hidraw") {
            continue;
        }
        let Some((vid, pid)) = read_uevent(&entry.path()) else {
            continue;
        };
        if vid != VENDOR_ID || !A75_PIDS.contains(&pid) {
            continue;
        }
        let dev = format!("/dev/{name}");
        let desc = read_descriptor(&entry.path());
        if contains(&desc, &[0x06, 0xFF, 0xFF, 0x09, 0x02]) {
            nodes.feature = Some(dev);
        } else if contains(&desc, &[0x06, 0xFF, 0xFF, 0x09, 0x01]) {
            // Interface 1 contains both keyboard/consumer reports and the
            // vendor input report 5; classify it as the analog/vendor input.
            nodes.vendor_input = Some(dev);
        } else if contains(&desc, &[0x05, 0x01, 0x09, 0x06]) {
            nodes.keyboard = Some(dev);
        }
    }
    nodes
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub firmware: String,
    pub mled: String,
    pub profile: u8,
    pub mode: String,
    pub report_rate: u32,
    pub debounce: u8,
    pub dev_id: u16,
    pub keyboard_disabled: bool,
    pub info: String,
}

#[derive(Debug, Clone, Default)]
pub struct LedState {
    pub mode: u8,
    pub speed: u8,
    pub brightness: u8,
    pub option: u8,
    pub rgb: [u8; 3],
}

pub struct Device {
    fd: RawFd,
    pub path: String,
    last_query: Option<Instant>,
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

impl Device {
    pub fn open() -> Result<Self> {
        let nodes = discover();
        let Some(path) = nodes.feature.clone() else {
            return Err(HidError::NotFound(
                "VGN A75 vendor HID interface not found. Install 99-vgn-a75.rules or check /dev/hidraw*."
                    .into(),
            ));
        };
        let cpath = std::ffi::CString::new(path.clone())
            .map_err(|_| HidError::Io("invalid path".into()))?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(map_io(io::Error::last_os_error()));
        }
        let mut dev = Device {
            fd,
            path,
            last_query: None,
        };
        // Probe once so a stalled/closed node is reported immediately.
        let _ = dev.query(CMD_GET_REV, &[])?;
        // Leave a previous analog-test session: the keyboard does not type
        // while SET_MAGNETISM_REPORT is enabled.
        let _ = dev.set_analog_report(false);
        Ok(dev)
    }

    fn ioctl(&self, request: u64, buf: &mut [u8; BUF_LEN]) -> Result<()> {
        let rc = unsafe {
            libc::ioctl(
                self.fd,
                request as c_ulong,
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        if rc < 0 {
            return Err(map_io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn checksum_index(cmd: u8) -> usize {
        if cmd == 0x07 || cmd == 0x08 {
            8
        } else {
            7
        }
    }

    fn frame(cmd: u8, fields: &[(usize, u8)]) -> [u8; REPORT_LEN] {
        let mut p = [0u8; REPORT_LEN];
        p[0] = cmd;
        for &(i, v) in fields {
            if i < REPORT_LEN {
                p[i] = v;
            }
        }
        let idx = Self::checksum_index(cmd);
        p[idx] = 0xFFu8.wrapping_sub(p[..idx].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        p
    }

    fn write_packet(&mut self, packet: &[u8; REPORT_LEN]) -> Result<()> {
        if let Some(t) = self.last_query {
            let gap = Duration::from_millis(200);
            if t.elapsed() < gap {
                std::thread::sleep(gap - t.elapsed());
            }
        }
        let mut buf = [0u8; BUF_LEN];
        buf[1..].copy_from_slice(packet);
        self.ioctl(HIDIOCSFEATURE, &mut buf)
    }

    fn read_packet(&mut self) -> Result<[u8; REPORT_LEN]> {
        let mut buf = [0u8; BUF_LEN];
        self.ioctl(HIDIOCGFEATURE, &mut buf)?;
        let mut out = [0u8; REPORT_LEN];
        out.copy_from_slice(&buf[1..]);
        self.last_query = Some(Instant::now());
        Ok(out)
    }

    pub fn send(&mut self, cmd: u8, fields: &[(usize, u8)], settle: Duration) -> Result<()> {
        let packet = Self::frame(cmd, fields);
        self.write_packet(&packet)?;
        if !settle.is_zero() {
            std::thread::sleep(settle);
        }
        Ok(())
    }

    pub fn query(&mut self, cmd: u8, fields: &[(usize, u8)]) -> Result<[u8; REPORT_LEN]> {
        let packet = Self::frame(cmd, fields);
        let mut last = [0u8; REPORT_LEN];
        for attempt in 0..3 {
            match self.write_packet(&packet) {
                Ok(()) => {}
                Err(HidError::Timeout) => {
                    std::thread::sleep(Duration::from_millis(120 + 80 * attempt));
                    continue;
                }
                Err(e) => return Err(e),
            }
            std::thread::sleep(Duration::from_millis(40));
            match self.read_packet() {
                Ok(data) => {
                    if data[0] == cmd {
                        return Ok(data);
                    }
                    last = data;
                }
                Err(HidError::Timeout) => {
                    std::thread::sleep(Duration::from_millis(120 + 80 * attempt));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(HidError::Protocol(format!(
            "Unexpected response for command 0x{cmd:02X}: {}",
            hex(&last[..12])
        )))
    }

    pub fn query_raw(&mut self, cmd: u8, fields: &[(usize, u8)]) -> Result<[u8; REPORT_LEN]> {
        let packet = Self::frame(cmd, fields);
        for attempt in 0..3 {
            match self.write_packet(&packet) {
                Ok(()) => {}
                Err(HidError::Timeout) => {
                    std::thread::sleep(Duration::from_millis(120 + 80 * attempt));
                    continue;
                }
                Err(e) => return Err(e),
            }
            std::thread::sleep(Duration::from_millis(40));
            match self.read_packet() {
                Ok(data) => return Ok(data),
                Err(HidError::Timeout) => {
                    std::thread::sleep(Duration::from_millis(120 + 80 * attempt));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(HidError::Timeout)
    }

    // -- status ------------------------------------------------------------
    pub fn status(&mut self) -> Result<Status> {
        let ver = self.query(CMD_GET_REV, &[])?;
        let raw = u16::from_le_bytes([ver[1], ver[2]]);
        let firmware = format!("{}.{:02X}", raw >> 8, raw & 0xFF);

        let mled = self.query(CMD_GET_MLED, &[]).ok().map(|d| {
            let v = u16::from_le_bytes([d[1], d[2]]);
            format!("{}.{:02X}", v >> 8, v & 0xFF)
        }).unwrap_or_else(|| "?".into());

        let report = self.query(CMD_GET_REPORT, &[]).unwrap_or([0u8; REPORT_LEN]);
        let profile = report[1];
        let report_rate = match report[2] {
            1 => 1000,
            2 => 500,
            4 => 250,
            8 => 125,
            _ => 0,
        };

        let debounce = self.query(CMD_GET_DEBOUNCE, &[]).map(|d| d[2]).unwrap_or(0);
        let keymode = self.query(CMD_GET_KEY_MAGNETISM_MODE, &[(1, 0)]).unwrap_or([0u8; REPORT_LEN]);
        let mode = match keymode[1] {
            0 => "comfort",
            1 => "sensitive",
            2 => "gaming",
            3 => "custom",
            _ => "unknown",
        }
        .to_string();
        let keyboard_disabled = keymode[2] != 0;

        let info = self.query(CMD_GET_INFOR, &[]).unwrap_or([0u8; REPORT_LEN]);
        let dev_id = u16::from_le_bytes([info[1], info[2]]);

        Ok(Status {
            firmware,
            mled,
            profile,
            mode,
            report_rate,
            debounce,
            dev_id,
            keyboard_disabled,
            info: hex(&info[1..16]),
        })
    }

    pub fn led_state(&mut self) -> Result<LedState> {
        let d = self.query(CMD_GET_LEDPARAM, &[])?;
        Ok(LedState {
            mode: d[1],
            speed: 5u8.saturating_sub(d[2]),
            brightness: d[3],
            option: d[4],
            rgb: [d[5], d[6], d[7]],
        })
    }

    // -- actuation ---------------------------------------------------------
    pub fn set_actuation(
        &mut self,
        profile: u8,
        travel_mm: f32,
        lift_mm: f32,
        rapid_trigger: bool,
        rt_press_mm: f32,
        rt_lift_mm: f32,
        deadzone_mm: f32,
    ) -> Result<()> {
        let fields = [
            (1, profile),
            (2, 1),
            (3, if rapid_trigger { 0x80 } else { 0 }),
            (4, travel_to_raw(travel_mm)),
            (5, 1),
            (6, 0),
            (8, travel_to_raw(lift_mm)),
            (9, fire_to_raw(rt_press_mm)),
            (10, fire_to_raw(rt_lift_mm)),
            (11, 0),
            (16, 20),
            (25, deadzone_to_raw(deadzone_mm)),
        ];
        self.send(CMD_SET_MAGNETISM_MODE, &fields, Duration::from_millis(450))
    }

    #[allow(dead_code)]
    pub fn set_actuation_key(
        &mut self,
        profile: u8,
        index: u8,
        travel_mm: f32,
        lift_mm: f32,
        rapid_trigger: bool,
        rt_press_mm: f32,
        rt_lift_mm: f32,
        deadzone_mm: f32,
        mode_flags: u8,
    ) -> Result<()> {
        let mut flags = mode_flags & 0x7F;
        if rapid_trigger {
            flags |= 0x80;
        }
        let fields = [
            (1, profile),
            (2, 1),
            (3, flags),
            (4, travel_to_raw(travel_mm)),
            (5, 0),
            (6, index),
            (8, travel_to_raw(lift_mm)),
            (9, fire_to_raw(rt_press_mm)),
            (10, fire_to_raw(rt_lift_mm)),
            (11, 0),
            (16, 20),
            (25, deadzone_to_raw(deadzone_mm)),
        ];
        self.send(CMD_SET_MAGNETISM_MODE, &fields, Duration::from_millis(450))
    }

    #[allow(dead_code)]
    pub fn set_preset(&mut self, preset: u8) -> Result<()> {
        self.send(CMD_SET_KEY_MAGNETISM_MODE, &[(1, preset), (2, 0)], Duration::from_millis(450))
    }

    pub fn set_profile(&mut self, profile: u8) -> Result<()> {
        self.send(CMD_SET_PROFILE, &[(1, profile)], Duration::from_millis(450))
    }

    pub fn set_report_rate(&mut self, profile: u8, hz: u32) -> Result<()> {
        let idx = match hz {
            1000 => 1,
            500 => 2,
            250 => 4,
            125 => 8,
            _ => return Err(HidError::Protocol(format!("unsupported rate {hz}"))),
        };
        self.send(CMD_SET_REPORT, &[(1, profile), (2, idx)], Duration::from_millis(200))
    }

    pub fn set_debounce(&mut self, value: u8) -> Result<()> {
        self.send(CMD_SET_DEBOUNCE, &[(2, value)], Duration::from_millis(450))
    }

    pub fn set_analog_report(&mut self, on: bool) -> Result<()> {
        self.send(CMD_SET_MAGNETISM_REPORT, &[(1, on as u8)], Duration::from_millis(80))
    }

    pub fn calibrate(&mut self, maximum: bool) -> Result<()> {
        let cmd = if maximum {
            CMD_SET_MAGNETISM_MAX_CAL
        } else {
            CMD_SET_MAGNETISM_CAL
        };
        self.send(cmd, &[(1, 1)], Duration::from_millis(200))
    }

    // -- RGB ---------------------------------------------------------------
    pub fn set_led(
        &mut self,
        mode: u8,
        speed: u8,
        brightness: u8,
        option: u8,
        rgb: [u8; 3],
    ) -> Result<()> {
        // Command 0x07, checksum at byte 8 (VHUB CheckSumType.BIT8).
        //
        // The A75's v2 class stores MAXSPEED - speed in [2].  Byte [4] is
        // an option nibble *plus* the colour-mode nibble:
        //   0x0 = COMMONCOLOR[0] (red), 0x7 = NORMAL / use RGB,
        //   0x6 = DAZZLE.  Leaving it at 0 is why solid stayed red.
        let speed_field = 5u8.saturating_sub(speed.min(4));
        let option_field = if mode == 13 {
            option & 0x0F
        } else {
            ((option & 0x0F) << 4) | 0x07
        };
        let fields = [
            (1, mode),
            (2, speed_field),
            (3, brightness.min(4)),
            (4, option_field),
            (5, rgb[0]),
            (6, rgb[1]),
            (7, rgb[2]),
        ];
        self.send(CMD_SET_LEDPARAM, &fields, Duration::from_millis(800))
    }

    /// Full "lights off": a solid-black frame clears per-key overrides that
    /// would otherwise stay lit in mode 0, then the real Off mode is sent.
    pub fn set_off(&mut self) -> Result<()> {
        self.set_led(1, 4, 4, 0, [0, 0, 0])?;
        std::thread::sleep(Duration::from_millis(250));
        self.set_led(0, 4, 0, 0, [0, 0, 0])
    }

    pub fn set_key_color(&mut self, profile: u8, index: u8, rgb: [u8; 3]) -> Result<()> {
        let fields = [
            (1, profile),
            (2, index),
            (8, rgb[0]),
            (9, rgb[1]),
            (10, rgb[2]),
        ];
        self.send(CMD_SET_USERPIC_SIMPLE, &fields, Duration::from_millis(140))
    }

    /// Write the full per-profile key matrix (9 pages of 56 bytes).
    ///
    /// A75/VHUB uses SET_KEYMATRIX 0x09 with a 64-byte report:
    ///   [1]=profile [2]=0xF8 [3]=1 [4]=page [7]=checksum
    ///   [8..63]=56 bytes of matrix data
    pub fn set_matrix(&mut self, profile: u8, matrix: &[u8]) -> Result<()> {
        for page in 0..9usize {
            let mut p = [0u8; REPORT_LEN];
            p[0] = CMD_SET_KEYMATRIX;
            p[1] = profile;
            p[2] = 248;
            p[3] = 1;
            p[4] = page as u8;
            let start = page * 56;
            if start < matrix.len() {
                let end = (start + 56).min(matrix.len());
                p[8..8 + (end - start)].copy_from_slice(&matrix[start..end]);
            }
            p[7] = 0xFFu8.wrapping_sub(p[..7].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
            self.write_packet(&p)?;
            std::thread::sleep(Duration::from_millis(35));
        }
        std::thread::sleep(Duration::from_millis(900));
        Ok(())
    }

    pub fn matrix(&mut self, profile: u8) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(512);
        for page in 0..8u8 {
            let d = self.query_raw(CMD_GET_KEYMATRIX, &[(1, profile), (2, page)])?;
            out.extend_from_slice(&d);
        }
        Ok(out)
    }
}

pub fn travel_to_raw(mm: f32) -> u8 {
    ((10.0 * (mm - 0.1)).round().clamp(0.0, 255.0)) as u8
}
pub fn fire_to_raw(mm: f32) -> u8 {
    ((10.0 * mm).round().clamp(0.0, 255.0)) as u8
}
pub fn deadzone_to_raw(mm: f32) -> u8 {
    if mm <= 0.0 {
        0
    } else {
        ((10.0 * mm - 1.0).round().clamp(0.0, 255.0)) as u8
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

// Commands (A75 uses the shifted v2 map for report/profile/option/LED).
#[allow(dead_code)]
pub const CMD_SET_REPORT: u8 = 0x04;
#[allow(dead_code)]
pub const CMD_SET_PROFILE: u8 = 0x05;
#[allow(dead_code)]
pub const CMD_SET_KBOPTION: u8 = 0x06;
pub const CMD_SET_LEDPARAM: u8 = 0x07;
#[allow(dead_code)]
pub const CMD_SET_SLEDPARAM: u8 = 0x08;
pub const CMD_SET_DEBOUNCE: u8 = 0x11;
pub const CMD_SET_KEYMATRIX: u8 = 0x09;
pub const CMD_SET_USERPIC_SIMPLE: u8 = 0x14;
pub const CMD_SET_MAGNETISM_MODE: u8 = 0x1A;
pub const CMD_SET_MAGNETISM_REPORT: u8 = 0x1B;
pub const CMD_SET_MAGNETISM_CAL: u8 = 0x1C;
#[allow(dead_code)]
pub const CMD_SET_KEY_MAGNETISM_MODE: u8 = 0x1D;
pub const CMD_SET_MAGNETISM_MAX_CAL: u8 = 0x1E;

pub const CMD_GET_REV: u8 = 0x80;
pub const CMD_GET_REPORT: u8 = 0x84;
#[allow(dead_code)]
pub const CMD_GET_PROFILE: u8 = 0x85;
#[allow(dead_code)]
pub const CMD_GET_KBOPTION: u8 = 0x86;
pub const CMD_GET_LEDPARAM: u8 = 0x87;
#[allow(dead_code)]
pub const CMD_GET_SLEDPARAM: u8 = 0x88;
pub const CMD_GET_KEYMATRIX: u8 = 0x89;
pub const CMD_GET_INFOR: u8 = 0x8F;
pub const CMD_GET_DEBOUNCE: u8 = 0x91;
pub const CMD_GET_KEY_MAGNETISM_MODE: u8 = 0x9D;
pub const CMD_GET_MLED: u8 = 0xAE;

pub fn mode_name(mode: u8) -> &'static str {
    match mode {
        0 => "Off",
        1 => "Solid",
        2 => "Breath",
        3 => "Neon",
        4 => "Wave",
        5 => "Ripple",
        6 => "Raindrop",
        7 => "Snake",
        8 => "Press action",
        9 => "Coverage",
        10 => "Sine wave",
        11 => "Kaleidoscope",
        12 => "Line wave",
        13 => "User picture",
        14 => "Laser",
        15 => "Circle wave",
        16 => "Dazzling",
        17 => "Rain down",
        18 => "Meteor",
        19 => "Press action off",
        20 => "Music follow 3",
        21 => "Screen colour",
        22 => "Music follow 2",
        23 => "Train",
        24 => "Fireworks",
        66 => "User colour",
        _ => "Unknown",
    }
}

pub fn mode_from_name(name: &str) -> u8 {
    match name {
        "Off" => 0,
        "Solid" => 1,
        "Breath" => 2,
        "Neon" => 3,
        "Wave" => 4,
        "Ripple" => 5,
        "Raindrop" => 6,
        "Snake" => 7,
        "Press action" => 8,
        "Coverage" => 9,
        "Sine wave" => 10,
        "Kaleidoscope" => 11,
        "Line wave" => 12,
        "User picture" => 13,
        "Laser" => 14,
        "Circle wave" => 15,
        "Dazzling" => 16,
        "Rain down" => 17,
        "Meteor" => 18,
        "Press action off" => 19,
        "Music follow 3" => 20,
        "Screen colour" => 21,
        "Music follow 2" => 22,
        "Train" => 23,
        "Fireworks" => 24,
        "User colour" => 66,
        _ => 1,
    }
}

pub fn read_fd(fd: RawFd) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 64];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(buf[..n as usize].to_vec())
    }
}

pub fn open_input(path: &str) -> io::Result<RawFd> {
    let cpath = std::ffi::CString::new(path).unwrap();
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

#[allow(dead_code)]
fn _unused(_: c_int) {}
