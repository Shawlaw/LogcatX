use super::{AdbCollectorApp, RecordingCleanupAge, detail_label};
use crate::{fs_utils, models::AppEvent};
use eframe::egui;
use std::{
    path::PathBuf,
    thread,
    time::{Duration, SystemTime},
};

impl RecordingCleanupAge {
    pub(super) fn cutoff(self) -> Option<SystemTime> {
        let days = match self {
            Self::All => return None,
            Self::OlderThan7Days => 7,
            Self::OlderThan30Days => 30,
            Self::OlderThan90Days => 90,
        };
        Some(
            SystemTime::now()
                .checked_sub(Duration::from_secs(days * 24 * 60 * 60))
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    }

    pub(super) fn label_key(self) -> &'static str {
        match self {
            Self::All => "recordings_cleanup.age.all",
            Self::OlderThan7Days => "recordings_cleanup.age.7_days",
            Self::OlderThan30Days => "recordings_cleanup.age.30_days",
            Self::OlderThan90Days => "recordings_cleanup.age.90_days",
        }
    }
}

impl AdbCollectorApp {
    pub(super) fn refresh_recording_size(&mut self) {
        self.recording_storage_loading = true;
        let tx = self.tx.clone();
        let root = self.record_directory();
        thread::spawn(move || {
            let result = fs_utils::scan_recordings(&root);
            let _ = tx.send(AppEvent::RecordingStorageRefreshed(result));
        });
    }

    pub(super) fn ui_recording_storage_section(&mut self, ui: &mut egui::Ui) {
        let root = self.record_directory();
        ui.label(egui::RichText::new(self.tr("log_files.recordings")).strong());
        ui.add_space(6.0);
        egui::Grid::new("log-files-recordings")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                detail_label(ui, &self.tr("log_files.recordings_path"));
                ui.label(fs_utils::display_path(&root));
                ui.end_row();

                detail_label(ui, &self.tr("log_files.recordings_size"));
                ui.label(self.tr_args(
                    "log_files.files_and_size",
                    &[
                        ("files", self.recording_storage.file_count.to_string()),
                        (
                            "size",
                            fs_utils::format_bytes(self.recording_storage.total_bytes),
                        ),
                    ],
                ));
                ui.end_row();

                detail_label(ui, &self.tr("log_files.column.oldest"));
                ui.label(self.format_log_time(self.recording_storage.oldest_modified));
                ui.end_row();

                detail_label(ui, &self.tr("log_files.column.latest"));
                ui.label(self.format_log_time(self.recording_storage.newest_modified));
                ui.end_row();
            });
        if self.recording_storage_loading {
            ui.small(self.tr("log_files.scanning_recordings"));
        } else if self.recording_storage.file_count == 0 {
            ui.small(self.tr("log_files.recordings_empty"));
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.tr("log_files.open_recordings")).clicked() {
                match std::fs::create_dir_all(&root) {
                    Ok(()) => {
                        if let Err(err) = fs_utils::open_path(&root) {
                            self.set_error(err);
                        }
                    }
                    Err(err) => self.set_error(format!(
                        "Failed to create the recordings folder {}: {err}",
                        root.display()
                    )),
                }
            }
            if ui.button(self.tr("log_files.cleanup_recordings")).clicked() {
                self.open_recording_cleanup_dialog();
            }
        });
    }

    fn open_recording_cleanup_dialog(&mut self) {
        self.show_recording_cleanup = true;
        self.recording_cleanup_age = RecordingCleanupAge::All;
        self.request_recording_cleanup_preview();
    }

    fn request_recording_cleanup_preview(&mut self) {
        self.recording_cleanup_preview_generation += 1;
        let generation = self.recording_cleanup_preview_generation;
        self.recording_cleanup_preview = None;
        self.recording_cleanup_preview_loading = true;
        let tx = self.tx.clone();
        let root = self.record_directory();
        let cutoff = self.recording_cleanup_age.cutoff();
        let protected = self.active_recording_paths();
        thread::spawn(move || {
            let result = fs_utils::preview_recording_cleanup(&root, cutoff, &protected);
            let _ = tx.send(AppEvent::RecordingCleanupPreviewed { generation, result });
        });
    }

    pub(super) fn handle_recording_cleanup_previewed(
        &mut self,
        generation: u64,
        result: Result<fs_utils::CleanupPreview, String>,
    ) {
        if generation != self.recording_cleanup_preview_generation {
            return;
        }
        self.recording_cleanup_preview_loading = false;
        match result {
            Ok(preview) => self.recording_cleanup_preview = Some(preview),
            Err(err) => self.set_error(err),
        }
    }

    fn start_recording_cleanup(&mut self) {
        if self.recording_cleanup_in_progress {
            return;
        }
        self.recording_cleanup_in_progress = true;
        let tx = self.tx.clone();
        let root = self.record_directory();
        let cutoff = self.recording_cleanup_age.cutoff();
        let protected = self.active_recording_paths();
        thread::spawn(move || {
            let result = fs_utils::cleanup_recordings(&root, cutoff, &protected);
            let _ = tx.send(AppEvent::RecordingCleanupFinished(result));
        });
    }

    pub(super) fn handle_recording_cleanup_finished(
        &mut self,
        result: Result<fs_utils::CleanupOutcome, String>,
    ) {
        self.recording_cleanup_in_progress = false;
        match result {
            Ok(outcome) => {
                let mut message = self.tr_args(
                    "recordings_cleanup.done",
                    &[
                        ("files", outcome.deleted_files.to_string()),
                        ("size", fs_utils::format_bytes(outcome.freed_bytes)),
                    ],
                );
                if !outcome.failed_paths.is_empty() {
                    message.push(' ');
                    message.push_str(&self.tr_args(
                        "recordings_cleanup.partial",
                        &[("failed", outcome.failed_paths.len().to_string())],
                    ));
                }
                self.show_recording_cleanup = false;
                if outcome.failed_paths.is_empty() {
                    self.set_info(message);
                } else {
                    self.set_error(message);
                }
                self.refresh_recording_size();
            }
            Err(err) => {
                self.set_error(self.tr_args("recordings_cleanup.failed", &[("error", err)]))
            }
        }
    }

    pub(super) fn ui_recording_cleanup_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_recording_cleanup {
            return;
        }

        let root = self.record_directory();
        let mut cancel = false;
        let mut delete = false;
        let mut open = true;
        egui::Window::new(self.tr("recordings_cleanup.title"))
            .collapsible(false)
            .resizable(false)
            .default_size([560.0, 410.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label(self.tr("recordings_cleanup.body"));
                ui.add_space(8.0);
                ui.label(self.tr_args(
                    "recordings_cleanup.path",
                    &[("path", fs_utils::display_path(&root))],
                ));
                ui.add_space(8.0);
                ui.label(self.tr("recordings_cleanup.age"));
                let previous_age = self.recording_cleanup_age;
                let age_options: Vec<(RecordingCleanupAge, String)> = [
                    RecordingCleanupAge::All,
                    RecordingCleanupAge::OlderThan7Days,
                    RecordingCleanupAge::OlderThan30Days,
                    RecordingCleanupAge::OlderThan90Days,
                ]
                .into_iter()
                .map(|age| (age, self.tr(age.label_key())))
                .collect();
                egui::ComboBox::from_id_salt("recording-cleanup-age")
                    .selected_text(self.tr(self.recording_cleanup_age.label_key()))
                    .show_ui(ui, |ui| {
                        for (age, label) in age_options {
                            ui.selectable_value(&mut self.recording_cleanup_age, age, label);
                        }
                    });
                if self.recording_cleanup_age != previous_age {
                    self.request_recording_cleanup_preview();
                }
                ui.add_space(8.0);

                if self.recording_cleanup_preview_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(self.tr("recordings_cleanup.scanning"));
                    });
                } else if let Some(preview) = &self.recording_cleanup_preview {
                    if preview.matching_files == 0 {
                        ui.small(self.tr("recordings_cleanup.none"));
                    } else {
                        ui.label(self.tr_args(
                            "recordings_cleanup.preview",
                            &[
                                ("files", preview.matching_files.to_string()),
                                ("size", fs_utils::format_bytes(preview.matching_bytes)),
                            ],
                        ));
                    }
                    if preview.protected_files > 0 {
                        ui.small(self.tr_args(
                            "recordings_cleanup.preview_protected",
                            &[
                                ("files", preview.protected_files.to_string()),
                                ("size", fs_utils::format_bytes(preview.protected_bytes)),
                            ],
                        ));
                    }
                }
                if self.recording_cleanup_in_progress {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(self.tr("recordings_cleanup.deleting"));
                    });
                }

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_delete = !self.recording_cleanup_in_progress
                        && !self.recording_cleanup_preview_loading
                        && self
                            .recording_cleanup_preview
                            .as_ref()
                            .is_some_and(|preview| preview.matching_files > 0);
                    let label = self.recording_cleanup_preview.as_ref().map_or_else(
                        || self.tr("recordings_cleanup.delete"),
                        |preview| {
                            self.tr_args(
                                "recordings_cleanup.delete_count",
                                &[("files", preview.matching_files.to_string())],
                            )
                        },
                    );
                    if ui
                        .add_enabled(can_delete, egui::Button::new(label))
                        .clicked()
                    {
                        delete = true;
                    }
                    if ui.button(self.tr("recordings_cleanup.cancel")).clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel || !open {
            self.show_recording_cleanup = false;
        } else if delete {
            self.start_recording_cleanup();
        }
    }

    fn active_recording_paths(&self) -> Vec<PathBuf> {
        self.recordings
            .values()
            .map(|task| task.local_path.clone())
            .collect()
    }
}
