use super::AdbCollectorApp;
use crate::{fs_utils, recorder};
use eframe::egui;
use rfd::FileDialog;
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

impl AdbCollectorApp {
    pub(super) fn is_device_recording(&self, device_id: &str) -> bool {
        self.recording_serial(device_id).is_some()
    }

    pub(super) fn is_device_recording_stopping(&self, device_id: &str) -> bool {
        self.find_device(device_id).is_some_and(|device| {
            device
                .transport_serials
                .iter()
                .any(|serial| self.recording_stopping.contains(serial))
        })
    }

    /// Recording may have started on USB while the row has since switched its
    /// primary transport; look at every transport the row owns.
    fn recording_serial(&self, device_id: &str) -> Option<String> {
        self.find_device(device_id).and_then(|device| {
            device
                .transport_serials
                .iter()
                .find(|serial| self.recordings.contains_key(*serial))
                .cloned()
        })
    }

    pub(super) fn open_record_dialog(&mut self, device_id: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        let Some(device) = self.find_device(&device_id).cloned() else {
            return;
        };
        if device.info.state != "device" {
            self.set_error(self.tr_args(
                "status.device_invalid_state",
                &[
                    ("serial", self.device_identity_label(&device_id)),
                    ("state", self.device_state_text(&device.info.state)),
                ],
            ));
            return;
        }
        if self.is_device_recording(&device_id) {
            return;
        }

        self.record_device_id = Some(device_id.clone());
        self.record_mode = recorder::RecordingMode::Quick;
        self.record_error = None;
        let directory = self.record_directory();
        let path = recorder::default_recording_path(
            directory.as_path(),
            &self.device_identity_label(&device_id),
        );
        self.record_path_input = fs_utils::display_path(&path);
        self.show_record_dialog = true;
    }

    pub(super) fn ui_record_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_record_dialog {
            return;
        }
        let Some(device_id) = self.record_device_id.clone() else {
            self.show_record_dialog = false;
            return;
        };
        let Some(device) = self.find_device(&device_id).cloned() else {
            self.show_record_dialog = false;
            return;
        };
        if device.info.state != "device" {
            self.show_record_dialog = false;
            return;
        }

        let long_available = !self.config.scrcpy_path.trim().is_empty();
        let quick_label = self.tr("record.mode.quick");
        let long_label = self.tr("record.mode.long");
        let mut start = false;
        let mut cancel = false;

        egui::Window::new(self.tr("record.title"))
            .collapsible(false)
            .resizable(false)
            .fixed_size([560.0, 430.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label(self.tr_args(
                    "record.target",
                    &[("device", self.device_identity_label(&device_id))],
                ));
                ui.add_space(8.0);

                ui.label(self.tr("record.mode"));
                if ui
                    .radio_value(
                        &mut self.record_mode,
                        recorder::RecordingMode::Quick,
                        quick_label,
                    )
                    .clicked()
                {
                    self.record_error = None;
                }
                if ui
                    .add_enabled(
                        long_available,
                        egui::RadioButton::new(
                            self.record_mode == recorder::RecordingMode::Long,
                            long_label,
                        ),
                    )
                    .clicked()
                {
                    self.record_mode = recorder::RecordingMode::Long;
                    self.record_error = None;
                }
                if !long_available {
                    ui.small(self.tr("record.mode.long_unavailable"));
                }
                ui.add_space(8.0);

                ui.label(self.tr("record.path"));
                ui.horizontal(|ui| {
                    let response = ui.add_sized(
                        [ui.available_width() - 92.0, 0.0],
                        egui::TextEdit::singleline(&mut self.record_path_input),
                    );
                    super::text_menu::text_edit_menu(
                        &response,
                        &mut self.record_path_input,
                        &self.i18n,
                    );
                    if response.changed() {
                        self.record_error = None;
                    }
                    if ui.button(self.tr("record.browse")).clicked()
                        && let Some(path) = self.pick_recording_path()
                    {
                        self.record_path_input = fs_utils::display_path(&path);
                    }
                });
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label(self.tr("record.bitrate"));
                    egui::ComboBox::from_id_salt("record-bitrate")
                        .selected_text(format!("{} Mbps", self.record_bitrate_mbps))
                        .show_ui(ui, |ui| {
                            for bitrate in [4, 8, 12, 16, 24, 32] {
                                ui.selectable_value(
                                    &mut self.record_bitrate_mbps,
                                    bitrate,
                                    format!("{bitrate} Mbps"),
                                );
                            }
                        });
                    if self.record_mode == recorder::RecordingMode::Quick {
                        ui.add_space(18.0);
                        ui.label(self.tr("record.time_limit"));
                        egui::ComboBox::from_id_salt("record-time-limit")
                            .selected_text(format!("{} s", self.record_time_limit_secs))
                            .show_ui(ui, |ui| {
                                for seconds in [30, 60, 120, 180] {
                                    ui.selectable_value(
                                        &mut self.record_time_limit_secs,
                                        seconds,
                                        format!("{seconds} s"),
                                    );
                                }
                            });
                    }
                });
                ui.add_space(6.0);
                ui.small(match self.record_mode {
                    recorder::RecordingMode::Quick => self.tr("record.hint.quick"),
                    recorder::RecordingMode::Long => self.tr("record.hint.long"),
                });
                if let Some(error) = &self.record_error {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(error).color(egui::Color32::from_rgb(192, 57, 43)),
                    );
                }

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_start = !self.record_path_input.trim().is_empty()
                        && (self.record_mode == recorder::RecordingMode::Quick || long_available);
                    if ui
                        .add_enabled(can_start, egui::Button::new(self.tr("record.start")))
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button(self.tr("record.cancel")).clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel {
            self.show_record_dialog = false;
            self.record_device_id = None;
            self.record_error = None;
        } else if start {
            match self.start_recording(&device_id) {
                Ok(()) => {
                    self.show_record_dialog = false;
                    self.record_device_id = None;
                    self.record_error = None;
                }
                Err(err) => self.record_error = Some(err),
            }
        }
    }

    fn start_recording(&mut self, device_id: &str) -> Result<(), String> {
        let Some(serial) = self.device_primary_transport_serial(device_id) else {
            return Err("The selected device has no usable ADB transport.".to_owned());
        };
        if self.recordings.contains_key(&serial) {
            return Err("This device is already recording.".to_owned());
        }
        let mut path = PathBuf::from(self.record_path_input.trim());
        if path.as_os_str().is_empty() {
            return Err("Choose a file to save the recording to.".to_owned());
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("mp4"))
        {
            path.set_extension("mp4");
        }
        if self.record_mode == recorder::RecordingMode::Long
            && self.config.scrcpy_path.trim().is_empty()
        {
            return Err("Configure scrcpy in Settings to use long recording.".to_owned());
        }

        let task = match self.record_mode {
            recorder::RecordingMode::Quick => recorder::start_quick(
                &self.config.adb_path,
                &serial,
                path,
                self.record_bitrate_mbps,
                self.record_time_limit_secs,
            )?,
            recorder::RecordingMode::Long => recorder::start_long(
                &self.config.scrcpy_path,
                &self.config.adb_path,
                &serial,
                path,
                self.record_bitrate_mbps,
            )?,
        };
        self.record_path_input = fs_utils::display_path(&task.local_path);
        self.recordings.insert(serial.clone(), task);
        self.recording_stopping.remove(&serial);
        self.set_info(self.tr_args(
            "record.status.starting",
            &[("device", self.transport_identity_label(&serial))],
        ));
        Ok(())
    }

    pub(super) fn stop_recording(&mut self, device_id: &str) {
        let Some(serial) = self.recording_serial(device_id) else {
            return;
        };
        let Some(task) = self.recordings.get(&serial) else {
            return;
        };
        task.request_stop();
        self.recording_stopping.insert(serial.clone());
        self.set_info(self.tr_args(
            "record.status.stopping",
            &[("device", self.transport_identity_label(&serial))],
        ));
    }

    pub(super) fn poll_recording_events(&mut self) {
        let mut events = Vec::new();
        for serial in self.recordings.keys().cloned().collect::<Vec<_>>() {
            if let Some(task) = self.recordings.get(&serial) {
                while let Some(event) = task.try_recv() {
                    events.push((serial.clone(), event));
                }
            }
        }

        for (serial, event) in events {
            match event {
                recorder::RecordingEvent::Started => {
                    self.set_info(self.tr_args(
                        "record.status.started",
                        &[("device", self.transport_identity_label(&serial))],
                    ));
                }
                recorder::RecordingEvent::Finished(result) => {
                    self.recordings.remove(&serial);
                    self.recording_stopping.remove(&serial);
                    let device = self.transport_identity_label(&serial);
                    match result {
                        Ok(outcome) => {
                            let mut message = self.tr_args(
                                "record.status.saved",
                                &[
                                    ("device", device),
                                    ("path", fs_utils::display_path(&outcome.local_path)),
                                    ("size", fs_utils::format_bytes(outcome.bytes)),
                                ],
                            );
                            if outcome.forced_stop {
                                message.push(' ');
                                message.push_str(&self.tr("record.warning.forced"));
                            }
                            self.set_info(message);
                        }
                        Err(err) => {
                            self.set_error(self.tr_args(
                                "record.status.failed",
                                &[("device", device), ("error", err)],
                            ));
                        }
                    }
                }
            }
        }
    }

    pub(super) fn stop_all_recordings_for_shutdown(&mut self) {
        if self.recordings.is_empty() {
            return;
        }
        let tasks: Vec<(String, recorder::RecordingTask)> = self.recordings.drain().collect();
        for (_, task) in &tasks {
            task.request_stop();
        }
        // Give every worker a shared 10 s budget to finalize and pull. Joining
        // unconditionally could block the exit for minutes on a stalled
        // transfer; dropping the handle after the budget lets the process exit
        // (the device-side temporary file is intentionally left behind then).
        let deadline = Instant::now() + Duration::from_secs(10);
        for (serial, task) in tasks {
            let mut finished = false;
            while Instant::now() < deadline {
                if matches!(task.try_recv(), Some(recorder::RecordingEvent::Finished(_))) {
                    finished = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !finished {
                log::warn!("Recording on {serial} did not finish before app shutdown.");
            }
        }
        self.recording_stopping.clear();
    }

    fn record_directory(&self) -> PathBuf {
        let configured = self.config.record_dir.trim();
        if configured.is_empty() {
            self.app_paths.exe_dir.join("recordings")
        } else {
            PathBuf::from(configured)
        }
    }

    fn pick_recording_path(&self) -> Option<PathBuf> {
        let current = PathBuf::from(self.record_path_input.trim());
        let directory = current
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.record_directory());
        let file_name = current
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "recording.mp4".to_owned());
        FileDialog::new()
            .set_directory(directory)
            .set_file_name(file_name)
            .add_filter("MP4 video", &["mp4"])
            .save_file()
    }
}
