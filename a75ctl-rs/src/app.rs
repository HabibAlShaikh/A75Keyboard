use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
};

use crate::input::{
    label_for, layout, physical_keys, remap_targets, spawn_reader, InputState,
};
use crate::protocol::{mode_from_name, mode_name, Device, LedState, Status};
use crate::socd::{SocdInterceptor, SocdMode, SocdRule};

/// Factory key matrix for profile 0, captured from the A75 before any remap.
/// 9 pages x 56 bytes = 504 bytes; GET_KEYMATRIX returns 512 including an
/// 8-byte footer that is not part of the keymap.
const FACTORY_MATRIX: &[u8; 504] = include_bytes!("../factory_matrix.bin");

#[derive(Debug, Clone, Copy)]
struct KeyTune {
    travel: f32,
    lift: f32,
    rt: bool,
    rt_press: f32,
    rt_lift: f32,
    deadzone: f32,
}

type SocdPair = SocdRule;

#[derive(Debug, Clone)]
struct Preset {
    name: String,
    per_key: HashMap<u16, KeyTune>,
    travel: f32,
    lift: f32,
    deadzone: f32,
    rt: bool,
    rt_press: f32,
    rt_lift: f32,
    rate: u32,
    debounce: u8,
    tachyon: bool,
    socd: Vec<SocdPair>,
}

fn derive_remaps(factory: &[u8], current: &[u8]) -> Vec<(u16, u16)> {
    let n = (factory.len().min(current.len())) / 4;
    let mut out = Vec::new();
    for i in 0..n {
        let f = &factory[i * 4..i * 4 + 4];
        let c = &current[i * 4..i * 4 + 4];
        let src = f[2] as u16;
        let dst = c[2] as u16;
        if src != 0 && src != dst {
            out.push((src, dst));
        }
    }
    out
}


// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum Cmd {
    Refresh,
    Reconnect,
    ReadMatrix,
    WriteMatrix {
        matrix: Vec<u8>,
        note: String,
    },
    Actuation {
        travel: f32,
        lift: f32,
        rt: bool,
        rt_press: f32,
        rt_lift: f32,
        deadzone: f32,
    },
    ActuationBatch {
        items: Vec<(u8, f32, f32, bool, f32, f32, f32)>,
    },
    ToggleBatch {
        indices: Vec<u8>,
        dots: bool,
        travel: f32,
        lift: f32,
    },
    ResetModes {
        indices: Vec<u8>,
        travel: f32,
        lift: f32,
        rt: bool,
        rt_press: f32,
        rt_lift: f32,
        deadzone: f32,
    },
    ActuationKey {
        index: u8,
        travel: f32,
        lift: f32,
        rt: bool,
        rt_press: f32,
        rt_lift: f32,
        deadzone: f32,
    },
    ToggleKey {
        index: u8,
        dots: bool,
        travel: f32,
        lift: f32,
    },
    Wasd,
    Caps3,
    Led {
        mode: u8,
        speed: u8,
        brightness: u8,
        option: u8,
        rgb: [u8; 3],
    },
    PaintIndex {
        index: u8,
        rgb: [u8; 3],
    },
    PaintAll {
        rgb: [u8; 3],
    },
    SetRate {
        profile: u8,
        hz: u32,
    },
    SetDebounce(u8),
    SetProfile(u8),
    Analog(bool),
    Calibrate(bool),
}

#[derive(Debug, Clone)]
enum Ev {
    Status(Status),
    Led(LedState),
    Matrix(Vec<u8>),
    Busy(bool),
    Progress(u32, u32),
    Log(String),
    Error(String),
    Connected(bool),
}

fn usage_index(matrix: &[u8], usage: u16) -> Option<u8> {
    for i in 0..matrix.len() / 4 {
        let rec = &matrix[i * 4..i * 4 + 4];
        if rec[2] as u16 == usage {
            return Some(i as u8);
        }
    }
    None
}

/// Physical slots for the quick presets, used when the active keymap has
/// already remapped the original usage away.
fn factory_index(usage: u16) -> Option<u8> {
    match usage {
        0x1A => Some(14), // W
        0x04 => Some(9),  // A
        0x16 => Some(15), // S
        0x07 => Some(21), // D
        0x39 => Some(3),  // Caps
        _ => None,
    }
}

fn remap_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("a75ctl").join("remap.bin")
}

fn perkey_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("a75ctl").join("perkey.txt")
}

fn save_perkey_file(path: &Path, keys: &HashMap<u16, KeyTune>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    let mut rows: Vec<_> = keys.iter().collect();
    rows.sort_by_key(|(u, _)| **u);
    for (usage, t) in rows {
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}\n",
            usage,
            t.travel,
            t.lift,
            t.rt as u8,
            t.rt_press,
            t.rt_lift,
            t.deadzone
        ));
    }
    fs::write(path, out)
}

fn load_perkey_file(path: &Path) -> HashMap<u16, KeyTune> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('|').collect();
        if f.len() == 7 {
            if let Ok(usage) = f[0].parse::<u16>() {
                out.insert(
                    usage,
                    KeyTune {
                        travel: f[1].parse().unwrap_or(2.0),
                        lift: f[2].parse().unwrap_or(2.0),
                        rt: f[3] == "1",
                        rt_press: f[4].parse().unwrap_or(0.2),
                        rt_lift: f[5].parse().unwrap_or(0.3),
                        deadzone: f[6].parse().unwrap_or(3.7),
                    },
                );
            }
        }
    }
    out
}

fn socd_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("a75ctl").join("socd.txt")
}

fn save_socd_file(path: &Path, pairs: &[SocdPair]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for p in pairs {
        out.push_str(&format!("{}|{}|{}\n", p.a, p.b, p.mode.code()));
    }
    fs::write(path, out)
}

fn load_socd_file(path: &Path) -> Vec<SocdPair> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('|').collect();
        if f.len() == 3 {
            if let (Ok(a), Ok(b)) = (f[0].parse(), f[1].parse()) {
                out.push(SocdPair {
                    a,
                    b,
                    mode: SocdMode::from_code(f[2].parse().unwrap_or(0)),
                });
            }
        }
    }
    out
}

fn profiles_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("a75ctl").join("profiles.txt")
}

fn save_remap_file(
    path: &Path,
    default: &[u8],
    shadow: &[u8],
    remaps: &[(u16, u16)],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut b = Vec::with_capacity(8 + 8 + default.len() + shadow.len() + 4 + remaps.len() * 4);
    b.extend_from_slice(b"A75R1\0\0\0");
    b.extend_from_slice(&(default.len() as u32).to_le_bytes());
    b.extend_from_slice(default);
    b.extend_from_slice(&(shadow.len() as u32).to_le_bytes());
    b.extend_from_slice(shadow);
    b.extend_from_slice(&(remaps.len() as u32).to_le_bytes());
    for (src, dst) in remaps {
        b.extend_from_slice(&src.to_le_bytes());
        b.extend_from_slice(&dst.to_le_bytes());
    }
    fs::write(path, b)
}

fn load_remap_file(path: &Path) -> Option<(Vec<u8>, Vec<u8>, Vec<(u16, u16)>)> {
    let b = fs::read(path).ok()?;
    if b.len() < 20 || &b[..8] != b"A75R1\0\0\0".as_slice() {
        return None;
    }
    let mut pos = 8usize;
    let read_u32 = |p: &mut usize| -> Option<u32> {
        if *p + 4 > b.len() {
            return None;
        }
        let v = u32::from_le_bytes(b[*p..*p + 4].try_into().ok()?);
        *p += 4;
        Some(v)
    };
    let dl = read_u32(&mut pos)? as usize;
    if dl > 4096 || pos + dl > b.len() {
        return None;
    }
    let default = b[pos..pos + dl].to_vec();
    pos += dl;
    let sl = read_u32(&mut pos)? as usize;
    if sl > 4096 || sl != dl || pos + sl > b.len() {
        return None;
    }
    let shadow = b[pos..pos + sl].to_vec();
    pos += sl;
    let n = read_u32(&mut pos)? as usize;
    if n > 1024 || pos + n * 4 > b.len() {
        return None;
    }
    let mut remaps = Vec::with_capacity(n);
    for _ in 0..n {
        let s = u16::from_le_bytes(b[pos..pos + 2].try_into().ok()?);
        let d = u16::from_le_bytes(b[pos + 2..pos + 4].try_into().ok()?);
        pos += 4;
        remaps.push((s, d));
    }
    if default.len() < 4 || default.len() % 4 != 0 {
        return None;
    }
    Some((default, shadow, remaps))
}

fn save_presets_file(path: &Path, presets: &[Preset]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::from("A75PROFILES1\n");
    for p in presets {
        let name = p.name.replace(['|', '\n', '\r'], " ");
        out.push_str(&format!(
            "P|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
            name,
            p.travel,
            p.lift,
            p.deadzone,
            p.rt as u8,
            p.rt_press,
            p.rt_lift,
            p.rate,
            p.debounce,
            p.tachyon as u8
        ));
        let mut keys: Vec<_> = p.per_key.iter().collect();
        keys.sort_by_key(|(u, _)| **u);
        for (usage, t) in keys {
            out.push_str(&format!(
                "K|{}|{}|{}|{}|{}|{}|{}\n",
                usage,
                t.travel,
                t.lift,
                t.rt as u8,
                t.rt_press,
                t.rt_lift,
                t.deadzone
            ));
        }
        for pair in &p.socd {
            out.push_str(&format!(
                "S|{}|{}|{}\n",
                pair.a,
                pair.b,
                pair.mode.code()
            ));
        }
    }
    fs::write(path, out)
}

fn load_presets_file(path: &Path) -> Vec<Preset> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur: Option<Preset> = None;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split('|').collect();
        match f.first().copied() {
            Some("P") if f.len() >= 11 => {
                if let Some(p) = cur.take() {
                    out.push(p);
                }
                cur = Some(Preset {
                    name: f[1].to_string(),
                    per_key: HashMap::new(),
                    travel: f[2].parse().unwrap_or(2.0),
                    lift: f[3].parse().unwrap_or(2.0),
                    deadzone: f[4].parse().unwrap_or(3.7),
                    rt: f[5] == "1",
                    rt_press: f[6].parse().unwrap_or(0.2),
                    rt_lift: f[7].parse().unwrap_or(0.3),
                    rate: f[8].parse().unwrap_or(1000),
                    debounce: f[9].parse().unwrap_or(5),
                    tachyon: f[10] == "1",
                    socd: Vec::new(),
                });
            }
            Some("K") if f.len() >= 8 => {
                if let Some(p) = cur.as_mut() {
                    if let Ok(usage) = f[1].parse::<u16>() {
                        p.per_key.insert(
                            usage,
                            KeyTune {
                                travel: f[2].parse().unwrap_or(2.0),
                                lift: f[3].parse().unwrap_or(2.0),
                                rt: f[4] == "1",
                                rt_press: f[5].parse().unwrap_or(0.2),
                                rt_lift: f[6].parse().unwrap_or(0.3),
                                deadzone: f[7].parse().unwrap_or(3.7),
                            },
                        );
                    }
                }
            }
            Some("S") if f.len() >= 4 => {
                if let Some(p) = cur.as_mut() {
                    if let (Ok(a), Ok(b)) = (f[1].parse::<u16>(), f[2].parse::<u16>()) {
                        p.socd.push(SocdPair {
                            a,
                            b,
                            mode: SocdMode::from_code(f[3].parse().unwrap_or(0)),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(p) = cur {
        out.push(p);
    }
    out
}

fn worker_loop(rx: Receiver<Cmd>, tx: Sender<Ev>) {
    let mut dev: Option<Device> = None;
    let mut profile: u8 = 0;
    let mut last_status = Instant::now() - Duration::from_secs(10);

    loop {
        if dev.is_none() {
            match Device::open() {
                Ok(mut d) => {
                    let _ = tx.send(Ev::Connected(true));
                    let _ = tx.send(Ev::Log(format!("Connected to {}", d.path)));
                    match d.status() {
                        Ok(s) => {
                            profile = s.profile;
                            let _ = tx.send(Ev::Status(s));
                        }
                        Err(e) => {
                            let _ = tx.send(Ev::Error(e.to_string()));
                        }
                    }
                    if let Ok(l) = d.led_state() {
                        let _ = tx.send(Ev::Led(l));
                    }
                    dev = Some(d);
                    last_status = Instant::now();
                }
                Err(e) => {
                    let _ = tx.send(Ev::Connected(false));
                    let _ = tx.send(Ev::Error(e.to_string()));
                    thread::sleep(Duration::from_millis(1200));
                    continue;
                }
            }
        }

        let wait = if last_status.elapsed() > Duration::from_millis(2500) {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(120)
        };

        let cmd = match rx.recv_timeout(wait) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if cmd.is_none() {
            if last_status.elapsed() > Duration::from_millis(2500) {
                let mut drop_dev = false;
                if let Some(d) = dev.as_mut() {
                    match d.status() {
                        Ok(s) => {
                            profile = s.profile;
                            let _ = tx.send(Ev::Status(s));
                            if let Ok(l) = d.led_state() {
                                let _ = tx.send(Ev::Led(l));
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Ev::Error(e.to_string()));
                            drop_dev = true;
                        }
                    }
                }
                if drop_dev {
                    dev = None;
                    let _ = tx.send(Ev::Connected(false));
                }
                last_status = Instant::now();
            }
            continue;
        }

        let Some(cmd) = cmd else { continue };
        if matches!(cmd, Cmd::Reconnect) {
            dev = None;
            let _ = tx.send(Ev::Log("Reconnecting...".into()));
            let _ = tx.send(Ev::Connected(false));
            continue;
        }

        let Some(d) = dev.as_mut() else { continue };
        let mut fail: Option<String> = None;
        match cmd {
            Cmd::Refresh => {
                match d.status() {
                    Ok(s) => {
                        profile = s.profile;
                        let _ = tx.send(Ev::Status(s));
                    }
                    Err(e) => fail = Some(e.to_string()),
                }
                if fail.is_none() {
                    match d.led_state() {
                        Ok(l) => {
                            let _ = tx.send(Ev::Led(l));
                        }
                        Err(e) => fail = Some(e.to_string()),
                    }
                }
            }
            Cmd::ReadMatrix => {
                match d.matrix(profile) {
                    Ok(m) => {
                        let _ = tx.send(Ev::Matrix(m));
                    }
                    Err(e) => fail = Some(e.to_string()),
                }
            },
            Cmd::WriteMatrix { matrix, note } => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.set_matrix(profile, &matrix) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(note));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::Actuation {
                travel,
                lift,
                rt,
                rt_press,
                rt_lift,
                deadzone,
            } => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) =
                    d.set_actuation(profile, travel, lift, rt, rt_press, rt_lift, deadzone)
                {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "Applied actuation: {travel:.2} mm / {lift:.2} mm, RT {}, deadzone {deadzone:.2} mm",
                        if rt { "on" } else { "off" }
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::ActuationKey {
                index,
                travel,
                lift,
                rt,
                rt_press,
                rt_lift,
                deadzone,
            } => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.set_actuation_key(
                    profile, index, travel, lift, rt, rt_press, rt_lift, deadzone, 0,
                ) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "Applied per-key actuation to matrix index {index}: {travel:.2}/{lift:.2} mm, RT {}",
                        if rt { "on" } else { "off" }
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::ActuationBatch { items } => {
                let _ = tx.send(Ev::Busy(true));
                let total = items.len() as u32;
                let mut done = 0u32;
                let mut err = None;
                for (index, travel, lift, rt, rt_press, rt_lift, deadzone) in &items {
                    if let Err(e) = d.set_actuation_key(
                        profile, *index, *travel, *lift, *rt, *rt_press, *rt_lift, *deadzone, 0,
                    ) {
                        err = Some(e.to_string());
                        break;
                    }
                    done += 1;
                    let _ = tx.send(Ev::Progress(done, total));
                }
                if let Some(e) = err {
                    fail = Some(e);
                } else {
                    let _ = tx.send(Ev::Progress(done, total));
                    let _ = tx.send(Ev::Log(format!("Applied actuation to {done} selected keys")));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::ToggleBatch {
                indices,
                dots,
                travel,
                lift,
            } => {
                let _ = tx.send(Ev::Busy(true));
                let flags = if dots { 5 } else { 4 };
                let total = indices.len() as u32;
                let mut done = 0u32;
                let mut err = None;
                for index in &indices {
                    if let Err(e) = d.set_actuation_key(
                        profile, *index, travel, lift, false, 0.2, 0.3, 0.0, flags,
                    ) {
                        err = Some(e.to_string());
                        break;
                    }
                    done += 1;
                    let _ = tx.send(Ev::Progress(done, total));
                }
                if let Some(e) = err {
                    fail = Some(e);
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "{done} selected keys set to {}",
                        if dots { "toggle dots" } else { "toggle hold" }
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::ResetModes {
                indices,
                travel,
                lift,
                rt,
                rt_press,
                rt_lift,
                deadzone,
            } => {
                let _ = tx.send(Ev::Busy(true));
                let total = indices.len() as u32;
                let mut done = 0u32;
                let mut err = None;
                for index in &indices {
                    if let Err(e) = d.set_actuation_key(
                        profile,
                        *index,
                        travel,
                        lift,
                        rt,
                        rt_press,
                        rt_lift,
                        deadzone,
                        0,
                    ) {
                        err = Some(e.to_string());
                        break;
                    }
                    done += 1;
                    let _ = tx.send(Ev::Progress(done, total));
                }
                if let Some(e) = err {
                    fail = Some(e);
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "{done} selected keys reset to Normal mode"
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::ToggleKey {
                index,
                dots,
                travel,
                lift,
            } => {
                let _ = tx.send(Ev::Busy(true));
                let flags = if dots { 5 } else { 4 };
                if let Err(e) = d.set_actuation_key(
                    profile, index, travel, lift, false, 0.2, 0.3, 0.0, flags,
                ) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "Key {index}: {} mode",
                        if dots { "toggle dots" } else { "toggle hold" }
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::Wasd => {
                let _ = tx.send(Ev::Busy(true));
                match d.matrix(profile) {
                    Ok(matrix) => {
                        thread::sleep(Duration::from_millis(900));
                        let mut err = None;
                        if let Err(e) = d.set_actuation(profile, 2.0, 2.0, false, 0.2, 0.3, 3.7) {
                            err = Some(e.to_string());
                        }
                        if err.is_none() {
                            thread::sleep(Duration::from_millis(700));
                            for (usage, name) in [
                                (0x1Au16, "W"),
                                (0x04, "A"),
                                (0x16, "S"),
                                (0x07, "D"),
                            ] {
                                if let Some(idx) = usage_index(&matrix, usage).or_else(|| factory_index(usage)) {
                                    if let Err(e) = d.set_actuation_key(
                                        profile, idx, 0.1, 2.0, false, 0.2, 0.3, 3.7, 0,
                                    ) {
                                        err = Some(e.to_string());
                                        break;
                                    }
                                    let _ = tx.send(Ev::Log(format!(
                                        "WASD: {name} -> 0.1 mm (matrix {idx})"
                                    )));
                                }
                            }
                        }
                        if let Some(e) = err {
                            fail = Some(e);
                        } else {
                            let _ = tx.send(Ev::Log("WASD hair-trigger preset applied".into()));
                        }
                    }
                    Err(e) => fail = Some(e.to_string()),
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::Caps3 => {
                let _ = tx.send(Ev::Busy(true));
                match d.matrix(profile) {
                    Ok(matrix) => {
                        thread::sleep(Duration::from_millis(900));
                        if let Some(idx) = usage_index(&matrix, 0x39).or_else(|| factory_index(0x39)) {
                            if let Err(e) = d.set_actuation_key(
                                profile, idx, 3.0, 3.0, false, 0.2, 0.3, 3.7, 0,
                            ) {
                                fail = Some(e.to_string());
                            } else {
                                let _ = tx.send(Ev::Log(format!(
                                    "Caps Lock -> 3.0 mm (matrix {idx})"
                                )));
                            }
                        } else {
                            fail = Some("Caps Lock not found in matrix".into());
                        }
                    }
                    Err(e) => fail = Some(e.to_string()),
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::Led {
                mode,
                speed,
                brightness,
                option,
                rgb,
            } => {
                let _ = tx.send(Ev::Busy(true));
                let led_result = if mode == 0 {
                    d.set_off()
                } else {
                    d.set_led(mode, speed, brightness, option, rgb)
                };
                if let Err(e) = led_result {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "RGB: {} #{:02x}{:02x}{:02x}",
                        mode_name(mode),
                        rgb[0],
                        rgb[1],
                        rgb[2]
                    )));
                    let _ = tx.send(Ev::Led(LedState {
                        mode,
                        speed,
                        brightness,
                        option,
                        rgb,
                    }));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::PaintIndex { index, rgb } => {
                let _ = tx.send(Ev::Busy(true));
                thread::sleep(Duration::from_millis(1000));
                if let Err(e) = d.set_key_color(profile, index, rgb) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "Painted matrix index {index} #{:02x}{:02x}{:02x}",
                        rgb[0], rgb[1], rgb[2]
                    )));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::PaintAll { rgb } => {
                let _ = tx.send(Ev::Busy(true));
                match d.matrix(profile) {
                    Ok(matrix) => {
                        thread::sleep(Duration::from_millis(1000));
                        let valid: Vec<u8> = (0..matrix.len() / 4)
                            .filter(|i| matrix[i * 4..i * 4 + 4].iter().any(|b| *b != 0))
                            .map(|i| i as u8)
                            .collect();
                        let total = valid.len() as u32;
                        let mut done = 0u32;
                        let mut err = None;
                        for (n, idx) in valid.iter().enumerate() {
                            if let Err(e) = d.set_key_color(profile, *idx, rgb) {
                                err = Some(e.to_string());
                                break;
                            }
                            done += 1;
                            if n % 4 == 0 {
                                let _ = tx.send(Ev::Progress(done, total));
                            }
                            if (n + 1) % 8 == 0 {
                                thread::sleep(Duration::from_millis(300));
                            }
                        }
                        let _ = tx.send(Ev::Progress(done, total));
                        if let Some(e) = err {
                            fail = Some(e);
                        } else {
                            let _ = tx.send(Ev::Log(format!(
                                "Painted {done}/{total} keys #{:02x}{:02x}{:02x}",
                                rgb[0], rgb[1], rgb[2]
                            )));
                        }
                    }
                    Err(e) => fail = Some(e.to_string()),
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::SetRate { profile: p, hz } => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.set_report_rate(p, hz) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!("Report rate: {hz} Hz")));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::SetDebounce(v) => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.set_debounce(v) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!("Debounce: {v}")));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::SetProfile(p) => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.set_profile(p) {
                    fail = Some(e.to_string());
                } else {
                    profile = p;
                    let _ = tx.send(Ev::Log(format!("Profile: {p}")));
                }
                let _ = tx.send(Ev::Busy(false));
            }
            Cmd::Analog(on) => {
                if let Err(e) = d.set_analog_report(on) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(format!(
                        "Analog report: {}",
                        if on { "on (keyboard will not type)" } else { "off" }
                    )));
                }
            }
            Cmd::Reconnect => {}
            Cmd::Calibrate(max) => {
                let _ = tx.send(Ev::Busy(true));
                if let Err(e) = d.calibrate(max) {
                    fail = Some(e.to_string());
                } else {
                    let _ = tx.send(Ev::Log(if max {
                        "Maximum calibration started".into()
                    } else {
                        "Travel calibration started - do not touch the keyboard".into()
                    }));
                }
                let _ = tx.send(Ev::Busy(false));
            }
        }

        if let Some(e) = fail {
            let _ = tx.send(Ev::Error(e.clone()));
            let stalled = e.to_lowercase().contains("stalled");
            let timeout = e.to_lowercase().contains("time");
            if stalled || timeout {
                dev = None;
                let _ = tx.send(Ev::Connected(false));
            }
        }
        last_status = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Actuation,
    Advanced,
    Performance,
    Profiles,
    Rgb,
    Remap,
    Keys,
    Device,
}

pub struct App {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    input: Arc<Mutex<InputState>>,

    tab: Tab,
    status: Option<Status>,
    led: Option<LedState>,
    connected: bool,
    busy: bool,
    progress: Option<(u32, u32)>,
    error: Option<String>,
    log: VecDeque<String>,
    synced: bool,
    led_dirty: bool,

    travel: f32,
    lift: f32,
    deadzone: f32,
    rt: bool,
    rt_press: f32,
    rt_lift: f32,

    rgb: [u8; 3],
    rgb_dirty: bool,
    last_rgb_change: Instant,
    led_mode: u8,
    led_speed: u8,
    led_brightness: u8,
    led_option: u8,

    profile: u8,
    rate: u32,
    debounce: u8,
    cal_confirm: bool,

    selected_keys: HashSet<u16>,
    selection_epoch: u64,
    marquee: Option<(Pos2, Pos2)>,
    per_key: HashMap<u16, KeyTune>,
    perkey_path: PathBuf,
    edit_travel: f32,
    edit_lift: f32,
    edit_rt: bool,
    edit_rt_press: f32,
    edit_rt_lift: f32,
    mixed_travel: bool,
    actuation_loaded_epoch: u64,

    socd_pairs: Vec<SocdPair>,
    socd_bind: Option<u8>,
    socd_key1: Option<u16>,
    socd_key2: Option<u16>,
    socd_mode: SocdMode,
    socd_path: PathBuf,
    enable_socd: bool,
    socd_engine: Option<SocdInterceptor>,
    socd_rules: Arc<Mutex<Vec<SocdRule>>>,

    tachyon: bool,
    tachyon_saved_led: Option<LedState>,
    tachyon_saved_rate: Option<u32>,

    presets: Vec<Preset>,
    preset_name: String,
    profiles_path: PathBuf,

    selected_usage: Option<u16>,
    heatmap: bool,
    usage_to_index: HashMap<u16, u8>,
    matrix: Vec<u8>,

    // Key remap state.  GET_KEYMATRIX returns the stored default matrix even
    // after a remap, so the active keymap is kept in this local shadow.
    matrix_default: Vec<u8>,
    matrix_shadow: Vec<u8>,
    remaps: Vec<(u16, u16)>,
    remap_source: u16,
    remap_target: u16,
    remap_path: PathBuf,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        thread::spawn(move || worker_loop(cmd_rx, ev_tx));

        let input = Arc::new(Mutex::new(InputState::default()));
        spawn_reader(input.clone());

        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals = egui::Visuals::dark();
            style.visuals.panel_fill = Color32::from_rgb(11, 16, 24);
            style.visuals.window_fill = Color32::from_rgb(16, 21, 30);
            style.visuals.extreme_bg_color = Color32::from_rgb(8, 12, 19);
            style.visuals.faint_bg_color = Color32::from_rgb(20, 29, 43);
            style.visuals.hyperlink_color = Color32::from_rgb(255, 212, 92);
            style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(26, 35, 49);
            style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(34, 48, 74);
            style.visuals.widgets.active.bg_fill = Color32::from_rgb(91, 81, 255);
            style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(139, 124, 255));
            style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(38, 54, 83));
            style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(48, 63, 89));
            style.visuals.selection.bg_fill = Color32::from_rgb(91, 81, 255);
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.slider_width = 320.0;
            style
                .text_styles
                .insert(egui::TextStyle::Heading, FontId::proportional(22.0));
            style
                .text_styles
                .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        });

        let remap_path = remap_file_path();
        let (_saved_default, mut saved_shadow, saved_remaps) =
            load_remap_file(&remap_path).unwrap_or_default();
        if saved_shadow.len() > FACTORY_MATRIX.len() {
            saved_shadow.truncate(FACTORY_MATRIX.len());
        }
        let use_saved = saved_shadow.len() == FACTORY_MATRIX.len();

        let _ = cmd_tx.send(Cmd::Refresh);
        if !use_saved {
            let _ = cmd_tx.send(Cmd::ReadMatrix);
        } else if !saved_remaps.is_empty() {
            let _ = cmd_tx.send(Cmd::WriteMatrix {
                matrix: saved_shadow.clone(),
                note: "Re-applied saved key remaps".into(),
            });
        }

        let start_tab = match std::env::var("A75_TAB").as_deref() {
            Ok("advanced") => Tab::Advanced,
            Ok("performance") => Tab::Performance,
            Ok("profiles") => Tab::Profiles,
            Ok("rgb") => Tab::Rgb,
            Ok("remap") => Tab::Remap,
            Ok("keys") => Tab::Keys,
            Ok("device") => Tab::Device,
            _ => Tab::Actuation,
        };

        if use_saved {
            let mut map = HashMap::new();
            for (src, dst) in &saved_remaps {
                map.insert(*dst, *src);
            }
            input.lock().unwrap().remap_display = map;
        }

        let mut usage_to_index = HashMap::new();
        for i in 0..FACTORY_MATRIX.len() / 4 {
            let rec = &FACTORY_MATRIX[i * 4..i * 4 + 4];
            let usage = rec[2] as u16;
            if usage != 0 {
                usage_to_index.insert(usage, i as u8);
            }
        }

        let profiles_path = profiles_file_path();
        let presets = load_presets_file(&profiles_path);
        let socd_path = socd_file_path();
        let socd_pairs = load_socd_file(&socd_path);
        let socd_rules = Arc::new(Mutex::new(socd_pairs.clone()));
        let perkey_path = perkey_file_path();
        let per_key = load_perkey_file(&perkey_path);

        Self {
            cmd_tx,
            ev_rx,
            input,
            tab: start_tab,
            status: None,
            led: None,
            connected: false,
            busy: false,
            progress: None,
            error: None,
            log: VecDeque::new(),
            synced: false,
            led_dirty: false,
            travel: 2.0,
            lift: 2.0,
            deadzone: 3.7,
            rt: false,
            rt_press: 0.2,
            rt_lift: 0.3,
            rgb: [255, 90, 0],
            rgb_dirty: false,
            last_rgb_change: Instant::now(),
            led_mode: 1,
            led_speed: 4,
            led_brightness: 4,
            led_option: 0,
            profile: 0,
            rate: 1000,
            debounce: 5,
            cal_confirm: false,
            selected_keys: HashSet::new(),
            selection_epoch: 0,
            marquee: None,
            per_key,
            perkey_path,
            edit_travel: 2.0,
            edit_lift: 2.0,
            edit_rt: false,
            edit_rt_press: 0.2,
            edit_rt_lift: 0.3,
            mixed_travel: false,
            actuation_loaded_epoch: 0,
            socd_pairs,
            socd_bind: None,
            socd_key1: None,
            socd_key2: None,
            socd_mode: SocdMode::LastInput,
            socd_path,
            enable_socd: false,
            socd_engine: None,
            socd_rules,
            tachyon: false,
            tachyon_saved_led: None,
            tachyon_saved_rate: None,
            presets,
            preset_name: "My preset".into(),
            profiles_path,
            selected_usage: None,
            heatmap: false,
            usage_to_index,
            matrix: FACTORY_MATRIX.to_vec(),
            matrix_default: FACTORY_MATRIX.to_vec(),
            matrix_shadow: if use_saved { saved_shadow } else { Vec::new() },
            remaps: if use_saved { saved_remaps } else { Vec::new() },
            remap_source: 0x39,
            remap_target: 0x4A,
            remap_path,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    fn sync_remap_display(&self) {
        let mut map = HashMap::new();
        for (src, dst) in &self.remaps {
            map.insert(*dst, *src);
        }
        self.input.lock().unwrap().remap_display = map;
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.log.push_back(msg.into());
        while self.log.len() > 80 {
            self.log.pop_front();
        }
    }

    fn sync_socd_rules(&self) {
        if let Ok(mut rules) = self.socd_rules.lock() {
            *rules = self.socd_pairs.clone();
        }
    }

    fn set_socd_enabled(&mut self, on: bool) {
        if on == self.enable_socd {
            return;
        }
        if !on {
            self.socd_engine.take();
            self.enable_socd = false;
            self.note("SOCD interceptor disabled; physical keyboard grab released.");
            return;
        }
        if self.socd_pairs.is_empty() {
            self.note("Add at least one SOCD pair before enabling the interceptor.");
            return;
        }
        self.sync_socd_rules();
        match SocdInterceptor::start(self.socd_rules.clone()) {
            Ok(engine) => {
                let path = engine.physical_path().to_string();
                self.socd_engine = Some(engine);
                self.enable_socd = true;
                self.note(format!(
                    "SOCD interceptor enabled on {path}; keyboard is now grabbed until disabled or the app exits."
                ));
            }
            Err(e) => {
                self.socd_engine = None;
                self.enable_socd = false;
                self.error = Some(format!("SOCD interceptor: {e}"));
                self.note(format!("Could not start SOCD interceptor: {e}"));
            }
        }
    }

    fn refresh_socd_state(&mut self) {
        let stopped = self.socd_engine.as_ref().and_then(|engine| {
            if engine.is_running() {
                None
            } else {
                Some(
                    engine
                        .last_error()
                        .unwrap_or_else(|| "interceptor thread stopped".into()),
                )
            }
        });
        if let Some(err) = stopped {
            self.socd_engine = None;
            self.enable_socd = false;
            self.error = Some(format!("SOCD interceptor stopped: {err}"));
            self.note(format!("SOCD interceptor stopped: {err}"));
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.ev_rx.try_recv() {
                Ok(Ev::Status(s)) => {
                    if !self.synced {
                        self.profile = s.profile;
                        if s.report_rate != 0 {
                            self.rate = s.report_rate;
                        }
                        self.debounce = s.debounce;
                        self.synced = true;
                    }
                    self.status = Some(s);
                }
                Ok(Ev::Led(l)) => {
                    let matches_request = l.mode == self.led_mode && l.rgb == self.rgb;
                    // While a write is pending, ignore stale status reads; only
                    // accept the event that matches the requested state.
                    if !self.led_dirty || matches_request {
                        self.led_mode = l.mode;
                        self.led_speed = l.speed;
                        self.led_brightness = l.brightness;
                        self.led_option = l.option;
                        self.rgb = l.rgb;
                        self.led_dirty = false;
                        self.led = Some(l);
                    }
                }
                Ok(Ev::Matrix(m)) => {
                    let usable = m[..m.len().min(FACTORY_MATRIX.len())].to_vec();
                    let read_remaps = derive_remaps(FACTORY_MATRIX, &usable);
                    // A GET issued before a remap write can arrive afterwards
                    // with the old matrix.  Do not drop local remaps unless the
                    // readback actually contains all of them.
                    let local_present = self.remaps.iter().all(|r| read_remaps.contains(r));
                    if self.remaps.is_empty() || local_present {
                        self.matrix_shadow = usable;
                        self.remaps = read_remaps;
                        self.sync_remap_display();
                        if !self.remaps.is_empty() {
                            self.persist_remaps();
                        }
                        self.note(format!(
                            "Key matrix: {} slots, {} remapped",
                            self.matrix_shadow.len() / 4,
                            self.remaps.len()
                        ));
                    } else {
                        self.note(
                            "Matrix readback did not include the local remaps; kept the local keymap."
                        );
                    }
                    self.matrix = FACTORY_MATRIX.to_vec();
                    self.usage_to_index.clear();
                    for i in 0..FACTORY_MATRIX.len() / 4 {
                        let rec = &FACTORY_MATRIX[i * 4..i * 4 + 4];
                        let usage = rec[2] as u16;
                        if usage != 0 {
                            self.usage_to_index.insert(usage, i as u8);
                        }
                    }
                    self.matrix_default = FACTORY_MATRIX.to_vec();
                }
                Ok(Ev::Busy(b)) => self.busy = b,
                Ok(Ev::Progress(done, total)) => self.progress = Some((done, total)),
                Ok(Ev::Log(s)) => self.note(s),
                Ok(Ev::Error(e)) => {
                    self.error = Some(e.clone());
                    self.note(format!("ERROR: {e}"));
                }
                Ok(Ev::Connected(c)) => self.connected = c,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn nav_button(&mut self, ui: &mut egui::Ui, label: &str, tab: Tab) {
        let selected = self.tab == tab;
        let fill = if selected {
            Color32::from_rgb(91, 81, 255)
        } else {
            Color32::TRANSPARENT
        };
        let color = if selected {
            Color32::WHITE
        } else {
            Color32::from_rgb(167, 182, 208)
        };
        let btn = egui::Button::new(
            RichText::new(format!("  {label}"))
                .size(14.0)
                .color(color),
        )
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(ui.available_width(), 34.0));
        if ui.add(btn).clicked() {
            self.tab = tab;
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(232.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(13, 19, 29))
                    .inner_margin(egui::Margin::symmetric(14, 18)),
            )
            .show(ui, |ui| {
                ui.label(
                    RichText::new("A75")
                        .size(30.0)
                        .strong()
                        .color(Color32::from_rgb(248, 249, 251)),
                );
                ui.label(
                    RichText::new("C O N T R O L")
                        .size(9.0)
                        .color(Color32::from_rgb(255, 212, 92)),
                );
                ui.add_space(14.0);

                egui::Frame::new()
                    .fill(Color32::from_rgb(22, 31, 46))
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(38, 54, 83)))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("●").color(if self.connected {
                                    Color32::from_rgb(74, 222, 128)
                                } else {
                                    Color32::from_rgb(248, 113, 113)
                                }),
                            );
                            ui.label(RichText::new("VGN A75").strong());
                        });
                        if let Some(st) = &self.status {
                            ui.label(
                                RichText::new(format!("fw v{} · MLED v{}", st.firmware, st.mled))
                                    .size(11.0)
                                    .color(Color32::from_rgb(167, 182, 208)),
                            );
                        }
                        ui.label(
                            RichText::new("ROYUAN 3151:4015")
                                .size(10.0)
                                .color(Color32::from_rgb(124, 132, 141)),
                        );
                        ui.add_space(6.0);
                        if ui.button("Reconnect").clicked() {
                            self.send(Cmd::Reconnect);
                        }
                    });

                ui.add_space(18.0);
                ui.label(
                    RichText::new("CONFIGURATION")
                        .size(9.0)
                        .strong()
                        .color(Color32::from_rgb(124, 132, 141)),
                );
                ui.add_space(4.0);
                self.nav_button(ui, "Actuation", Tab::Actuation);
                self.nav_button(ui, "Advanced Keys", Tab::Advanced);
                self.nav_button(ui, "Performance", Tab::Performance);
                self.nav_button(ui, "Profiles", Tab::Profiles);
                ui.add_space(14.0);
                ui.label(
                    RichText::new("TOOLS")
                        .size(9.0)
                        .strong()
                        .color(Color32::from_rgb(124, 132, 141)),
                );
                ui.add_space(4.0);
                self.nav_button(ui, "Lighting", Tab::Rgb);
                self.nav_button(ui, "Key Remap", Tab::Remap);
                self.nav_button(ui, "Key Test", Tab::Keys);
                self.nav_button(ui, "Device", Tab::Device);

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new("a75ctl 0.2 · native HID")
                            .size(10.0)
                            .color(Color32::from_rgb(92, 103, 125)),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Per-key tuning, RGB, live key test")
                            .size(10.0)
                            .color(Color32::from_rgb(92, 103, 125)),
                    );
                });
            });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                let (title, subtitle) = match self.tab {
                    Tab::Actuation => (
                        "Actuation",
                        "Per-key actuation depth and rapid trigger for the selected keys",
                    ),
                    Tab::Advanced => (
                        "Advanced Keys",
                        "SOCD key pairs, resolution modes and advanced key modes",
                    ),
                    Tab::Performance => (
                        "Performance",
                        "Tachyon mode, polling rate, deadzone and debounce",
                    ),
                    Tab::Profiles => (
                        "Profiles",
                        "Save and load your tuning presets",
                    ),
                    Tab::Rgb => ("RGB Settings", "Main effect and per-key colour table"),
                    Tab::Remap => (
                        "Key Remap",
                        "Make any physical key send a different HID key",
                    ),
                    Tab::Keys => ("Key Test", "Live HID input and analog telemetry"),
                    Tab::Device => ("Device", "Firmware, profile, debounce and calibration"),
                };
                ui.label(
                    RichText::new(title)
                        .size(25.0)
                        .strong()
                        .color(Color32::from_rgb(248, 249, 251)),
                );
                ui.label(
                    RichText::new(subtitle)
                        .size(12.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = egui::Button::new(
                    RichText::new("Save to keyboard")
                        .strong()
                        .color(Color32::from_rgb(22, 31, 46)),
                )
                .fill(Color32::from_rgb(255, 212, 92))
                .corner_radius(CornerRadius::same(8));
                if ui.add_enabled(!self.busy, save).clicked() {
                    self.save_all();
                }
                if let Some(st) = &self.status {
                    egui::Frame::new()
                        .fill(Color32::from_rgb(22, 31, 46))
                        .corner_radius(CornerRadius::same(8))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(38, 54, 83)))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("Profile {} · {}", st.profile, st.mode))
                                    .size(12.0)
                                    .color(Color32::from_rgb(203, 213, 225)),
                            );
                        });
                }
                if self.busy {
                    ui.spinner();
                }
            });
        });
        ui.add_space(10.0);
    }

    fn save_all(&mut self) {
        self.apply_actuation();
        if self.led_mode == 0 {
            self.send(Cmd::Led {
                mode: 0,
                speed: 4,
                brightness: 0,
                option: 0,
                rgb: [0, 0, 0],
            });
        } else {
            self.send(Cmd::Led {
                mode: self.led_mode,
                speed: self.led_speed,
                brightness: self.led_brightness,
                option: self.led_option,
                rgb: self.rgb,
            });
        }
        self.note("Saved current settings to the keyboard.");
    }

    fn slider_row(
        ui: &mut egui::Ui,
        label: &str,
        value: &mut f32,
        min: f32,
        max: f32,
        suffix: &str,
        help: &str,
    ) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::DragValue::new(value)
                            .range(min..=max)
                            .speed(0.01)
                            .fixed_decimals(2)
                            .suffix(suffix),
                    );
                });
            });
            let width = (ui.available_width() - 8.0).max(140.0);
            ui.spacing_mut().slider_width = width;
            ui.add(
                egui::Slider::new(value, min..=max)
                    .step_by(0.05)
                    .fixed_decimals(2)
                    .suffix(suffix)
                    .show_value(false),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(help)
                        .size(11.0)
                        .color(Color32::from_rgb(139, 147, 167)),
                )
                .wrap(),
            );
            ui.add_space(4.0);
        });
    }

    fn rgb_synced_marker(&mut self) {
        self.last_rgb_change = Instant::now();
    }

    fn apply_actuation(&mut self) {
        self.send(Cmd::Actuation {
            travel: self.travel,
            lift: self.lift,
            rt: self.rt,
            rt_press: self.rt_press,
            rt_lift: self.rt_lift,
            deadzone: self.deadzone,
        });
    }

    fn persist_remaps(&mut self) {
        if let Err(e) = save_remap_file(
            &self.remap_path,
            &self.matrix_default,
            &self.matrix_shadow,
            &self.remaps,
        ) {
            self.note(format!("Could not save key remaps: {e}"));
        }
    }

    fn apply_remap(&mut self) {
        if self.matrix_shadow.len() < 4 {
            self.note("Key matrix has not been read yet. Click Read key matrix.");
            return;
        }
        let source = self.remap_source;
        let target = self.remap_target;
        if source == 0xFFFF {
            self.note("Fn cannot be remapped; it is handled by the keyboard firmware.");
            return;
        }
        if target > 0xFF {
            self.note("That target is not a standard one-byte HID usage.");
            return;
        }
        let Some(src_idx) = usage_index(&self.matrix_default, source) else {
            self.note(format!(
                "{} was not found in the physical key matrix.",
                label_for(source)
            ));
            return;
        };
        // If the target exists physically, copy its factory record (including
        // any modifier flags).  Otherwise synthesise a normal key record.
        let rec: [u8; 4] = if let Some(ti) = usage_index(&self.matrix_default, target) {
            let ti = ti as usize;
            [
                self.matrix_default[ti * 4],
                self.matrix_default[ti * 4 + 1],
                self.matrix_default[ti * 4 + 2],
                self.matrix_default[ti * 4 + 3],
            ]
        } else {
            [0, 0, target as u8, 0]
        };
        let base = src_idx as usize * 4;
        self.matrix_shadow[base..base + 4].copy_from_slice(&rec);

        self.remaps.retain(|(s, _)| *s != source);
        self.remaps.push((source, target));
        self.sync_remap_display();
        self.input.lock().unwrap().reset_keys();
        self.persist_remaps();
        self.send(Cmd::WriteMatrix {
            matrix: self.matrix_shadow.clone(),
            note: format!(
                "Remapped {} -> {}. Press the physical key to test.",
                label_for(source),
                label_for(target)
            ),
        });
    }

    fn restore_remap_source(&mut self, source: u16) {
        if self.matrix_shadow.len() < 4 {
            return;
        }
        let Some(idx) = usage_index(&self.matrix_default, source) else {
            return;
        };
        let base = idx as usize * 4;
        let rec = [
            self.matrix_default[base],
            self.matrix_default[base + 1],
            self.matrix_default[base + 2],
            self.matrix_default[base + 3],
        ];
        self.matrix_shadow[base..base + 4].copy_from_slice(&rec);
        self.remaps.retain(|(s, _)| *s != source);
        self.sync_remap_display();
        self.input.lock().unwrap().reset_keys();
        self.persist_remaps();
        self.send(Cmd::WriteMatrix {
            matrix: self.matrix_shadow.clone(),
            note: format!("Restored {}.", label_for(source)),
        });
    }

    fn restore_all_remaps(&mut self) {
        if self.matrix_default.is_empty() {
            return;
        }
        self.matrix_shadow = self.matrix_default.clone();
        self.remaps.clear();
        self.sync_remap_display();
        self.input.lock().unwrap().reset_keys();
        self.persist_remaps();
        self.send(Cmd::WriteMatrix {
            matrix: self.matrix_shadow.clone(),
            note: "Restored the default key matrix.".into(),
        });
    }

    fn remap_tab(&mut self, ui: &mut egui::Ui) {
        if self.selected_usage.is_none() {
            self.selected_usage = Some(self.remap_source);
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Click a key on the map above to choose the remap source. Current: {}",
                        label_for(self.remap_source)
                    ))
                    .size(12.0)
                    .color(Color32::from_rgb(167, 182, 208)),
                );
                ui.add_space(6.0);
                card(ui, "Change a key", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Physical key");
                        egui::ComboBox::from_id_salt("remap_source")
                            .selected_text(label_for(self.remap_source))
                            .width(150.0)
                            .show_ui(ui, |ui| {
                                for (name, usage) in physical_keys() {
                                    if ui
                                        .selectable_label(self.remap_source == usage, name)
                                        .clicked()
                                    {
                                        self.remap_source = usage;
                                        self.selected_usage = Some(usage);
                                    }
                                }
                            });
                        ui.label("acts as");
                        egui::ComboBox::from_id_salt("remap_target")
                            .selected_text(label_for(self.remap_target))
                            .width(150.0)
                            .show_ui(ui, |ui| {
                                for (name, usage) in remap_targets() {
                                    if ui
                                        .selectable_label(self.remap_target == usage, name)
                                        .clicked()
                                    {
                                        self.remap_target = usage;
                                    }
                                }
                            });
                    });
                    ui.label(
                        RichText::new(format!(
                            "{} will send {}.",
                            label_for(self.remap_source),
                            label_for(self.remap_target)
                        ))
                        .size(12.0)
                        .color(Color32::from_rgb(255, 212, 92)),
                    );
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(
                            RichText::new("Remap key")
                                .strong()
                                .color(Color32::from_rgb(22, 31, 46)),
                        )
                        .fill(Color32::from_rgb(255, 212, 92))
                        .corner_radius(CornerRadius::same(8));
                        if ui
                            .add_enabled(
                                !self.busy && self.matrix_default.len() >= 4,
                                btn,
                            )
                            .clicked()
                        {
                            self.apply_remap();
                        }
                        if self.matrix_shadow.is_empty()
                            && ui.button("Read key matrix").clicked()
                        {
                            self.send(Cmd::ReadMatrix);
                        }
                        if ui.button("Open Key Test").clicked() {
                            self.tab = Tab::Keys;
                        }
                        if self.busy {
                            ui.spinner();
                        }
                    });
                    ui.label(
                        RichText::new(
                            "The keyboard is written as a complete 9-page matrix. \
                             Test the physical key in Key Test or in any text field.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Active remaps", |ui| {
                    if self.remaps.is_empty() {
                        ui.label(
                            RichText::new("No remapped keys.")
                                .size(12.0)
                                .color(Color32::from_rgb(167, 182, 208)),
                        );
                    } else {
                        for (src, dst) in self.remaps.clone() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{}  ->  {}",
                                        label_for(src),
                                        label_for(dst)
                                    ))
                                    .size(13.0),
                                );
                                if ui.button("Restore").clicked() {
                                    self.restore_remap_source(src);
                                }
                            });
                        }
                        ui.add_space(4.0);
                        if ui.button("Restore all keys").clicked() {
                            self.restore_all_remaps();
                        }
                    }
                    ui.label(
                        RichText::new(format!(
                            "Key matrix: {} bytes · {} remap(s) · saved to {}",
                            self.matrix_shadow.len(),
                            self.remaps.len(),
                            self.remap_path.display()
                        ))
                        .size(10.0)
                        .color(Color32::from_rgb(124, 132, 141)),
                    );
                });
            });
    }

    fn selected_summary(&self) -> String {
        let mut labels: Vec<&'static str> = self
            .selected_keys
            .iter()
            .map(|u| label_for(*u))
            .collect();
        labels.sort_unstable();
        let total = labels.len();
        labels.truncate(10);
        let mut out = labels.join(", ");
        if total > 10 {
            out.push_str(&format!(" … +{} more", total - 10));
        }
        out
    }

    fn key_tune(&self, usage: u16) -> KeyTune {
        self.per_key.get(&usage).copied().unwrap_or(KeyTune {
            travel: self.travel,
            lift: self.lift,
            rt: self.rt,
            rt_press: self.rt_press,
            rt_lift: self.rt_lift,
            deadzone: self.deadzone,
        })
    }

    fn all_physical_usages() -> Vec<u16> {
        physical_keys()
            .into_iter()
            .map(|(_, usage)| usage)
            .filter(|u| *u != 0xFFFF)
            .collect()
    }

    fn select_all_keys(&mut self) {
        self.selected_keys = Self::all_physical_usages().into_iter().collect();
        self.selected_usage = self.selected_keys.iter().next().copied();
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
    }

    fn deselect_all_keys(&mut self) {
        self.selected_keys.clear();
        self.selected_usage = None;
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
    }

    fn select_wasd(&mut self) {
        self.selected_keys.clear();
        for usage in [0x1Au16, 0x04, 0x16, 0x07] {
            self.selected_keys.insert(usage);
        }
        self.selected_usage = Some(0x1A);
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
    }

    fn invert_selection(&mut self) {
        for usage in Self::all_physical_usages() {
            if !self.selected_keys.remove(&usage) {
                self.selected_keys.insert(usage);
            }
        }
        self.selected_usage = self.selected_keys.iter().next().copied();
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
    }

    fn keyboard_panel(&mut self, ui: &mut egui::Ui) {
        let (pressed, counts, down, up) = {
            let st = self.input.lock().unwrap();
            (
                st.pressed.clone(),
                st.counts.clone(),
                st.down.clone(),
                st.up.clone(),
            )
        };
        let mut clicked = None;
        egui::Frame::new()
            .fill(Color32::from_rgb(22, 31, 46))
            .stroke(Stroke::new(1.0, Color32::from_rgb(38, 54, 83)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("KEYBOARD MAP")
                            .size(10.0)
                            .strong()
                            .color(Color32::from_rgb(167, 182, 208)),
                    );
                    ui.add_space(6.0);
                    if ui.small_button("Select All").clicked() {
                        self.select_all_keys();
                    }
                    if ui.small_button("Deselect All").clicked() {
                        self.deselect_all_keys();
                    }
                    if ui.small_button("WASD").clicked() {
                        self.select_wasd();
                    }
                    if ui.small_button("Invert Selection").clicked() {
                        self.invert_selection();
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} selected", self.selected_keys.len()))
                            .size(11.0)
                            .color(Color32::from_rgb(255, 212, 92)),
                    );
                    if let Some(slot) = self.socd_bind {
                        ui.separator();
                        ui.label(
                            RichText::new(format!("Click a key for SOCD Key {slot}"))
                                .size(11.0)
                                .strong()
                                .color(Color32::from_rgb(91, 81, 255)),
                        );
                        if ui.small_button("Cancel").clicked() {
                            self.socd_bind = None;
                        }
                    } else if self.tab == Tab::Remap {
                        ui.separator();
                        ui.label(
                            RichText::new(format!("Remap source: {}", label_for(self.remap_source)))
                                .size(11.0)
                                .color(Color32::from_rgb(167, 182, 208)),
                        );
                    }
                });
                ui.add_space(4.0);
                let heatmap = self.tab == Tab::Keys && self.heatmap;
                clicked = keyboard_canvas(
                    ui,
                    &pressed,
                    &counts,
                    &down,
                    &up,
                    heatmap,
                    &mut self.selected_keys,
                    &mut self.selected_usage,
                    &mut self.marquee,
                    &mut self.selection_epoch,
                    &self.per_key,
                    self.travel,
                    &self.socd_pairs,
                    true,
                );
            });
        if let Some(usage) = clicked {
            if let Some(slot) = self.socd_bind.take() {
                if slot == 1 {
                    self.socd_key1 = Some(usage);
                } else {
                    self.socd_key2 = Some(usage);
                }
                self.note(format!(
                    "SOCD Key {slot} bound to {}.",
                    label_for(usage)
                ));
            } else if self.tab == Tab::Remap {
                self.remap_source = usage;
            }
        }
        ui.add_space(6.0);
    }

    fn load_selection_edit(&mut self) {
        if self.actuation_loaded_epoch == self.selection_epoch {
            return;
        }
        self.actuation_loaded_epoch = self.selection_epoch;
        let tunes: Vec<KeyTune> = self.selected_keys.iter().map(|u| self.key_tune(*u)).collect();
        if tunes.is_empty() {
            return;
        }
        let first = tunes[0];
        self.edit_travel = tunes.iter().map(|t| t.travel).sum::<f32>() / tunes.len() as f32;
        self.mixed_travel = tunes
            .iter()
            .any(|t| (t.travel - first.travel).abs() > 0.001);
        self.edit_lift = first.lift;
        self.edit_rt = first.rt;
        self.edit_rt_press = first.rt_press;
        self.edit_rt_lift = first.rt_lift;
    }

    fn apply_selected_batch(&mut self) {
        if self.selected_keys.is_empty() {
            self.note("Select one or more keys on the map first.");
            return;
        }
        if self.mixed_travel {
            self.note("Actuation is Mixed. Move the Actuation point slider to choose a value.");
            return;
        }
        let tune = KeyTune {
            travel: self.edit_travel,
            lift: self.edit_lift,
            rt: self.edit_rt,
            rt_press: self.edit_rt_press,
            rt_lift: self.edit_rt_lift,
            deadzone: self.deadzone,
        };

        // A global write is one firmware packet instead of ~82 x 450 ms
        // per-key writes.  This is the Select All fast path.
        let all = Self::all_physical_usages();
        let all_selected = self.selected_keys.len() == all.len()
            && all.iter().all(|usage| self.selected_keys.contains(usage));
        if all_selected {
            self.travel = tune.travel;
            self.lift = tune.lift;
            self.rt = tune.rt;
            self.rt_press = tune.rt_press;
            self.rt_lift = tune.rt_lift;
            self.deadzone = tune.deadzone;
            self.edit_travel = tune.travel;
            self.edit_lift = tune.lift;
            self.edit_rt = tune.rt;
            self.edit_rt_press = tune.rt_press;
            self.edit_rt_lift = tune.rt_lift;
            self.mixed_travel = false;
            for usage in &all {
                self.per_key.insert(*usage, tune);
            }
            let _ = save_perkey_file(&self.perkey_path, &self.per_key);
            self.send(Cmd::Actuation {
                travel: tune.travel,
                lift: tune.lift,
                rt: tune.rt,
                rt_press: tune.rt_press,
                rt_lift: tune.rt_lift,
                deadzone: tune.deadzone,
            });
            self.note(format!(
                "Applying global {:.1} mm / {:.1} mm to all {} keys in one packet.",
                tune.travel,
                tune.lift,
                all.len()
            ));
            return;
        }

        let mut items = Vec::new();
        let mut skipped = 0usize;
        for usage in &self.selected_keys {
            if let Some(index) = self.usage_to_index.get(usage).copied() {
                self.per_key.insert(*usage, tune);
                items.push((
                    index,
                    tune.travel,
                    tune.lift,
                    tune.rt,
                    tune.rt_press,
                    tune.rt_lift,
                    tune.deadzone,
                ));
            } else {
                skipped += 1;
            }
        }
        if items.is_empty() {
            self.note("None of the selected keys has a writable matrix slot.");
            return;
        }
        let _ = save_perkey_file(&self.perkey_path, &self.per_key);
        self.send(Cmd::ActuationBatch { items });
        self.note(format!(
            "Applying {:.1} mm / {:.1} mm to {} selected key(s){}.",
            tune.travel,
            tune.lift,
            self.selected_keys.len().saturating_sub(skipped),
            if skipped > 0 { format!(" ({skipped} skipped)") } else { String::new() }
        ));
    }

    fn toggle_selected(&mut self, dots: bool) {
        let tune = self
            .selected_keys
            .iter()
            .next()
            .map(|u| self.key_tune(*u))
            .unwrap_or(KeyTune {
                travel: self.travel,
                lift: self.lift,
                rt: self.rt,
                rt_press: self.rt_press,
                rt_lift: self.rt_lift,
                deadzone: self.deadzone,
            });
        let indices: Vec<u8> = self
            .selected_keys
            .iter()
            .filter_map(|u| self.usage_to_index.get(u).copied())
            .collect();
        if indices.is_empty() {
            self.note("Select a key with a writable matrix slot first.");
            return;
        }
        self.send(Cmd::ToggleBatch {
            indices,
            dots,
            travel: tune.travel,
            lift: tune.lift,
        });
    }

    fn reset_selected_modes(&mut self) {
        let tune = self
            .selected_keys
            .iter()
            .next()
            .map(|u| self.key_tune(*u))
            .unwrap_or(KeyTune {
                travel: self.travel,
                lift: self.lift,
                rt: self.rt,
                rt_press: self.rt_press,
                rt_lift: self.rt_lift,
                deadzone: self.deadzone,
            });
        let indices: Vec<u8> = self
            .selected_keys
            .iter()
            .filter_map(|u| self.usage_to_index.get(u).copied())
            .collect();
        if indices.is_empty() {
            self.note("Select a key with a writable matrix slot first.");
            return;
        }
        self.send(Cmd::ResetModes {
            indices,
            travel: tune.travel,
            lift: tune.lift,
            rt: tune.rt,
            rt_press: tune.rt_press,
            rt_lift: tune.rt_lift,
            deadzone: tune.deadzone,
        });
    }

    fn set_tachyon(&mut self, on: bool) {
        if on == self.tachyon {
            return;
        }
        if on {
            self.tachyon_saved_rate = Some(self.rate);
            self.tachyon_saved_led = self.led.clone();
            self.tachyon = true;
            self.rate = 1000;
            // Stop complex animations while Tachyon is active.
            self.led_mode = 1;
            self.led_speed = 4;
            self.led_brightness = 4;
            self.led_option = 0;
            self.send(Cmd::SetRate { profile: self.profile, hz: 1000 });
            self.send(Cmd::Led {
                mode: 1,
                speed: 4,
                brightness: 4,
                option: 0,
                rgb: self.rgb,
            });
            self.note("Tachyon ON: 1000 Hz and RGB switched to solid.");
        } else {
            self.tachyon = false;
            if let Some(l) = self.tachyon_saved_led.take() {
                self.led_mode = l.mode;
                self.led_speed = l.speed;
                self.led_brightness = l.brightness;
                self.led_option = l.option;
                self.rgb = l.rgb;
                self.send(Cmd::Led {
                    mode: l.mode,
                    speed: l.speed,
                    brightness: l.brightness,
                    option: l.option,
                    rgb: l.rgb,
                });
            }
            if let Some(rate) = self.tachyon_saved_rate.take() {
                self.rate = rate;
                self.send(Cmd::SetRate { profile: self.profile, hz: rate });
            }
            self.note("Tachyon OFF: previous RGB/effect restored.");
        }
    }

    fn add_socd_pair(&mut self) {
        let (Some(a), Some(b)) = (self.socd_key1, self.socd_key2) else {
            self.note("Bind both SOCD keys first.");
            return;
        };
        if a == b {
            self.note("SOCD keys must be different.");
            return;
        }
        if self
            .socd_pairs
            .iter()
            .any(|p| (p.a == a && p.b == b) || (p.a == b && p.b == a))
        {
            self.note("That SOCD pair already exists.");
            return;
        }
        if self
            .socd_pairs
            .iter()
            .any(|p| p.a == a || p.b == a || p.a == b || p.b == b)
        {
            self.note("A key can only belong to one SOCD pair.");
            return;
        }
        self.socd_pairs.push(SocdPair {
            a,
            b,
            mode: self.socd_mode,
        });
        let _ = save_socd_file(&self.socd_path, &self.socd_pairs);
        self.sync_socd_rules();
        self.socd_key1 = None;
        self.socd_key2 = None;
        self.note(format!(
            "SOCD pair {} + {} added ({}).",
            label_for(a),
            label_for(b),
            self.socd_mode.label()
        ));
    }

    fn save_preset(&mut self) {
        let name = self.preset_name.trim().to_string();
        if name.is_empty() {
            self.note("Give the preset a name first.");
            return;
        }
        let preset = Preset {
            name: name.clone(),
            per_key: self.per_key.clone(),
            travel: self.travel,
            lift: self.lift,
            deadzone: self.deadzone,
            rt: self.rt,
            rt_press: self.rt_press,
            rt_lift: self.rt_lift,
            rate: self.rate,
            debounce: self.debounce,
            tachyon: self.tachyon,
            socd: self.socd_pairs.clone(),
        };
        self.presets.retain(|p| p.name != name);
        self.presets.push(preset);
        if let Err(e) = save_presets_file(&self.profiles_path, &self.presets) {
            self.note(format!("Could not save preset file: {e}"));
        } else {
            self.note(format!("Saved preset '{name}'."));
        }
    }

    fn load_preset(&mut self, index: usize) {
        let Some(p) = self.presets.get(index).cloned() else {
            return;
        };
        self.travel = p.travel;
        self.lift = p.lift;
        self.deadzone = p.deadzone;
        self.rt = p.rt;
        self.rt_press = p.rt_press;
        self.rt_lift = p.rt_lift;
        self.rate = p.rate;
        self.debounce = p.debounce;
        let want_tachyon = p.tachyon;
        self.per_key = p.per_key;
        let _ = save_perkey_file(&self.perkey_path, &self.per_key);
        self.socd_pairs = p.socd;
        let _ = save_socd_file(&self.socd_path, &self.socd_pairs);
        self.sync_socd_rules();
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.send(Cmd::Actuation {
            travel: p.travel,
            lift: p.lift,
            rt: p.rt,
            rt_press: p.rt_press,
            rt_lift: p.rt_lift,
            deadzone: p.deadzone,
        });
        let items: Vec<(u8, f32, f32, bool, f32, f32, f32)> = self
            .per_key
            .iter()
            .filter_map(|(usage, tune)| {
                self.usage_to_index.get(usage).copied().map(|index| {
                    (
                        index,
                        tune.travel,
                        tune.lift,
                        tune.rt,
                        tune.rt_press,
                        tune.rt_lift,
                        tune.deadzone,
                    )
                })
            })
            .collect();
        if !items.is_empty() {
            self.send(Cmd::ActuationBatch { items });
        }
        self.send(Cmd::SetRate { profile: self.profile, hz: p.rate });
        self.send(Cmd::SetDebounce(p.debounce));
        self.set_tachyon(want_tachyon);
        if !want_tachyon {
            self.rate = p.rate;
            self.send(Cmd::SetRate { profile: self.profile, hz: p.rate });
        }
        self.note(format!("Loaded preset '{}'.", p.name));
    }

    fn advanced_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                card(ui, "SOCD key pair", |ui| {
                    ui.horizontal(|ui| {
                        let mut enabled = self.enable_socd;
                        if ui
                            .add(egui::Checkbox::new(
                                &mut enabled,
                                "Enable OS-level SOCD interceptor",
                            ))
                            .changed()
                        {
                            self.set_socd_enabled(enabled);
                        }
                        if self.enable_socd {
                            if let Some(engine) = &self.socd_engine {
                                ui.label(
                                    RichText::new(format!(
                                        "Grabbed {} → virtual keyboard",
                                        engine.physical_path()
                                    ))
                                    .size(11.0)
                                    .color(Color32::from_rgb(80, 230, 140)),
                                );
                            }
                        }
                    });
                    ui.label(
                        RichText::new(
                            "The interceptor grabs the A75 evdev keyboard and forwards all \
                             keys through /dev/uinput. It is released automatically when \
                             disabled or when the app exits.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                    ui.horizontal_wrapped(|ui| {
                        let a = self
                            .socd_key1
                            .map(label_for)
                            .unwrap_or("not bound");
                        if ui.button(format!("Bind Key 1: {a}")).clicked() {
                            self.socd_bind = Some(1);
                        }
                        let b = self
                            .socd_key2
                            .map(label_for)
                            .unwrap_or("not bound");
                        if ui.button(format!("Bind Key 2: {b}")).clicked() {
                            self.socd_bind = Some(2);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Resolution mode");
                        egui::ComboBox::from_id_salt("socd_mode")
                            .selected_text(self.socd_mode.label())
                            .width(330.0)
                            .show_ui(ui, |ui| {
                                for mode in SocdMode::all() {
                                    if ui
                                        .selectable_label(self.socd_mode == mode, mode.label())
                                        .clicked()
                                    {
                                        self.socd_mode = mode;
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(
                            RichText::new("Add SOCD pair")
                                .strong()
                                .color(Color32::from_rgb(22, 31, 46)),
                        )
                        .fill(Color32::from_rgb(255, 212, 92));
                        if ui.add_enabled(
                            self.socd_key1.is_some() && self.socd_key2.is_some(),
                            btn,
                        ).clicked() {
                            self.add_socd_pair();
                        }
                        if self.socd_bind.is_some() {
                            ui.label(
                                RichText::new("Click a key on the map above.")
                                    .size(11.0)
                                    .color(Color32::from_rgb(91, 81, 255)),
                            );
                        }
                    });
                    if self.socd_mode == SocdMode::DeeperPress {
                        ui.label(
                            RichText::new(
                                "Rappy Snappy needs live per-key analog depth. The A75 does \
                                 not expose that stream to the controller, so this mode is \
                                 stored but passed through by the interceptor.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(255, 180, 90)),
                        );
                    }
                    ui.label(
                        RichText::new(
                            "The A75 firmware has no SOCD command. Enable the interceptor \
                             above to enforce Last Input Priority or Neutral in Linux; it \
                             needs evdev and /dev/uinput access (see the udev rule).",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Active SOCD pairs", |ui| {
                    if self.socd_pairs.is_empty() {
                        ui.label(
                            RichText::new("No SOCD pairs configured.")
                                .size(12.0)
                                .color(Color32::from_rgb(167, 182, 208)),
                        );
                    } else {
                        for (i, pair) in self.socd_pairs.clone().iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "S{}   {} + {}",
                                        i + 1,
                                        label_for(pair.a),
                                        label_for(pair.b)
                                    ))
                                    .strong(),
                                );
                                ui.label(
                                    RichText::new(pair.mode.label())
                                        .size(11.0)
                                        .color(Color32::from_rgb(167, 182, 208)),
                                );
                                if ui.button("Delete").clicked() {
                                    self.socd_pairs.remove(i);
                                    let _ = save_socd_file(&self.socd_path, &self.socd_pairs);
                                    self.sync_socd_rules();
                                }
                            });
                        }
                    }
                });

                ui.add_space(6.0);
                card(ui, "Selected keys - advanced modes", |ui| {
                    ui.label(format!(
                        "{} key(s) selected: {}",
                        self.selected_keys.len(),
                        self.selected_summary()
                    ));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.selected_keys.is_empty() && !self.busy,
                                egui::Button::new("Toggle hold"),
                            )
                            .clicked()
                        {
                            self.toggle_selected(false);
                        }
                        if ui
                            .add_enabled(
                                !self.selected_keys.is_empty() && !self.busy,
                                egui::Button::new("Toggle dots"),
                            )
                            .clicked()
                        {
                            self.toggle_selected(true);
                        }
                        if ui
                            .add_enabled(
                                !self.selected_keys.is_empty() && !self.busy,
                                egui::Button::new("Normal mode"),
                            )
                            .clicked()
                        {
                            self.reset_selected_modes();
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Toggle modes are firmware per-key flags and remain independent \
                             of SOCD pairing.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });
            });
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                card(ui, "Tachyon Mode", |ui| {
                    let mut on = self.tachyon;
                    if ui
                        .add(egui::Checkbox::new(&mut on, "Enable Tachyon Mode"))
                        .changed()
                    {
                        self.set_tachyon(on);
                    }
                    if self.tachyon {
                        egui::Frame::new()
                            .fill(Color32::from_rgb(57, 47, 20))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(
                                        "TACHYON ON — 1000 Hz maximum; complex RGB animations \
                                         are paused and lighting is solid to prioritise input.",
                                    )
                                    .size(11.0)
                                    .color(Color32::from_rgb(255, 212, 92)),
                                );
                            });
                    }
                    ui.label(
                        RichText::new(
                            "The A75 firmware exposes a 1000 Hz USB report rate, not a true \
                             sub-1 ms matrix scan. Tachyon here is a low-latency performance \
                             preset: maximum polling plus simplified lighting.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Polling rate", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("USB report rate");
                        egui::ComboBox::from_id_salt("report_rate")
                            .selected_text(format!("{} Hz", self.rate))
                            .show_ui(ui, |ui| {
                                for hz in [1000u32, 500, 250, 125] {
                                    if ui
                                        .selectable_label(self.rate == hz, format!("{hz} Hz"))
                                        .clicked()
                                    {
                                        self.rate = hz;
                                        self.send(Cmd::SetRate {
                                            profile: self.profile,
                                            hz,
                                        });
                                    }
                                }
                            });
                    });
                    ui.label(
                        RichText::new("A75 hardware maximum: 1000 Hz.")
                            .size(11.0)
                            .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Deadzone", |ui| {
                    Self::slider_row(
                        ui,
                        "Bottom deadzone",
                        &mut self.deadzone,
                        0.0,
                        4.0,
                        " mm",
                        "Ignore the last part of travel. 0 disables the deadzone.",
                    );
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Apply deadzone to all keys"))
                        .clicked()
                    {
                        self.send(Cmd::Actuation {
                            travel: self.travel,
                            lift: self.lift,
                            rt: self.rt,
                            rt_press: self.rt_press,
                            rt_lift: self.rt_lift,
                            deadzone: self.deadzone,
                        });
                    }
                });

                ui.add_space(6.0);
                card(ui, "Debounce", |ui| {
                    let mut d = self.debounce;
                    if ui
                        .add(egui::Slider::new(&mut d, 0..=20).text("Debounce (ms)"))
                        .changed()
                    {
                        self.debounce = d;
                        self.send(Cmd::SetDebounce(d));
                    }
                });
            });
    }

    fn profiles_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                card(ui, "Save current preset", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut self.preset_name);
                        if ui.button("Save preset").clicked() {
                            self.save_preset();
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Preset stores per-key actuation, rapid trigger, deadzone, polling, \
                             debounce, Tachyon state and SOCD pair configuration.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Saved presets", |ui| {
                    if self.presets.is_empty() {
                        ui.label(
                            RichText::new("No presets saved yet.")
                                .size(12.0)
                                .color(Color32::from_rgb(167, 182, 208)),
                        );
                    } else {
                        for (i, preset) in self.presets.clone().iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&preset.name).strong());
                                ui.label(
                                    RichText::new(format!(
                                        "{} keys · {:.1} mm · {} Hz",
                                        preset.per_key.len(),
                                        preset.travel,
                                        preset.rate
                                    ))
                                    .size(11.0)
                                    .color(Color32::from_rgb(167, 182, 208)),
                                );
                                if ui.button("Load").clicked() {
                                    self.load_preset(i);
                                }
                                if ui.button("Delete").clicked() {
                                    self.presets.remove(i);
                                    let _ = save_presets_file(&self.profiles_path, &self.presets);
                                }
                            });
                        }
                    }
                });

                ui.add_space(6.0);
                card(ui, "Firmware profiles", |ui| {
                    ui.horizontal(|ui| {
                        for p in 0..4u8 {
                            if ui
                                .selectable_label(self.profile == p, format!("Profile {p}"))
                                .clicked()
                            {
                                self.profile = p;
                                self.send(Cmd::SetProfile(p));
                            }
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Firmware profiles are onboard slots. Local presets above are \
                             stored by A75 Control and re-applied from this app.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );
                });
            });
    }

    fn actuation_tab(&mut self, ui: &mut egui::Ui) {
        self.load_selection_edit();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                card(ui, "Selection", |ui| {
                    if self.selected_keys.is_empty() {
                        ui.label(
                            RichText::new(
                                "Select keys on the keyboard map above. Click for one key, \
                                 Ctrl+Click to add/remove, drag on empty space for a marquee, \
                                 or use the quick-select buttons.",
                            )
                            .size(12.0)
                            .color(Color32::from_rgb(167, 182, 208)),
                        );
                    } else {
                        ui.label(
                            RichText::new(format!(
                                "{} selected: {}",
                                self.selected_keys.len(),
                                self.selected_summary()
                            ))
                            .strong(),
                        );
                    }
                });

                ui.add_space(6.0);
                card(ui, "Presets", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (label, t, l, rt, p, rl) in [
                            ("Comfort 2.0 mm", 2.0f32, 2.0f32, false, 0.2f32, 0.3f32),
                            ("Sensitive 0.5 mm", 0.5, 2.0, false, 0.2, 0.3),
                            ("Gaming 0.5 mm + RT", 0.5, 2.0, true, 0.3, 0.3),
                            ("Hair trigger 0.1 mm", 0.1, 2.0, false, 0.2, 0.3),
                        ] {
                            if ui
                                .add_enabled(!self.selected_keys.is_empty() && !self.busy, egui::Button::new(label))
                                .clicked()
                            {
                                self.edit_travel = t;
                                self.edit_lift = l;
                                self.edit_rt = rt;
                                self.edit_rt_press = p;
                                self.edit_rt_lift = rl;
                                self.mixed_travel = false;
                                self.apply_selected_batch();
                            }
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("WASD -> 0.1 mm"))
                            .clicked()
                        {
                            self.select_wasd();
                            self.edit_travel = 0.1;
                            self.edit_lift = 2.0;
                            self.edit_rt = false;
                            self.edit_rt_press = 0.2;
                            self.edit_rt_lift = 0.3;
                            self.mixed_travel = false;
                            self.apply_selected_batch();
                        }
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Caps Lock -> 3.0 mm"))
                            .clicked()
                        {
                            self.selected_keys.clear();
                            self.selected_keys.insert(0x39);
                            self.selected_usage = Some(0x39);
                            self.selection_epoch = self.selection_epoch.wrapping_add(1);
                            self.edit_travel = 3.0;
                            self.mixed_travel = false;
                            self.apply_selected_batch();
                        }
                    });
                    ui.label(
                        RichText::new("Presets apply to the keys currently selected in the map.")
                            .size(11.0)
                            .color(Color32::from_rgb(167, 182, 208)),
                    );
                });

                ui.add_space(6.0);
                card(ui, "Per-key travel", |ui| {
                    let live_depth = self.input.lock().unwrap().analog_depth_mm();
                    travel_diagram(
                        ui,
                        self.edit_travel,
                        self.edit_lift,
                        self.deadzone,
                        live_depth,
                    );
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Actuation point").strong());
                        if self.mixed_travel {
                            ui.label(
                                RichText::new("Mixed")
                                    .strong()
                                    .color(Color32::from_rgb(255, 212, 92)),
                            );
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if !self.mixed_travel {
                                    ui.add(
                                        egui::DragValue::new(&mut self.edit_travel)
                                            .range(0.1..=4.0)
                                            .speed(0.05)
                                            .fixed_decimals(2)
                                            .suffix(" mm"),
                                    );
                                }
                            },
                        );
                    });
                    let slider = egui::Slider::new(&mut self.edit_travel, 0.1..=4.0)
                        .step_by(0.1)
                        .fixed_decimals(1)
                        .show_value(false)
                        .text(if self.mixed_travel { "Mixed" } else { "" });
                    let width = (ui.available_width() - 8.0).max(140.0);
                    ui.spacing_mut().slider_width = width;
                    if ui.add(slider).changed() {
                        self.mixed_travel = false;
                    }
                    ui.label(
                        RichText::new(
                            "Distance the key must travel before it registers. \
                             When selected keys differ, the slider shows Mixed until moved.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(167, 182, 208)),
                    );

                    ui.add_space(6.0);
                    Self::slider_row(
                        ui,
                        "Release point",
                        &mut self.edit_lift,
                        0.1,
                        4.0,
                        " mm",
                        "Distance at which the key releases. Keep it above the actuation point.",
                    );
                });

                ui.add_space(6.0);
                card(ui, "Rapid trigger", |ui| {
                    ui.checkbox(&mut self.edit_rt, "Enable rapid trigger for selected keys");
                    ui.add_enabled_ui(self.edit_rt, |ui| {
                        Self::slider_row(
                            ui,
                            "Press sensitivity",
                            &mut self.edit_rt_press,
                            0.1,
                            2.0,
                            " mm",
                            "Upward movement needed to trigger again.",
                        );
                        Self::slider_row(
                            ui,
                            "Release sensitivity",
                            &mut self.edit_rt_lift,
                            0.1,
                            2.0,
                            " mm",
                            "Downward movement needed to release.",
                        );
                    });
                });

                ui.add_space(6.0);
                card(ui, "Apply", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let btn = egui::Button::new(
                            RichText::new(format!(
                                "Apply to {} selected key(s)",
                                self.selected_keys.len()
                            ))
                            .strong()
                            .color(Color32::from_rgb(22, 31, 46)),
                        )
                        .fill(Color32::from_rgb(255, 212, 92));
                        if ui
                            .add_enabled(
                                !self.busy && !self.selected_keys.is_empty() && !self.mixed_travel,
                                btn,
                            )
                            .clicked()
                        {
                            self.apply_selected_batch();
                        }
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Use global defaults"))
                            .clicked()
                        {
                            self.edit_travel = self.travel;
                            self.edit_lift = self.lift;
                            self.edit_rt = self.rt;
                            self.edit_rt_press = self.rt_press;
                            self.edit_rt_lift = self.rt_lift;
                            self.mixed_travel = false;
                            self.apply_selected_batch();
                        }
                    });
                    if self.mixed_travel {
                        ui.label(
                            RichText::new(
                                "Move the Actuation point slider to resolve Mixed before applying.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(255, 212, 92)),
                        );
                    }
                    ui.label(
                        RichText::new(format!(
                            "encoded raw: press {} · lift {} · RT {}/{}",
                            crate::protocol::travel_to_raw(self.edit_travel),
                            crate::protocol::travel_to_raw(self.edit_lift),
                            crate::protocol::fire_to_raw(self.edit_rt_press),
                            crate::protocol::fire_to_raw(self.edit_rt_lift)
                        ))
                        .monospace()
                        .size(11.0)
                        .color(Color32::from_rgb(139, 147, 167)),
                    );
                });
            });
    }

    fn rgb_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    card(ui, "Effect", |ui| {
                        let before_mode = self.led_mode;
                        egui::ComboBox::from_label("Mode")
                            .selected_text(mode_name(self.led_mode))
                            .show_ui(ui, |ui| {
                                for name in [
                                    "Off",
                                    "Solid",
                                    "Breath",
                                    "Neon",
                                    "Wave",
                                    "Ripple",
                                    "Raindrop",
                                    "Snake",
                                    "Press action",
                                    "Coverage",
                                    "Sine wave",
                                    "Kaleidoscope",
                                    "Line wave",
                                    "User picture",
                                    "Laser",
                                    "Circle wave",
                                    "Dazzling",
                                    "Rain down",
                                    "Meteor",
                                    "Press action off",
                                    "User colour",
                                ] {
                                    let v = mode_from_name(name);
                                    ui.selectable_value(&mut self.led_mode, v, name);
                                }
                            });
                        if self.led_mode != before_mode {
                            self.led_dirty = true;
                        }
                        ui.horizontal(|ui| {
                            ui.label("Colour");
                            let picker = ui.color_edit_button_srgb(&mut self.rgb);
                            if picker.changed() {
                                self.rgb_dirty = true;
                                self.led_dirty = true;
                                self.last_rgb_change = Instant::now();
                                if self.led_mode == 0 {
                                    self.led_mode = 1;
                                    // Coming from Off/black: restore a visible level.
                                    self.led_speed = 4;
                                    self.led_brightness = 4;
                                }
                            }
                            ui.label(
                                RichText::new(format!(
                                    "#{:02x}{:02x}{:02x}",
                                    self.rgb[0], self.rgb[1], self.rgb[2]
                                ))
                                .monospace(),
                            );
                        });
                        ui.horizontal_wrapped(|ui| {
                            for (name, c) in [
                                ("Red", [255, 0, 0]),
                                ("Yellow", [255, 200, 0]),
                                ("Green", [0, 255, 0]),
                                ("Blue", [0, 0, 255]),
                                ("Violet", [128, 0, 255]),
                                ("White", [255, 255, 255]),
                            ] {
                                let text_color = if c[0] as u16 + c[1] as u16 + c[2] as u16 > 420 {
                                    Color32::from_rgb(20, 26, 38)
                                } else {
                                    Color32::WHITE
                                };
                                let btn = egui::Button::new(
                                    RichText::new(name).size(11.0).strong().color(text_color),
                                )
                                .fill(Color32::from_rgb(c[0], c[1], c[2]))
                                .corner_radius(CornerRadius::same(6))
                                .min_size(egui::vec2(62.0, 26.0));
                                if ui.add(btn).clicked() {
                                    self.rgb = c;
                                    self.led_mode = 1;
                                    self.led_speed = 4;
                                    self.led_brightness = 4;
                                    self.led_dirty = true;
                                    self.rgb_dirty = false;
                                    self.led = Some(LedState {
                                        mode: 1,
                                        speed: 4,
                                        brightness: 4,
                                        option: 0,
                                        rgb: c,
                                    });
                                    self.send(Cmd::Led {
                                        mode: 1,
                                        speed: 4,
                                        brightness: 4,
                                        option: 0,
                                        rgb: c,
                                    });
                                }
                            }
                        });
                        if ui
                            .add(
                                egui::Slider::new(&mut self.led_speed, 0..=4)
                                    .text("Speed")
                                    .show_value(true),
                            )
                            .changed()
                        {
                            self.led_dirty = true;
                        }
                        if ui
                            .add(
                                egui::Slider::new(&mut self.led_brightness, 0..=4)
                                    .text("Brightness")
                                    .show_value(true),
                            )
                            .changed()
                        {
                            self.led_dirty = true;
                        }
                        ui.horizontal(|ui| {
                            let btn = egui::Button::new(
                                RichText::new("Apply effect")
                                    .strong()
                                    .color(Color32::from_rgb(22, 31, 46)),
                            )
                            .fill(Color32::from_rgb(255, 212, 92));
                            if ui.add_enabled(!self.busy, btn).clicked() {
                                self.send(Cmd::Led {
                                    mode: self.led_mode,
                                    speed: self.led_speed,
                                    brightness: self.led_brightness,
                                    option: self.led_option,
                                    rgb: self.rgb,
                                });
                                self.rgb_dirty = false;
                                self.led_dirty = self.led_mode != 0;
                                self.led = Some(LedState {
                                    mode: self.led_mode,
                                    speed: self.led_speed,
                                    brightness: self.led_brightness,
                                    option: self.led_option,
                                    rgb: self.rgb,
                                });
                            }
                            if ui.button("Read LED state").clicked() {
                                self.led_dirty = false;
                                self.send(Cmd::Refresh);
                            }
                            if ui.button("Lights off").clicked() {
                                self.led_mode = 0;
                                self.led_dirty = false;
                                self.send(Cmd::Led {
                                    mode: 0,
                                    speed: 4,
                                    brightness: 0,
                                    option: 0,
                                    rgb: [0, 0, 0],
                                });
                            }
                        });
                        if let Some(led) = &self.led {
                            ui.label(
                                RichText::new(format!(
                                    "Keyboard reports: {} · speed {} · brightness {} · #{:02x}{:02x}{:02x}",
                                    mode_name(led.mode),
                                    led.speed,
                                    led.brightness,
                                    led.rgb[0],
                                    led.rgb[1],
                                    led.rgb[2]
                                ))
                                .monospace()
                                .size(11.0)
                                .color(Color32::from_rgb(139, 147, 167)),
                            );
                        }
                    });
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    card(ui, "Per-key colour", |ui| {
                        ui.label(
                            RichText::new(
                                "Global effects are instant. Per-key colours write the \
                                 SET_USERPIC_SIMPLE table and take a few seconds for all \
                                 keys; select User picture / User colour to display them.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(139, 147, 167)),
                        );
                        let selected = self.selected_usage;
                        let mapped = selected.and_then(|u| self.usage_to_index.get(&u).copied());
                        ui.label(format!(
                            "Selected key: {}",
                            selected
                                .map(label_for)
                                .unwrap_or("click a key in Keys & Test")
                        ));
                        if let Some(idx) = mapped {
                            ui.label(
                                RichText::new(format!("matrix index {idx}"))
                                    .monospace()
                                    .size(11.0),
                            );
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!self.busy && mapped.is_some(), egui::Button::new("Paint selected key"))
                                .clicked()
                            {
                                if let Some(idx) = mapped {
                                    self.send(Cmd::PaintIndex {
                                        index: idx,
                                        rgb: self.rgb,
                                    });
                                }
                            }
                            if ui
                                .add_enabled(!self.busy, egui::Button::new("Paint all keys"))
                                .clicked()
                            {
                                self.send(Cmd::PaintAll { rgb: self.rgb });
                            }
                        });
                    });
                    ui.add_space(8.0);
                    card(ui, "Colour test", |ui| {
                        ui.label(
                            RichText::new(
                                "Click a swatch to apply it instantly as a solid effect.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(139, 147, 167)),
                        );
                        ui.horizontal_wrapped(|ui| {
                            for (name, c) in [
                                ("Red", [255, 0, 0]),
                                ("Orange", [255, 90, 0]),
                                ("Yellow", [255, 200, 0]),
                                ("Green", [0, 255, 0]),
                                ("Cyan", [0, 220, 255]),
                                ("Blue", [0, 0, 255]),
                                ("Violet", [128, 0, 255]),
                                ("White", [255, 255, 255]),
                            ] {
                                let btn = egui::Button::new(name).fill(Color32::from_rgb(
                                    c[0], c[1], c[2],
                                ));
                                if ui.add(btn).clicked() {
                                    self.rgb = c;
                                    self.led_mode = 1;
                                    self.led_speed = 4;
                                    self.led_brightness = 4;
                                    self.rgb_dirty = false;
                                    self.led_dirty = false;
                                    self.rgb_synced_marker();
                                    self.send(Cmd::Led {
                                        mode: 1,
                                        speed: 4,
                                        brightness: 4,
                                        option: 0,
                                        rgb: c,
                                    });
                                }
                            }
                        });
                    });
                });
            });
        });
    }

    fn keys_tab(&mut self, ui: &mut egui::Ui) {
        let (counts, last, last_down, count, rollover, analog_on, analog_value, history, events, kbd, vend) = {
            let st = self.input.lock().unwrap();
            (
                st.counts.clone(),
                st.last_key,
                st.last_down,
                st.press_count,
                st.rollover,
                st.analog_on,
                st.analog_value,
                st.analog_history.iter().copied().collect::<Vec<_>>(),
                st.events.iter().cloned().collect::<Vec<_>>(),
                st.keyboard_connected,
                st.vendor_connected,
            )
        };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            card(ui, "Live input", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Last key: {}", last.map(label_for).unwrap_or("—")));
                    ui.separator();
                    ui.label(format!(
                        "Event age: {}",
                        last_down
                            .map(|t| format!("{} ms", t.elapsed().as_millis()))
                            .unwrap_or_else(|| "—".into())
                    ));
                    ui.separator();
                    ui.label(format!("Presses: {count}"));
                    ui.separator();
                    ui.label(format!("Keys hit: {}", counts.len()));
                    ui.separator();
                    ui.label(format!("Rollover: {rollover}"));
                    ui.separator();
                    ui.label(format!(
                        "Keyboard: {}",
                        if kbd { "connected" } else { "not found" }
                    ));
                    ui.separator();
                    ui.checkbox(&mut self.heatmap, "Heatmap");
                    if ui.button("Reset counters").clicked() {
                        let mut st = self.input.lock().unwrap();
                        st.counts.clear();
                        st.press_count = 0;
                        st.events.clear();
                        st.reset_keys();
                    }
                });
            });

            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    card(ui, "Analog test", |ui| {
                        ui.horizontal(|ui| {
                            let text = if analog_on {
                                "Stop analog test"
                            } else {
                                "Start analog test"
                            };
                            if ui.button(text).clicked() {
                                let on = !analog_on;
                                self.send(Cmd::Analog(on));
                                self.input.lock().unwrap().analog_on = on;
                            }
                            ui.label(format!(
                                "value {analog_value}  tag 0x{:02x}  {}",
                                self.input.lock().unwrap().analog_tag,
                                if vend { "vendor IN ok" } else { "vendor IN missing" }
                            ));
                        });
                        ui.label(
                            RichText::new(
                                "While analog reporting is on, the keyboard stops typing. \
                                 Press a Hall-effect key to see the raw telemetry below.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(255, 190, 120)),
                        );
                        ui.label(
                            RichText::new(
                                "Switch to Actuation to watch the live depth marker cross the trigger line.",
                            )
                            .size(11.0)
                            .color(Color32::from_rgb(0, 220, 255)),
                        );
                        analog_graph(ui, &history);
                    });
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    card(ui, "Events", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for e in events.iter().rev() {
                                    ui.label(RichText::new(e).monospace().size(12.0));
                                }
                                if events.is_empty() {
                                    ui.label(
                                        RichText::new("Press a key to populate the event log.")
                                            .size(12.0)
                                            .color(Color32::from_rgb(139, 147, 167)),
                                    );
                                }
                            });
                    });
                });
            });
        });
    }

    fn device_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    card(ui, "Status", |ui| {
                        egui::Grid::new("status_grid")
                            .num_columns(2)
                            .spacing([18.0, 6.0])
                            .show(ui, |ui| {
                                if let Some(st) = &self.status {
                                    for (k, v) in [
                                        ("Firmware", format!("v{}", st.firmware)),
                                        ("MLED firmware", format!("v{}", st.mled)),
                                        ("Device id", format!("{} / 0x{:04x}", st.dev_id, st.dev_id)),
                                        ("Profile", st.profile.to_string()),
                                        ("Mode", st.mode.clone()),
                                        (
                                            "Report rate",
                                            if st.report_rate == 0 {
                                                "unknown".into()
                                            } else {
                                                format!("{} Hz", st.report_rate)
                                            },
                                        ),
                                        ("Debounce", st.debounce.to_string()),
                                        (
                                            "Keyboard disabled",
                                            if st.keyboard_disabled { "yes".into() } else { "no".into() },
                                        ),
                                        ("Info", st.info.clone()),
                                        ("Key matrix", format!("{} records", self.matrix.len() / 4)),
                                        (
                                            "Keyboard test",
                                            if self.input.lock().unwrap().keyboard_connected {
                                                "input node open".into()
                                            } else {
                                                "input node missing".into()
                                            },
                                        ),
                                    ] {
                                        ui.label(RichText::new(k).color(Color32::from_rgb(139, 147, 167)));
                                        ui.label(RichText::new(v).monospace());
                                        ui.end_row();
                                    }
                                } else {
                                    ui.label("No status yet.");
                                    ui.end_row();
                                }
                            });
                    });
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    card(ui, "Settings", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Report rate");
                            egui::ComboBox::from_id_salt("rate")
                                .selected_text(format!("{} Hz", self.rate))
                                .show_ui(ui, |ui| {
                                    for hz in [1000, 500, 250, 125] {
                                        ui.selectable_value(&mut self.rate, hz, format!("{hz} Hz"));
                                    }
                                });
                            if ui.button("Set").clicked() {
                                self.send(Cmd::SetRate {
                                    profile: self.profile,
                                    hz: self.rate,
                                });
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Debounce");
                            ui.add(egui::DragValue::new(&mut self.debounce).range(0..=255));
                            if ui.button("Set").clicked() {
                                self.send(Cmd::SetDebounce(self.debounce));
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Profile");
                            ui.add(egui::DragValue::new(&mut self.profile).range(0..=3));
                            if ui.button("Set").clicked() {
                                self.send(Cmd::SetProfile(self.profile));
                            }
                        });
                        if self.matrix_shadow.is_empty()
                            && ui.button("Read key matrix").clicked()
                        {
                            self.send(Cmd::ReadMatrix);
                        }
                    });
                });
            });

            ui.add_space(6.0);
            card(ui, "Calibration", |ui| {
                ui.label(
                    RichText::new(
                        "Calibration changes the travel reference. Do not touch the keyboard \
                         while it runs. Only use this if keys no longer reach 100%.",
                    )
                    .size(12.0)
                    .color(Color32::from_rgb(255, 190, 120)),
                );
                ui.checkbox(&mut self.cal_confirm, "I understand the risk");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.cal_confirm, egui::Button::new("Travel calibration"))
                        .clicked()
                    {
                        self.send(Cmd::Calibrate(false));
                    }
                    if ui
                        .add_enabled(self.cal_confirm, egui::Button::new("Maximum calibration"))
                        .clicked()
                    {
                        self.send(Cmd::Calibrate(true));
                    }
                });
            });

            ui.add_space(6.0);
            card(ui, "Log", |ui| {
                for line in self.log.iter().rev().take(12) {
                    ui.label(RichText::new(line).monospace().size(11.0));
                }
            });
        });
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Drop the SOCD grab first: the interceptor's Drop joins the evdev
        // thread, emits key-ups for anything logically held, and releases
        // EVIOCGRAB.  Then leave analog-report mode as before.
        self.enable_socd = false;
        self.socd_engine.take();
        let _ = self.cmd_tx.send(Cmd::Analog(false));
        std::thread::sleep(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();
        self.refresh_socd_state();

        if let Some((done, total)) = self.progress {
            if done >= total && total > 0 {
                self.progress = None;
            }
        }

        if self.rgb_dirty
            && !self.busy
            && self.last_rgb_change.elapsed() > Duration::from_millis(450)
        {
            self.send(Cmd::Led {
                mode: if self.led_mode == 0 { 1 } else { self.led_mode },
                speed: self.led_speed,
                brightness: self.led_brightness,
                option: self.led_option,
                rgb: self.rgb,
            });
            self.rgb_dirty = false;
            self.led_dirty = true;
            self.led = Some(LedState {
                mode: if self.led_mode == 0 { 1 } else { self.led_mode },
                speed: self.led_speed,
                brightness: self.led_brightness,
                option: self.led_option,
                rgb: self.rgb,
            });
            self.note(format!(
                "Colour applied: #{:02x}{:02x}{:02x}",
                self.rgb[0], self.rgb[1], self.rgb[2]
            ));
        }

        self.sidebar(ui);

        egui::Panel::bottom("bottom")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(13, 19, 29))
                    .inner_margin(egui::Margin::symmetric(18, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if let Some(e) = &self.error {
                            format!("⚠ {e}")
                        } else {
                            "Ready".to_string()
                        })
                        .size(11.0)
                        .color(if self.error.is_some() {
                            Color32::from_rgb(255, 120, 110)
                        } else {
                            Color32::from_rgb(139, 147, 167)
                        }),
                    );
                    if self.error.is_some() && ui.small_button("dismiss").clicked() {
                        self.error = None;
                    }
                    if let Some((done, total)) = self.progress {
                        ui.add(
                            egui::ProgressBar::new(if total == 0 {
                                0.0
                            } else {
                                done as f32 / total as f32
                            })
                            .desired_width(180.0),
                        );
                        ui.label(format!("{done}/{total}"));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("HID feature reports · 64 bytes")
                                .size(10.0)
                                .color(Color32::from_rgb(110, 118, 135)),
                        );
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(11, 16, 24))
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ui, |ui| {
                self.header(ui);
                self.keyboard_panel(ui);
                match self.tab {
                    Tab::Actuation => self.actuation_tab(ui),
                    Tab::Advanced => self.advanced_tab(ui),
                    Tab::Performance => self.performance_tab(ui),
                    Tab::Profiles => self.profiles_tab(ui),
                    Tab::Rgb => self.rgb_tab(ui),
                    Tab::Remap => self.remap_tab(ui),
                    Tab::Keys => self.keys_tab(ui),
                    Tab::Device => self.device_tab(ui),
                }
            });

        let analog_live = self.input.lock().unwrap().analog_on;
        let interval = if self.tab == Tab::Keys || self.progress.is_some() || analog_live {
            16
        } else {
            80
        };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }
}

fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(Color32::from_rgb(22, 31, 46))
        .stroke(Stroke::new(1.0, Color32::from_rgb(38, 54, 83)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title.to_uppercase())
                    .size(11.0)
                    .strong()
                    .color(Color32::from_rgb(167, 182, 208)),
            );
            ui.add_space(4.0);
            body(ui);
        });
}

fn travel_diagram(
    ui: &mut egui::Ui,
    travel: f32,
    lift: f32,
    deadzone: f32,
    live_depth: Option<f32>,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(430.0), 170.0),
        Sense::hover(),
    );
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(8), Color32::from_rgb(11, 16, 24));
    let top = rect.top() + 18.0;
    let bottom = rect.bottom() - 18.0;
    let h = bottom - top;
    let x = rect.left() + 70.0;
    p.line_segment(
        [egui::pos2(x, top), egui::pos2(x, bottom)],
        Stroke::new(4.0, Color32::from_rgb(38, 54, 83)),
    );
    for i in 0..=6 {
        let y = top + h * (i as f32 / 6.0);
        let mm = 4.0 * (1.0 - i as f32 / 6.0);
        p.line_segment(
            [egui::pos2(x - 8.0, y), egui::pos2(x + 8.0, y)],
            Stroke::new(1.0, Color32::from_rgb(61, 77, 107)),
        );
        p.text(
            egui::pos2(x - 14.0, y),
            Align2::RIGHT_CENTER,
            format!("{mm:.1}"),
            FontId::proportional(10.0),
            Color32::from_rgb(139, 147, 167),
        );
    }
    let y_for = |mm: f32| top + (mm.clamp(0.0, 4.0) / 4.0) * h;
    let y_press = y_for(travel);
    let y_lift = y_for(lift);
    let y_dead = y_for(deadzone);
    p.line_segment(
        [egui::pos2(x - 24.0, y_press), egui::pos2(rect.right() - 12.0, y_press)],
        Stroke::new(2.0, Color32::from_rgb(91, 81, 255)),
    );
    p.text(
        egui::pos2(rect.right() - 14.0, y_press - 10.0),
        Align2::RIGHT_CENTER,
        format!("press {travel:.2} mm"),
        FontId::proportional(11.0),
        Color32::from_rgb(139, 124, 255),
    );
    p.line_segment(
        [egui::pos2(x - 24.0, y_lift), egui::pos2(rect.right() - 12.0, y_lift)],
        Stroke::new(2.0, Color32::from_rgb(255, 212, 92)),
    );
    p.text(
        egui::pos2(rect.right() - 14.0, y_lift + 12.0),
        Align2::RIGHT_CENTER,
        format!("release {lift:.2} mm"),
        FontId::proportional(11.0),
        Color32::from_rgb(255, 212, 92),
    );
    if deadzone > 0.0 {
        p.line_segment(
            [egui::pos2(x - 24.0, y_dead), egui::pos2(rect.right() - 12.0, y_dead)],
            Stroke::new(1.0, Color32::from_rgb(150, 156, 170)),
        );
        p.text(
            egui::pos2(x + 18.0, y_dead - 12.0),
            Align2::LEFT_CENTER,
            format!("deadzone {deadzone:.2} mm"),
            FontId::proportional(10.0),
            Color32::from_rgb(150, 156, 170),
        );
    }
    if let Some(mm) = live_depth {
        let y_live = y_for(mm);
        let live_color = Color32::from_rgb(0, 220, 255);
        p.line_segment(
            [egui::pos2(x - 24.0, y_live), egui::pos2(rect.right() - 12.0, y_live)],
            Stroke::new(2.0, live_color),
        );
        p.rect_filled(
            Rect::from_min_max(
                egui::pos2(rect.right() - 9.0, y_live - 3.0),
                egui::pos2(rect.right() - 3.0, y_live + 3.0),
            ),
            CornerRadius::same(2),
            live_color,
        );
        p.text(
            egui::pos2(rect.right() - 14.0, y_live - 10.0),
            Align2::RIGHT_CENTER,
            format!("live {mm:.2} mm"),
            FontId::proportional(10.0),
            live_color,
        );
    }
    p.text(
        egui::pos2(x + 18.0, bottom + 10.0),
        Align2::LEFT_CENTER,
        "bottom",
        FontId::proportional(10.0),
        Color32::from_rgb(139, 147, 167),
    );
    p.text(
        egui::pos2(x + 18.0, top - 10.0),
        Align2::LEFT_CENTER,
        "rest",
        FontId::proportional(10.0),
        Color32::from_rgb(139, 147, 167),
    );
}

fn keyboard_canvas(
    ui: &mut egui::Ui,
    pressed: &std::collections::HashSet<u16>,
    counts: &std::collections::HashMap<u16, u32>,
    down: &std::collections::HashMap<u16, Instant>,
    up: &std::collections::HashMap<u16, Instant>,
    heatmap: bool,
    selected_keys: &mut HashSet<u16>,
    primary: &mut Option<u16>,
    marquee: &mut Option<(Pos2, Pos2)>,
    epoch: &mut u64,
    per_key: &HashMap<u16, KeyTune>,
    global_travel: f32,
    socd: &[SocdPair],
    show_depth: bool,
) -> Option<u16> {
    const SOCD_COLORS: [Color32; 8] = [
        Color32::from_rgb(0, 210, 255),
        Color32::from_rgb(255, 150, 60),
        Color32::from_rgb(80, 230, 140),
        Color32::from_rgb(255, 110, 180),
        Color32::from_rgb(180, 130, 255),
        Color32::from_rgb(255, 220, 80),
        Color32::from_rgb(120, 200, 255),
        Color32::from_rgb(255, 120, 120),
    ];

    let now = Instant::now();
    let keys = layout();
    let max_x = keys.iter().map(|k| k.x + k.w).fold(0.0f32, f32::max);
    let max_y = keys.iter().map(|k| k.y + 1.0).fold(0.0f32, f32::max);
    let avail = ui.available_width().min(1120.0);
    let unit = (avail / max_x).min(44.0);
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(unit * max_x, unit * max_y + 6.0),
        Sense::click_and_drag(),
    );
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(11, 16, 24));

    let mut rects: Vec<(&'static str, u16, Rect)> = Vec::with_capacity(keys.len());
    for key in &keys {
        let r = Rect::from_min_size(
            egui::pos2(
                rect.left() + key.x * unit + 2.0,
                rect.top() + key.y * unit + 2.0,
            ),
            Vec2::new(key.w * unit - 4.0, unit - 4.0),
        );
        rects.push((key.label, key.usage, r));
    }
    let key_at = |pos: Pos2| -> Option<u16> {
        rects
            .iter()
            .find(|(_, _, r)| r.contains(pos))
            .map(|(_, usage, _)| *usage)
    };

    let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
    let mut clicked = None;

    // Raw pointer handling makes marquee selection work even when egui does
    // not classify the gesture as a drag yet.
    let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let primary_released = ui.input(|i| i.pointer.primary_released());
    if primary_pressed && resp.hovered() {
        let pos = ui
            .input(|i| i.pointer.interact_pos())
            .or_else(|| resp.interact_pointer_pos());
        if let Some(pos) = pos {
            if key_at(pos).is_none() {
                *marquee = Some((pos, pos));
            }
        }
    }
    if primary_down {
        if let Some((start, _)) = *marquee {
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                *marquee = Some((start, pos));
            }
        }
    }
    if primary_released {
        if let Some((a, b)) = marquee.take() {
            let sel = Rect::from_two_pos(a, b);
            if sel.width() > 4.0 || sel.height() > 4.0 {
                let hits: Vec<u16> = rects
                    .iter()
                    .filter(|(_, _, r)| r.intersects(sel))
                    .map(|(_, usage, _)| *usage)
                    .collect();
                if !ctrl {
                    selected_keys.clear();
                }
                for usage in hits {
                    selected_keys.insert(usage);
                }
                *primary = selected_keys.iter().next().copied();
                *epoch = epoch.wrapping_add(1);
                // A marquee is not a click on a key.
                return None;
            }
        }
    }
    if resp.clicked() {
        let pos = resp
            .interact_pointer_pos()
            .or_else(|| ui.ctx().pointer_interact_pos())
            .or_else(|| ui.input(|i| i.pointer.latest_pos()));
        if let Some(pos) = pos {
            if let Some(usage) = key_at(pos) {
                if ctrl {
                    if !selected_keys.remove(&usage) {
                        selected_keys.insert(usage);
                    }
                } else {
                    selected_keys.clear();
                    selected_keys.insert(usage);
                }
                *primary = Some(usage);
                *epoch = epoch.wrapping_add(1);
                clicked = Some(usage);
            } else if !ctrl {
                selected_keys.clear();
                *primary = None;
                *epoch = epoch.wrapping_add(1);
            }
        }
    }

    for (label, usage, r) in &rects {
        let is_pressed = pressed.contains(usage);
        let is_selected = selected_keys.contains(usage);
        let max_count = counts.values().copied().max().unwrap_or(1).max(1) as f32;
        let fill = if is_selected {
            Color32::from_rgb(91, 81, 255)
        } else if is_pressed {
            Color32::from_rgb(139, 124, 255)
        } else if heatmap {
            let c = *counts.get(usage).unwrap_or(&0) as f32;
            if c <= 0.0 {
                Color32::from_rgb(26, 35, 49)
            } else {
                let t = (c / max_count).sqrt().clamp(0.0, 1.0);
                Color32::from_rgb(
                    (201.0 + (91.0 - 201.0) * t) as u8,
                    (173.0 + (81.0 - 173.0) * t) as u8,
                    (102.0 + (255.0 - 102.0) * t) as u8,
                )
            }
        } else {
            Color32::from_rgb(201, 173, 102)
        };
        p.rect_filled(*r, CornerRadius::same(4), fill);

        let mut level = 0.0f32;
        if let Some(t0) = down.get(usage) {
            level = (now.duration_since(*t0).as_secs_f32() / 0.09).clamp(0.0, 1.0);
        } else if let Some(t0) = up.get(usage) {
            let f = 1.0 - (now.duration_since(*t0).as_secs_f32() / 0.22).clamp(0.0, 1.0);
            level = f;
        }
        if level > 0.01 {
            let fill_rect = Rect::from_min_max(
                egui::pos2(r.left(), r.bottom() - r.height() * level),
                r.max,
            );
            let alpha = (210.0 * level) as u8;
            p.rect_filled(
                fill_rect,
                CornerRadius::same(4),
                Color32::from_rgba_unmultiplied(139, 124, 255, alpha),
            );
        }

        let stroke = if is_selected {
            Stroke::new(2.5, Color32::from_rgb(255, 212, 92))
        } else {
            Stroke::new(1.0, Color32::from_rgb(61, 77, 107))
        };
        p.rect_stroke(*r, CornerRadius::same(4), stroke, StrokeKind::Inside);

        let text_color = if is_selected || is_pressed {
            Color32::WHITE
        } else {
            Color32::from_rgb(26, 26, 26)
        };
        let label_size = (unit * 0.24).clamp(8.0, 12.0);
        let depth = per_key.get(usage).map(|t| t.travel).unwrap_or(global_travel);
        let label_y = if show_depth { r.center().y - 4.0 } else { r.center().y };
        p.text(
            egui::pos2(r.center().x, label_y),
            Align2::CENTER_CENTER,
            *label,
            FontId::proportional(label_size),
            text_color,
        );
        if show_depth && !heatmap {
            let depth_color = if is_selected || is_pressed {
                Color32::from_rgb(240, 240, 255)
            } else {
                Color32::from_rgb(55, 48, 30)
            };
            p.text(
                egui::pos2(r.center().x, r.bottom() - 3.0),
                Align2::CENTER_BOTTOM,
                format!("{depth:.1}mm"),
                FontId::proportional((unit * 0.18).clamp(7.0, 9.5)),
                depth_color,
            );
        }
        if let Some(idx) = socd.iter().position(|pair| pair.a == *usage || pair.b == *usage) {
            let badge = Rect::from_min_size(
                egui::pos2(r.right() - 18.0, r.top() + 3.0),
                Vec2::new(15.0, 11.0),
            );
            p.rect_filled(badge, CornerRadius::same(3), SOCD_COLORS[idx % SOCD_COLORS.len()]);
            p.text(
                badge.center(),
                Align2::CENTER_CENTER,
                format!("S{}", idx + 1),
                FontId::proportional(7.5),
                Color32::from_rgb(10, 14, 20),
            );
        }
    }

    if let Some((a, b)) = *marquee {
        let sel = Rect::from_two_pos(a, b);
        p.rect_filled(
            sel,
            CornerRadius::same(2),
            Color32::from_rgba_unmultiplied(91, 81, 255, 40),
        );
        p.rect_stroke(
            sel,
            CornerRadius::same(2),
            Stroke::new(1.5, Color32::from_rgb(255, 212, 92)),
            StrokeKind::Inside,
        );
    }

    clicked
}

fn analog_graph(ui: &mut egui::Ui, history: &[u16]) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(900.0), 170.0),
        Sense::hover(),
    );
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(8), Color32::from_rgb(11, 16, 24));
    p.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, Color32::from_rgb(38, 54, 83)),
        StrokeKind::Inside,
    );
    if history.len() < 2 {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Enable Analog Test to stream raw Hall values",
            FontId::proportional(12.0),
            Color32::from_rgb(139, 147, 167),
        );
        return;
    }
    let min = history.iter().copied().min().unwrap_or(0) as f32;
    let max = history.iter().copied().max().unwrap_or(1) as f32;
    let span = (max - min).max(1.0);
    let n = history.len();
    let points: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * (i as f32 / (n - 1) as f32);
            let y = rect.bottom() - 12.0
                - (rect.height() - 24.0) * ((*v as f32 - min) / span);
            egui::pos2(x, y)
        })
        .collect();
    p.add(egui::Shape::line(
        points,
        Stroke::new(1.6, Color32::from_rgb(255, 212, 92)),
    ));
    p.text(
        egui::pos2(rect.left() + 8.0, rect.top() + 10.0),
        Align2::LEFT_TOP,
        format!("raw {min:.0} … {max:.0}"),
        FontId::monospace(10.0),
        Color32::from_rgb(139, 147, 167),
    );
}

#[cfg(test)]
mod preset_tests {
    use super::*;

    #[test]
    fn preset_file_round_trip() {
        let path = std::env::temp_dir().join("a75ctl-preset-roundtrip.txt");
        let mut per_key = HashMap::new();
        per_key.insert(
            0x1Au16,
            KeyTune {
                travel: 0.2,
                lift: 2.0,
                rt: true,
                rt_press: 0.2,
                rt_lift: 0.3,
                deadzone: 3.7,
            },
        );
        let presets = vec![Preset {
            name: "Round trip".into(),
            per_key,
            travel: 1.5,
            lift: 2.0,
            deadzone: 3.6,
            rt: true,
            rt_press: 0.2,
            rt_lift: 0.3,
            rate: 1000,
            debounce: 5,
            tachyon: true,
            socd: vec![SocdPair {
                a: 0x04,
                b: 0x07,
                mode: SocdMode::Neutral,
            }],
        }];
        save_presets_file(&path, &presets).unwrap();
        let loaded = load_presets_file(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Round trip");
        assert_eq!(loaded[0].socd.len(), 1);
        assert_eq!(loaded[0].socd[0].mode, SocdMode::Neutral);
        let w = loaded[0].per_key.get(&0x1A).unwrap();
        assert!((w.travel - 0.2).abs() < 0.001);
    }
}
