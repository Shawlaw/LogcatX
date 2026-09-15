//! Standalone ADB process fixture used by Rust integration and native UI E2E.
//! Each test gets its own directory; no global environment or real ADB state.
use std::{fs, io::{self, Write}, path::Path, thread, time::Duration};

fn value<'a>(config: &'a str, key: &str, default: &'a str) -> &'a str {
    config.lines().find_map(|line| line.strip_prefix(&format!("{key}="))).unwrap_or(default)
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let root = exe.parent().unwrap();
    let config = fs::read_to_string(root.join("fixture.txt")).unwrap_or_default();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let connect = value(&config, "connect", "127.0.0.1:37002");
    let pairing = value(&config, "pair", "127.0.0.1:37001");
    let mode = value(&config, "mode", "normal");
    if mode == "hang" { thread::sleep(Duration::from_secs(60)); }
    let mut record = fs::OpenOptions::new().append(true).create(true).open(root.join("calls.log")).unwrap();
    if args.first().map(String::as_str) != Some("pair") { let _ = writeln!(record, "{}", args.join(" ")); }
    match args.first().map(String::as_str).unwrap_or("") {
        "version" => println!("Android Debug Bridge version 1.0.41\nVersion 37.0.1-fixture"),
        "mdns" => {
            if mode == "no-mdns" { eprintln!("mdns is unavailable"); std::process::exit(1); }
            println!("List of discovered mdns services");
            println!("adb-fixture-pair\t_adb-tls-pairing._tcp\t{pairing}");
            let address = if root.join("paired").exists() { connect } else { value(&config, "before_pair", connect) };
            println!("adb-fixture-connect\t_adb-tls-connect._tcp\t{address}");
            if mode == "multiple" { println!("adb-fixture-other\t_adb-tls-connect._tcp\t127.0.0.1:37003"); }
        }
        "pair" => {
            let mut code = String::new();
            io::stdin().read_line(&mut code).unwrap();
            let valid = code.trim() == "012345" && args.get(1).map(String::as_str) == Some(pairing);
            let _ = writeln!(record, "pair stdin_valid={valid} args_has_code={}", args.len() > 2);
            if valid {
                fs::write(root.join("paired"), "ok").unwrap();
                println!("Successfully paired to {pairing} [guid=fixture]");
            } else { eprintln!("Failed: pairing code incorrect"); std::process::exit(1); }
        }
        "connect" => {
            let target = args.get(1).map(String::as_str).unwrap_or("");
            if (target == connect || target == "127.0.0.1:5555") && mode != "connect-fail" {
                fs::write(root.join("connected"), target).unwrap();
                println!("connected to {target}");
            } else { eprintln!("failed to connect to {target}"); std::process::exit(1); }
        }
        "disconnect" => { let _ = fs::remove_file(root.join("connected")); println!("disconnected"); }
        "devices" => {
            println!("List of devices attached");
            if let Ok(target) = fs::read_to_string(root.join("connected")) { println!("{target}\tdevice product:fixture model:Wireless_Device"); }
            if value(&config, "display_devices", "false") == "true" {
                println!("FIXTURE_USB_A\tdevice product:fixture model:Pixel_Test");
                println!("FIXTURE_USB_B\tdevice product:fixture model:Android_Test");
            }
        }
        "-s" => shell(root, &args),
        _ => {},
    }
}

fn shell(_root: &Path, args: &[String]) {
    let command = args.join(" ");
    if command.contains("ro.serialno") { println!("{}", args[1]); }
    else if command.contains("ro.product.manufacturer") { println!("Google"); }
    else if command.contains("ro.product.model") { println!("{}", if args[1].ends_with('B') { "Android Test" } else { "Pixel Test" }); }
    else if command.contains("ro.build.version.release") { println!("15"); }
    else if command.contains("getprop") { println!("[ro.product.manufacturer]: [Google]\n[ro.product.model]: [Pixel Test]\n[ro.build.version.release]: [15]"); }
}
