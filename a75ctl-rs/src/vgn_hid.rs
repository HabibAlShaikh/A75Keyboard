//! Exact VGN USB HID packet builders and a small hidraw transport.
//!
//! Two very different protocols exist in the official VGN VHUB 4.0.9
//! Windows build:
//!
//! * A75 legacy (this keyboard): 64-byte **feature** reports on the vendor
//!   interface, usage page 0xFFFF, usage 0x0002, interface 2.
//! * VGN SDK v2 (Flash75 / Neon75 Extreme): 64-byte **output** reports on
//!   usage page 0xFFA0 or 0xFFB0, usage 1, report id 0.
//!
//! The A75 firmware does not implement the SDK-v2 feature set.  Its command
//! map is different: for example, an A75 feature packet whose byte 0 is 0x2C
//! is SET_FLASHCHIPERASSE, and SDK-v2 group 4/6 packets are interpreted as
//! different A75 commands.  Never send SDK-v2 packets to an A75.
#![allow(dead_code)]

use std::io;
use std::os::unix::io::RawFd;
use std::path::Path;

use libc::{c_ulong, c_void};

pub const A75_VID: u16 = 0x3151;
pub const A75_PID: u16 = 0x4015;
/// A75 variants seen in VGN/ROYUAN builds.
pub const A75_PIDS: [u16; 3] = [0x4010, 0x4011, 0x4015];
pub const A75_USAGE_PAGE: u16 = 0xFFFF;
pub const A75_USAGE: u16 = 0x0002;
pub const A75_FEATURE_INTERFACE: u8 = 2;
pub const A75_FEATURE_REPORT_LEN: usize = 64;

pub const SDK_V2_USAGE_PAGE_FLASH75: u16 = 0xFFA0;
pub const SDK_V2_USAGE_PAGE_EXTREME: u16 = 0xFFB0;
pub const SDK_V2_USAGE: u16 = 1;
pub const SDK_V2_REPORT_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    A75Legacy,
    VgnSdkV2,
    Rongyuan520,
    Unknown,
}

pub fn is_a75_pid(pid: u16) -> bool {
    A75_PIDS.contains(&pid)
}

pub fn classify(pid: u16, usage_page: u16, usage: u16, feature_report_len: usize) -> Family {
    if is_a75_pid(pid) && usage_page == A75_USAGE_PAGE && usage == A75_USAGE {
        Family::A75Legacy
    } else if usage == SDK_V2_USAGE
        && (usage_page == SDK_V2_USAGE_PAGE_FLASH75
            || usage_page == SDK_V2_USAGE_PAGE_EXTREME)
    {
        Family::VgnSdkV2
    } else if usage_page == 0xFFFF && usage == 1 && feature_report_len >= 520 {
        Family::Rongyuan520
    } else {
        Family::Unknown
    }
}

fn le16(v: u16) -> [u8; 2] {
    [v as u8, (v >> 8) as u8]
}

fn mm1000(mm: f32) -> u16 {
    (mm * 1000.0).round().clamp(0.0, u16::MAX as f32) as u16
}

fn checksum7(p: &mut [u8; 64]) {
    p[7] = 0xFFu8.wrapping_sub(p[..7].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
}

// ---------------------------------------------------------------------------
// A75 legacy protocol (validated on firmware v1.12)
// ---------------------------------------------------------------------------

/// A75 raw travel encoding: 0x00 = 0.1 mm, 0x27 = 4.0 mm.
pub fn a75_travel_raw(mm: f32) -> u8 {
    ((mm - 0.1) * 10.0).round().clamp(0.0, 255.0) as u8
}

/// A75 rapid-trigger / release encoding: 0.1 mm units (1 = 0.1 mm).
pub fn a75_fire_raw(mm: f32) -> u8 {
    (mm * 10.0).round().clamp(0.0, 255.0) as u8
}

/// A75 bottom-deadzone encoding.
pub fn a75_deadzone_raw(mm: f32) -> u8 {
    if mm <= 0.0 { 0 } else { (mm * 10.0 - 1.0).round().clamp(0.0, 255.0) as u8 }
}

/// SET_MAGNETISM_MODE 0x1A, per-key form.
///
/// `matrix_index` is the A75 matrix slot from GET_KEYMATRIX 0x89, **not** a
/// HID usage.  `mode_flags`: bit 7 = rapid trigger, 4 = toggle hold, 5 = toggle
/// dots; 0 for a normal key.
pub fn a75_per_key_actuation(
    profile: u8,
    matrix_index: u8,
    travel_mm: f32,
    lift_mm: f32,
    rapid_trigger: bool,
    rt_press_mm: f32,
    rt_lift_mm: f32,
    deadzone_mm: f32,
    mode_flags: u8,
) -> [u8; 64] {
    let mut p = [0u8; 64];
    let mut flags = mode_flags & 0x7F;
    if rapid_trigger {
        flags |= 0x80;
    }
    p[0] = 0x1A;
    p[1] = profile;
    p[2] = 1; // setting, not report
    p[3] = flags;
    p[4] = a75_travel_raw(travel_mm);
    p[5] = 0; // 0 = per-key, not global
    p[6] = matrix_index;
    p[8] = a75_travel_raw(lift_mm);
    p[9] = a75_fire_raw(rt_press_mm);
    p[10] = a75_fire_raw(rt_lift_mm);
    p[11] = 0;
    p[16] = 20;
    p[25] = a75_deadzone_raw(deadzone_mm);
    checksum7(&mut p);
    p
}

/// A75 SET_REPORT 0x04 / GET_REPORT 0x84.
/// `[2]`: 1 = 1000 Hz, 2 = 500 Hz, 4 = 250 Hz, 8 = 125 Hz.
pub fn a75_report_rate(profile: u8, hz: u32) -> Option<[u8; 64]> {
    let index = match hz {
        1000 => 1,
        500 => 2,
        250 => 4,
        125 => 8,
        _ => return None,
    };
    let mut p = [0u8; 64];
    p[0] = 0x04;
    p[1] = profile;
    p[2] = index;
    checksum7(&mut p);
    Some(p)
}

/// One 64-byte page of SET_KEYMATRIX 0x09.  Nine pages carry 504 bytes.
pub fn a75_matrix_page(profile: u8, page: u8, data: &[u8]) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = 0x09;
    p[1] = profile;
    p[2] = 0xF8; // total length 0x01F8 = 504
    p[3] = 0x01;
    p[4] = page;
    let start = page as usize * 56;
    for i in 0..56 {
        if let Some(b) = data.get(start + i) {
            p[8 + i] = *b;
        }
    }
    checksum7(&mut p);
    p
}

// ---------------------------------------------------------------------------
// VGN SDK v2 (official VHUB; Flash75 / Neon75 Extreme, NOT A75)
// ---------------------------------------------------------------------------

pub const HIGHER_KEY: u8 = 6;
pub const PERFORMANCE: u8 = 4;
pub const WRITE: u8 = 2;
pub const READ: u8 = 1;

pub const HIGHER_NONE: u8 = 0;
pub const HIGHER_DKS: u8 = 1;
pub const HIGHER_MPT: u8 = 2;
pub const HIGHER_MT: u8 = 3;
pub const HIGHER_TGL: u8 = 4;
pub const HIGHER_END: u8 = 5;
pub const HIGHER_SOCD: u8 = 6;
pub const HIGHER_RS: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocdResolution {
    Cover,
    PriorityA,
    PriorityB,
    Neutral,
}

impl SocdResolution {
    fn pair_values(self) -> [u8; 2] {
        match self {
            SocdResolution::Cover => [0, 0],
            SocdResolution::PriorityA => [1, 2],
            SocdResolution::PriorityB => [2, 1],
            SocdResolution::Neutral => [3, 3],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkRate {
    R8K,
    R4K,
    R2K,
    R1K,
    R500,
    R250,
    R125,
}

impl SdkRate {
    pub fn code(self) -> u8 {
        match self {
            SdkRate::R8K => 0,
            SdkRate::R4K => 1,
            SdkRate::R2K => 2,
            SdkRate::R1K => 3,
            SdkRate::R500 => 4,
            SdkRate::R250 => 5,
            SdkRate::R125 => 6,
        }
    }
}

fn sdk_header(kind: u8, row: u8, col: u8, mode: u8) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = kind;
    p[1] = WRITE;
    p[2] = row;
    p[3] = col;
    p[4] = mode;
    p
}

/// SDK v2 PERFORMANCE / SET_PERFORMANCE.
///
/// All millimetre fields are encoded as **micrometres**: mm * 1000 in a
/// little-endian u16 (0.1 mm = 100 = 0x0064; 4.0 mm = 4000 = 0x0FA0).
pub fn sdk_performance(
    row: u8,
    col: u8,
    mode: u8,
    normal_press_mm: f32,
    normal_release_mm: f32,
    rt_first_mm: f32,
    rt_press_mm: f32,
    rt_release_mm: f32,
    press_dead_mm: f32,
    release_dead_mm: f32,
    axis: u8,
    calibrate: u8,
    axis_v2: u16,
    axis_range_max: u16,
    axis_coefficient: u16,
) -> [u8; 64] {
    let mut p = sdk_header(PERFORMANCE, row, col, mode);
    p[1] = 2; // SetPerformance
    p[5..7].copy_from_slice(&le16(mm1000(normal_press_mm)));
    p[7..9].copy_from_slice(&le16(mm1000(normal_release_mm)));
    p[9..11].copy_from_slice(&le16(mm1000(rt_first_mm)));
    p[11..13].copy_from_slice(&le16(mm1000(rt_press_mm)));
    p[13..15].copy_from_slice(&le16(mm1000(rt_release_mm)));
    p[15..17].copy_from_slice(&le16(mm1000(press_dead_mm)));
    p[17..19].copy_from_slice(&le16(mm1000(release_dead_mm)));
    p[19] = axis;
    p[20] = calibrate;
    p[21..23].copy_from_slice(&le16(axis_v2));
    p[23..25].copy_from_slice(&le16(axis_range_max));
    p[25..27].copy_from_slice(&le16(axis_coefficient));
    p
}

/// SDK v2 PERFORMANCE / GET_PERFORMANCE.
pub fn sdk_get_performance(row: u8, col: u8) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = PERFORMANCE;
    p[1] = 1;
    p[2] = row;
    p[3] = col;
    p
}

/// SDK v2 HIGHER_KEY / WRITE / NONE.
pub fn sdk_higher_none(row: u8, col: u8) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_NONE);
    p[1] = WRITE;
    p
}

/// SDK v2 HIGHER_KEY / DKS.
pub fn sdk_higher_dks(
    row: u8,
    col: u8,
    keycodes: [u16; 4],
    trigger_points: [u8; 4],
    deadbands_ms: [u16; 2],
) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_DKS);
    p[1] = WRITE;
    for (i, kc) in keycodes.iter().enumerate() {
        let o = 5 + i * 2;
        p[o..o + 2].copy_from_slice(&le16(*kc));
    }
    p[13..17].copy_from_slice(&trigger_points);
    p[17..19].copy_from_slice(&le16(deadbands_ms[0]));
    p[19..21].copy_from_slice(&le16(deadbands_ms[1]));
    p
}

/// SDK v2 HIGHER_KEY / MPT (three actions + deadbands).
pub fn sdk_higher_mpt(row: u8, col: u8, keycodes: [u16; 3], deadbands_ms: [u16; 3]) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_MPT);
    p[1] = WRITE;
    for (i, kc) in keycodes.iter().enumerate() {
        let o = 5 + i * 2;
        p[o..o + 2].copy_from_slice(&le16(*kc));
    }
    for (i, db) in deadbands_ms.iter().enumerate() {
        let o = 11 + i * 2;
        p[o..o + 2].copy_from_slice(&le16(*db));
    }
    p
}

pub fn sdk_higher_mt(row: u8, col: u8, keycodes: [u16; 2], time: u16) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_MT);
    p[1] = WRITE;
    p[5..7].copy_from_slice(&le16(keycodes[0]));
    p[7..9].copy_from_slice(&le16(keycodes[1]));
    p[9..11].copy_from_slice(&le16(time));
    p
}

pub fn sdk_higher_tgl(row: u8, col: u8, keycode: u16, time: u16) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_TGL);
    p[1] = WRITE;
    p[5..7].copy_from_slice(&le16(keycode));
    p[7..9].copy_from_slice(&le16(time));
    p
}

pub fn sdk_higher_end(row: u8, col: u8, keycodes: [u16; 2], delay: u16) -> [u8; 64] {
    let mut p = sdk_header(HIGHER_KEY, row, col, HIGHER_END);
    p[1] = WRITE;
    p[5..7].copy_from_slice(&le16(keycodes[0]));
    p[7..9].copy_from_slice(&le16(keycodes[1]));
    p[9..11].copy_from_slice(&le16(delay));
    p
}

/// SDK v2 HIGHER_KEY / SOCD.  Returns the two reports the official app sends.
///
/// Packet 1 bytes:
///  0=6, 1=2, 2=row1, 3=col1, 4=6, 5=row2, 6=col2,
///  7..8=keycode1, 9..10=keycode2, 11..12=delay, 13=resolution[0]
/// Packet 2 swaps the two keys and uses resolution[1].
///
/// The resolution byte pair is:
///  Cover -> [0,0], PriorityA -> [1,2], PriorityB -> [2,1], Neutral -> [3,3].
pub fn sdk_higher_socd(
    row1: u8,
    col1: u8,
    row2: u8,
    col2: u8,
    keycode1: u16,
    keycode2: u16,
    delay: u16,
    resolution: SocdResolution,
) -> ([u8; 64], [u8; 64]) {
    let [r0, r1] = resolution.pair_values();
    let mut a = sdk_header(HIGHER_KEY, row1, col1, HIGHER_SOCD);
    a[1] = WRITE;
    a[5] = row2;
    a[6] = col2;
    a[7..9].copy_from_slice(&le16(keycode1));
    a[9..11].copy_from_slice(&le16(keycode2));
    a[11..13].copy_from_slice(&le16(delay));
    a[13] = r0;

    let mut b = sdk_header(HIGHER_KEY, row2, col2, HIGHER_SOCD);
    b[1] = WRITE;
    b[5] = row1;
    b[6] = col1;
    b[7..9].copy_from_slice(&le16(keycode2));
    b[9..11].copy_from_slice(&le16(keycode1));
    b[11..13].copy_from_slice(&le16(delay));
    b[13] = r1;
    (a, b)
}

/// SDK v2 HIGHER_KEY / RS (rapid swap), also a two-packet operation.
pub fn sdk_higher_rs(
    row1: u8,
    col1: u8,
    row2: u8,
    col2: u8,
    keycode1: u16,
    keycode2: u16,
    delay: u16,
) -> ([u8; 64], [u8; 64]) {
    let mut a = sdk_header(HIGHER_KEY, row1, col1, HIGHER_RS);
    a[1] = WRITE;
    a[5] = row2;
    a[6] = col2;
    a[7..9].copy_from_slice(&le16(keycode1));
    a[9..11].copy_from_slice(&le16(keycode2));
    a[11..13].copy_from_slice(&le16(delay));

    let mut b = sdk_header(HIGHER_KEY, row2, col2, HIGHER_RS);
    b[1] = WRITE;
    b[5] = row1;
    b[6] = col1;
    b[7..9].copy_from_slice(&le16(keycode2));
    b[9..11].copy_from_slice(&le16(keycode1));
    b[11..13].copy_from_slice(&le16(delay));
    (a, b)
}

/// SDK v2 GLOBAL / ReportRateSetting / WRITE.
/// Rate codes: 8K=0, 4K=1, 2K=2, 1K=3, 500=4, 250=5, 125=6.
pub fn sdk_report_rate(rate: SdkRate) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = 2; // Global
    p[1] = 5; // ReportRateSetting
    p[2] = WRITE;
    p[3] = rate.code();
    p
}

/// SDK v2 GLOBAL / HighPollingRateReset.
pub fn sdk_high_polling_reset() -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = 2; // Global
    p[1] = 15; // HighPollingRateReset
    p[2] = 1;
    p
}

// ---------------------------------------------------------------------------
// hidraw transport
// ---------------------------------------------------------------------------

const fn iowr(nr: u64, len: usize) -> u64 {
    (3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}

pub struct HidrawDevice {
    fd: RawFd,
    report_len: usize,
}

impl Drop for HidrawDevice {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

impl HidrawDevice {
    pub fn open(path: &Path, report_len: usize) -> io::Result<Self> {
        let cpath = std::ffi::CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, report_len })
    }

    /// A75 and SDK-v2 feature-style send.  The hidraw buffer includes a
    /// leading report id 0; payload is the 64-byte report.
    pub fn write_feature(&self, payload: &[u8]) -> io::Result<()> {
        if payload.len() != self.report_len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "wrong report length"));
        }
        let mut buf = vec![0u8; self.report_len + 1];
        buf[1..].copy_from_slice(payload);
        let req = iowr(0x06, self.report_len + 1) as c_ulong;
        let rc = unsafe { libc::ioctl(self.fd, req, buf.as_mut_ptr() as *mut c_void) };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Read a feature report back, including the leading report id byte.
    pub fn read_feature(&self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.report_len + 1];
        let req = iowr(0x07, self.report_len + 1) as c_ulong;
        let rc = unsafe { libc::ioctl(self.fd, req, buf.as_mut_ptr() as *mut c_void) };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(buf) }
    }

    /// Output report path used by SDK v2 (`sendReport(0, report)`).
    pub fn write_output(&self, payload: &[u8]) -> io::Result<()> {
        if payload.len() != self.report_len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "wrong report length"));
        }
        let mut buf = vec![0u8; self.report_len + 1];
        buf[1..].copy_from_slice(payload);
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Read an input report (used by SDK v2 after an output report).
    pub fn read_input(&self, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, len) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(buf) }
    }

    /// Safety-gated send.  Unknown devices are rejected rather than guessing a
    /// framing, and A75 packets always use the feature path.
    pub fn send(&self, family: Family, payload: &[u8]) -> io::Result<()> {
        match family {
            Family::A75Legacy | Family::Rongyuan520 => self.write_feature(payload),
            Family::VgnSdkV2 => self.write_output(payload),
            Family::Unknown => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unknown VGN protocol family; refusing to send",
            )),
        }
    }
}

/// Open the A75 feature interface found by the existing discovery code.
pub fn open_a75_feature() -> io::Result<HidrawDevice> {
    let nodes = crate::protocol::discover();
    let Some(path) = nodes.feature else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "A75 feature interface not found"));
    };
    HidrawDevice::open(Path::new(&path), A75_FEATURE_REPORT_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a75_travel_encoding() {
        assert_eq!(a75_travel_raw(0.1), 0x00);
        assert_eq!(a75_travel_raw(1.5), 14);
        assert_eq!(a75_travel_raw(2.0), 19);
        assert_eq!(a75_travel_raw(4.0), 39);
    }

    #[test]
    fn a75_per_key_packet_and_checksum() {
        let p = a75_per_key_actuation(0, 9, 0.1, 2.0, false, 0.2, 0.3, 3.7, 0);
        assert_eq!(p[0], 0x1A);
        assert_eq!(p[4], 0);
        assert_eq!(p[6], 9);
        assert_eq!(p[16], 20);
        assert_eq!(
            p[7],
            0xFFu8.wrapping_sub(p[..7].iter().fold(0u8, |a, b| a.wrapping_add(*b)))
        );
    }

    #[test]
    fn sdk_performance_uses_micrometres() {
        let p = sdk_performance(
            1, 2, 2, 0.1, 2.0, 0.2, 0.2, 0.3, 0.1, 0.2, 0, 0, 0, 0, 0,
        );
        assert_eq!(p[0], 4);
        assert_eq!(p[1], 2);
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 100);
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 2000);
    }

    #[test]
    fn sdk_dks_layout() {
        let p = sdk_higher_dks(2, 3, [0x04, 0x05, 0x06, 0x07], [10, 20, 30, 40], [100, 200]);
        assert_eq!(&p[..5], &[6, 2, 2, 3, 1]);
        assert_eq!(&p[5..7], &[4, 0]);
        assert_eq!(&p[7..9], &[5, 0]);
        assert_eq!(&p[9..11], &[6, 0]);
        assert_eq!(&p[11..13], &[7, 0]);
        assert_eq!(&p[13..17], &[10, 20, 30, 40]);
        assert_eq!(&p[17..19], &[100, 0]);
        assert_eq!(&p[19..21], &[200, 0]);
    }

    #[test]
    fn a75_rate_layout() {
        let p = a75_report_rate(0, 1000).unwrap();
        assert_eq!(&p[..3], &[0x04, 0, 1]);
        assert_eq!(
            p[7],
            0xFFu8.wrapping_sub(p[..7].iter().fold(0u8, |a, b| a.wrapping_add(*b)))
        );
    }

    #[test]
    fn sdk_socd_two_packets() {
        let (a, b) = sdk_higher_socd(1, 2, 3, 4, 0x04, 0x07, 10, SocdResolution::PriorityA);
        assert_eq!(&a[..7], &[6, 2, 1, 2, 6, 3, 4]);
        assert_eq!(&a[7..9], &[4, 0]);
        assert_eq!(&a[9..11], &[7, 0]);
        assert_eq!(a[13], 1);
        assert_eq!(&b[..7], &[6, 2, 3, 4, 6, 1, 2]);
        assert_eq!(&b[7..9], &[7, 0]);
        assert_eq!(&b[9..11], &[4, 0]);
        assert_eq!(b[13], 2);
    }

    #[test]
    fn sdk_rate_packet() {
        let p = sdk_report_rate(SdkRate::R1K);
        assert_eq!(&p[..4], &[2, 5, 2, 3]);
        assert_eq!(p[4], 0);
    }

    #[test]
    fn family_classification() {
        assert_eq!(
            classify(A75_PID, A75_USAGE_PAGE, A75_USAGE, 64),
            Family::A75Legacy
        );
        assert_eq!(
            classify(0x4010, A75_USAGE_PAGE, A75_USAGE, 64),
            Family::A75Legacy
        );
        assert_eq!(
            classify(0x0CA3, SDK_V2_USAGE_PAGE_FLASH75, 1, 64),
            Family::VgnSdkV2
        );
    }
}
