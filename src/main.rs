#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

mod adb;
mod app;
mod config;
mod fs_utils;
mod i18n;
mod managed_child;
mod models;

const DEFAULT_WINDOW_SIZE: [f32; 2] = [1280.0, 820.0];
const MIN_WINDOW_SIZE: [f32; 2] = [1100.0, 720.0];

fn main() -> eframe::Result<()> {
    let console_mode = cfg!(feature = "console") || std::env::args().any(|arg| arg == "--console");
    let paths = config::resolve_app_paths().unwrap_or_else(|err| fatal_error(&err));

    desktop_logger::init(&paths.app_log_path, console_mode, 2)
        .unwrap_or_else(|err| fatal_error(&err));
    desktop_logger::set_panic_hook(&paths.app_log_path);

    let config_exists = paths.config_path.exists();
    let (config, startup_error) = match config::load_config(&paths.config_path, &paths) {
        Ok(config) => (config, None),
        Err(err) if config_exists => (config::AppConfig::with_defaults(&paths), Some(err)),
        Err(_) => (config::AppConfig::with_defaults(&paths), None),
    };
    let boot_i18n = i18n::I18n::new(&config.language);

    log::info!(
        "========== LogcatX v{} startup ==========",
        env!("CARGO_PKG_VERSION")
    );
    log::info!("Portable mode: {}", paths.portable_mode);
    log::info!(
        "Config path: {}",
        fs_utils::display_path(paths.config_path.as_path())
    );
    log::info!(
        "App log path: {}",
        fs_utils::display_path(paths.app_log_path.as_path())
    );
    log::info!("Default device log directory: {}", config.log_dir);
    log::info!("ADB path: {}", config.adb_path);
    log::info!("Language: {}", config.language);

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icons/icon_256.png"))
        .unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("LogcatX")
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        &format!(
            "{} v{}",
            boot_i18n.tr("app.title"),
            env!("CARGO_PKG_VERSION")
        ),
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
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

fn install_fonts(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();

    // Load CJK font for Chinese characters.
    if let Some((font_name, font_data)) = load_cjk_font() {
        fonts
            .font_data
            .insert(font_name.clone(), FontData::from_owned(font_data).into());
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, font_name.clone());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(font_name.clone());
        log::info!("Loaded CJK UI font: {font_name}");
    } else {
        log::warn!("No CJK-capable system font found; Chinese text may not render correctly");
    }

    // On Windows, load Segoe UI Symbol with highest priority so that Unicode icon
    // characters (nav icons, symbols) render correctly. CJK fonts ship blank glyphs
    // for many symbol code-points, which prevents egui from falling back to NotoSans.
    // Putting a comprehensive symbol font first fixes the empty-box rendering.
    if let Some((sym_name, sym_data)) = load_symbol_font() {
        fonts
            .font_data
            .insert(sym_name.clone(), FontData::from_owned(sym_data).into());
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, sym_name.clone());
        log::info!("Loaded symbol font: {sym_name}");
    }

    ctx.set_fonts(fonts);
}

fn load_symbol_font() -> Option<(String, Vec<u8>)> {
    #[cfg(target_os = "windows")]
    {
        let windows_dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let path = windows_dir.join("Fonts").join("seguisym.ttf");
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(("segoe-ui-symbol".to_owned(), bytes));
        }
    }
    let _ = (); // suppress unused warning on non-Windows
    None
}

fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    for (name, path) in candidate_cjk_fonts() {
        if let Ok(bytes) = std::fs::read(&path) {
            return Some((name, bytes));
        }
    }
    None
}

fn candidate_cjk_fonts() -> Vec<(String, std::path::PathBuf)> {
    let mut candidates = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let windows_dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let font_dir = windows_dir.join("Fonts");
        candidates.extend([
            ("microsoft-yahei".to_owned(), font_dir.join("msyh.ttc")),
            (
                "microsoft-yahei-bold".to_owned(),
                font_dir.join("msyhbd.ttc"),
            ),
            ("simhei".to_owned(), font_dir.join("simhei.ttf")),
            ("simsun".to_owned(), font_dir.join("simsun.ttc")),
        ]);
    }

    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            (
                "pingfang".to_owned(),
                std::path::PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
            ),
            (
                "stheiti".to_owned(),
                std::path::PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
            ),
        ]);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        candidates.extend([
            (
                "noto-sans-cjk".to_owned(),
                std::path::PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            ),
            (
                "noto-sans-cjk-sc".to_owned(),
                std::path::PathBuf::from(
                    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
                ),
            ),
            (
                "wqy-zenhei".to_owned(),
                std::path::PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"),
            ),
        ]);
    }

    candidates
}

fn fatal_error(message: &str) -> ! {
    let _ = rfd::MessageDialog::new()
        .set_title("LogcatX")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
    std::process::exit(1);
}
