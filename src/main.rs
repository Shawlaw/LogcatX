#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

mod adb;
mod app;
mod config;
mod fs_utils;
mod logger;
mod models;

fn main() -> eframe::Result<()> {
    let console_mode = cfg!(feature = "console") || std::env::args().any(|arg| arg == "--console");
    let paths = config::resolve_app_paths().unwrap_or_else(|err| fatal_error(&err));

    logger::init(&paths.app_log_path, console_mode, 2).unwrap_or_else(|err| fatal_error(&err));
    logger::set_panic_hook(&paths.app_log_path);

    let config_exists = paths.config_path.exists();
    let (config, startup_error) = match config::load_config(&paths.config_path) {
        Ok(config) => (config, None),
        Err(err) if config_exists => (config::AppConfig::with_defaults(&paths), Some(err)),
        Err(_) => (config::AppConfig::with_defaults(&paths), None),
    };

    log::info!(
        "========== ADB Logcat Collector v{} startup ==========",
        env!("CARGO_PKG_VERSION")
    );
    log::info!("Portable mode: {}", paths.portable_mode);
    log::info!("Config path: {}", paths.config_path.display());
    log::info!("App log path: {}", paths.app_log_path.display());
    log::info!("Default device log directory: {}", config.log_dir);
    log::info!("ADB path: {}", config.adb_path);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([980.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ADB Logcat Collector",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::AdbCollectorApp::new(
                cc,
                app::AppBootstrap {
                    app_paths: paths.clone(),
                    config,
                    config_exists,
                    startup_error,
                    version: env!("CARGO_PKG_VERSION"),
                },
            )))
        }),
    )
}

fn fatal_error(message: &str) -> ! {
    let _ = rfd::MessageDialog::new()
        .set_title("ADB Logcat Collector")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
    std::process::exit(1);
}
