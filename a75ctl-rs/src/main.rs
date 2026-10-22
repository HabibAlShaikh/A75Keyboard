mod app;
mod input;
mod protocol;
mod socd;
mod vgn_hid;

use std::os::unix::io::AsRawFd;
use std::time::Duration;

const FACTORY_MATRIX: &[u8; 504] = include_bytes!("../factory_matrix.bin");

fn status() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let st = dev.status()?;
    println!("node        : {}", dev.path);
    println!("firmware    : v{}", st.firmware);
    println!("MLED        : v{}", st.mled);
    println!("device id   : {} / 0x{:04x}", st.dev_id, st.dev_id);
    println!("profile     : {}", st.profile);
    println!("mode        : {}", st.mode);
    println!("report rate : {} Hz", st.report_rate);
    println!("debounce    : {}", st.debounce);
    if let Ok(led) = dev.led_state() {
        println!(
            "LED         : {} speed={} brightness={} #{:02x}{:02x}{:02x}",
            protocol::mode_name(led.mode),
            led.speed,
            led.brightness,
            led.rgb[0],
            led.rgb[1],
            led.rgb[2]
        );
    }
    Ok(())
}

fn selftest() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    println!("opened {}", dev.path);
    println!("{:?}", dev.status()?);

    println!("solid red");
    dev.set_led(1, 4, 4, 0, [255, 0, 0])?;
    std::thread::sleep(Duration::from_millis(1200));
    println!("readback {:?}", dev.led_state()?);

    println!("solid green");
    dev.set_led(1, 4, 4, 0, [0, 255, 0])?;
    std::thread::sleep(Duration::from_millis(1200));
    println!("readback {:?}", dev.led_state()?);

    println!("solid orange (restore)");
    dev.set_led(1, 4, 4, 0, [255, 90, 60])?;
    std::thread::sleep(Duration::from_millis(1200));
    println!("readback {:?}", dev.led_state()?);

    println!("actuation 2.0 / 2.0, RT off");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    println!("done");
    Ok(())
}

fn paint_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    let valid: Vec<u8> = (0..matrix.len() / 4)
        .filter(|i| matrix[i * 4..i * 4 + 4].iter().any(|b| *b != 0))
        .map(|i| i as u8)
        .collect();
    println!("valid keys: {}", valid.len());
    std::thread::sleep(Duration::from_millis(1000));
    for (n, idx) in valid.iter().take(12).enumerate() {
        dev.set_key_color(0, *idx, [255, 90, 60])?;
        if (n + 1) % 4 == 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    println!("painted 12 keys without stalling");
    println!("{:?}", dev.status()?);
    Ok(())
}

fn rgb_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    for delay in [250u64, 700, 1200] {
        println!("--- delay {delay} ms");
        for (name, c) in [("red", [255, 0, 0]), ("green", [0, 255, 0]), ("blue", [0, 0, 255])] {
            dev.set_led(1, 4, 4, 0, c)?;
            std::thread::sleep(Duration::from_millis(delay));
            println!("{name:>5}: {:?}", dev.led_state()?);
        }
    }
    dev.set_led(1, 4, 4, 0, [255, 90, 60])?;
    Ok(())
}

fn perkey_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    let idx = (0..matrix.len() / 4)
        .find(|i| matrix[i * 4 + 2] == 0x04)
        .map(|i| i as u8)
        .ok_or("A key not found")?;
    println!("A matrix index {idx}");
    println!("global 2.0/2.0");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    std::thread::sleep(Duration::from_millis(900));
    println!("per-key A 0.1/2.0");
    dev.set_actuation_key(0, idx, 0.1, 2.0, false, 0.2, 0.3, 3.7, 0)?;
    std::thread::sleep(Duration::from_millis(900));
    println!("restore global 2.0/2.0");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    println!("{:?}", dev.status()?);
    Ok(())
}

fn dump_matrix() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    println!("matrix bytes: {} ({} slots)", matrix.len(), matrix.len() / 4);
    let mut valid = 0;
    for i in 0..matrix.len() / 4 {
        let r = &matrix[i * 4..i * 4 + 4];
        if r.iter().any(|b| *b != 0) {
            valid += 1;
            let usage = r[2] as u16;
            println!(
                "{i:3}: {:02x} {:02x} {:02x} {:02x}  usage=0x{usage:02x} {}",
                r[0], r[1], r[2], r[3], input::label_for(usage)
            );
        }
    }
    println!("valid slots: {valid}");
    Ok(())
}

fn parse_usage(text: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let v = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)?
    } else {
        text.parse::<u16>()?
    };
    if v > 0xFF {
        return Err(format!("HID usage 0x{v:02x} does not fit one byte").into());
    }
    Ok(v as u8)
}

fn remap(from: u8, to: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let active = dev.matrix(0)?;
    let mut out = active[..active.len().min(FACTORY_MATRIX.len())].to_vec();
    let find = |matrix: &[u8], usage: u8| {
        (0..matrix.len() / 4).find(|i| matrix[i * 4 + 2] == usage)
    };
    let Some(idx) = find(FACTORY_MATRIX, from) else {
        return Err(format!("source usage 0x{from:02x} not found in factory matrix").into());
    };
    let target = find(FACTORY_MATRIX, to)
        .map(|i| FACTORY_MATRIX[i * 4..i * 4 + 4].to_vec())
        .unwrap_or_else(|| vec![0, 0, to, 0]);
    println!(
        "source slot {idx}: {:02x} {:02x} {:02x} {:02x} ({})",
        out[idx * 4], out[idx * 4 + 1], out[idx * 4 + 2], out[idx * 4 + 3],
        input::label_for(from as u16)
    );
    println!("target record: {:02x?} ({})", target, input::label_for(to as u16));
    out[idx * 4..idx * 4 + 4].copy_from_slice(&target);
    dev.set_matrix(0, &out)?;
    std::thread::sleep(Duration::from_millis(600));
    let back = dev.matrix(0)?;
    let r = &back[idx * 4..idx * 4 + 4];
    println!(
        "readback slot {idx}: {:02x} {:02x} {:02x} {:02x} (usage 0x{:02x})",
        r[0], r[1], r[2], r[3], r[2]
    );
    println!(
        "remap written (GET_KEYMATRIX returns the stored default, so the \
         readback above may still show the old usage; press the physical key to test)"
    );
    Ok(())
}

fn save_matrix(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    std::fs::write(path, &matrix)?;
    println!("wrote {} bytes to {path}", matrix.len());
    Ok(())
}

fn restore_matrix() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    dev.set_matrix(0, FACTORY_MATRIX)?;
    println!("factory key matrix written back ({} bytes)", FACTORY_MATRIX.len());
    Ok(())
}

fn nodes() -> Result<(), Box<dyn std::error::Error>> {
    let n = protocol::discover();
    println!("feature: {:?}\nkeyboard: {:?}\nvendor_input: {:?}", n.feature, n.keyboard, n.vendor_input);
    Ok(())
}

fn acquire_single_instance() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let path = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/a75ctl.lock"))
        .unwrap_or_else(|_| "/tmp/a75ctl.lock".into());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(format!(
            "Another A75 Control instance is already running ({path}).\n\
             Close it first; multiple controllers make the keyboard's feature reports glitchy."
        )
        .into());
    }
    Ok(file)
}

fn wasd_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    println!("global 2.0/2.0");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    std::thread::sleep(Duration::from_millis(900));
    for (usage, name) in [(0x1Au16, "W"), (0x04, "A"), (0x16, "S"), (0x07, "D")] {
        let idx = (0..matrix.len() / 4)
            .find(|i| matrix[i * 4 + 2] as u16 == usage)
            .map(|i| i as u8)
            .ok_or(format!("{name} not found"))?;
        println!("{name} -> matrix {idx}, 0.1 mm");
        dev.set_actuation_key(0, idx, 0.1, 2.0, false, 0.2, 0.3, 3.7, 0)?;
    }
    std::thread::sleep(Duration::from_millis(900));
    println!("restore global 2.0/2.0");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    println!("{:?}", dev.status()?);
    Ok(())
}

fn toggle_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    let matrix = dev.matrix(0)?;
    let idx = (0..matrix.len() / 4)
        .find(|i| matrix[i * 4 + 2] == 0x45)
        .map(|i| i as u8)
        .ok_or("F12 not found")?;
    println!("F12 -> matrix {idx}, toggle dots");
    dev.set_actuation_key(0, idx, 1.0, 1.0, false, 0.2, 0.3, 0.0, 5)?;
    std::thread::sleep(Duration::from_millis(900));
    println!("restore normal global");
    dev.set_actuation(0, 2.0, 2.0, false, 0.2, 0.3, 3.7)?;
    println!("done");
    Ok(())
}

fn solid_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    for (name, c) in [
        ("GREEN", [0, 255, 0]),
        ("BLUE", [0, 0, 255]),
        ("RED", [255, 0, 0]),
    ] {
        dev.set_led(1, 4, 4, 0, c)?;
        println!("{name} for 4 seconds");
        std::thread::sleep(Duration::from_secs(4));
    }
    dev.set_led(0, 4, 0, 0, [0, 0, 0])?;
    println!("OFF for 4 seconds");
    std::thread::sleep(Duration::from_secs(4));
    dev.set_led(1, 4, 4, 0, [255, 90, 60])?;
    println!("restored orange");
    Ok(())
}

fn set_color(hex: &str) -> Result<(), Box<dyn std::error::Error>> {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16)?;
    let rgb = [((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8];
    let mut dev = protocol::Device::open()?;
    dev.set_led(1, 4, 4, 0, rgb)?;
    println!("solid #{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    Ok(())
}

fn set_off() -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = protocol::Device::open()?;
    dev.set_led(0, 4, 0, 0, [0, 0, 0])?;
    println!("off");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let _instance_lock = match acquire_single_instance() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Some(pos) = args.iter().position(|a| a == "--color") {
        let hex = args.get(pos + 1).ok_or("--color needs RRGGBB")?;
        return set_color(hex);
    }
    if args.iter().any(|a| a == "--off") {
        return set_off();
    }
    if args.iter().any(|a| a == "--dump-matrix") {
        return dump_matrix();
    }
    if args.iter().any(|a| a == "--restore-matrix") {
        return restore_matrix();
    }
    if let Some(pos) = args.iter().position(|a| a == "--save-matrix") {
        let path = args.get(pos + 1).ok_or("--save-matrix needs PATH")?;
        return save_matrix(path);
    }
    if let Some(pos) = args.iter().position(|a| a == "--remap") {
        let from = args.get(pos + 1).ok_or("--remap needs FROM TO")?;
        let to = args.get(pos + 2).ok_or("--remap needs FROM TO")?;
        return remap(parse_usage(from)?, parse_usage(to)?);
    }
    if args.iter().any(|a| a == "--nodes") {
        return nodes();
    }
    if args.iter().any(|a| a == "--status") {
        return status();
    }
    if args.iter().any(|a| a == "--selftest") {
        return selftest();
    }
    if args.iter().any(|a| a == "--paint-test") {
        return paint_test();
    }
    if args.iter().any(|a| a == "--rgb-test") {
        return rgb_test();
    }
    if args.iter().any(|a| a == "--perkey-test") {
        return perkey_test();
    }
    if args.iter().any(|a| a == "--wasd-test") {
        return wasd_test();
    }
    if args.iter().any(|a| a == "--toggle-test") {
        return toggle_test();
    }
    if args.iter().any(|a| a == "--solid-test") {
        return solid_test();
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 860.0])
            .with_min_inner_size([920.0, 680.0])
            .with_title("A75 Control"),
        ..Default::default()
    };

    eframe::run_native(
        "A75 Control",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )?;
    Ok(())
}
