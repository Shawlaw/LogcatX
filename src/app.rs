use crate::{
    adb,
    config::{self, AppConfig},
    fs_utils,
    models::{AppEvent, DeviceEntry, DeviceInfo, DeviceRunState, SharedChild, StatusMessage},
};
use eframe::egui::{self, Align, Color32, RichText};
use rfd::FileDialog;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

pub struct AdbCollectorApp {
    config: AppConfig,
    devices: Vec<DeviceEntry>,
    total_log_bytes: u64,
    status: Option<StatusMessage>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    adb_path_input: String,
    log_dir_input: String,
    show_settings: bool,
    require_initial_setup: bool,
    show_clear_confirm: bool,
    selected_serial: Option<String>,
}

impl AdbCollectorApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config_exists = config::config_file_exists();
        let (config, startup_error) = match config::load_config() {
            Ok(config) => (config, None),
            Err(err) => (AppConfig::with_defaults(), Some(err)),
        };
        let require_initial_setup =
            !config_exists || !config.is_complete() || startup_error.is_some();
        let (tx, rx) = mpsc::channel();

        let mut app = Self {
            adb_path_input: config.adb_path.clone(),
            log_dir_input: config.log_dir.clone(),
            config,
            devices: Vec::new(),
            total_log_bytes: 0,
            status: None,
            tx,
            rx,
            show_settings: require_initial_setup,
            require_initial_setup,
            show_clear_confirm: false,
            selected_serial: None,
        };

        if !app.require_initial_setup {
            app.refresh_devices();
            app.refresh_log_size();
        } else if let Some(err) = startup_error {
            app.set_error(format!(
                "Config could not be loaded; please confirm settings before use: {err}"
            ));
        } else {
            app.set_info("Please confirm the ADB path and log directory before using the app.");
        }

        app
    }

    fn handle_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                AppEvent::DevicesRefreshed(result) => match result {
                    Ok(devices) => {
                        self.merge_devices(devices);
                        self.set_info("Device list refreshed.");
                    }
                    Err(err) => self.set_error(err),
                },
                AppEvent::LogSizeRefreshed(result) => match result {
                    Ok(size) => {
                        self.total_log_bytes = size;
                    }
                    Err(err) => self.set_error(err),
                },
                AppEvent::CollectionSpawned {
                    serial,
                    output_path,
                    child,
                } => {
                    if let Some(device) = self.find_device_mut(&serial) {
                        device.run_state = DeviceRunState::Running;
                        device.output_path = Some(output_path.clone());
                        device.child = Some(child);
                        device.started_at = Some(std::time::SystemTime::now());
                    }
                    self.set_info(format!(
                        "Started collecting logcat for {serial} -> {}",
                        output_path.display()
                    ));
                    self.refresh_log_size();
                }
                AppEvent::CollectionEnded {
                    serial,
                    exit_code,
                    error,
                } => {
                    let was_stopping = self
                        .find_device(&serial)
                        .map(|device| matches!(device.run_state, DeviceRunState::Stopping))
                        .unwrap_or(false);

                    if let Some(device) = self.find_device_mut(&serial) {
                        device.child = None;
                        device.started_at = None;
                        device.run_state = match error {
                            Some(ref err) => DeviceRunState::Error(err.clone()),
                            None => DeviceRunState::Idle,
                        };
                    }

                    if let Some(err) = error {
                        self.set_error(format!("Collector for {serial} stopped with error: {err}"));
                    } else if was_stopping {
                        self.set_info(format!("Stopped collecting logcat for {serial}."));
                    } else if let Some(code) = exit_code {
                        if code == 0 {
                            self.set_info(format!("Collector for {serial} exited."));
                        } else {
                            self.set_error(format!(
                                "Collector for {serial} exited unexpectedly with code {code}."
                            ));
                            if let Some(device) = self.find_device_mut(&serial) {
                                device.run_state =
                                    DeviceRunState::Error(format!("Exited with code {code}"));
                            }
                        }
                    } else {
                        self.set_info(format!("Collector for {serial} exited."));
                    }

                    self.refresh_devices();
                    self.refresh_log_size();
                }
                AppEvent::CleanupFinished(result) => match result {
                    Ok(()) => {
                        self.set_info("Historical logs cleared.");
                        self.refresh_log_size();
                    }
                    Err(err) => self.set_error(err),
                },
            }
        }
    }

    fn ui_summary(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Log directory:").strong());
            ui.monospace(self.config.log_dir.as_str());
            ui.separator();
            ui.label(RichText::new("ADB:").strong());
            ui.monospace(self.config.adb_path.as_str());
            ui.separator();
            ui.label(RichText::new("Historical log size:").strong());
            ui.monospace(fs_utils::format_bytes(self.total_log_bytes));
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("Refresh devices").clicked() {
                self.refresh_devices();
            }
            if ui.button("Refresh size").clicked() {
                self.refresh_log_size();
            }
            if ui.button("Clear history").clicked() {
                self.show_clear_confirm = true;
            }
            if ui.button("Settings").clicked() {
                self.show_settings = true;
            }
        });
    }

    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        ui.heading("ADB devices");
        ui.label("Double-click a device row to start logcat collection. Use Stop to end a running session.");
        ui.add_space(8.0);

        if self.devices.is_empty() {
            ui.label("No connected devices.");
            return;
        }

        let mut start_serial: Option<String> = None;
        let mut stop_serial: Option<String> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("device_grid")
                .striped(true)
                .min_col_width(90.0)
                .show(ui, |ui| {
                    ui.strong("Serial");
                    ui.strong("ADB state");
                    ui.strong("Session");
                    ui.strong("Output file");
                    ui.strong("Action");
                    ui.end_row();

                    for device in &mut self.devices {
                        let selected =
                            self.selected_serial.as_deref() == Some(device.info.serial.as_str());
                        let response = ui.selectable_label(selected, device.info.serial.as_str());
                        if response.clicked() {
                            self.selected_serial = Some(device.info.serial.clone());
                        }
                        if response.double_clicked() {
                            start_serial = Some(device.info.serial.clone());
                        }

                        ui.label(device.info.state.as_str());
                        ui.label(device.status_text());
                        let output_text = device
                            .output_path
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "-".to_owned());
                        ui.monospace(output_text);

                        match device.run_state {
                            DeviceRunState::Idle | DeviceRunState::Error(_) => {
                                let can_start = device.info.state == "device";
                                if ui
                                    .add_enabled(can_start, egui::Button::new("Start"))
                                    .clicked()
                                {
                                    start_serial = Some(device.info.serial.clone());
                                }
                            }
                            DeviceRunState::Starting | DeviceRunState::Running => {
                                if ui.button("Stop").clicked() {
                                    stop_serial = Some(device.info.serial.clone());
                                }
                            }
                            DeviceRunState::Stopping => {
                                ui.label("Stopping...");
                            }
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(serial) = start_serial {
            self.start_collection(serial);
        }
        if let Some(serial) = stop_serial {
            self.stop_collection(&serial);
        }
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status {
            let color = if status.is_error {
                Color32::from_rgb(200, 70, 70)
            } else {
                Color32::from_rgb(70, 160, 90)
            };
            ui.colored_label(color, status.text.as_str());
        }
    }

    fn ui_settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let title = if self.require_initial_setup {
            "Initial setup"
        } else {
            "Settings"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    "Confirm the ADB executable path and the directory used to store logcat files.",
                );
                ui.add_space(8.0);

                ui.label("ADB executable");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.adb_path_input);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = FileDialog::new().pick_file() {
                            self.adb_path_input = path.display().to_string();
                        }
                    }
                });

                ui.add_space(8.0);
                ui.label("Log directory");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.log_dir_input);
                    if ui.button("Browse").clicked() {
                        if let Some(path) = FileDialog::new().pick_folder() {
                            self.log_dir_input = path.display().to_string();
                        }
                    }
                });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.save_settings();
                    }

                    if !self.require_initial_setup && ui.button("Cancel").clicked() {
                        self.adb_path_input = self.config.adb_path.clone();
                        self.log_dir_input = self.config.log_dir.clone();
                        self.show_settings = false;
                    }
                });
            });
    }

    fn ui_clear_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_clear_confirm {
            return;
        }

        egui::Window::new("Clear historical logs")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Delete all historical .log files under the configured log directory?");
                ui.label("Active capture files will be preserved.");
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Delete").clicked() {
                        self.show_clear_confirm = false;
                        self.clear_history_logs();
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_clear_confirm = false;
                    }
                });
            });
    }

    fn save_settings(&mut self) {
        let candidate = AppConfig {
            adb_path: self.adb_path_input.trim().to_owned(),
            log_dir: self.log_dir_input.trim().to_owned(),
        };

        if candidate.adb_path.is_empty() || candidate.log_dir.is_empty() {
            self.set_error("ADB path and log directory are required.");
            return;
        }

        if let Err(err) = adb::validate_adb_path(candidate.adb_path.as_str()) {
            self.set_error(err);
            return;
        }

        let log_dir = PathBuf::from(candidate.log_dir.as_str());
        let resolved_log_dir = match config::ensure_log_dir(&log_dir) {
            Ok(path) => path,
            Err(err) => {
                self.set_error(err);
                return;
            }
        };

        let saved = AppConfig {
            adb_path: candidate.adb_path,
            log_dir: resolved_log_dir.display().to_string(),
        };

        if let Err(err) = config::save_config(&saved) {
            self.set_error(err);
            return;
        }

        self.config = saved.clone();
        self.adb_path_input = saved.adb_path.clone();
        self.log_dir_input = saved.log_dir.clone();
        self.show_settings = false;
        self.require_initial_setup = false;
        self.set_info("Settings saved.");
        self.refresh_devices();
        self.refresh_log_size();
    }

    fn refresh_devices(&self) {
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();

        thread::spawn(move || {
            let result =
                adb::validate_adb_path(&adb_path).and_then(|_| adb::list_devices(&adb_path));
            let _ = tx.send(AppEvent::DevicesRefreshed(result));
        });
    }

    fn refresh_log_size(&self) {
        let tx = self.tx.clone();
        let log_dir = self.config.log_dir.clone();

        thread::spawn(move || {
            let result = fs_utils::dir_size(PathBuf::from(log_dir).as_path());
            let _ = tx.send(AppEvent::LogSizeRefreshed(result));
        });
    }

    fn clear_history_logs(&mut self) {
        let tx = self.tx.clone();
        let log_dir = PathBuf::from(self.config.log_dir.as_str());
        let protected_paths: Vec<PathBuf> = self
            .devices
            .iter()
            .filter(|device| device.is_active())
            .filter_map(|device| device.output_path.clone())
            .collect();

        self.set_info("Clearing historical logs...");
        thread::spawn(move || {
            let result = fs_utils::clear_history_logs(&log_dir, &protected_paths);
            let _ = tx.send(AppEvent::CleanupFinished(result));
        });
    }

    fn start_collection(&mut self, serial: String) {
        if self.require_initial_setup {
            self.set_error("Finish the initial setup before starting collection.");
            self.show_settings = true;
            return;
        }

        if let Some(device) = self.find_device(&serial) {
            if device.info.state != "device" {
                self.set_error(format!(
                    "Device {serial} is not in `device` state and cannot start logcat."
                ));
                return;
            }
        }

        let output_path = fs_utils::session_log_path(
            PathBuf::from(self.config.log_dir.as_str()).as_path(),
            &serial,
        );
        if let Some(device) = self.find_device_mut(&serial) {
            if device.is_active() {
                self.set_info(format!("{serial} is already collecting."));
                return;
            }
            device.run_state = DeviceRunState::Starting;
            device.output_path = Some(output_path.clone());
        }

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let serial_for_thread = serial.clone();

        thread::spawn(move || {
            let child_holder: SharedChild = std::sync::Arc::new(std::sync::Mutex::new(None));

            match adb::spawn_logcat(&adb_path, &serial_for_thread, &output_path) {
                Ok(child) => {
                    if let Ok(mut guard) = child_holder.lock() {
                        *guard = Some(child);
                    } else {
                        let _ = tx.send(AppEvent::CollectionEnded {
                            serial: serial_for_thread,
                            exit_code: None,
                            error: Some("Failed to store collector process handle.".to_owned()),
                        });
                        return;
                    }

                    let _ = tx.send(AppEvent::CollectionSpawned {
                        serial: serial_for_thread.clone(),
                        output_path: output_path.clone(),
                        child: child_holder.clone(),
                    });

                    let (exit_code, error) = wait_for_process_exit(&child_holder);
                    let _ = tx.send(AppEvent::CollectionEnded {
                        serial: serial_for_thread,
                        exit_code,
                        error,
                    });
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::CollectionEnded {
                        serial: serial_for_thread,
                        exit_code: None,
                        error: Some(err),
                    });
                }
            }
        });
    }

    fn stop_collection(&mut self, serial: &str) {
        let Some(device) = self.find_device_mut(serial) else {
            return;
        };

        let Some(child) = device.child.clone() else {
            device.run_state = DeviceRunState::Idle;
            return;
        };

        device.run_state = DeviceRunState::Stopping;
        match child.lock() {
            Ok(mut guard) => {
                if let Some(process) = guard.as_mut() {
                    if let Err(err) = process.kill() {
                        device.run_state =
                            DeviceRunState::Error(format!("Failed to stop collector: {err}"));
                        self.set_error(format!("Failed to stop collector for {serial}: {err}"));
                    } else {
                        self.set_info(format!("Stopping collector for {serial}..."));
                    }
                } else {
                    device.run_state = DeviceRunState::Idle;
                }
            }
            Err(_) => {
                device.run_state =
                    DeviceRunState::Error("Collector handle is poisoned.".to_owned());
                self.set_error(format!(
                    "Failed to stop collector for {serial}: internal lock error"
                ));
            }
        }
    }

    fn merge_devices(&mut self, devices: Vec<DeviceInfo>) {
        let mut existing: HashMap<String, DeviceEntry> = self
            .devices
            .drain(..)
            .map(|device| (device.info.serial.clone(), device))
            .collect();

        let mut merged = Vec::with_capacity(devices.len() + existing.len());

        for info in devices {
            if let Some(mut current) = existing.remove(&info.serial) {
                current.info.state = info.state;
                merged.push(current);
            } else {
                merged.push(DeviceEntry::new(info));
            }
        }

        for (_, mut device) in existing {
            if device.is_active() {
                device.info.state = "disconnected".to_owned();
                merged.push(device);
            }
        }

        merged.sort_by(|a, b| a.info.serial.cmp(&b.info.serial));
        self.devices = merged;
    }

    fn find_device(&self, serial: &str) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|device| device.info.serial == serial)
    }

    fn find_device_mut(&mut self, serial: &str) -> Option<&mut DeviceEntry> {
        self.devices
            .iter_mut()
            .find(|device| device.info.serial == serial)
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage::info(text));
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage::error(text));
    }
}

impl eframe::App for AdbCollectorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events();

        egui::TopBottomPanel::bottom("status_panel").show(ctx, |ui| {
            self.ui_status(ui);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui_summary(ui);
            ui.separator();
            self.ui_devices(ui);
        });

        self.ui_settings_dialog(ctx);
        self.ui_clear_confirm_dialog(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

fn wait_for_process_exit(child: &SharedChild) -> (Option<i32>, Option<String>) {
    loop {
        thread::sleep(Duration::from_millis(300));

        let mut guard = match child.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return (
                    None,
                    Some("Collector process lock was poisoned.".to_owned()),
                );
            }
        };

        let Some(process) = guard.as_mut() else {
            return (None, None);
        };

        match process.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                *guard = None;
                return (code, None);
            }
            Ok(None) => {}
            Err(err) => {
                *guard = None;
                return (
                    None,
                    Some(format!("Failed to wait for collector process: {err}")),
                );
            }
        }
    }
}
