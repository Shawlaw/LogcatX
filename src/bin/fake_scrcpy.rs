//! Process-level scrcpy double used by integration tests. The release
//! packaging script does not include this binary.

use std::{env, fs, time::Duration};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["--version"] {
        println!("scrcpy 3.3.4 <https://github.com/Genymobile/scrcpy>");
        return;
    }

    if let Some(path) = env::var_os("FAKE_SCRCPY_LOG") {
        let record = serde_json::json!({
            "args": args,
            "adb": env::var("ADB").ok(),
        });
        fs::write(
            path,
            serde_json::to_vec(&record).expect("encode fake scrcpy call"),
        )
        .expect("write fake scrcpy call");
    }
    let has_start_app = args
        .iter()
        .any(|arg| arg == "--start-app" || arg.starts_with("--start-app="));
    let has_no_control = args.iter().any(|arg| arg == "--no-control");
    if has_start_app && has_no_control {
        eprintln!("ERROR: Cannot start an Android app if control is disabled");
        std::process::exit(1);
    }
    if env::var_os("FAKE_SCRCPY_FAIL_STARTUP").is_some() {
        println!("synthetic scrcpy stdout");
        eprintln!("ERROR: synthetic scrcpy startup failure");
        std::process::exit(3);
    }
    // Model a mirror that survives LogcatX's startup observation window.
    std::thread::sleep(Duration::from_millis(600));
}
