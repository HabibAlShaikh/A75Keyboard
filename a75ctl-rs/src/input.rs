//! Keyboard layout, HID input decoding and the live input reader thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::{discover, open_input, read_fd};

#[derive(Debug, Clone)]
pub struct KeyDef {
    pub label: &'static str,
    pub usage: u16,
    pub x: f32,
    pub y: f32,
    pub w: f32,
}

pub fn layout() -> Vec<KeyDef> {
    let mut keys = Vec::new();
    let mut add = |label, usage, x, y, w| {
        keys.push(KeyDef {
            label,
            usage,
            x,
            y,
            w,
        })
    };

    // Row 0: Esc, F1..F12, PrtSc, Del
    add("Esc", 0x29, 0.0, 0.0, 1.0);
    for i in 0..12 {
        add(
            match i {
                0 => "F1",
                1 => "F2",
                2 => "F3",
                3 => "F4",
                4 => "F5",
                5 => "F6",
                6 => "F7",
                7 => "F8",
                8 => "F9",
                9 => "F10",
                10 => "F11",
                _ => "F12",
            },
            0x3A + i as u16,
            1.25 + i as f32,
            0.0,
            1.0,
        );
    }
    // The A75 has Del at the far right of the F-row; there is no PrtSc key.
    add("Del", 0x4C, 14.5, 0.0, 1.0);

    // Row 1: ` 1..0 - = Backspace Home
    let row1 = [
        ("`", 0x35),
        ("1", 0x1E),
        ("2", 0x1F),
        ("3", 0x20),
        ("4", 0x21),
        ("5", 0x22),
        ("6", 0x23),
        ("7", 0x24),
        ("8", 0x25),
        ("9", 0x26),
        ("0", 0x27),
        ("-", 0x2D),
        ("=", 0x2E),
    ];
    let mut x = 0.0;
    for (l, u) in row1 {
        add(l, u, x, 1.15, 1.0);
        x += 1.0;
    }
    add("Bksp", 0x2A, x, 1.15, 2.0);
    // The physical A75 has Insert here, not Home.
    add("Insert", 0x49, x + 2.0, 1.15, 1.0);

    // Row 2: Tab Q..P [ ] \ PgUp
    let row2 = [
        ("Q", 0x14),
        ("W", 0x1A),
        ("E", 0x08),
        ("R", 0x15),
        ("T", 0x17),
        ("Y", 0x1C),
        ("U", 0x18),
        ("I", 0x0C),
        ("O", 0x12),
        ("P", 0x13),
    ];
    let mut x = 1.5;
    add("Tab", 0x2B, 0.0, 2.30, 1.5);
    for (l, u) in row2 {
        add(l, u, x, 2.30, 1.0);
        x += 1.0;
    }
    add("[", 0x2F, x, 2.30, 1.0);
    add("]", 0x30, x + 1.0, 2.30, 1.0);
    add("\\", 0x31, x + 2.0, 2.30, 1.5);
    add("PgUp", 0x4B, x + 3.5, 2.30, 1.0);

    // Row 3: Caps A..L ; ' Enter PgDn
    let row3 = [
        ("A", 0x04),
        ("S", 0x16),
        ("D", 0x07),
        ("F", 0x09),
        ("G", 0x0A),
        ("H", 0x0B),
        ("J", 0x0D),
        ("K", 0x0E),
        ("L", 0x0F),
    ];
    let mut x = 1.75;
    add("Caps", 0x39, 0.0, 3.45, 1.75);
    for (l, u) in row3 {
        add(l, u, x, 3.45, 1.0);
        x += 1.0;
    }
    add(";", 0x33, x, 3.45, 1.0);
    add("'", 0x34, x + 1.0, 3.45, 1.0);
    add("Enter", 0x28, x + 2.0, 3.45, 2.25);
    add("PgDn", 0x4E, x + 4.25, 3.45, 1.0);

    // Row 4: LShift Z..M , . / RShift Up End
    let row4 = [
        ("Z", 0x1D),
        ("X", 0x1B),
        ("C", 0x06),
        ("V", 0x19),
        ("B", 0x05),
        ("N", 0x11),
        ("M", 0x10),
    ];
    let mut x = 2.25;
    add("LShift", 0xE1, 0.0, 4.60, 2.25);
    for (l, u) in row4 {
        add(l, u, x, 4.60, 1.0);
        x += 1.0;
    }
    add(",", 0x36, x, 4.60, 1.0);
    add(".", 0x37, x + 1.0, 4.60, 1.0);
    add("/", 0x38, x + 2.0, 4.60, 1.0);
    add("RShift", 0xE5, x + 3.0, 4.60, 1.75);
    add("Up", 0x52, x + 4.75, 4.60, 1.0);
    add("End", 0x4D, x + 5.75, 4.60, 1.0);

    // Row 5: LCtrl Win Alt Space RAlt Fn RCtrl Left Down Right
    let mut x = 0.0;
    add("LCtrl", 0xE0, x, 5.75, 1.25);
    x += 1.25;
    add("Win", 0xE3, x, 5.75, 1.25);
    x += 1.25;
    add("Alt", 0xE2, x, 5.75, 1.25);
    x += 1.25;
    add("Space", 0x2C, x, 5.75, 6.25);
    x += 6.25;
    add("RAlt", 0xE6, x, 5.75, 1.25);
    x += 1.25;
    add("Fn", 0xFFFF, x, 5.75, 1.25);
    x += 1.25;
    add("RCtrl", 0xE4, x, 5.75, 1.25);
    x += 1.25;
    add("Left", 0x50, x, 5.75, 1.0);
    add("Down", 0x51, x + 1.0, 5.75, 1.0);
    add("Right", 0x4F, x + 2.0, 5.75, 1.0);

    keys
}

pub fn label_for(usage: u16) -> &'static str {
    for key in layout() {
        if key.usage == usage {
            return key.label;
        }
    }
    match usage {
        0x46 => "PrtSc",
        0x47 => "ScrollLock",
        0x48 => "Pause",
        0x4A => "Home",
        0x53 => "NumLock",
        0x64 => "F13",
        0x65 => "Menu",
        0xE0 => "LCtrl",
        0xE1 => "LShift",
        0xE2 => "LAlt",
        0xE3 => "LWin",
        0xE4 => "RCtrl",
        0xE5 => "RShift",
        0xE6 => "RAlt",
        0xE7 => "RWin",
        _ => "?",
    }
}

/// Keys that can be chosen as a remap target.  Includes keys that are not
/// present on the A75 (Home, PrtSc, ...), which is the whole point of remapping.
pub fn remap_targets() -> Vec<(&'static str, u16)> {
    let mut out: Vec<(&'static str, u16)> = Vec::new();
    let mut seen = HashSet::new();
    for key in layout() {
        if key.usage != 0xFFFF && seen.insert(key.usage) {
            out.push((key.label, key.usage));
        }
    }
    for (label, usage) in [
        ("Home", 0x4Au16),
        ("PrtSc", 0x46),
        ("ScrollLock", 0x47),
        ("Pause", 0x48),
        ("Menu", 0x65),
        ("NumLock", 0x53),
        ("RWin", 0xE7),
    ] {
        if seen.insert(usage) {
            out.push((label, usage));
        }
    }
    out
}

pub fn physical_keys() -> Vec<(&'static str, u16)> {
    let mut seen = HashSet::new();
    layout()
        .into_iter()
        .filter(|k| k.usage != 0xFFFF && seen.insert(k.usage))
        .map(|k| (k.label, k.usage))
        .collect()
}

#[derive(Debug)]
pub struct InputState {
    pub pressed: HashSet<u16>,
    // Raw HID usages used for release matching; `pressed` is remap-aware for
    // the on-screen keyboard.
    pub pressed_raw: HashSet<u16>,
    pub remap_display: HashMap<u16, u16>,
    pub last_key: Option<u16>,
    pub press_count: u64,
    pub counts: std::collections::HashMap<u16, u32>,
    pub down: std::collections::HashMap<u16, Instant>,
    pub up: std::collections::HashMap<u16, Instant>,
    pub last_down: Option<Instant>,
    pub rollover: usize,
    pub analog_on: bool,
    pub analog_tag: u8,
    pub analog_value: u16,
    pub analog_history: VecDeque<u16>,
    pub keyboard_connected: bool,
    pub vendor_connected: bool,
    pub events: VecDeque<String>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            pressed: HashSet::new(),
            pressed_raw: HashSet::new(),
            remap_display: HashMap::new(),
            last_key: None,
            press_count: 0,
            counts: std::collections::HashMap::new(),
            down: std::collections::HashMap::new(),
            up: std::collections::HashMap::new(),
            last_down: None,
            rollover: 0,
            analog_on: false,
            analog_tag: 0,
            analog_value: 0,
            analog_history: VecDeque::with_capacity(512),
            keyboard_connected: false,
            vendor_connected: false,
            events: VecDeque::with_capacity(32),
        }
    }
}

impl InputState {
    pub fn apply_keyboard_report(&mut self, report: &[u8]) {
        if report.is_empty() {
            return;
        }
        // Boot keyboard: [mod, reserved, k1..k6]; some devices prefix report id 1.
        let (mods, keys) = if report.len() >= 9 && report[0] == 1 {
            (report[1], &report[3..9])
        } else if report.len() >= 8 {
            (report[0], &report[2..8])
        } else {
            return;
        };

        let mut new = HashSet::new();
        for (bit, usage) in [
            (0x01, 0xE0u16),
            (0x02, 0xE1),
            (0x04, 0xE2),
            (0x08, 0xE3),
            (0x10, 0xE4),
            (0x20, 0xE5),
            (0x40, 0xE6),
            (0x80, 0xE7),
        ] {
            if mods & bit != 0 {
                new.insert(usage);
            }
        }
        for &k in keys {
            if k != 0 {
                new.insert(k as u16);
            }
        }

        // Report order gives a deterministic "last key" when several keys
        // arrive in the same report.  Keep raw usages for press/release
        // matching, but use the remapped physical key for the UI counters.
        for &raw in keys {
            let raw = raw as u16;
            let disp = *self.remap_display.get(&raw).unwrap_or(&raw);
            if raw != 0 && !self.pressed_raw.contains(&raw) {
                self.last_key = Some(disp);
                self.press_count += 1;
                *self.counts.entry(disp).or_insert(0) += 1;
                let now = Instant::now();
                self.down.insert(disp, now);
                self.up.remove(&disp);
                self.last_down = Some(now);
                self.events.push_back(if disp == raw {
                    format!("down  {}", label_for(raw))
                } else {
                    format!("down  {} ({})", label_for(raw), label_for(disp))
                });
                while self.events.len() > 24 {
                    self.events.pop_front();
                }
            }
        }
        for (bit, usage) in [
            (0x01, 0xE0u16),
            (0x02, 0xE1),
            (0x04, 0xE2),
            (0x08, 0xE3),
            (0x10, 0xE4),
            (0x20, 0xE5),
            (0x40, 0xE6),
            (0x80, 0xE7),
        ] {
            if mods & bit != 0 {
                let disp = *self.remap_display.get(&usage).unwrap_or(&usage);
                if !self.pressed_raw.contains(&usage) {
                    self.last_key = Some(disp);
                    self.press_count += 1;
                    *self.counts.entry(disp).or_insert(0) += 1;
                    let now = Instant::now();
                    self.down.insert(disp, now);
                    self.up.remove(&disp);
                    self.last_down = Some(now);
                    self.events.push_back(if disp == usage {
                        format!("down  {}", label_for(usage))
                    } else {
                        format!("down  {} ({})", label_for(usage), label_for(disp))
                    });
                    while self.events.len() > 24 {
                        self.events.pop_front();
                    }
                }
            }
        }
        for &raw in &self.pressed_raw {
            if !new.contains(&raw) {
                let disp = *self.remap_display.get(&raw).unwrap_or(&raw);
                self.up.insert(disp, Instant::now());
                self.events.push_back(if disp == raw {
                    format!("up    {}", label_for(raw))
                } else {
                    format!("up    {} ({})", label_for(raw), label_for(disp))
                });
                while self.events.len() > 24 {
                    self.events.pop_front();
                }
            }
        }
        let mut display_new = HashSet::new();
        for raw in &new {
            display_new.insert(*self.remap_display.get(raw).unwrap_or(raw));
        }
        self.pressed_raw = new;
        self.pressed = display_new;
        self.rollover = self.pressed.len();

    }

    pub fn apply_vendor_report(&mut self, report: &[u8]) {
        if report.len() >= 4 && report[0] == 0x05 && report[1] == 0x1B {
            let value = u16::from_be_bytes([report[2], report[3]]);
            self.analog_tag = report[1];
            self.analog_value = value;
            self.analog_history.push_back(value);
            while self.analog_history.len() > 600 {
                self.analog_history.pop_front();
            }
        }
    }

    /// Approximate live depth for the travel diagram.  The vendor stream is a
    /// raw 16-bit Hall value; its absolute millimetre calibration is not
    /// exposed, so map the full u16 range onto 0..=4.0 mm.
    pub fn analog_depth_mm(&self) -> Option<f32> {
        if !self.analog_on {
            return None;
        }
        Some((self.analog_value as f32 / u16::MAX as f32 * 4.0).clamp(0.0, 4.0))
    }

    pub fn reset_keys(&mut self) {
        self.pressed.clear();
        self.pressed_raw.clear();
        self.down.clear();
        self.up.clear();
        self.rollover = 0;
    }
}

pub fn spawn_reader(shared: Arc<Mutex<InputState>>) {
    thread::spawn(move || loop {
        let nodes = discover();
        let mut fds: HashMap<RawFd, bool> = HashMap::new(); // true = keyboard
        if let Some(path) = nodes.keyboard.as_deref() {
            if let Ok(fd) = open_input(path) {
                fds.insert(fd, true);
            }
        }
        if let Some(path) = nodes.vendor_input.as_deref() {
            if let Ok(fd) = open_input(path) {
                fds.insert(fd, false);
            }
        }
        {
            let mut st = shared.lock().unwrap();
            st.keyboard_connected = fds.values().any(|v| *v);
            st.vendor_connected = fds.values().any(|v| !*v);
        }
        if fds.is_empty() {
            thread::sleep(Duration::from_millis(800));
            continue;
        }

        let mut pollfds: Vec<libc::pollfd> = fds
            .keys()
            .map(|fd| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();

        let mut alive = true;
        while alive {
            let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, 60) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if rc == 0 {
                continue;
            }
            for pfd in pollfds.iter() {
                if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    alive = false;
                    break;
                }
                if pfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                match read_fd(pfd.fd) {
                    Ok(data) if !data.is_empty() => {
                        let mut st = shared.lock().unwrap();
                        if *fds.get(&pfd.fd).unwrap_or(&false) {
                            st.apply_keyboard_report(&data);
                        } else {
                            st.apply_vendor_report(&data);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if e.raw_os_error() != Some(libc::EAGAIN) {
                            break;
                        }
                    }
                }
            }
        }

        for fd in fds.keys() {
            unsafe {
                libc::close(*fd);
            }
        }
        {
            let mut st = shared.lock().unwrap();
            st.keyboard_connected = false;
            st.vendor_connected = false;
            st.reset_keys();
        }
        thread::sleep(Duration::from_millis(700));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keyboard_report_and_rollover() {
        let mut st = InputState::default();
        st.apply_keyboard_report(&[0, 0, 0x04, 0, 0, 0, 0, 0]);
        assert!(st.pressed.contains(&0x04));
        assert_eq!(st.rollover, 1);
        assert_eq!(st.press_count, 1);
        st.apply_keyboard_report(&[0, 0, 0x04, 0x16, 0x07, 0, 0, 0]);
        assert!(st.pressed.contains(&0x16));
        assert_eq!(st.rollover, 3);
        assert_eq!(st.last_key, Some(0x07));
        st.apply_keyboard_report(&[0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(st.pressed.is_empty());
        assert_eq!(st.rollover, 0);
    }

    #[test]
    fn parses_modifier_bits() {
        let mut st = InputState::default();
        st.apply_keyboard_report(&[0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert!(st.pressed.contains(&0xE1));
    }

    #[test]
    fn parses_analog_report() {
        let mut st = InputState::default();
        st.apply_vendor_report(&[0x05, 0x1B, 0x12, 0x34]);
        assert_eq!(st.analog_value, 0x1234);
        assert_eq!(st.analog_history.len(), 1);
    }

    #[test]
    fn analog_depth_is_optional_and_scaled() {
        let mut st = InputState::default();
        assert_eq!(st.analog_depth_mm(), None);
        st.analog_on = true;
        st.analog_value = 0x8000;
        let depth = st.analog_depth_mm().unwrap();
        assert!((depth - 2.0).abs() < 0.01);
        st.analog_value = u16::MAX;
        assert_eq!(st.analog_depth_mm(), Some(4.0));
    }
}
