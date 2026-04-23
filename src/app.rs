use crate::{
    adb,
    config::{self, AppConfig, AppPaths},
    fs_utils,
    i18n::I18n,
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

pub struct AppBootstrap {
    pub app_paths: AppPaths,
    pub config: AppConfig,
    pub config_exists: bool,
    pub startup_error: Option<String>,
    pub version: &'static str,
}

pub struct AdbCollectorApp {
    app_paths: AppPaths,
    config: AppConfig,
    devices: Vec<DeviceEntry>,
    total_log_bytes: u64,
    status: Option<StatusMessage>,
    last_error: Option<StatusMessage>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    adb_path_input: String,
    log_dir_input: String,
    language_input: String,
    i18n: I18n,
    show_settings: bool,
    require_initial_setup: bool,
    show_clear_confirm: bool,
    selected_serial: Option<String>,
    version: String,
}

impl AdbCollectorApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, bootstrap: AppBootstrap) -> Self {
        let config = bootstrap.config;
        let require_initial_setup =
            !bootstrap.config_exists || !config.is_complete() || bootstrap.startup_error.is_some();
        let (tx, rx) = mpsc::channel();

        let mut app = Self {
            adb_path_input: config.adb_path.clone(),
            log_dir_input: config.log_dir.clone(),
            app_paths: bootstrap.app_paths,
            config,
            devices: Vec::new(),
            total_log_bytes: 0,
            status: None,
            last_error: None,
            tx,
            rx,
            language_input: String::new(),
            i18n: I18n::new("en"),
            show_settings: require_initial_setup,
            require_initial_setup,
            show_clear_confirm: false,
            selected_serial: None,
            version: bootstrap.version.to_owned(),
        };
        app.language_input = app.config.language.clone();
        app.i18n.set_language(&app.config.language);

        if !app.require_initial_setup {
            app.refresh_devices();
            app.refresh_log_size();
        } else if let Some(err) = bootstrap.startup_error {
            app.set_error(app.tr_args("status.config_load_error", &[("error", err)]));
        } else if !app.adb_path_input.trim().is_empty() {
            app.set_info(app.tr_args(
                "status.detected_adb",
                &[("path", app.adb_path_input.clone())],
            ));
        } else {
            app.set_info(app.tr("status.initial_no_adb"));
        }

        app
    }

    fn handle_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                AppEvent::DevicesRefreshed(result) => match result {
                    Ok(devices) => {
                        self.merge_devices(devices);
                        self.set_info(self.tr("status.device_list_refreshed"));
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
                    self.set_info(
                        self.tr_args("status.started_collection", &[("serial", serial.clone())]),
                    );
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
                        self.set_error(self.tr_args(
                            "status.collector_error",
                            &[("serial", serial.clone()), ("error", err)],
                        ));
                    } else if was_stopping {
                        self.set_info(
                            self.tr_args("status.stopped_collection", &[("serial", serial)]),
                        );
                    } else if let Some(code) = exit_code {
                        if code == 0 {
                            self.set_info(
                                self.tr_args("status.collector_exit", &[("serial", serial)]),
                            );
                        } else {
                            let message = self.tr_args(
                                "status.collector_exit_unexpected",
                                &[("serial", serial.clone()), ("code", code.to_string())],
                            );
                            self.set_error(message.clone());
                            if let Some(device) = self.find_device_mut(&serial) {
                                device.run_state = DeviceRunState::Error(message);
                            }
                        }
                    } else {
                        self.set_info(self.tr_args("status.collector_exit", &[("serial", serial)]));
                    }

                    self.refresh_devices();
                    self.refresh_log_size();
                }
                AppEvent::CleanupFinished(result) => match result {
                    Ok(()) => {
                        self.set_info(self.tr("status.history_cleared"));
                        self.refresh_log_size();
                    }
                    Err(err) => self.set_error(err),
                },
            }
        }
    }

    fn ui_summary(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.tr("app.title"));
        ui.small(format!(
            "{} v{} • {}",
            self.tr("misc.click_to_open"),
            self.version,
            if self.app_paths.portable_mode {
                self.tr("settings.mode.portable")
            } else {
                self.tr("settings.mode.appdata")
            }
        ));
        ui.add_space(8.0);

        ui.columns(4, |columns| {
            self.stat_card(
                &mut columns[0],
                &self.tr("overview.connected"),
                self.devices
                    .iter()
                    .filter(|device| device.info.state == "device")
                    .count()
                    .to_string(),
            );
            self.stat_card(
                &mut columns[1],
                &self.tr("overview.running"),
                self.devices
                    .iter()
                    .filter(|device| device.is_active())
                    .count()
                    .to_string(),
            );
            self.stat_card(
                &mut columns[2],
                &self.tr("overview.storage"),
                fs_utils::format_bytes(self.total_log_bytes),
            );
            self.stat_card(
                &mut columns[3],
                &self.tr("overview.language"),
                self.language_name(self.i18n.language()).to_owned(),
            );
        });

        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.tr("toolbar.refresh_devices")).clicked() {
                self.refresh_devices();
            }
            if ui.button(self.tr("toolbar.refresh_size")).clicked() {
                self.refresh_log_size();
            }
            if ui.button(self.tr("toolbar.open_logs")).clicked() {
                if let Err(err) = fs_utils::open_path(PathBuf::from(&self.config.log_dir).as_path())
                {
                    self.set_error(err);
                }
            }
            if ui.button(self.tr("toolbar.open_app_log")).clicked() {
                if let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path()) {
                    self.set_error(err);
                }
            }
            if ui.button(self.tr("toolbar.settings")).clicked() {
                self.show_settings = true;
            }
            if ui.button(self.tr("toolbar.clear_history")).clicked() {
                self.show_clear_confirm = true;
            }
        });
    }

    fn ui_selected_device(&mut self, ui: &mut egui::Ui) {
        let selected = self
            .selected_serial
            .as_deref()
            .and_then(|serial| self.find_device(serial).cloned());
        let heading = self.tr("device.selected");
        let state_label = self.tr("device.state");
        let session_label = self.tr("device.session");
        let started_label = self.tr("device.started");
        let latest_file_label = self.tr("device.latest_file");
        let never_text = self.tr("misc.never");
        let no_file_text = self.tr("device.no_file");
        let open_file_text = self.tr("device.action.open_file");
        let open_folder_text = self.tr("device.action.open_folder");
        let none_selected_text = self.tr("device.none_selected");

        ui.group(|ui| {
            ui.heading(heading);
            if let Some(device) = selected {
                ui.label(format!("{}: {}", state_label, device.info.state));
                ui.label(format!(
                    "{}: {}",
                    session_label,
                    self.run_state_text(&device.run_state)
                ));
                ui.label(format!(
                    "{}: {}",
                    started_label,
                    device
                        .started_at
                        .map(format_system_time)
                        .unwrap_or(never_text)
                ));
                ui.label(format!(
                    "{}: {}",
                    latest_file_label,
                    device
                        .output_path
                        .as_ref()
                        .and_then(|path| path.file_name().and_then(|name| name.to_str()))
                        .map(str::to_owned)
                        .unwrap_or(no_file_text)
                ));
                ui.horizontal(|ui| {
                    if let Some(path) = &device.output_path {
                        if ui.button(open_file_text).clicked() {
                            if let Err(err) = fs_utils::open_path(path) {
                                self.set_error(err);
                            }
                        }
                        if let Some(parent) = path.parent() {
                            if ui.button(open_folder_text).clicked() {
                                if let Err(err) = fs_utils::open_path(parent) {
                                    self.set_error(err);
                                }
                            }
                        }
                    }
                });
            } else {
                ui.label(none_selected_text);
            }
        });
    }

    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.tr("devices.title"));
        ui.label(self.tr("devices.hint"));
        ui.add_space(8.0);

        if self.devices.is_empty() {
            ui.label(self.tr("devices.empty"));
            return;
        }

        let mut start_serial: Option<String> = None;
        let mut stop_serial: Option<String> = None;
        let mut open_output: Option<PathBuf> = None;
        let i18n = self.i18n.clone();
        let serial_text = self.tr("device.column.serial");
        let state_text = self.tr("device.column.state");
        let session_text = self.tr("device.column.session");
        let started_text = self.tr("device.column.started");
        let output_text = self.tr("device.column.output");
        let actions_text = self.tr("device.column.actions");
        let never_text = self.tr("misc.never");
        let start_text = self.tr("device.action.start");
        let stop_text = self.tr("device.action.stop");
        let open_text = self.tr("device.action.open");
        let stopping_text = self.tr("run_state.stopping");

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("device_grid")
                .striped(true)
                .min_col_width(90.0)
                .show(ui, |ui| {
                    ui.strong(serial_text);
                    ui.strong(state_text);
                    ui.strong(session_text);
                    ui.strong(started_text);
                    ui.strong(output_text);
                    ui.strong(actions_text);
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
                        ui.label(run_state_text_with(&i18n, &device.run_state));
                        ui.label(
                            device
                                .started_at
                                .map(format_system_time)
                                .unwrap_or_else(|| never_text.clone()),
                        );
                        let output_name = device
                            .output_path
                            .as_ref()
                            .map(|path| {
                                if let Some(name) = path.file_name().and_then(|name| name.to_str())
                                {
                                    name.to_owned()
                                } else {
                                    path.to_string_lossy().into_owned()
                                }
                            })
                            .unwrap_or_else(|| "-".to_owned());
                        ui.monospace(output_name);

                        ui.horizontal(|ui| match device.run_state {
                            DeviceRunState::Idle | DeviceRunState::Error(_) => {
                                let can_start = device.info.state == "device";
                                if ui
                                    .add_enabled(can_start, egui::Button::new(start_text.clone()))
                                    .clicked()
                                {
                                    start_serial = Some(device.info.serial.clone());
                                }
                                if let Some(path) = &device.output_path {
                                    if ui.button(open_text.clone()).clicked() {
                                        open_output = Some(path.clone());
                                    }
                                }
                            }
                            DeviceRunState::Starting | DeviceRunState::Running => {
                                if ui.button(stop_text.clone()).clicked() {
                                    stop_serial = Some(device.info.serial.clone());
                                }
                                if let Some(path) = &device.output_path {
                                    if ui.button(open_text.clone()).clicked() {
                                        open_output = Some(path.clone());
                                    }
                                }
                            }
                            DeviceRunState::Stopping => {
                                ui.label(stopping_text.clone());
                            }
                        });
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
        if let Some(path) = open_output {
            if let Err(err) = fs_utils::open_path(&path) {
                self.set_error(err);
            }
        }
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status {
            let color = if status.is_error {
                Color32::from_rgb(200, 70, 70)
            } else {
                Color32::from_rgb(70, 160, 90)
            };
            ui.colored_label(color, format!("[{}] {}", status.timestamp, status.text));
        }
        if let Some(last_error) = &self.last_error {
            ui.add_space(4.0);
            ui.colored_label(
                Color32::from_rgb(200, 70, 70),
                format!(
                    "{} [{}]: {}",
                    self.tr("misc.last_error"),
                    last_error.timestamp,
                    last_error.text
                ),
            );
        }
    }

    fn ui_settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let title = if self.require_initial_setup {
            self.tr("settings.initial_title")
        } else {
            self.tr("settings.title")
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(self.tr("settings.intro"));
                ui.small(self.tr("settings.explainer"));
                ui.horizontal(|ui| {
                    if ui.button(self.tr("settings.open_config_dir")).clicked() {
                        if let Err(err) = fs_utils::open_path(self.app_paths.config_dir.as_path()) {
                            self.set_error(err);
                        }
                    }
                    if ui.button(self.tr("settings.open_app_log")).clicked() {
                        if let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path())
                        {
                            self.set_error(err);
                        }
                    }
                });
                ui.add_space(8.0);

                ui.label(self.tr("settings.adb"));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.adb_path_input);
                    if ui.button(self.tr("settings.browse")).clicked() {
                        if let Some(path) = FileDialog::new().pick_file() {
                            self.adb_path_input = fs_utils::display_path(path.as_path());
                        }
                    }
                    if ui.button(self.tr("settings.use_adb")).clicked() {
                        self.adb_path_input = "adb".to_owned();
                    }
                });

                ui.add_space(8.0);
                ui.label(self.tr("settings.log_dir"));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.log_dir_input);
                    if ui.button(self.tr("settings.browse")).clicked() {
                        if let Some(path) = FileDialog::new().pick_folder() {
                            self.log_dir_input = fs_utils::display_path(path.as_path());
                        }
                    }
                    if ui.button(self.tr("settings.use_default")).clicked() {
                        self.log_dir_input =
                            fs_utils::display_path(&self.app_paths.exe_dir.join("logs"));
                    }
                });
                ui.add_space(8.0);
                ui.label(self.tr("settings.language"));
                egui::ComboBox::from_id_salt("language-select")
                    .selected_text(self.language_name(&self.language_input))
                    .show_ui(ui, |ui| {
                        for (code, _) in I18n::supported_languages() {
                            let label = self.language_name(code);
                            ui.selectable_value(
                                &mut self.language_input,
                                (*code).to_owned(),
                                label,
                            );
                        }
                    });
                ui.small(self.tr("settings.language.help"));
                ui.small(if self.app_paths.portable_mode {
                    self.tr("settings.mode.portable")
                } else {
                    self.tr("settings.mode.appdata")
                });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(self.tr("settings.save")).clicked() {
                        self.save_settings();
                    }

                    if !self.require_initial_setup
                        && ui.button(self.tr("settings.cancel")).clicked()
                    {
                        self.adb_path_input = self.config.adb_path.clone();
                        self.log_dir_input = self.config.log_dir.clone();
                        self.language_input = self.config.language.clone();
                        self.show_settings = false;
                    }
                });
            });
    }

    fn ui_clear_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_clear_confirm {
            return;
        }

        egui::Window::new(self.tr("clear.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(self.tr("clear.body"));
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(self.tr("clear.delete")).clicked() {
                        self.show_clear_confirm = false;
                        self.clear_history_logs();
                    }
                    if ui.button(self.tr("clear.cancel")).clicked() {
                        self.show_clear_confirm = false;
                    }
                });
            });
    }

    fn save_settings(&mut self) {
        let candidate = AppConfig {
            adb_path: self.adb_path_input.trim().to_owned(),
            log_dir: self.log_dir_input.trim().to_owned(),
            app_log_max_size_mb: self.config.app_log_max_size_mb,
            language: self.language_input.clone(),
        };

        if candidate.adb_path.is_empty() || candidate.log_dir.is_empty() {
            self.set_error(self.tr("status.required_fields"));
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
            adb_path: fs_utils::display_path_string(&candidate.adb_path),
            log_dir: fs_utils::display_path(&resolved_log_dir),
            app_log_max_size_mb: candidate.app_log_max_size_mb,
            language: self.language_input.clone(),
        };

        if let Err(err) = config::save_config(&self.app_paths.config_path, &saved) {
            self.set_error(err);
            return;
        }

        self.config = saved.clone();
        self.adb_path_input = saved.adb_path.clone();
        self.log_dir_input = saved.log_dir.clone();
        self.language_input = saved.language.clone();
        self.i18n.set_language(&saved.language);
        self.show_settings = false;
        self.require_initial_setup = false;
        self.set_info(self.tr("status.settings_saved"));
        self.refresh_devices();
        self.refresh_log_size();
    }

    fn refresh_devices(&self) {
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();

        thread::spawn(move || {
            let result = adb::list_devices(&adb_path);
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
        let mut protected_paths = protected_paths;
        protected_paths.push(self.app_paths.app_log_path.clone());

        self.set_info(self.tr("status.clearing_history"));
        thread::spawn(move || {
            let result = fs_utils::clear_history_logs(&log_dir, &protected_paths);
            let _ = tx.send(AppEvent::CleanupFinished(result));
        });
    }

    fn start_collection(&mut self, serial: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }

        if let Some(device) = self.find_device(&serial) {
            if device.info.state != "device" {
                self.set_error(
                    self.tr_args("status.device_invalid_state", &[("serial", serial.clone())]),
                );
                return;
            }
        }

        let output_path = fs_utils::session_log_path(
            PathBuf::from(self.config.log_dir.as_str()).as_path(),
            &serial,
        );
        if let Some(device) = self.find_device_mut(&serial) {
            if device.is_active() {
                self.set_info(self.tr_args(
                    "status.device_already_collecting",
                    &[("serial", serial.clone())],
                ));
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
        let Some(index) = self
            .devices
            .iter()
            .position(|device| device.info.serial == serial)
        else {
            return;
        };

        let Some(child) = self.devices[index].child.clone() else {
            self.devices[index].run_state = DeviceRunState::Idle;
            return;
        };

        self.devices[index].run_state = DeviceRunState::Stopping;
        match child.lock() {
            Ok(mut guard) => {
                if let Some(process) = guard.as_mut() {
                    if let Err(err) = process.kill() {
                        let message = self.tr_args(
                            "status.stop_failed",
                            &[("serial", serial.to_owned()), ("error", err.to_string())],
                        );
                        self.devices[index].run_state = DeviceRunState::Error(message.clone());
                        self.set_error(message);
                    } else {
                        self.set_info(self.tr_args(
                            "status.stopping_collection",
                            &[("serial", serial.to_owned())],
                        ));
                    }
                } else {
                    self.devices[index].run_state = DeviceRunState::Idle;
                }
            }
            Err(_) => {
                self.devices[index].run_state =
                    DeviceRunState::Error("Collector handle is poisoned.".to_owned());
                self.set_error(self.tr_args(
                    "status.stop_failed",
                    &[
                        ("serial", serial.to_owned()),
                        ("error", "internal lock error".to_owned()),
                    ],
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
        let text = text.into();
        log::info!("{text}");
        self.status = Some(StatusMessage::info(text));
    }

    fn set_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::error!("{text}");
        let error = StatusMessage::error(text);
        self.status = Some(error.clone());
        self.last_error = Some(error);
    }

    fn tr(&self, key: &str) -> String {
        self.i18n.tr(key)
    }

    fn tr_args(&self, key: &str, args: &[(&str, String)]) -> String {
        self.i18n.tr_args(key, args)
    }

    fn language_name(&self, code: &str) -> String {
        match code {
            "zh-CN" => self.tr("language.zh-CN"),
            _ => self.tr("language.english"),
        }
    }

    fn run_state_text(&self, run_state: &DeviceRunState) -> String {
        run_state_text_with(&self.i18n, run_state)
    }

    fn stat_card(&self, ui: &mut egui::Ui, title: &str, value: String) {
        ui.group(|ui| {
            ui.label(RichText::new(title).strong());
            ui.add_space(4.0);
            ui.heading(value);
        });
    }
}

fn run_state_text_with(i18n: &I18n, run_state: &DeviceRunState) -> String {
    match run_state {
        DeviceRunState::Idle => i18n.tr("run_state.idle"),
        DeviceRunState::Starting => i18n.tr("run_state.starting"),
        DeviceRunState::Running => i18n.tr("run_state.running"),
        DeviceRunState::Stopping => i18n.tr("run_state.stopping"),
        DeviceRunState::Error(message) => {
            i18n.tr_args("run_state.error", &[("message", message.clone())])
        }
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
            self.ui_selected_device(ui);
            ui.add_space(8.0);
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

fn format_system_time(time: std::time::SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Local> = time.into();
    datetime.format("%H:%M:%S").to_string()
}
