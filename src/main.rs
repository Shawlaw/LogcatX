mod adb;
mod app;
mod config;
mod fs_utils;
mod models;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([980.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ADB Logcat Collector",
        options,
        Box::new(|cc| Ok(Box::new(app::AdbCollectorApp::new(cc)))),
    )
}
