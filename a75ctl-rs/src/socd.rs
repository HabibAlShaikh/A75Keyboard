//! OS-level SOCD interceptor using Linux evdev + uinput.
//!
//! The A75 firmware has no SOCD command, so the only way to enforce SOCD is
//! to take the keyboard's evdev event node, suppress/rewrite the opposing key
//! events, and forward everything to a virtual uinput keyboard.
//!
//! The physical keyboard is opened and grabbed with `EVIOCGRAB`; the virtual
//! keyboard is created through `/dev/uinput`.  No external crates are used.
//!
//! Supported rules:
//! * Last Input Priority (Snappy Tappy)
//! * Neutral
//!
//! Deeper Press (Rappy Snappy) is intentionally not enforced: it needs live
//! per-key analog depth, which the A75 does not stream.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use libc::{c_int, c_ulong, c_void};

use crate::protocol::{A75_PIDS, VENDOR_ID};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0x00;
const SYN_DROPPED: u16 = 0x03;
const KEY_MAX: u16 = 0x02ff;

const BUS_USB: u16 = 0x03;
const VIRTUAL_VENDOR: u16 = 0x1d6b; // Linux Foundation
const VIRTUAL_PRODUCT: u16 = 0xa75c;

// Linux ioctl numbers (x86_64 and arm64 both use the same _IOC layout).
const EVIOCGRAB: c_ulong = 0x4004_4590;
const UI_SET_EVBIT: c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: c_ulong = 0x4004_5565;
const UI_DEV_SETUP: c_ulong = 0x405c_5503;
const UI_DEV_CREATE: c_ulong = 0x0000_5501;
const UI_DEV_DESTROY: c_ulong = 0x0000_5502;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocdMode {
    LastInput,
    DeeperPress,
    Neutral,
}

impl SocdMode {
    pub fn label(self) -> &'static str {
        match self {
            SocdMode::LastInput => "Last Input Priority (Snappy Tappy)",
            SocdMode::DeeperPress => "Deeper Press Priority (Rappy Snappy)",
            SocdMode::Neutral => "Neutral",
        }
    }

    pub fn all() -> [SocdMode; 3] {
        [
            SocdMode::LastInput,
            SocdMode::DeeperPress,
            SocdMode::Neutral,
        ]
    }

    pub fn code(self) -> u8 {
        match self {
            SocdMode::LastInput => 0,
            SocdMode::DeeperPress => 1,
            SocdMode::Neutral => 2,
        }
    }

    pub fn from_code(v: u8) -> Self {
        match v {
            1 => SocdMode::DeeperPress,
            2 => SocdMode::Neutral,
            _ => SocdMode::LastInput,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SocdRule {
    pub a: u16,
    pub b: u16,
    pub mode: SocdMode,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
struct UinputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: UinputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// Read a sysfs hex value such as `/sys/.../id/vendor` (`3151`).
fn read_hex(path: &Path) -> Option<u16> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    let text = text.strip_prefix("0x").unwrap_or(text);
    u16::from_str_radix(text, 16).ok()
}

/// Find the A75's main keyboard event node.
///
/// Several A75 interfaces expose event nodes with the same VID/PID.  Prefer
/// the one whose sysfs path is interface 0 (`:1.0`) and whose capabilities
/// contain key events; skip consumer/system/mouse nodes.
pub fn find_a75_event() -> io::Result<(PathBuf, String)> {
    let dir = fs::read_dir("/sys/class/input")
        .map_err(|e| io::Error::new(e.kind(), format!("cannot read /sys/class/input: {e}")))?;
    let mut best: Option<(i32, PathBuf, String)> = None;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("event") {
            continue;
        }
        let dev = entry.path();
        let Some(vid) = read_hex(&dev.join("device/id/vendor")) else {
            continue;
        };
        let Some(pid) = read_hex(&dev.join("device/id/product")) else {
            continue;
        };
        if vid != VENDOR_ID || !A75_PIDS.contains(&pid) {
            continue;
        }
        let Some(dev_name) = fs::read_to_string(dev.join("device/name"))
            .ok()
            .map(|s| s.trim().to_string())
        else {
            continue;
        };
        if dev_name.contains("Consumer")
            || dev_name.contains("System")
            || dev_name.contains("Mouse")
        {
            continue;
        }
        let keys = fs::read_to_string(dev.join("device/capabilities/key")).unwrap_or_default();
        if keys.trim().is_empty() || keys.trim() == "0" {
            continue;
        }
        let canonical = fs::canonicalize(&dev).unwrap_or_else(|_| dev.clone());
        let mut score = 0;
        if canonical.to_string_lossy().contains(":1.0") {
            score += 100;
        }
        if dev_name == "ROYUAN VGN A75" {
            score += 40;
        }
        if dev_name.ends_with("Keyboard") {
            score += 20;
        }
        let node = PathBuf::from("/dev/input").join(&name);
        if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
            best = Some((score, node, dev_name));
        }
    }
    best.map(|(_, path, name)| (path, name)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "VGN A75 evdev keyboard not found. Check /dev/input/event* permissions \
             (the udev rule should grant uaccess).",
        )
    })
}

fn set_uinput_bit(fd: RawFd, req: c_ulong, value: c_ulong) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, req, value) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_uinput() -> io::Result<RawFd> {
    let cpath = std::ffi::CString::new("/dev/uinput").unwrap();
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        let msg = if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
            "permission denied opening /dev/uinput. Install packaging/70-vgn-a75.rules              and run: sudo udevadm control --reload-rules && sudo udevadm trigger"
                .to_string()
        } else {
            format!("cannot open /dev/uinput: {e}")
        };
        return Err(io::Error::new(e.kind(), msg));
    }

    let fail = |fd: RawFd, e: io::Error| -> io::Result<RawFd> {
        unsafe { libc::close(fd) };
        Err(e)
    };

    if let Err(e) = set_uinput_bit(fd, UI_SET_EVBIT, EV_KEY as c_ulong) {
        return fail(fd, e);
    }
    // Claim every key so normal typing passes through unchanged.
    for code in 0..=KEY_MAX {
        if let Err(e) = set_uinput_bit(fd, UI_SET_KEYBIT, code as c_ulong) {
            return fail(fd, e);
        }
    }

    let mut setup = UinputSetup {
        id: UinputId {
            bustype: BUS_USB,
            vendor: VIRTUAL_VENDOR,
            product: VIRTUAL_PRODUCT,
            version: 1,
        },
        name: [0; 80],
        ff_effects_max: 0,
    };
    let name = b"VGN A75 SOCD Virtual Keyboard";
    setup.name[..name.len()].copy_from_slice(name);

    let rc = unsafe {
        libc::ioctl(
            fd,
            UI_DEV_SETUP,
            &setup as *const UinputSetup as *const c_void,
        )
    };
    if rc < 0 {
        return fail(fd, io::Error::last_os_error());
    }
    let rc = unsafe { libc::ioctl(fd, UI_DEV_CREATE) };
    if rc < 0 {
        return fail(fd, io::Error::last_os_error());
    }
    Ok(fd)
}

fn emit(fd: RawFd, type_: u16, code: u16, value: i32) -> io::Result<()> {
    let event = InputEvent {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_,
        code,
        value,
    };
    let mut attempts = 0;
    loop {
        let n = unsafe {
            libc::write(
                fd,
                &event as *const InputEvent as *const c_void,
                std::mem::size_of::<InputEvent>(),
            )
        };
        if n == std::mem::size_of::<InputEvent>() as isize {
            return Ok(());
        }
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) if attempts < 4 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                _ => return Err(e),
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write to uinput",
        ));
    }
}

fn emit_pair(fd: RawFd, events: &[(u16, i32)]) -> io::Result<()> {
    for &(code, value) in events {
        emit(fd, EV_KEY, code, value)?;
    }
    Ok(())
}

fn emit_syn(fd: RawFd) -> io::Result<()> {
    emit(fd, EV_SYN, SYN_REPORT, 0)
}

fn hid_usage_to_linux_key(usage: u16) -> Option<u16> {
    // HID keyboard usages -> Linux KEY_* codes.  Only the standard keyboard
    // and keypad ranges are needed for the A75.
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    match usage {
        0x04..=0x1D => Some(LETTERS[(usage - 0x04) as usize]),
        0x1E..=0x27 => Some(usage - 0x1E + 2),     // 1..0
        0x28 => Some(28),                          // Enter
        0x29 => Some(1),                           // Esc
        0x2A => Some(14),                          // Backspace
        0x2B => Some(15),                          // Tab
        0x2C => Some(57),                          // Space
        0x2D => Some(12),                          // -
        0x2E => Some(13),                          // =
        0x2F => Some(26),                          // [
        0x30 => Some(27),                          // ]
        0x31 | 0x32 => Some(43),                   // backslash / non-US #
        0x33 => Some(39),                          // ;
        0x34 => Some(40),                          // '
        0x35 => Some(41),                          // `
        0x36 => Some(51),                          // ,
        0x37 => Some(52),                          // .
        0x38 => Some(53),                          // /
        0x39 => Some(58),                          // Caps Lock
        0x3A..=0x43 => Some(59 + (usage - 0x3A)),  // F1..F10
        0x44 => Some(87),                          // F11
        0x45 => Some(88),                          // F12
        0x46 => Some(99),                          // Print Screen
        0x47 => Some(70),                          // Scroll Lock
        0x48 => Some(119),                         // Pause
        0x49 => Some(110),                         // Insert
        0x4A => Some(102),                         // Home
        0x4B => Some(104),                         // Page Up
        0x4C => Some(111),                         // Delete
        0x4D => Some(107),                         // End
        0x4E => Some(109),                         // Page Down
        0x4F => Some(106),                         // Right
        0x50 => Some(105),                         // Left
        0x51 => Some(108),                         // Down
        0x52 => Some(103),                         // Up
        0x53 => Some(69),                          // Num Lock
        0x54 => Some(98),                          // KP /
        0x55 => Some(55),                          // KP *
        0x56 => Some(74),                          // KP -
        0x57 => Some(78),                          // KP +
        0x58 => Some(96),                          // KP Enter
        0x59..=0x61 => Some(79 + (usage - 0x59)),  // KP 1..9
        0x62 => Some(82),                          // KP 0
        0x63 => Some(83),                          // KP .
        0x64 => Some(86),                          // non-US backslash
        0x65 => Some(139),                         // Menu
        0x66 => Some(116),                         // Power
        0x67 => Some(117),                         // KP =
        0x68..=0x73 => Some(183 + (usage - 0x68)), // F13..F24
        0xE0 => Some(29),                          // LCtrl
        0xE1 => Some(42),                          // LShift
        0xE2 => Some(56),                          // LAlt
        0xE3 => Some(125),                         // LGui
        0xE4 => Some(97),                          // RCtrl
        0xE5 => Some(54),                          // RShift
        0xE6 => Some(100),                         // RAlt
        0xE7 => Some(126),                         // RGui
        _ => None,
    }
}

fn pair_for(code: u16, rules: &[SocdRule]) -> Option<(u16, SocdMode)> {
    for rule in rules {
        let (Some(a), Some(b)) = (
            hid_usage_to_linux_key(rule.a),
            hid_usage_to_linux_key(rule.b),
        ) else {
            continue;
        };
        if code == a {
            return Some((b, rule.mode));
        }
        if code == b {
            return Some((a, rule.mode));
        }
    }
    None
}

/// Pure state machine for one physical keyboard.  Kept separate from the
/// ioctl/thread code so the SOCD behaviour is unit-testable.
#[derive(Default)]
struct Router {
    physical: HashSet<u16>,
    logical: HashSet<u16>,
}

impl Router {
    fn normal_key(&mut self, code: u16, value: i32) -> Vec<(u16, i32)> {
        match value {
            1 => {
                if self.physical.insert(code) && self.logical.insert(code) {
                    vec![(code, 1)]
                } else {
                    Vec::new()
                }
            }
            0 => {
                self.physical.remove(&code);
                if self.logical.remove(&code) {
                    vec![(code, 0)]
                } else {
                    Vec::new()
                }
            }
            2 => {
                if self.logical.contains(&code) {
                    vec![(code, 2)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn handle_key(&mut self, code: u16, value: i32, rules: &[SocdRule]) -> Vec<(u16, i32)> {
        let Some((opponent, mode)) = pair_for(code, rules) else {
            return self.normal_key(code, value);
        };

        if mode == SocdMode::DeeperPress {
            return self.normal_key(code, value);
        }

        match value {
            1 => {
                if !self.physical.insert(code) {
                    return Vec::new();
                }
                let opponent_held = self.physical.contains(&opponent);
                if !opponent_held {
                    if self.logical.insert(code) {
                        return vec![(code, 1)];
                    }
                    return Vec::new();
                }
                match mode {
                    SocdMode::LastInput => {
                        let mut out = Vec::new();
                        if self.logical.remove(&opponent) {
                            out.push((opponent, 0));
                        }
                        if self.logical.insert(code) {
                            out.push((code, 1));
                        }
                        out
                    }
                    SocdMode::Neutral => {
                        let mut out = Vec::new();
                        if self.logical.remove(&opponent) {
                            out.push((opponent, 0));
                        }
                        if self.logical.remove(&code) {
                            out.push((code, 0));
                        }
                        out
                    }
                    SocdMode::DeeperPress => Vec::new(),
                }
            }
            0 => {
                self.physical.remove(&code);
                let opponent_held = self.physical.contains(&opponent);
                let mut out = Vec::new();
                if self.logical.remove(&code) {
                    out.push((code, 0));
                }
                if opponent_held && self.logical.insert(opponent) {
                    out.push((opponent, 1));
                }
                out
            }
            2 => {
                if self.logical.contains(&code) {
                    vec![(code, 2)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn release_all(&mut self) -> Vec<(u16, i32)> {
        let mut keys: Vec<u16> = self.logical.drain().collect();
        keys.sort_unstable();
        self.physical.clear();
        keys.into_iter().map(|code| (code, 0)).collect()
    }
}

pub struct SocdInterceptor {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    join: Option<JoinHandle<()>>,
    physical_path: String,
}

impl SocdInterceptor {
    pub fn start(rules: Arc<Mutex<Vec<SocdRule>>>) -> io::Result<Self> {
        let (path, _name) = find_a75_event()?;
        let cpath = std::ffi::CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in evdev path"))?;
        let phys = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if phys < 0 {
            let e = io::Error::last_os_error();
            let msg = if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
                format!(
                    "permission denied opening {}. Install packaging/70-vgn-a75.rules                      (it must sort before 71-seat.rules), then run:                      sudo udevadm control --reload-rules && sudo udevadm trigger                      and replug the keyboard. For a quick temporary test:                      sudo setfacl -m u:$USER:rw {}",
                    path.display(),
                    path.display()
                )
            } else {
                format!("cannot open {}: {e}", path.display())
            };
            return Err(io::Error::new(e.kind(), msg));
        }
        let uinput = match create_uinput() {
            Ok(fd) => fd,
            Err(e) => {
                unsafe { libc::close(phys) };
                return Err(e);
            }
        };
        let grab = unsafe { libc::ioctl(phys, EVIOCGRAB, 1 as c_int) };
        if grab < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                libc::ioctl(uinput, UI_DEV_DESTROY);
                libc::close(uinput);
                libc::close(phys);
            }
            return Err(io::Error::new(
                e.kind(),
                format!("EVIOCGRAB failed on {}: {e}", path.display()),
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let error = Arc::new(Mutex::new(None));
        let stop_thread = stop.clone();
        let running_thread = running.clone();
        let error_thread = error.clone();
        let join = thread::Builder::new()
            .name("a75-socd".into())
            .spawn(move || {
                run_loop(
                    phys,
                    uinput,
                    rules,
                    stop_thread,
                    running_thread,
                    error_thread,
                )
            });

        match join {
            Ok(join) => Ok(Self {
                stop,
                running,
                error,
                join: Some(join),
                physical_path: path.display().to_string(),
            }),
            Err(e) => {
                unsafe {
                    libc::ioctl(phys, EVIOCGRAB, 0 as c_int);
                    libc::ioctl(uinput, UI_DEV_DESTROY);
                    libc::close(uinput);
                    libc::close(phys);
                }
                Err(e)
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    pub fn physical_path(&self) -> &str {
        &self.physical_path
    }
}

impl Drop for SocdInterceptor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn set_error(error: &Arc<Mutex<Option<String>>>, msg: String) {
    if let Ok(mut e) = error.lock() {
        *e = Some(msg);
    }
}

fn run_loop(
    phys: RawFd,
    uinput: RawFd,
    rules: Arc<Mutex<Vec<SocdRule>>>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
) {
    let mut router = Router::default();
    let mut pollfd = libc::pollfd {
        fd: phys,
        events: libc::POLLIN,
        revents: 0,
    };
    'outer: while !stop.load(Ordering::SeqCst) {
        pollfd.revents = 0;
        let rc = unsafe { libc::poll(&mut pollfd, 1, 100) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            set_error(&error, format!("evdev poll failed: {e}"));
            break 'outer;
        }
        if rc == 0 {
            continue;
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break 'outer;
        }
        if pollfd.revents & libc::POLLIN == 0 {
            continue;
        }
        loop {
            let mut event: InputEvent = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::read(
                    phys,
                    &mut event as *mut InputEvent as *mut c_void,
                    std::mem::size_of::<InputEvent>(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => break,
                    _ => {
                        set_error(&error, format!("evdev read failed: {e}"));
                        break 'outer;
                    }
                }
            }
            if n == 0 {
                break;
            }
            if n as usize != std::mem::size_of::<InputEvent>() {
                set_error(&error, "short read from evdev".into());
                break 'outer;
            }
            match event.type_ {
                EV_KEY => {
                    let snapshot = rules.lock().map(|r| r.clone()).unwrap_or_default();
                    let out = router.handle_key(event.code, event.value, &snapshot);
                    if let Err(e) = emit_pair(uinput, &out) {
                        set_error(&error, format!("uinput write failed: {e}"));
                        break 'outer;
                    }
                }
                EV_SYN if event.code == SYN_REPORT => {
                    if let Err(e) = emit_syn(uinput) {
                        set_error(&error, format!("uinput SYN failed: {e}"));
                        break 'outer;
                    }
                }
                EV_SYN if event.code == SYN_DROPPED => {
                    let ups = router.release_all();
                    let _ = emit_pair(uinput, &ups);
                    let _ = emit_syn(uinput);
                }
                _ => {}
            }
        }
    }

    // Release anything still logically down before destroying the virtual
    // keyboard, then drop the physical grab.  This is the lock-up guard.
    let ups = router.release_all();
    let _ = emit_pair(uinput, &ups);
    let _ = emit_syn(uinput);
    unsafe {
        libc::ioctl(phys, EVIOCGRAB, 0 as c_int);
        libc::ioctl(uinput, UI_DEV_DESTROY);
        libc::close(uinput);
        libc::close(phys);
    }
    running.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(a: u16, b: u16, mode: SocdMode) -> Vec<SocdRule> {
        vec![SocdRule { a, b, mode }]
    }

    #[test]
    fn hid_usage_mapping() {
        assert_eq!(hid_usage_to_linux_key(0x04), Some(30)); // A
        assert_eq!(hid_usage_to_linux_key(0x07), Some(32)); // D
        assert_eq!(hid_usage_to_linux_key(0x1A), Some(17)); // W
        assert_eq!(hid_usage_to_linux_key(0xE1), Some(42)); // LShift
    }

    #[test]
    fn last_input_priority() {
        let r = rule(0x04, 0x07, SocdMode::LastInput);
        let mut router = Router::default();
        assert_eq!(router.handle_key(30, 1, &r), vec![(30, 1)]);
        assert_eq!(router.handle_key(32, 1, &r), vec![(30, 0), (32, 1)]);
        assert_eq!(router.handle_key(32, 0, &r), vec![(32, 0), (30, 1)]);
        assert_eq!(router.handle_key(30, 0, &r), vec![(30, 0)]);
    }

    #[test]
    fn neutral_both_suppressed_then_remaining_reemitted() {
        let r = rule(0x04, 0x07, SocdMode::Neutral);
        let mut router = Router::default();
        assert_eq!(router.handle_key(30, 1, &r), vec![(30, 1)]);
        assert_eq!(router.handle_key(32, 1, &r), vec![(30, 0)]);
        assert_eq!(router.handle_key(30, 0, &r), vec![(32, 1)]);
        assert_eq!(router.handle_key(32, 0, &r), vec![(32, 0)]);
    }

    #[test]
    fn normal_keys_pass_through() {
        let mut router = Router::default();
        assert_eq!(router.handle_key(57, 1, &[]), vec![(57, 1)]);
        assert_eq!(router.handle_key(57, 2, &[]), vec![(57, 2)]);
        assert_eq!(router.handle_key(57, 0, &[]), vec![(57, 0)]);
    }

    #[test]
    fn release_all_emits_ups() {
        let r = rule(0x04, 0x07, SocdMode::LastInput);
        let mut router = Router::default();
        router.handle_key(30, 1, &r);
        assert_eq!(router.release_all(), vec![(30, 0)]);
        assert!(router.release_all().is_empty());
    }

    #[test]
    fn uinput_setup_is_92_bytes() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    #[test]
    #[ignore]
    fn finds_a75_event_node() {
        let (path, name) = find_a75_event().expect("A75 evdev node");
        assert!(path.exists(), "{}", path.display());
        assert!(!name.is_empty());
    }

    #[test]
    #[ignore]
    fn uinput_smoke() {
        // Manual hardware smoke test: creates and immediately destroys a
        // virtual keyboard.  It does not grab the physical device.
        let fd = create_uinput().expect("create uinput device");
        unsafe {
            libc::ioctl(fd, UI_DEV_DESTROY);
            libc::close(fd);
        }
    }
}
