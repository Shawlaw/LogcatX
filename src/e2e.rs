//! Opt-in native renderer screenshots. No test input or simulated app state is
//! injected here; native E2E drives the normal Windows accessibility provider.
//! This module is excluded from ordinary preview / release builds.
use eframe::egui;
use std::{path::PathBuf, sync::OnceLock};

pub fn capture_frame(ctx: &egui::Context) {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    let Some(directory) =
        DIRECTORY.get_or_init(|| std::env::var_os("LOGCATX_E2E_CAPTURE_DIR").map(PathBuf::from))
    else {
        return;
    };
    let request = directory.join("capture.request");
    if let Ok(name) = std::fs::read_to_string(&request) {
        let _ = std::fs::remove_file(&request);
        if !name.is_empty()
            && name.len() <= 80
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                directory.join(format!("{name}.png")),
            )));
        }
    }
    let screenshots = ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| {
                if let egui::Event::Screenshot {
                    user_data, image, ..
                } = event
                {
                    user_data
                        .data
                        .as_ref()?
                        .downcast_ref::<PathBuf>()
                        .map(|path| (path.clone(), image.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    });
    for (path, pixels) in screenshots {
        let bytes: Vec<u8> = pixels
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_array())
            .collect();
        if let Err(error) = image::save_buffer(
            &path,
            &bytes,
            pixels.width() as u32,
            pixels.height() as u32,
            image::ColorType::Rgba8,
        ) {
            log::error!("E2E screenshot failed: {error}");
        }
    }
}
