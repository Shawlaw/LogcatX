use crate::{
    adb,
    config::{self, AppConfig, AppPaths, UpdateProxyConfig, UpdateProxyMode},
    fs_utils,
    i18n::I18n,
    ime::ImeEnterGuard,
    models::{
        AppEvent, DeviceEntry, DeviceInfo, DeviceRunState, ForegroundApp, ForegroundAppAction,
        Screenshot, SharedChild, StatusMessage,
    },
    scrcpy, updater,
};
use chrono::{Local, TimeZone};
use desktop_updater::{CheckResult, DownloadedUpdate, UpdateCandidate};
use eframe::egui::{self, Align, Color32, RichText};
use rfd::FileDialog;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROJECT_URL: &str = "https://github.com/Shawlaw/LogcatX";
const PLATFORM_TOOLS_URL_EN: &str = "https://developer.android.com/tools/releases/platform-tools";
const PLATFORM_TOOLS_URL_ZH_CN: &str =
    "https://developer.android.google.cn/tools/releases/platform-tools?hl=zh-cn";
const SCRCPY_RELEASES_URL: &str = "https://github.com/Genymobile/scrcpy/releases";
const DEFAULT_DEVICE_DROP_DIR: &str = "/sdcard/Download";
const DEFAULT_NEW_DISPLAY_WIDTH: &str = "720";
const DEFAULT_NEW_DISPLAY_HEIGHT: &str = "1600";
const DEFAULT_NEW_DISPLAY_DPI: &str = "320";
// Includes the footer controls plus the card's bottom inner margin on the
// fixed-height settings page.
const SETTINGS_FOOTER_HEIGHT: f32 = 70.0;
const SETTINGS_FOOTER_ERROR_HEIGHT: f32 = 116.0;
const SETTINGS_FOOTER_ERROR_VIEWPORT_HEIGHT: f32 =
    SETTINGS_FOOTER_ERROR_HEIGHT - SETTINGS_FOOTER_HEIGHT;
const SETTINGS_MIN_SCROLL_HEIGHT: f32 = 140.0;

#[derive(Clone, Debug, Default)]
struct DroppedPayload {
    apk_paths: Vec<PathBuf>,
    file_paths: Vec<PathBuf>,
}

impl DroppedPayload {
    fn is_empty(&self) -> bool {
        self.apk_paths.is_empty() && self.file_paths.is_empty()
    }

    fn total_count(&self) -> usize {
        self.apk_paths.len() + self.file_paths.len()
    }
}

#[derive(Clone, Debug)]
struct PendingForegroundConfirm {
    serial: String,
    action: ForegroundAppAction,
    app: ForegroundApp,
}

/// Display-ready facts about an available update; the full candidate is kept
/// separately because only a fresh, signature-verified check may be downloaded.
#[derive(Clone, Debug)]
struct UpdateInfo {
    version: String,
    notes_url: Option<String>,
}

/// Outcome of the most recent update check; availability is tracked separately
/// through `update_info` so a restored session can show a known update without
/// having contacted the network yet.
#[derive(Clone, Debug)]
enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateConnectionTestPhase {
    Idle,
    Testing,
    Succeeded(updater::UpdateConnectionTestResult),
    Failed(updater::UpdateConnectionTestError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationPage {
    Devices,
    Logs,
    LogFiles,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupTimeFilter {
    AllHistory,
    OlderThan7Days,
    OlderThan30Days,
    OlderThan90Days,
    BeforeDate,
}

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
    log_storage: fs_utils::LogStorageReport,
    log_storage_loading: bool,
    status: Option<StatusMessage>,
    last_error: Option<StatusMessage>,
    status_log: Vec<StatusMessage>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    adb_path_input: String,
    scrcpy_path_input: String,
    log_dir_input: String,
    language_input: String,
    auto_update_input: bool,
    update_proxy_mode_input: UpdateProxyMode,
    update_proxy_url_input: String,
    update_proxy_test_url_input: String,
    update_connection_test: UpdateConnectionTestPhase,
    scroll_to_update_proxy_settings: bool,
    settings_save_error: Option<String>,
    i18n: I18n,
    ime_enter_guard: ImeEnterGuard,
    show_settings: bool,
    require_initial_setup: bool,
    active_page: NavigationPage,
    show_clear_confirm: bool,
    cleanup_all_devices: bool,
    cleanup_selected_directories: HashSet<String>,
    cleanup_time_filter: CleanupTimeFilter,
    cleanup_before_date_input: String,
    cleanup_preview: Option<fs_utils::CleanupPreview>,
    cleanup_preview_filter: Option<fs_utils::CleanupFilter>,
    cleanup_preview_generation: u64,
    cleanup_preview_loading: bool,
    cleanup_in_progress: bool,
    show_alias_dialog: bool,
    show_logcat_args_dialog: bool,
    show_new_display_dialog: bool,
    show_connect_dialog: bool,
    selected_serial: Option<String>,
    version: String,
    connect_target_input: String,
    connect_in_progress: bool,
    restarting_adb_server: bool,
    screenshot_in_progress: bool,
    disconnecting_serial: Option<String>,
    alias_input_serial: Option<String>,
    alias_input_value: String,
    logcat_args_input_serial: Option<String>,
    logcat_args_input_value: String,
    new_display_device_id: Option<String>,
    new_display_use_device_defaults: bool,
    new_display_width_input: String,
    new_display_height_input: String,
    new_display_dpi_input: String,
    new_display_start_app_input: String,
    new_display_app_filter_input: String,
    new_display_apps: Vec<String>,
    new_display_apps_loading: bool,
    new_display_apps_error: Option<String>,
    pending_drop_payload: Option<DroppedPayload>,
    pending_drop_target_serial: Option<String>,
    pending_foreground_confirm: Option<PendingForegroundConfirm>,
    drop_task_in_progress: bool,
    foreground_task_in_progress: bool,
    devices_page_has_vertical_scroll: bool,
    device_poll_in_flight: bool,
    last_device_poll_at: Option<Instant>,
    last_device_snapshot: Vec<DeviceInfo>,
    last_auto_poll_error: Option<String>,
    sidebar_icon: Option<egui::TextureHandle>,
    update_phase: UpdatePhase,
    update_info: Option<UpdateInfo>,
    update_candidate: Option<UpdateCandidate>,
    update_dismissed: bool,
    update_cache: updater::UpdateStatusCache,
    show_update_dialog: bool,
    viewport_focused: Option<bool>,
    update_downloading: bool,
    update_download_progress: Option<(u64, u64)>,
    update_progress_rx: Option<Receiver<(u64, u64)>>,
    update_downloaded: Option<DownloadedUpdate>,
    update_applying: bool,
    exit_after_update_apply: bool,
}

impl AdbCollectorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, bootstrap: AppBootstrap) -> Self {
        let config = bootstrap.config;
        let update_proxy_mode_input = config.update_proxy.mode;
        let update_proxy_url_input = config.update_proxy.url.clone();
        let require_initial_setup =
            !bootstrap.config_exists || !config.is_complete() || bootstrap.startup_error.is_some();
        let (tx, rx) = mpsc::channel();

        let sidebar_icon =
            eframe::icon_data::from_png_bytes(include_bytes!("../icons/icon_128.png"))
                .ok()
                .map(|icon_data| {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [icon_data.width as usize, icon_data.height as usize],
                        &icon_data.rgba,
                    );
                    cc.egui_ctx
                        .load_texture("sidebar_icon", image, egui::TextureOptions::LINEAR)
                });

        let update_cache = updater::load_status_cache(&updater::status_cache_path(
            &bootstrap.app_paths.config_dir,
        ));
        let restored_update = update_cache
            .is_available_for(bootstrap.version)
            .then(|| UpdateInfo {
                version: update_cache.version.clone().unwrap_or_default(),
                notes_url: update_cache.notes_url.clone(),
            })
            .filter(|info| !info.version.is_empty());
        let update_dismissed = update_cache.is_dismissed();

        let mut app = Self {
            adb_path_input: config.adb_path.clone(),
            scrcpy_path_input: config.scrcpy_path.clone(),
            log_dir_input: config.log_dir.clone(),
            app_paths: bootstrap.app_paths,
            config,
            devices: Vec::new(),
            log_storage: fs_utils::LogStorageReport::default(),
            log_storage_loading: false,
            status: None,
            last_error: None,
            status_log: Vec::new(),
            tx,
            rx,
            language_input: String::new(),
            auto_update_input: false,
            update_proxy_mode_input,
            update_proxy_url_input,
            update_proxy_test_url_input: updater::DEFAULT_PROXY_TEST_URL.to_owned(),
            update_connection_test: UpdateConnectionTestPhase::Idle,
            scroll_to_update_proxy_settings: false,
            settings_save_error: None,
            i18n: I18n::new("en"),
            ime_enter_guard: ImeEnterGuard::default(),
            show_settings: require_initial_setup,
            require_initial_setup,
            active_page: NavigationPage::Devices,
            show_clear_confirm: false,
            cleanup_all_devices: true,
            cleanup_selected_directories: HashSet::new(),
            cleanup_time_filter: CleanupTimeFilter::AllHistory,
            cleanup_before_date_input: Local::now().format("%Y-%m-%d").to_string(),
            cleanup_preview: None,
            cleanup_preview_filter: None,
            cleanup_preview_generation: 0,
            cleanup_preview_loading: false,
            cleanup_in_progress: false,
            show_alias_dialog: false,
            show_logcat_args_dialog: false,
            show_new_display_dialog: false,
            show_connect_dialog: false,
            selected_serial: None,
            version: bootstrap.version.to_owned(),
            connect_target_input: String::new(),
            connect_in_progress: false,
            restarting_adb_server: false,
            screenshot_in_progress: false,
            disconnecting_serial: None,
            alias_input_serial: None,
            alias_input_value: String::new(),
            logcat_args_input_serial: None,
            logcat_args_input_value: String::new(),
            new_display_device_id: None,
            new_display_use_device_defaults: true,
            new_display_width_input: DEFAULT_NEW_DISPLAY_WIDTH.to_owned(),
            new_display_height_input: DEFAULT_NEW_DISPLAY_HEIGHT.to_owned(),
            new_display_dpi_input: DEFAULT_NEW_DISPLAY_DPI.to_owned(),
            new_display_start_app_input: String::new(),
            new_display_app_filter_input: String::new(),
            new_display_apps: Vec::new(),
            new_display_apps_loading: false,
            new_display_apps_error: None,
            pending_drop_payload: None,
            pending_drop_target_serial: None,
            pending_foreground_confirm: None,
            drop_task_in_progress: false,
            foreground_task_in_progress: false,
            devices_page_has_vertical_scroll: false,
            device_poll_in_flight: false,
            last_device_poll_at: None,
            last_device_snapshot: Vec::new(),
            last_auto_poll_error: None,
            sidebar_icon,
            update_phase: UpdatePhase::Idle,
            update_info: restored_update,
            update_candidate: None,
            update_dismissed,
            update_cache,
            show_update_dialog: false,
            viewport_focused: None,
            update_downloading: false,
            update_download_progress: None,
            update_progress_rx: None,
            update_downloaded: None,
            update_applying: false,
            exit_after_update_apply: false,
        };
        app.language_input = app.config.language.clone();
        app.auto_update_input = app.config.auto_check_updates;
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
                        self.last_device_snapshot = devices.clone();
                        self.last_auto_poll_error = None;
                        self.merge_devices(devices);
                        self.set_info(self.tr("status.device_list_refreshed"));
                    }
                    Err(err) => self.set_error(err),
                },
                AppEvent::DevicesPolled(result) => {
                    self.device_poll_in_flight = false;
                    match result {
                        Ok(devices) => {
                            let changed = devices != self.last_device_snapshot;
                            self.last_device_snapshot = devices.clone();
                            self.last_auto_poll_error = None;
                            if changed {
                                self.merge_devices(devices);
                                self.set_info(self.tr("status.device_list_auto_refreshed"));
                            }
                        }
                        Err(err) => {
                            if self.last_auto_poll_error.as_deref() != Some(err.as_str()) {
                                self.last_auto_poll_error = Some(err.clone());
                                self.set_error(err);
                            }
                        }
                    }
                }
                AppEvent::LogStorageRefreshed(result) => {
                    self.log_storage_loading = false;
                    match result {
                        Ok(report) => {
                            self.log_storage = report;
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::CleanupPreviewed {
                    request_id,
                    filter,
                    result,
                } => {
                    if !is_current_cleanup_preview_response(
                        self.cleanup_preview_generation,
                        request_id,
                    ) {
                        continue;
                    }
                    self.cleanup_preview_loading = false;
                    self.cleanup_preview_filter = Some(filter);
                    match result {
                        Ok(preview) => self.cleanup_preview = Some(preview),
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::ScreenshotFinished { serial, result } => {
                    self.screenshot_in_progress = false;
                    match result {
                        Ok(screenshot) => {
                            if let Err(err) = self.copy_screenshot_to_clipboard(screenshot) {
                                self.set_error(err);
                            } else {
                                self.set_info(self.tr_args(
                                    "status.screenshot_copied",
                                    &[("serial", self.device_identity_label(&serial))],
                                ));
                            }
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::ScrcpyAppsLoaded { device_id, result } => {
                    if self.new_display_device_id.as_deref() != Some(device_id.as_str()) {
                        continue;
                    }
                    self.new_display_apps_loading = false;
                    match result {
                        Ok(apps) => {
                            self.new_display_apps = apps;
                            self.new_display_apps_error = None;
                        }
                        Err(err) => {
                            log::warn!("Failed to load installed packages for scrcpy: {err}");
                            self.new_display_apps.clear();
                            self.new_display_apps_error = Some(err);
                        }
                    }
                }
                AppEvent::DeviceConnectFinished { target, result } => {
                    self.connect_in_progress = false;
                    match result {
                        Ok(message) => {
                            self.show_connect_dialog = false;
                            self.connect_target_input.clear();
                            self.set_info(self.tr_args(
                                "status.device_connected",
                                &[("target", target.clone()), ("message", message)],
                            ));
                            if let Err(err) = self.remember_recent_connection(&target) {
                                self.set_error(err);
                            }
                            self.refresh_devices();
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::DeviceDisconnectFinished { serial, result } => {
                    self.disconnecting_serial = None;
                    match result {
                        Ok(message) => {
                            self.set_info(self.tr_args(
                                "status.device_disconnected",
                                &[
                                    ("serial", self.device_identity_label(&serial)),
                                    ("message", message),
                                ],
                            ));
                            self.refresh_devices();
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::AdbServerRestartFinished(result) => {
                    self.restarting_adb_server = false;
                    match result {
                        Ok(message) => {
                            self.set_info(
                                self.tr_args(
                                    "status.adb_server_restarted",
                                    &[("message", message)],
                                ),
                            );
                            self.last_device_snapshot.clear();
                            self.refresh_devices();
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::DeviceDropFinished { serial, result } => {
                    self.drop_task_in_progress = false;
                    match result {
                        Ok(message) => self.set_info(self.tr_args(
                            "status.drop_finished",
                            &[
                                ("serial", self.device_identity_label(&serial)),
                                ("message", message),
                            ],
                        )),
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::ForegroundAppResolved {
                    serial,
                    action,
                    result,
                } => {
                    self.foreground_task_in_progress = false;
                    match result {
                        Ok(app) => match action {
                            ForegroundAppAction::Inspect => self.set_info(self.tr_args(
                                "status.foreground_app_inspected",
                                &[
                                    ("serial", self.device_identity_label(&serial)),
                                    ("app", self.foreground_app_label(&app)),
                                ],
                            )),
                            ForegroundAppAction::ForceStop => {
                                self.start_foreground_app_execution(serial, action, app);
                            }
                            ForegroundAppAction::ClearData | ForegroundAppAction::Uninstall => {
                                self.pending_foreground_confirm = Some(PendingForegroundConfirm {
                                    serial,
                                    action,
                                    app,
                                });
                            }
                        },
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::ForegroundAppActionFinished {
                    serial,
                    action,
                    app,
                    result,
                } => {
                    self.foreground_task_in_progress = false;
                    match result {
                        Ok(message) => {
                            let status_key = match action {
                                ForegroundAppAction::ForceStop => {
                                    "status.foreground_app_force_stopped"
                                }
                                ForegroundAppAction::ClearData => {
                                    "status.foreground_app_data_cleared"
                                }
                                ForegroundAppAction::Uninstall => {
                                    "status.foreground_app_uninstalled"
                                }
                                ForegroundAppAction::Inspect => "status.foreground_app_inspected",
                            };
                            self.set_info(self.tr_args(
                                status_key,
                                &[
                                    ("serial", self.device_identity_label(&serial)),
                                    ("app", self.foreground_app_label(&app)),
                                    ("message", message),
                                ],
                            ));
                            if matches!(action, ForegroundAppAction::Uninstall) {
                                self.refresh_devices();
                            }
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::CollectionSpawned {
                    serial,
                    output_path,
                    child,
                } => {
                    let device_name = self.device_identity_label(&serial);
                    if let Some(device) = self.find_device_mut(&serial) {
                        device.run_state = DeviceRunState::Running;
                        device.output_path = Some(output_path.clone());
                        device.child = Some(child);
                        device.started_at = Some(std::time::SystemTime::now());
                    }
                    self.set_info(
                        self.tr_args("status.started_collection", &[("serial", device_name)]),
                    );
                    self.refresh_log_size();
                }
                AppEvent::CollectionEnded {
                    serial,
                    exit_code,
                    error,
                } => {
                    let device_name = self.device_identity_label(&serial);
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
                            &[("serial", device_name.clone()), ("error", err)],
                        ));
                    } else if was_stopping {
                        self.set_info(
                            self.tr_args("status.stopped_collection", &[("serial", device_name)]),
                        );
                    } else if let Some(code) = exit_code {
                        if code == 0 {
                            self.set_info(
                                self.tr_args("status.collector_exit", &[("serial", device_name)]),
                            );
                        } else {
                            let message = self.tr_args(
                                "status.collector_exit_unexpected",
                                &[("serial", device_name), ("code", code.to_string())],
                            );
                            self.set_error(message.clone());
                            if let Some(device) = self.find_device_mut(&serial) {
                                device.run_state = DeviceRunState::Error(message);
                            }
                        }
                    } else {
                        self.set_info(
                            self.tr_args("status.collector_exit", &[("serial", device_name)]),
                        );
                    }

                    self.refresh_devices();
                    self.refresh_log_size();
                }
                AppEvent::CleanupFinished(result) => {
                    self.cleanup_in_progress = false;
                    match result {
                        Ok(outcome) => {
                            self.show_clear_confirm = false;
                            self.set_info(self.tr_args(
                                "status.cleanup_finished",
                                &[
                                    ("files", outcome.deleted_files.to_string()),
                                    ("size", fs_utils::format_bytes(outcome.freed_bytes)),
                                ],
                            ));
                            if !outcome.failed_paths.is_empty() {
                                self.set_error(self.tr_args(
                                    "status.cleanup_partial",
                                    &[("count", outcome.failed_paths.len().to_string())],
                                ));
                            }
                            self.refresh_log_size();
                        }
                        Err(err) => self.set_error(err),
                    }
                }
                AppEvent::UpdateCheckFinished { automatic, result } => {
                    self.handle_update_check_finished(automatic, result);
                }
                AppEvent::UpdateConnectionTestFinished(result) => {
                    self.update_connection_test = match result {
                        Ok(result) => UpdateConnectionTestPhase::Succeeded(result),
                        Err(error) => UpdateConnectionTestPhase::Failed(error),
                    };
                    self.scroll_to_update_proxy_settings = true;
                }
                AppEvent::UpdateDownloadFinished(result) => {
                    self.update_downloading = false;
                    self.update_download_progress = None;
                    self.update_progress_rx = None;
                    match result {
                        Ok(downloaded) => {
                            log::info!(
                                "Application update {} downloaded and verified",
                                downloaded.candidate.version()
                            );
                            self.update_downloaded = Some(downloaded);
                            self.set_info(self.tr_args(
                                "status.update_downloaded",
                                &[("version", self.update_info_version())],
                            ));
                        }
                        Err(err) => {
                            log::warn!("Application update download failed: {err}");
                            self.set_error(
                                self.tr_args("update.download_failed", &[("error", err)]),
                            );
                        }
                    }
                }
                AppEvent::UpdateApplyStarted(result) => match result {
                    Ok(()) => {
                        // The helper now owns replacement and restart; closing
                        // the viewport lets on_exit stop adb collections.
                        log::info!("Update helper started; exiting for update");
                        self.exit_after_update_apply = true;
                    }
                    Err(err) => {
                        log::warn!("Failed to start the update helper: {err}");
                        self.update_applying = false;
                        self.set_error(self.tr_args("update.apply_failed", &[("error", err)]));
                    }
                },
            }
        }
    }

    /// Drains download progress messages published by the download thread.
    fn poll_update_download_progress(&mut self) {
        let Some(receiver) = self.update_progress_rx.take() else {
            return;
        };
        while let Ok(progress) = receiver.try_recv() {
            self.update_download_progress = Some(progress);
        }
        self.update_progress_rx = Some(receiver);
    }

    fn update_info_version(&self) -> String {
        self.update_info
            .as_ref()
            .map(|info| info.version.clone())
            .unwrap_or_default()
    }

    fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        egui::Frame::new()
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(icon) = &self.sidebar_icon {
                        ui.add(egui::Image::new(icon).fit_to_exact_size(egui::vec2(28.0, 28.0)));
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("LogcatX")
                            .size(18.0)
                            .strong()
                            .color(Color32::from_rgb(31, 37, 49)),
                    );
                    ui.add_space(6.0);
                    let update_available = self.update_info.is_some() && !self.update_dismissed;
                    let mut pill_text = egui::text::LayoutJob::default();
                    if update_available {
                        pill_text.append(
                            "● ",
                            0.0,
                            egui::TextFormat::simple(
                                egui::FontId::proportional(10.0),
                                Color32::from_rgb(255, 200, 60),
                            ),
                        );
                    }
                    pill_text.append(
                        &format!("v{}", self.version),
                        0.0,
                        egui::TextFormat::simple(egui::FontId::proportional(11.5), Color32::WHITE),
                    );
                    let pill_button = egui::Button::new(pill_text)
                        .fill(Color32::from_rgb(49, 106, 255))
                        .corner_radius(egui::CornerRadius::same(8))
                        .min_size(egui::vec2(0.0, 22.0));
                    let mut pill_response = ui
                        .add(pill_button)
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if update_available {
                        pill_response =
                            pill_response.on_hover_text(self.tr("update.new_version_tooltip"));
                    }
                    if pill_response.clicked() {
                        self.show_update_dialog = true;
                    }
                });
            });
        ui.add_space(12.0);

        let nav_item =
            |ui: &mut egui::Ui, icon: &str, text: String, active: bool| -> egui::Response {
                let desired_size = egui::vec2(ui.available_width(), 48.0);
                let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                let fill = if active {
                    Color32::from_rgb(241, 246, 255)
                } else if response.is_pointer_button_down_on() {
                    Color32::from_rgb(243, 245, 250)
                } else if response.hovered() {
                    Color32::from_rgb(248, 249, 252)
                } else {
                    Color32::TRANSPARENT
                };
                let stroke = if active {
                    egui::Stroke::new(1.0, Color32::from_rgb(217, 228, 255))
                } else if response.hovered() {
                    egui::Stroke::new(1.0, Color32::from_rgb(236, 239, 245))
                } else {
                    egui::Stroke::NONE
                };
                let rounding = egui::CornerRadius::same(12);
                ui.painter().rect(
                    rect,
                    rounding,
                    fill,
                    stroke,
                    egui::epaint::StrokeKind::Middle,
                );
                if active {
                    let accent = egui::Rect::from_min_max(
                        rect.min + egui::vec2(-1.0, 8.0),
                        rect.min + egui::vec2(3.0, rect.height() - 8.0),
                    );
                    ui.painter().rect_filled(
                        accent,
                        egui::CornerRadius::same(2),
                        Color32::from_rgb(56, 116, 255),
                    );
                }

                let icon_color = if active {
                    Color32::from_rgb(56, 116, 255)
                } else {
                    Color32::from_rgb(106, 113, 127)
                };
                let text_color = if active {
                    Color32::from_rgb(40, 46, 58)
                } else {
                    Color32::from_rgb(61, 68, 80)
                };
                ui.painter().text(
                    rect.left_center() + egui::vec2(22.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    icon,
                    egui::FontId::proportional(18.0),
                    icon_color,
                );
                ui.painter().text(
                    rect.left_center() + egui::vec2(56.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    text,
                    egui::FontId::proportional(15.0),
                    text_color,
                );
                response
            };

        for (page, key, icon) in [
            (NavigationPage::Devices, "nav.devices", "◫"),
            (NavigationPage::Logs, "nav.logs", "≣"),
            (NavigationPage::LogFiles, "nav.log_files", "▤"),
            (NavigationPage::Settings, "nav.settings", "⚙"),
        ] {
            let response = nav_item(ui, icon, self.tr(key), self.active_page == page);
            if response.clicked() {
                match page {
                    NavigationPage::Settings if self.require_initial_setup => {
                        self.show_settings = true;
                    }
                    _ => self.active_page = page,
                }
            }
            ui.add_space(4.0);
        }

        ui.with_layout(egui::Layout::bottom_up(Align::LEFT), |ui| {
            let github_button = egui::Button::new(
                RichText::new(self.tr("toolbar.project_homepage"))
                    .size(12.0)
                    .color(Color32::from_rgb(88, 95, 110)),
            )
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(218, 224, 234)))
            .corner_radius(egui::CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 26.0));
            if ui.add(github_button).clicked()
                && let Err(err) = fs_utils::open_url(PROJECT_URL)
            {
                self.set_error(err);
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "{} {}",
                    self.devices.len(),
                    self.tr("nav.devices_count")
                ))
                .size(12.5)
                .color(Color32::from_rgb(125, 132, 145)),
            );
            ui.label(
                RichText::new(format!("● {}", self.tr("nav.adb_connected")))
                    .size(13.0)
                    .color(Color32::from_rgb(51, 162, 94))
                    .strong(),
            );
            ui.add_space(6.0);
        });
    }

    fn ui_overview_cards(&self, ui: &mut egui::Ui) {
        ui.with_layout(
            egui::Layout::left_to_right(Align::TOP).with_main_wrap(false),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                for (title, value, fill, color) in [
                    (
                        self.tr("overview.connected"),
                        self.devices
                            .iter()
                            .filter(|device| device.info.state == "device")
                            .count()
                            .to_string(),
                        Color32::from_rgb(234, 243, 255),
                        Color32::from_rgb(65, 129, 255),
                    ),
                    (
                        self.tr("overview.running"),
                        self.devices
                            .iter()
                            .filter(|device| device.is_active())
                            .count()
                            .to_string(),
                        Color32::from_rgb(239, 246, 255),
                        Color32::from_rgb(74, 134, 255),
                    ),
                    (
                        self.tr("overview.storage"),
                        fs_utils::format_bytes(self.log_storage.total_bytes),
                        Color32::from_rgb(255, 244, 232),
                        Color32::from_rgb(255, 161, 62),
                    ),
                ] {
                    ui.allocate_ui_with_layout(
                        egui::vec2(200.0, 0.0),
                        egui::Layout::top_down(Align::LEFT),
                        |ui| self.stat_card(ui, &title, value, fill, color),
                    );
                }
            },
        );
    }

    fn ui_action_row(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .inner_margin(egui::Margin::symmetric(0, 0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                ui.horizontal_wrapped(|ui| {
                    let primary = egui::Button::new(
                        RichText::new(button_label("⟳", self.tr("toolbar.refresh_devices")))
                            .color(Color32::WHITE)
                            .size(13.5)
                            .strong(),
                    )
                    .fill(Color32::from_rgb(56, 116, 255))
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(10))
                    .min_size(egui::vec2(112.0, 38.0));
                    if ui.add(primary).clicked() {
                        self.refresh_devices();
                    }

                    let secondary_button = |icon: &str, text: String| {
                        egui::Button::new(
                            RichText::new(button_label(icon, text))
                                .size(13.5)
                                .color(Color32::from_rgb(67, 73, 86)),
                        )
                        .fill(Color32::from_rgb(255, 255, 255))
                        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(231, 235, 243)))
                        .corner_radius(egui::CornerRadius::same(10))
                        .min_size(egui::vec2(104.0, 38.0))
                    };

                    if ui
                        .add(secondary_button("⇄", self.tr("toolbar.connect_device")))
                        .clicked()
                    {
                        self.show_connect_dialog = true;
                    }
                    let restart_text = if self.restarting_adb_server {
                        self.tr("toolbar.restarting_adb_server")
                    } else {
                        self.tr("toolbar.restart_adb_server")
                    };
                    if ui
                        .add_enabled(
                            !self.restarting_adb_server,
                            secondary_button("⟳", restart_text),
                        )
                        .clicked()
                    {
                        self.start_adb_server_restart();
                    }
                    if ui
                        .add(secondary_button("◌", self.tr("toolbar.refresh_size")))
                        .clicked()
                    {
                        self.refresh_log_size();
                    }
                    if ui
                        .add(secondary_button("⧉", self.tr("toolbar.open_logs")))
                        .clicked()
                        && let Err(err) =
                            fs_utils::open_path(PathBuf::from(&self.config.log_dir).as_path())
                    {
                        self.set_error(err);
                    }
                    if ui
                        .add(secondary_button("⌘", self.tr("toolbar.open_app_log")))
                        .clicked()
                        && let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path())
                    {
                        self.set_error(err);
                    }
                });
            });
    }

    fn ui_main_content(&mut self, ui: &mut egui::Ui) {
        match self.active_page {
            NavigationPage::Devices => {
                // BottomPanel 已从底部扣除日志面板空间，CentralPanel 可用高度自动正确
                // 直接在 CentralPanel 内容区使用 ScrollArea，无需手动 allocate_exact_size
                ui.spacing_mut().scroll.floating = false;
                let scrollbar_gutter = if self.devices_page_has_vertical_scroll {
                    0.0
                } else {
                    ui.spacing().scroll.allocated_width()
                };
                let output = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, _viewport| {
                        let full_width = ui.available_width();
                        ui.set_min_width(full_width);
                        ui.set_max_width(full_width);
                        // bar_inner_margin 让视口缩窄，但 clip_rect 也同步缩窄，
                        // 需要右扩一点给描边和圆角抗锯齿留空间
                        let mut clip = ui.clip_rect();
                        clip.max.x += 4.0;
                        ui.set_clip_rect(clip);
                        self.ui_overview_cards(ui);
                        ui.add_space(14.0);
                        self.ui_action_row(ui);
                        ui.add_space(14.0);

                        let devices_width = content_view_width(full_width, scrollbar_gutter);
                        ui.allocate_ui_with_layout(
                            egui::vec2(full_width, 0.0),
                            egui::Layout::left_to_right(Align::Min),
                            |ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(devices_width, 0.0),
                                    egui::Layout::top_down(Align::LEFT),
                                    |ui| {
                                        ui.set_min_width(devices_width);
                                        ui.set_max_width(devices_width);
                                        self.ui_devices(ui);
                                    },
                                );
                                if scrollbar_gutter > 0.0 {
                                    ui.add_space(scrollbar_gutter);
                                }
                            },
                        );
                    });
                self.devices_page_has_vertical_scroll =
                    output.content_size.y > output.inner_rect.height() + 0.5;
            }
            NavigationPage::Logs => self.ui_logs_page(ui),
            NavigationPage::LogFiles => self.ui_log_files_page(ui),
            NavigationPage::Settings => self.ui_settings_page(ui),
        }
    }

    /// 底部日志状态面板（在 TopBottomPanel::bottom 中渲染，高度固定不受内容影响）
    fn ui_status_panel(&mut self, ui: &mut egui::Ui) {
        // 不用 Frame，手动绘制背景避免 ScrollArea 内容通过 min_rect 撑大绘制区域
        let card_size = ui.available_size();
        let (card_rect, _) = ui.allocate_exact_size(card_size, egui::Sense::hover());
        ui.painter().rect(
            card_rect,
            egui::CornerRadius::same(12),
            Color32::from_rgb(255, 255, 255),
            egui::Stroke::new(1.0, Color32::from_rgb(229, 233, 241)),
            egui::epaint::StrokeKind::Inside,
        );
        let inner_rect = card_rect - egui::Margin::symmetric(16, 12);
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner_rect)
                .layout(egui::Layout::top_down(Align::LEFT)),
        );
        content_ui.set_clip_rect(inner_rect);
        content_ui.set_min_width(inner_rect.width());
        content_ui.horizontal(|ui| {
            ui.heading(self.tr("status.panel_title"));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button(self.tr("status.panel_clear")).clicked() {
                    self.status = None;
                    self.last_error = None;
                    self.status_log.clear();
                }
            });
        });
        content_ui.add_space(6.0);
        let scroll_height = content_ui.available_height().max(20.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(scroll_height)
            .show(&mut content_ui, |ui| {
                ui.set_min_width(ui.available_width());
                for entry in &self.status_log {
                    let color = if entry.is_error {
                        Color32::from_rgb(220, 71, 71)
                    } else {
                        Color32::from_rgb(51, 162, 94)
                    };
                    ui.colored_label(color, format!("[{}] {}", entry.timestamp, entry.text));
                }
            });
    }

    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("devices.title"));
            ui.label(self.tr("devices.hint"));
            ui.add_space(10.0);

            if self.devices.is_empty() {
                ui.add_space(16.0);
                ui.label(
                    RichText::new(self.tr("devices.empty"))
                        .size(16.0)
                        .color(Color32::from_rgb(86, 93, 106)),
                );
                ui.add_space(12.0);
                return;
            }

            let mut start_serial: Option<String> = None;
            let mut stop_serial: Option<String> = None;
            let mut open_output: Option<PathBuf> = None;
            let mut copy_serial: Option<String> = None;
            let mut copy_log_path_serial: Option<String> = None;
            let mut screenshot_serial: Option<String> = None;
            let mut open_scrcpy_mirror_serial: Option<String> = None;
            let mut open_new_display_serial: Option<String> = None;
            let mut open_scrcpy_settings = false;
            let mut open_shell_serial: Option<String> = None;
            let mut disconnect_serial: Option<String> = None;
            let mut toggle_pin_serial: Option<String> = None;
            let mut open_alias_serial: Option<String> = None;
            let mut open_logcat_args_serial: Option<String> = None;
            let mut foreground_action: Option<(String, ForegroundAppAction)> = None;
            let i18n = self.i18n.clone();
            let serial_text = self.tr("device.column.serial");
            let android_version_text = self.tr("device.column.android_version");
            let state_text = self.tr("device.column.state");
            let session_text = self.tr("device.column.session");
            let started_text = self.tr("device.column.started");
            let output_text = self.tr("device.column.output");
            let actions_text = self.tr("device.column.actions");
            let never_text = self.tr("misc.never");
            let start_text = self.tr("device.action.start");
            let stop_text = self.tr("device.action.stop");
            let open_text = self.tr("device.action.open");
            let copy_text = self.tr("device.action.copy_serial");
            let copy_log_path_text = self.tr("device.action.copy_latest_log_path");
            let screenshot_text = self.tr("device.action.screenshot");
            let scrcpy_menu_text = self.tr("device.action.scrcpy_menu");
            let scrcpy_mirror_text = self.tr("device.action.scrcpy_mirror");
            let scrcpy_new_display_text = self.tr("device.action.scrcpy_new_display");
            let scrcpy_settings_text = self.tr("device.action.scrcpy_settings");
            let shell_text = self.tr("device.action.open_shell");
            let disconnect_text = self.tr("device.action.disconnect");
            let more_text = self.tr("device.action.more");
            let stopping_text = self.tr("run_state.stopping");
            let pin_text = self.tr("device.action.pin");
            let unpin_text = self.tr("device.action.unpin");
            let edit_alias_text = self.tr("device.action.edit_alias");
            let edit_logcat_args_text = self.tr("device.action.edit_logcat_args");
            let open_folder_text = self.tr("device.action.open_folder");
            let inspect_foreground_app_text = self.tr("device.action.inspect_foreground_app");
            let stop_foreground_app_text = self.tr("device.action.stop_foreground_app");
            let clear_foreground_app_data_text = self.tr("device.action.clear_foreground_app_data");
            let uninstall_foreground_app_text = self.tr("device.action.uninstall_foreground_app");

            // Fixed widths for secondary columns (tightened for better fit).
            // First column is flexible: fills remaining space, min 180 px.
            let fixed_cols: f32 = 100.0 + 80.0 + 80.0 + 100.0 + 70.0 + 90.0;
            let col_spacing: f32 = 8.0 * 6.0; // actual item_spacing.x * gaps
            let row_inner_margin: f32 = 12.0 * 2.0;
            let name_col_w =
                (ui.available_width() - row_inner_margin - col_spacing - fixed_cols).max(180.0);
            let widths = [name_col_w, 100.0, 80.0, 80.0, 100.0, 70.0, 90.0];

            // Header row: 12px left indent to align with row frame inner margin
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                for (idx, title) in [
                    serial_text.clone(),
                    actions_text.clone(),
                    session_text.clone(),
                    state_text.clone(),
                    output_text.clone(),
                    android_version_text.clone(),
                    started_text.clone(),
                ]
                .into_iter()
                .enumerate()
                {
                    ui.allocate_ui_with_layout(
                        egui::vec2(widths[idx], 24.0),
                        egui::Layout::top_down(Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(title)
                                    .strong()
                                    .color(Color32::from_rgb(89, 96, 110)),
                            );
                        },
                    );
                }
            }); // end horizontal header
            ui.add_space(8.0);

            let rounded_secondary = |text: String| {
                egui::Button::new(text)
                    .corner_radius(egui::CornerRadius::same(8))
                    .min_size(egui::vec2(56.0, 30.0))
            };
            for index in 0..self.devices.len() {
                let serial = self.devices[index].info.serial.clone();
                let device_id = self.devices[index].info.identity_key.clone();
                let state = self.devices[index].info.state.clone();
                let android_version = self.devices[index].info.android_version.clone();
                let run_state = self.devices[index].run_state.clone();
                let started_at = self.devices[index].started_at;
                let output_path = self.devices[index].output_path.clone();
                let selected = self.selected_serial.as_deref() == Some(device_id.as_str());
                let primary_name = self.device_primary_name(&device_id);
                let is_pinned = self.is_pinned_device(&device_id);
                let row_fill = if selected {
                    Color32::from_rgb(243, 247, 255)
                } else {
                    Color32::from_rgb(250, 251, 254)
                };

                egui::Frame::new()
                    .fill(row_fill)
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(233, 237, 244)))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let label = if is_pinned {
                                format!("{} ({})", primary_name, self.tr("device.pinned_short"))
                            } else {
                                primary_name.clone()
                            };
                            let cell_text = if primary_name != serial {
                                format!("{}\n{}", label, serial)
                            } else {
                                label.clone()
                            };
                            let name_response = ui.add_sized(
                                [widths[0], 44.0],
                                egui::SelectableLabel::new(
                                    selected,
                                    RichText::new(cell_text).size(14.5),
                                ),
                            );
                            if name_response.clicked() {
                                if selected {
                                    self.selected_serial = None;
                                } else {
                                    self.selected_serial = Some(device_id.clone());
                                }
                            }
                            if name_response.double_clicked() {
                                start_serial = Some(device_id.clone());
                            }

                            // Helper closure for centered cell (painter-based for reliable H+V centering)
                            let centered_cell = |ui: &mut egui::Ui, w: f32, text: String| {
                                let (rect, _) = ui
                                    .allocate_exact_size(egui::vec2(w, 44.0), egui::Sense::hover());
                                if ui.is_rect_visible(rect) {
                                    let font_id = egui::TextStyle::Body.resolve(ui.style());
                                    let color = ui.visuals().text_color();
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        text,
                                        font_id,
                                        color,
                                    );
                                }
                            };

                            // Actions column: vertical centering via top_down(Center) wrapper,
                            // horizontal centering via narrower group_w allocation for
                            // Idle/Running states so the button group aligns with the header.
                            ui.allocate_ui_with_layout(
                                egui::vec2(widths[1], 44.0),
                                egui::Layout::top_down(Align::Center),
                                |ui| {
                                    // Vertical center: button height ~30px, cell 44px → pad 7px
                                    ui.add_space(7.0);
                                    // group_w = Stopping uses full column (may overflow as before);
                                    // Idle/Error = Start(52)+gap(10)+More(44) ≈ 106;
                                    // Starting/Running = Stop(56)+gap(10)+More(44) ≈ 110.
                                    let group_w = match &run_state {
                                        DeviceRunState::Stopping => widths[1],
                                        DeviceRunState::Idle | DeviceRunState::Error(_) => 106.0,
                                        _ => 110.0,
                                    };
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(group_w, 30.0),
                                        egui::Layout::left_to_right(Align::Center),
                                        |ui| {
                                            match run_state {
                                                DeviceRunState::Idle | DeviceRunState::Error(_) => {
                                                    let can_start = state == "device";
                                                    let start_button = egui::Button::new(
                                                        RichText::new(start_text.clone())
                                                            .color(Color32::WHITE)
                                                            .strong(),
                                                    )
                                                    .fill(Color32::from_rgb(56, 116, 255))
                                                    .stroke(egui::Stroke::NONE)
                                                    .corner_radius(egui::CornerRadius::same(8))
                                                    .min_size(egui::vec2(52.0, 30.0));
                                                    if ui
                                                        .add_enabled(can_start, start_button)
                                                        .clicked()
                                                    {
                                                        start_serial = Some(serial.clone());
                                                    }
                                                }
                                                DeviceRunState::Starting
                                                | DeviceRunState::Running => {
                                                    if ui
                                                        .add(rounded_secondary(stop_text.clone()))
                                                        .clicked()
                                                    {
                                                        stop_serial = Some(serial.clone());
                                                    }
                                                }
                                                DeviceRunState::Stopping => {
                                                    ui.label(stopping_text.clone());
                                                }
                                            }

                                            ui.menu_button(&more_text, |ui| {
                                                let is_pinned = self.is_pinned_device(&device_id);
                                                let pin_label = if is_pinned {
                                                    unpin_text.clone()
                                                } else {
                                                    pin_text.clone()
                                                };
                                                if ui.add(rounded_secondary(pin_label)).clicked() {
                                                    toggle_pin_serial = Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add(rounded_secondary(edit_alias_text.clone()))
                                                    .clicked()
                                                {
                                                    open_alias_serial = Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add(rounded_secondary(
                                                        edit_logcat_args_text.clone(),
                                                    ))
                                                    .clicked()
                                                {
                                                    open_logcat_args_serial =
                                                        Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add(rounded_secondary(copy_text.clone()))
                                                    .clicked()
                                                {
                                                    copy_serial = Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                {
                                                    let has_log_path = output_path.is_some();
                                                    if ui
                                                        .add_enabled(
                                                            has_log_path,
                                                            rounded_secondary(
                                                                copy_log_path_text.clone(),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        copy_log_path_serial =
                                                            Some(device_id.clone());
                                                        ui.close_menu();
                                                    }
                                                }
                                                if ui
                                                    .add_enabled(
                                                        state == "device"
                                                            && !self.screenshot_in_progress,
                                                        rounded_secondary(screenshot_text.clone()),
                                                    )
                                                    .clicked()
                                                {
                                                    screenshot_serial = Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                ui.menu_button(scrcpy_menu_text.clone(), |ui| {
                                                    if ui
                                                        .add_enabled(
                                                            state == "device",
                                                            rounded_secondary(
                                                                scrcpy_mirror_text.clone(),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        open_scrcpy_mirror_serial =
                                                            Some(device_id.clone());
                                                        ui.close_menu();
                                                    }
                                                    if ui
                                                        .add_enabled(
                                                            state == "device",
                                                            rounded_secondary(
                                                                scrcpy_new_display_text.clone(),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        open_new_display_serial =
                                                            Some(device_id.clone());
                                                        ui.close_menu();
                                                    }
                                                    ui.separator();
                                                    if ui
                                                        .add(rounded_secondary(
                                                            scrcpy_settings_text.clone(),
                                                        ))
                                                        .clicked()
                                                    {
                                                        open_scrcpy_settings = true;
                                                        ui.close_menu();
                                                    }
                                                });
                                                if ui
                                                    .add_enabled(
                                                        state == "device",
                                                        rounded_secondary(shell_text.clone()),
                                                    )
                                                    .clicked()
                                                {
                                                    open_shell_serial = Some(device_id.clone());
                                                    ui.close_menu();
                                                }
                                                if let Some(path) = &output_path
                                                    && ui
                                                        .add(rounded_secondary(open_text.clone()))
                                                        .clicked()
                                                {
                                                    open_output = Some(path.clone());
                                                    ui.close_menu();
                                                }
                                                {
                                                    let device_dir = fs_utils::device_log_dir(
                                                        PathBuf::from(self.config.log_dir.as_str())
                                                            .as_path(),
                                                        &device_id,
                                                        self.device_alias(&device_id).as_deref(),
                                                    );
                                                    if device_dir.exists()
                                                        && ui
                                                            .add(rounded_secondary(
                                                                open_folder_text.clone(),
                                                            ))
                                                            .clicked()
                                                    {
                                                        if let Err(err) =
                                                            fs_utils::open_path(&device_dir)
                                                        {
                                                            self.set_error(err);
                                                        }
                                                        ui.close_menu();
                                                    }
                                                }
                                                ui.separator();
                                                if ui
                                                    .add_enabled(
                                                        state == "device"
                                                            && !self.foreground_task_in_progress,
                                                        rounded_secondary(
                                                            inspect_foreground_app_text.clone(),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    foreground_action = Some((
                                                        device_id.clone(),
                                                        ForegroundAppAction::Inspect,
                                                    ));
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        state == "device"
                                                            && !self.foreground_task_in_progress,
                                                        rounded_secondary(
                                                            stop_foreground_app_text.clone(),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    foreground_action = Some((
                                                        device_id.clone(),
                                                        ForegroundAppAction::ForceStop,
                                                    ));
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        state == "device"
                                                            && !self.foreground_task_in_progress,
                                                        rounded_secondary(
                                                            clear_foreground_app_data_text.clone(),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    foreground_action = Some((
                                                        device_id.clone(),
                                                        ForegroundAppAction::ClearData,
                                                    ));
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        state == "device"
                                                            && !self.foreground_task_in_progress,
                                                        rounded_secondary(
                                                            uninstall_foreground_app_text.clone(),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    foreground_action = Some((
                                                        device_id.clone(),
                                                        ForegroundAppAction::Uninstall,
                                                    ));
                                                    ui.close_menu();
                                                }
                                                if let Some(network_serial) =
                                                    self.device_network_transport_serial(&device_id)
                                                    && ui
                                                        .add_enabled(
                                                            self.disconnecting_serial.as_deref()
                                                                != Some(network_serial.as_str()),
                                                            rounded_secondary(
                                                                disconnect_text.clone(),
                                                            ),
                                                        )
                                                        .clicked()
                                                {
                                                    disconnect_serial = Some(network_serial);
                                                    ui.close_menu();
                                                }
                                            });
                                        },
                                    );
                                },
                            );
                            centered_cell(ui, widths[2], run_state_text_with(&i18n, &run_state));
                            {
                                let (badge_cell_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(widths[3], 44.0),
                                    egui::Sense::hover(),
                                );
                                if ui.is_rect_visible(badge_cell_rect) {
                                    draw_state_badge_centered(
                                        ui,
                                        badge_cell_rect,
                                        &self.device_state_text(&state),
                                        self.device_state_color(&state),
                                    );
                                }
                            }
                            let output_name = output_path
                                .as_ref()
                                .map(|path| {
                                    path.file_name()
                                        .and_then(|name| name.to_str())
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| path.to_string_lossy().into_owned())
                                })
                                .unwrap_or_else(|| "-".to_owned());
                            let _ = ui.add_sized(
                                [widths[4], 44.0],
                                egui::Label::new(RichText::new(output_name).size(13.0)).truncate(),
                            );
                            centered_cell(
                                ui,
                                widths[5],
                                android_version
                                    .unwrap_or_else(|| self.tr("device.android_version.unknown")),
                            );
                            centered_cell(
                                ui,
                                widths[6],
                                started_at
                                    .map(format_system_time)
                                    .unwrap_or_else(|| never_text.clone()),
                            );
                        });
                    });
                ui.add_space(8.0);
            }

            if let Some(serial) = start_serial {
                self.start_collection(serial);
            }
            if let Some(serial) = stop_serial {
                self.stop_collection(&serial);
            }
            if let Some(path) = open_output
                && let Err(err) = fs_utils::open_path(&path)
            {
                self.set_error(err);
            }
            if let Some(serial) = copy_serial {
                self.copy_serial_to_clipboard(&ctx, serial);
            }
            if let Some(serial) = copy_log_path_serial {
                self.copy_log_path_to_clipboard(&ctx, &serial);
            }
            if let Some(serial) = screenshot_serial {
                self.start_screenshot(serial);
            }
            if let Some(serial) = open_scrcpy_mirror_serial {
                self.start_scrcpy_mirror(serial);
            }
            if let Some(serial) = open_new_display_serial {
                self.open_new_display_dialog(serial);
            }
            if open_scrcpy_settings {
                self.active_page = NavigationPage::Settings;
            }
            if let Some(serial) = open_shell_serial {
                self.open_device_shell(serial);
            }
            if let Some(serial) = disconnect_serial {
                self.start_disconnect(serial);
            }
            if let Some(serial) = toggle_pin_serial {
                self.toggle_pinned_device(&serial);
            }
            if let Some(serial) = open_alias_serial {
                self.open_alias_editor(&serial);
            }
            if let Some(serial) = open_logcat_args_serial {
                self.open_logcat_args_editor(&serial);
            }
            if let Some((serial, action)) = foreground_action {
                self.start_foreground_app_action(serial, action);
            }
        });
    }

    fn ui_logs_page(&mut self, ui: &mut egui::Ui) {
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("logs.title"));
            ui.label(self.tr("logs.hint"));
            ui.add_space(10.0);
            self.ui_status_content(ui, None);
        });
    }

    fn ui_log_files_page(&mut self, ui: &mut egui::Ui) {
        let app_log_size = fs_utils::file_size(self.app_paths.app_log_path.as_path()).unwrap_or(0);
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("log_files.title"));
            ui.label(self.tr("log_files.hint"));
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button(self.tr("toolbar.open_logs")).clicked()
                    && let Err(err) =
                        fs_utils::open_path(PathBuf::from(&self.config.log_dir).as_path())
                {
                    self.set_error(err);
                }
                if ui.button(self.tr("toolbar.open_app_log")).clicked()
                    && let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path())
                {
                    self.set_error(err);
                }
                if ui.button(self.tr("toolbar.refresh_size")).clicked() {
                    self.refresh_log_size();
                }
                if ui.button(self.tr("toolbar.clear_history")).clicked() {
                    self.open_cleanup_dialog();
                }
            });
            ui.add_space(12.0);
            egui::Grid::new("log-files-summary")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    detail_label(ui, &self.tr("log_files.path"));
                    ui.label(self.config.log_dir.clone());
                    ui.end_row();

                    detail_label(ui, &self.tr("log_files.app_log"));
                    ui.label(fs_utils::display_path(
                        self.app_paths.app_log_path.as_path(),
                    ));
                    ui.end_row();

                    detail_label(ui, &self.tr("overview.storage"));
                    ui.label(fs_utils::format_bytes(self.log_storage.total_bytes));
                    ui.end_row();

                    detail_label(ui, &self.tr("log_files.device_logs"));
                    ui.label(self.tr_args(
                        "log_files.files_and_size",
                        &[
                            ("files", self.log_storage.log_file_count.to_string()),
                            ("size", fs_utils::format_bytes(self.log_storage.log_bytes)),
                        ],
                    ));
                    ui.end_row();

                    detail_label(ui, &self.tr("log_files.other_files"));
                    ui.label(fs_utils::format_bytes(self.log_storage.other_file_bytes));
                    ui.end_row();

                    detail_label(ui, &self.tr("log_files.app_log_size"));
                    ui.label(fs_utils::format_bytes(app_log_size));
                    ui.end_row();
                });
            ui.add_space(14.0);
            ui.label(RichText::new(self.tr("log_files.device_usage")).strong());
            ui.add_space(6.0);
            if self.log_storage_loading {
                ui.small(self.tr("log_files.scanning"));
            } else if self.log_storage.device_directories.is_empty() {
                ui.small(self.tr("log_files.empty"));
            } else {
                egui::Grid::new("log-files-device-usage")
                    .num_columns(5)
                    .spacing([16.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        detail_label(ui, &self.tr("log_files.column.device"));
                        detail_label(ui, &self.tr("log_files.column.files"));
                        detail_label(ui, &self.tr("log_files.column.size"));
                        detail_label(ui, &self.tr("log_files.column.oldest"));
                        detail_label(ui, &self.tr("log_files.column.latest"));
                        ui.end_row();

                        for usage in &self.log_storage.device_directories {
                            ui.label(self.log_directory_label(&usage.directory_name));
                            ui.label(usage.log_file_count.to_string());
                            ui.label(fs_utils::format_bytes(usage.total_bytes));
                            ui.label(self.format_log_time(usage.oldest_log_modified));
                            ui.label(self.format_log_time(usage.newest_log_modified));
                            ui.end_row();
                        }
                    });
            }
        });
    }

    fn ui_settings_page(&mut self, ui: &mut egui::Ui) {
        // Draw the card into an exact rectangle. A Frame expands to its
        // contents' minimum rect, which lets wide form rows grow the frame
        // back to CentralPanel's clip edge and hides its right rounded corner.
        // The dedicated child UI keeps both the border and overflowing form
        // content inside the central panel's already symmetric inset.
        let card_width = ui.available_width();
        let page_height = ui.available_height();
        let (card_rect, _) =
            ui.allocate_exact_size(egui::vec2(card_width, page_height), egui::Sense::hover());
        ui.painter().rect(
            card_rect,
            egui::CornerRadius::same(16),
            Color32::from_rgb(255, 255, 255),
            egui::Stroke::new(1.0, Color32::from_rgb(229, 233, 241)),
            egui::epaint::StrokeKind::Inside,
        );

        let inner_rect = card_rect - egui::Margin::symmetric(18, 18);
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner_rect)
                .layout(egui::Layout::top_down(Align::LEFT)),
        );
        content_ui.set_clip_rect(inner_rect);
        content_ui.set_min_width(inner_rect.width());
        content_ui.set_max_width(inner_rect.width());
        content_ui.heading(self.tr("settings.title"));
        content_ui.label(self.tr("settings.page_hint"));
        content_ui.add_space(10.0);

        // Split the remaining space into two fixed rectangles rather than
        // estimating the footer after rendering the scroll area. This keeps
        // the actions visible even when the form content is taller than the
        // viewport.
        let remaining_rect = content_ui.available_rect_before_wrap();
        let footer_height = self.settings_footer_height();
        let footer_top = (remaining_rect.max.y - footer_height).max(remaining_rect.min.y);
        let form_rect = egui::Rect::from_min_max(
            remaining_rect.min,
            // Keep the form content aligned with the card padding on the
            // left, but let its scrollbar use the card's right padding.
            egui::pos2(card_rect.max.x, footer_top),
        );
        let footer_rect = egui::Rect::from_min_max(
            egui::pos2(remaining_rect.min.x, footer_top),
            remaining_rect.max,
        );
        let mut form_ui = content_ui.new_child(
            egui::UiBuilder::new()
                .max_rect(form_rect)
                .layout(egui::Layout::top_down(Align::LEFT)),
        );
        form_ui.set_clip_rect(form_rect);
        form_ui.set_min_width(form_rect.width());
        form_ui.set_max_width(form_rect.width());
        // Page-wide scrolling uses a safety gutter, but this card's scrollbar
        // should sit beside its content within the card's own right padding.
        form_ui.spacing_mut().scroll.bar_inner_margin = 0.0;
        egui::ScrollArea::vertical()
            .id_salt("settings-page-form")
            .auto_shrink([false, false])
            .max_height(form_rect.height())
            .show(&mut form_ui, |ui| {
                ui.set_min_width(form_rect.width());
                ui.set_max_width(form_rect.width());
                self.ui_settings_fields(ui, true);
            });
        let mut footer_ui = content_ui.new_child(
            egui::UiBuilder::new()
                .max_rect(footer_rect)
                .layout(egui::Layout::top_down(Align::LEFT)),
        );
        footer_ui.set_clip_rect(footer_rect);
        footer_ui.set_min_width(footer_rect.width());
        footer_ui.set_max_width(footer_rect.width());
        self.ui_settings_actions(&mut footer_ui, true);
    }

    fn ui_status_content(&mut self, ui: &mut egui::Ui, max_height: Option<f32>) {
        egui::Frame::new()
            .fill(Color32::from_rgb(255, 255, 255))
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(229, 233, 241)))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.heading(self.tr("status.panel_title"));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(self.tr("status.panel_clear")).clicked() {
                            self.status = None;
                            self.last_error = None;
                            self.status_log.clear();
                        }
                    });
                });
                ui.add_space(6.0);
                let mut scroll = egui::ScrollArea::vertical().stick_to_bottom(true);
                if let Some(max_height) = max_height {
                    scroll = scroll.max_height(max_height);
                }
                scroll.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for entry in &self.status_log {
                        let color = if entry.is_error {
                            Color32::from_rgb(220, 71, 71)
                        } else {
                            Color32::from_rgb(51, 162, 94)
                        };
                        ui.colored_label(color, format!("[{}] {}", entry.timestamp, entry.text));
                    }
                });
            });
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

        let max_height = (ctx.available_rect().height() - 32.0).max(240.0);
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .max_height(max_height)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let form_height = (ui.available_height() - self.settings_footer_height())
                    .max(SETTINGS_MIN_SCROLL_HEIGHT);
                egui::ScrollArea::vertical()
                    .id_salt("settings-dialog-form")
                    .auto_shrink([false, false])
                    .max_height(form_height)
                    .show(ui, |ui| {
                        self.ui_settings_fields(ui, false);
                    });
                self.ui_settings_actions(ui, false);
            });
    }

    fn ui_settings_fields(&mut self, ui: &mut egui::Ui, inline_page: bool) {
        ui.label(self.tr("settings.intro"));
        ui.add(egui::Label::new(RichText::new(self.tr("settings.explainer")).small()).wrap());
        ui.horizontal(|ui| {
            if ui.button(self.tr("settings.open_config_dir")).clicked()
                && let Err(err) = fs_utils::open_path(self.app_paths.config_dir.as_path())
            {
                self.set_error(err);
            }
            if ui.button(self.tr("settings.open_app_log")).clicked()
                && let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path())
            {
                self.set_error(err);
            }
        });
        ui.add_space(8.0);

        ui.label(self.tr("settings.adb"));
        let path_input_width = settings_path_input_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                egui::vec2(path_input_width, 0.0),
                egui::TextEdit::singleline(&mut self.adb_path_input),
            );
            if ui.button(self.tr("settings.browse")).clicked()
                && let Some(path) = FileDialog::new().pick_file()
            {
                self.adb_path_input = fs_utils::display_path(path.as_path());
            }
            if ui.button(self.tr("settings.use_adb")).clicked() {
                self.adb_path_input = "adb".to_owned();
            }
        });
        if self.adb_path_input.trim().is_empty() {
            ui.add_space(4.0);
            ui.small(self.tr("settings.adb.download_hint"));
            ui.hyperlink_to(
                self.tr("settings.adb.download_link"),
                self.adb_download_url(),
            );
        }

        ui.add_space(8.0);
        ui.label(self.tr("settings.scrcpy"));
        let path_input_width = settings_path_input_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                egui::vec2(path_input_width, 0.0),
                egui::TextEdit::singleline(&mut self.scrcpy_path_input),
            );
            if ui.button(self.tr("settings.browse")).clicked()
                && let Some(path) = FileDialog::new().pick_file()
            {
                self.scrcpy_path_input = fs_utils::display_path(path.as_path());
            }
            if ui.button(self.tr("settings.use_scrcpy")).clicked() {
                self.scrcpy_path_input = "scrcpy".to_owned();
            }
        });
        if self.scrcpy_path_input.trim().is_empty() {
            ui.add_space(4.0);
            ui.small(self.tr("settings.scrcpy.download_hint"));
            ui.hyperlink_to(
                self.tr("settings.scrcpy.download_link"),
                SCRCPY_RELEASES_URL,
            );
        }

        ui.add_space(8.0);
        ui.label(self.tr("settings.log_dir"));
        let path_input_width = settings_path_input_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                egui::vec2(path_input_width, 0.0),
                egui::TextEdit::singleline(&mut self.log_dir_input),
            );
            if ui.button(self.tr("settings.browse")).clicked()
                && let Some(path) = FileDialog::new().pick_folder()
            {
                self.log_dir_input = fs_utils::display_path(path.as_path());
            }
            if ui.button(self.tr("settings.use_default")).clicked() {
                self.log_dir_input = fs_utils::display_path(&self.app_paths.exe_dir.join("logs"));
            }
        });
        ui.add_space(8.0);
        ui.label(self.tr("settings.language"));
        egui::ComboBox::from_id_salt(if inline_page {
            "language-select-page"
        } else {
            "language-select-dialog"
        })
        .selected_text(self.language_name(&self.language_input))
        .show_ui(ui, |ui| {
            for (code, _) in I18n::supported_languages() {
                let label = self.language_name(code);
                ui.selectable_value(&mut self.language_input, (*code).to_owned(), label);
            }
        });
        ui.small(self.tr("settings.language.help"));
        ui.small(if self.app_paths.portable_mode {
            self.tr("settings.mode.portable")
        } else {
            self.tr("settings.mode.appdata")
        });

        ui.add_space(8.0);
        ui.label(self.tr("update.dialog_title"));
        let auto_check_label = self.tr("update.auto_check");
        ui.checkbox(&mut self.auto_update_input, auto_check_label);
        ui.small(self.tr("update.auto_check_hint"));

        self.ui_update_proxy_settings(ui, inline_page);
    }

    fn ui_settings_actions(&mut self, ui: &mut egui::Ui, inline_page: bool) {
        if let Some(error) = &self.settings_save_error {
            settings_error_scroll_area().show(ui, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(error).color(Color32::from_rgb(192, 57, 43)))
                        .wrap(),
                );
            });
        }
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(self.tr("settings.save")).clicked() {
                self.save_settings();
            }

            if !self.require_initial_setup && ui.button(self.tr("settings.cancel")).clicked() {
                self.reset_settings_inputs();
                if inline_page {
                    self.active_page = NavigationPage::Devices;
                } else {
                    self.show_settings = false;
                }
            }
        });
    }

    fn settings_footer_height(&self) -> f32 {
        if self.settings_save_error.is_some() {
            SETTINGS_FOOTER_ERROR_HEIGHT
        } else {
            SETTINGS_FOOTER_HEIGHT
        }
    }

    fn ui_update_proxy_settings(&mut self, ui: &mut egui::Ui, inline_page: bool) {
        ui.add_space(8.0);
        let pending_scroll = std::mem::take(&mut self.scroll_to_update_proxy_settings);
        let mut newly_revealed_content = false;
        ui.label(self.tr("update.proxy_title"));
        let mut proxy_mode_changed = false;
        let mut proxy_input_changed = false;
        let automatic_label = self.tr("update.proxy_automatic");
        let custom_label = self.tr("update.proxy_custom");
        egui::ComboBox::from_id_salt(if inline_page {
            "update-proxy-select-page"
        } else {
            "update-proxy-select-dialog"
        })
        .selected_text(match self.update_proxy_mode_input {
            UpdateProxyMode::Automatic => automatic_label.clone(),
            UpdateProxyMode::Custom => custom_label.clone(),
        })
        .show_ui(ui, |ui| {
            proxy_mode_changed |= ui
                .selectable_value(
                    &mut self.update_proxy_mode_input,
                    UpdateProxyMode::Automatic,
                    automatic_label,
                )
                .changed();
            proxy_mode_changed |= ui
                .selectable_value(
                    &mut self.update_proxy_mode_input,
                    UpdateProxyMode::Custom,
                    custom_label,
                )
                .changed();
        });

        if self.update_proxy_mode_input == UpdateProxyMode::Automatic {
            ui.small(self.tr("update.proxy_automatic_hint"));
        } else {
            ui.label(self.tr("update.proxy_url"));
            proxy_input_changed |= ui
                .text_edit_singleline(&mut self.update_proxy_url_input)
                .changed();
            ui.small(self.tr("update.proxy_url_hint"));
        }

        if proxy_mode_changed || proxy_input_changed {
            self.update_connection_test = UpdateConnectionTestPhase::Idle;
        }
        newly_revealed_content |= proxy_mode_changed;

        let is_testing = matches!(
            self.update_connection_test,
            UpdateConnectionTestPhase::Testing
        );
        let mut test_requested = false;
        if ui
            .add_enabled(
                !is_testing,
                egui::Button::new(self.tr(if is_testing {
                    "update.proxy_testing"
                } else {
                    "update.proxy_test"
                })),
            )
            .clicked()
        {
            self.request_update_connection_test();
            test_requested = true;
        }
        ui.small(self.tr("update.proxy_test_hint"));
        let mut test_target_changed = false;
        let custom_target_response = egui::CollapsingHeader::new(
            self.tr("update.proxy_test_custom_target"),
        )
        .show(ui, |ui| {
            ui.label(self.tr("update.proxy_test_url"));
            test_target_changed |= ui
                .text_edit_singleline(&mut self.update_proxy_test_url_input)
                .changed();
            ui.small(self.tr("update.proxy_test_url_hint"));
        });
        if custom_target_response.header_response.clicked() {
            // Keep requesting while the collapsing animation reveals the
            // target fields, then make its bottom edge visible.
            self.scroll_to_update_proxy_settings = true;
            newly_revealed_content = true;
        }
        if test_target_changed {
            self.update_connection_test = UpdateConnectionTestPhase::Idle;
        }

        match self.update_connection_test {
            UpdateConnectionTestPhase::Idle => {}
            UpdateConnectionTestPhase::Testing => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small(self.tr("update.proxy_testing"));
                });
            }
            UpdateConnectionTestPhase::Succeeded(result) => {
                ui.small(
                    RichText::new(self.tr_args(
                        "update.proxy_test_success",
                        &[
                            ("status", result.status_code.to_string()),
                            ("elapsed", result.elapsed_ms.to_string()),
                        ],
                    ))
                    .color(Color32::from_rgb(39, 145, 85)),
                );
            }
            UpdateConnectionTestPhase::Failed(error) => {
                let key = match error {
                    updater::UpdateConnectionTestError::InvalidTarget => {
                        "update.proxy_test_invalid_target"
                    }
                    updater::UpdateConnectionTestError::InvalidProxy => "update.proxy_test_invalid",
                    updater::UpdateConnectionTestError::RequestFailed => "update.proxy_test_failed",
                    updater::UpdateConnectionTestError::HttpStatus(_) => {
                        "update.proxy_test_http_status"
                    }
                };
                let message = match error {
                    updater::UpdateConnectionTestError::HttpStatus(status) => {
                        self.tr_args(key, &[("status", status.to_string())])
                    }
                    _ => self.tr(key),
                };
                ui.small(RichText::new(message).color(Color32::from_rgb(192, 57, 43)));
            }
        }

        if should_reveal_proxy_settings_content(
            pending_scroll,
            newly_revealed_content,
            test_requested,
        ) {
            // This is intentionally after all conditional fields and test
            // feedback have been laid out, so the enclosing ScrollArea uses
            // their final bounds instead of their previous height.
            ui.scroll_to_cursor(Some(Align::BOTTOM));
        }
    }

    fn ui_new_display_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_new_display_dialog {
            return;
        }
        let Some(device_id) = self.new_display_device_id.clone() else {
            self.show_new_display_dialog = false;
            return;
        };

        let mut open = true;
        let mut launch_clicked = false;
        let mut cancel_clicked = false;
        let device_label = self.device_identity_label(&device_id);
        let title = self.tr("scrcpy.new_display.title");
        let follow_device_label = self.tr("scrcpy.new_display.follow_device");
        let dimensions_label = self.tr("scrcpy.new_display.dimensions");
        let width_label = self.tr("scrcpy.new_display.width");
        let height_label = self.tr("scrcpy.new_display.height");
        let dpi_label = self.tr("scrcpy.new_display.dpi");
        let start_app_label = self.tr("scrcpy.new_display.start_app");
        let start_app_hint = self.tr("scrcpy.new_display.start_app_hint");
        let select_app_label = self.tr("scrcpy.new_display.select_app");
        let filter_apps_label = self.tr("scrcpy.new_display.filter_apps");
        let filter_apps_hint = self.tr("scrcpy.new_display.filter_apps_hint");
        let loading_apps_label = self.tr("scrcpy.new_display.loading_apps");
        let no_apps_label = self.tr("scrcpy.new_display.no_apps");
        let no_matching_apps_label = self.tr("scrcpy.new_display.no_matching_apps");
        let hint = self.tr("scrcpy.new_display.hint");
        let launch_label = self.tr("scrcpy.new_display.launch");
        let cancel_label = self.tr("settings.cancel");
        let installed_apps = self.new_display_apps.clone();
        let apps_error = self.new_display_apps_error.clone();

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(self.tr_args(
                    "scrcpy.new_display.device",
                    &[("device", device_label.clone())],
                ));
                ui.add_space(6.0);
                ui.checkbox(
                    &mut self.new_display_use_device_defaults,
                    follow_device_label,
                );
                if !self.new_display_use_device_defaults {
                    ui.add_space(6.0);
                    ui.label(dimensions_label);
                    ui.horizontal(|ui| {
                        ui.label(width_label);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_display_width_input)
                                .desired_width(72.0),
                        );
                        ui.label(height_label);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_display_height_input)
                                .desired_width(72.0),
                        );
                        ui.label(dpi_label);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_display_dpi_input)
                                .desired_width(64.0),
                        );
                    });
                }
                ui.add_space(6.0);
                ui.label(start_app_label);
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_display_start_app_input)
                        .hint_text("com.example.app")
                        .desired_width(300.0),
                );
                ui.small(start_app_hint);
                if self.new_display_apps_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(loading_apps_label);
                    });
                } else if let Some(error) = apps_error {
                    ui.small(RichText::new(error).color(Color32::from_rgb(192, 57, 43)));
                } else if installed_apps.is_empty() {
                    ui.small(no_apps_label);
                } else {
                    ui.add_space(6.0);
                    ui.label(filter_apps_label);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_display_app_filter_input)
                            .hint_text(filter_apps_hint)
                            .desired_width(300.0),
                    );
                    let filtered_apps = filter_installed_packages(
                        &installed_apps,
                        &self.new_display_app_filter_input,
                    );
                    if filtered_apps.is_empty() {
                        ui.small(no_matching_apps_label);
                    } else {
                        egui::ComboBox::from_id_salt("new-display-start-app")
                            .selected_text(if self.new_display_start_app_input.is_empty() {
                                select_app_label.clone()
                            } else {
                                self.new_display_start_app_input.clone()
                            })
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(180.0)
                                    .show(ui, |ui| {
                                        for package in filtered_apps {
                                            ui.selectable_value(
                                                &mut self.new_display_start_app_input,
                                                package.to_owned(),
                                                package,
                                            );
                                        }
                                    });
                            });
                    }
                }
                ui.add_space(6.0);
                ui.small(hint);
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    cancel_clicked = ui.button(cancel_label).clicked();
                    launch_clicked = ui.button(launch_label).clicked();
                });
            });

        if !open || cancel_clicked {
            self.show_new_display_dialog = false;
            self.new_display_device_id = None;
            return;
        }
        if launch_clicked {
            let options = if self.new_display_use_device_defaults {
                Ok(scrcpy::NewDisplayOptions::default())
            } else {
                scrcpy::NewDisplayOptions::custom(
                    &self.new_display_width_input,
                    &self.new_display_height_input,
                    &self.new_display_dpi_input,
                )
            }
            .and_then(|options| options.with_start_app(&self.new_display_start_app_input));
            match options {
                Ok(options) => self.start_scrcpy_new_display(device_id, options),
                Err(err) => self.set_error(err),
            }
        }
    }

    fn ui_update_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_dialog {
            return;
        }

        let mut open = true;
        let mut check_clicked = false;
        let mut later_clicked = false;
        let mut download_clicked = false;
        let mut apply_clicked = false;
        let mut open_notes_url: Option<String> = None;
        let mut open_update_network_settings = false;

        egui::Window::new(self.tr("update.dialog_title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            // egui windows default to 420 logical px tall and never
            // auto-shrink; start at the minimal content height instead and
            // let the frame expand for the download/apply states.
            .default_size(egui::vec2(430.0, 250.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(400.0);
                ui.label(self.tr_args(
                    "update.current_version",
                    &[("version", self.version.clone())],
                ));
                ui.add_space(6.0);

                if !updater::updates_configured() && !demo_update_active() {
                    ui.label(self.tr("update.not_configured"));
                } else {
                    if let Some(banner) = self.update_demo_banner_text() {
                        ui.label(RichText::new(banner).color(Color32::from_rgb(210, 120, 10)));
                        ui.add_space(4.0);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.small(self.tr_args(
                            "update.proxy_status",
                            &[("mode", self.update_proxy_status_label())],
                        ));
                        if matches!(self.update_phase, UpdatePhase::Failed(_))
                            && ui.button(self.tr("update.proxy_settings")).clicked()
                        {
                            open_update_network_settings = true;
                        }
                    });
                    ui.add_space(4.0);
                    match self.update_phase.clone() {
                        UpdatePhase::Checking => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(self.tr("update.checking"));
                            });
                        }
                        UpdatePhase::UpToDate => {
                            ui.label(self.tr("update.up_to_date"));
                        }
                        UpdatePhase::Failed(error) => {
                            ui.label(
                                RichText::new(
                                    self.tr_args("update.check_failed", &[("error", error)]),
                                )
                                .color(Color32::from_rgb(192, 57, 43)),
                            );
                        }
                        UpdatePhase::Idle => {}
                    }

                    if let Some(info) = &self.update_info {
                        ui.label(
                            RichText::new(
                                self.tr_args(
                                    "update.available",
                                    &[("version", info.version.clone())],
                                ),
                            )
                            .strong()
                            .color(Color32::from_rgb(210, 120, 10)),
                        );
                        if let Some(notes_url) = info.notes_url.clone()
                            && ui.button(self.tr("update.notes")).clicked()
                        {
                            open_notes_url = Some(notes_url);
                        }
                        ui.add_space(4.0);

                        let busy = self.update_downloading || self.update_applying;
                        if self.update_downloaded.is_some() {
                            ui.label(self.tr("update.downloaded"));
                        } else if self.update_downloading {
                            ui.label(self.tr("update.downloading"));
                            let fraction = self
                                .update_download_progress
                                .and_then(|(done, total)| {
                                    (total > 0).then_some(done as f32 / total as f32)
                                })
                                .unwrap_or(0.0);
                            ui.add(
                                egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).show_percentage(),
                            );
                        } else if !busy {
                            let has_candidate = self.update_candidate.is_some();
                            download_clicked = ui
                                .add_enabled(
                                    has_candidate,
                                    egui::Button::new(self.tr("update.download")),
                                )
                                .clicked();
                        }
                        if self.update_downloaded.is_some() {
                            apply_clicked = ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(
                                        RichText::new(self.tr("update.restart_apply")).strong(),
                                    ),
                                )
                                .clicked();
                        }
                    }

                    if matches!(self.update_phase, UpdatePhase::Idle)
                        && self.update_cache.error.is_some()
                    {
                        ui.small(self.tr("update.auto_check_failed_hint"));
                    }
                    if let Some(checked_at) = self.formatted_last_update_check() {
                        ui.small(self.tr_args("update.last_auto_check", &[("time", checked_at)]));
                    }
                }

                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    later_clicked = ui.button(self.tr("update.later")).clicked();
                    let checking = matches!(self.update_phase, UpdatePhase::Checking);
                    check_clicked = ui
                        .add_enabled(!checking, egui::Button::new(self.tr("update.check")))
                        .clicked();
                });
            });

        if check_clicked {
            self.request_update_check(false);
        }
        if download_clicked {
            self.request_update_download();
        }
        if apply_clicked {
            self.request_update_apply();
        }
        if let Some(notes_url) = open_notes_url
            && let Err(err) = fs_utils::open_url(&notes_url)
        {
            self.set_error(err);
        }
        if open_update_network_settings {
            self.show_update_dialog = false;
            self.active_page = NavigationPage::Settings;
            self.scroll_to_update_proxy_settings = true;
            return;
        }
        if later_clicked || !open {
            self.show_update_dialog = false;
            self.dismiss_update_notice();
        }
    }

    /// Banner text explaining that the update flow is locally simulated, or
    /// `None` when the demo is inactive (always in release builds without the
    /// `update-preview` feature).
    #[cfg(any(debug_assertions, feature = "update-preview"))]
    fn update_demo_banner_text(&self) -> Option<String> {
        updater::demo::requested().then(|| {
            self.tr_args(
                "update.demo_banner",
                &[("version", updater::demo::DEMO_VERSION.to_owned())],
            )
        })
    }

    #[cfg(not(any(debug_assertions, feature = "update-preview")))]
    fn update_demo_banner_text(&self) -> Option<String> {
        None
    }

    fn formatted_last_update_check(&self) -> Option<String> {
        let checked_at = self.update_cache.checked_at.as_deref()?;
        if self.update_cache.current_version.as_deref() != Some(self.version.as_str()) {
            return None;
        }
        chrono::DateTime::parse_from_rfc3339(checked_at)
            .ok()
            .map(|checked_at| {
                checked_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .or_else(|| Some(checked_at.to_owned()))
    }

    fn ui_clear_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_clear_confirm {
            return;
        }

        let directories = self.log_storage.device_directories.clone();
        let all_devices_label = self.tr("clear.all_devices");
        let all_history_label = self.tr("clear.time.all");
        let seven_days_label = self.tr("clear.time.7_days");
        let thirty_days_label = self.tr("clear.time.30_days");
        let ninety_days_label = self.tr("clear.time.90_days");
        let before_date_label = self.tr("clear.time.before_date");
        let mut refresh_preview = false;
        let mut cancel = false;
        let mut delete = false;
        egui::Window::new(self.tr("clear.title"))
            .collapsible(false)
            .resizable(false)
            .default_size([560.0, 420.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label(self.tr("clear.body"));
                ui.add_space(10.0);
                ui.label(RichText::new(self.tr("clear.device_scope")).strong());
                if ui
                    .add_enabled(
                        !self.cleanup_in_progress,
                        egui::Checkbox::new(&mut self.cleanup_all_devices, all_devices_label),
                    )
                    .changed()
                {
                    refresh_preview = true;
                }
                if !self.cleanup_all_devices {
                    if directories.is_empty() {
                        ui.small(self.tr("clear.no_devices"));
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(110.0)
                            .show(ui, |ui| {
                                for usage in &directories {
                                    let label = self.log_directory_label(&usage.directory_name);
                                    let mut selected = self
                                        .cleanup_selected_directories
                                        .contains(&usage.directory_name);
                                    if ui
                                        .add_enabled(
                                            !self.cleanup_in_progress,
                                            egui::Checkbox::new(&mut selected, label),
                                        )
                                        .changed()
                                    {
                                        if selected {
                                            self.cleanup_selected_directories
                                                .insert(usage.directory_name.clone());
                                        } else {
                                            self.cleanup_selected_directories
                                                .remove(&usage.directory_name);
                                        }
                                        refresh_preview = true;
                                    }
                                }
                            });
                    }
                }
                ui.add_space(8.0);
                ui.label(RichText::new(self.tr("clear.time_scope")).strong());
                let previous_filter = self.cleanup_time_filter;
                ui.add_enabled_ui(!self.cleanup_in_progress, |ui| {
                    egui::ComboBox::from_id_salt("cleanup-time-filter")
                        .selected_text(self.cleanup_time_filter_label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.cleanup_time_filter,
                                CleanupTimeFilter::AllHistory,
                                all_history_label,
                            );
                            ui.selectable_value(
                                &mut self.cleanup_time_filter,
                                CleanupTimeFilter::OlderThan7Days,
                                seven_days_label,
                            );
                            ui.selectable_value(
                                &mut self.cleanup_time_filter,
                                CleanupTimeFilter::OlderThan30Days,
                                thirty_days_label,
                            );
                            ui.selectable_value(
                                &mut self.cleanup_time_filter,
                                CleanupTimeFilter::OlderThan90Days,
                                ninety_days_label,
                            );
                            ui.selectable_value(
                                &mut self.cleanup_time_filter,
                                CleanupTimeFilter::BeforeDate,
                                before_date_label,
                            );
                        });
                    if self.cleanup_time_filter == CleanupTimeFilter::BeforeDate
                        && ui
                            .add(
                                egui::TextEdit::singleline(&mut self.cleanup_before_date_input)
                                    .hint_text("YYYY-MM-DD"),
                            )
                            .changed()
                    {
                        refresh_preview = true;
                    }
                });
                if previous_filter != self.cleanup_time_filter {
                    refresh_preview = true;
                }
                if self.cleanup_time_filter == CleanupTimeFilter::BeforeDate
                    && self.cleanup_cutoff().is_none()
                {
                    ui.colored_label(
                        Color32::from_rgb(220, 71, 71),
                        self.tr("clear.invalid_date"),
                    );
                }

                // Start the replacement request before drawing the preview
                // and action buttons, so stale preview data cannot remain
                // deletable for the frame in which the filter changes.
                if refresh_preview {
                    self.request_cleanup_preview();
                    refresh_preview = false;
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new(self.tr("clear.preview_title")).strong());
                if self.cleanup_preview_loading {
                    ui.small(self.tr("clear.preview_loading"));
                } else if let Some(preview) = &self.cleanup_preview {
                    ui.label(self.tr_args(
                        "clear.preview_delete",
                        &[
                            ("files", preview.matching_files.to_string()),
                            ("size", fs_utils::format_bytes(preview.matching_bytes)),
                        ],
                    ));
                    ui.small(self.tr_args(
                        "clear.preview_keep",
                        &[
                            ("files", preview.protected_files.to_string()),
                            ("size", fs_utils::format_bytes(preview.protected_bytes)),
                        ],
                    ));
                } else {
                    ui.small(self.tr("clear.preview_unavailable"));
                }
                ui.small(self.tr("clear.app_log_preserved"));
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let can_delete = !self.cleanup_in_progress
                        && !self.cleanup_preview_loading
                        && self.cleanup_preview_filter.is_some()
                        && self
                            .cleanup_preview
                            .as_ref()
                            .is_some_and(|preview| preview.matching_files > 0);
                    let delete_label = self.cleanup_preview.as_ref().map_or_else(
                        || self.tr("clear.delete"),
                        |preview| {
                            self.tr_args(
                                "clear.delete_count",
                                &[("files", preview.matching_files.to_string())],
                            )
                        },
                    );
                    if ui
                        .add_enabled(can_delete, egui::Button::new(delete_label))
                        .clicked()
                    {
                        delete = true;
                    }
                    if ui
                        .add_enabled(
                            !self.cleanup_in_progress,
                            egui::Button::new(self.tr("clear.cancel")),
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });

        if cancel {
            self.show_clear_confirm = false;
        } else if delete && let Some(filter) = self.cleanup_preview_filter.clone() {
            self.clear_history_logs(filter);
        }
    }

    fn ui_alias_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_alias_dialog {
            return;
        }

        let Some(serial) = self.alias_input_serial.clone() else {
            self.show_alias_dialog = false;
            return;
        };
        let mut save = false;
        let mut clear = false;
        let mut cancel = false;
        egui::Window::new(self.tr("alias.title"))
            .collapsible(false)
            .resizable(false)
            .fixed_size([440.0, 170.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(self.tr_args(
                    "alias.target",
                    &[("serial", self.device_identity_label(&serial))],
                ));
                ui.add_space(10.0);
                ui.label(self.tr("alias.input"));
                ui.text_edit_singleline(&mut self.alias_input_value);
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(self.tr("settings.cancel")).clicked() {
                        cancel = true;
                    }
                    if ui.button(self.tr("device.action.clear_alias")).clicked() {
                        clear = true;
                    }
                    if ui.button(self.tr("device.action.save_alias")).clicked() {
                        save = true;
                    }
                });
            });

        if cancel {
            self.show_alias_dialog = false;
            self.alias_input_serial = None;
            self.alias_input_value.clear();
        } else if clear {
            self.show_alias_dialog = false;
            self.save_device_alias(&serial, String::new());
            self.alias_input_serial = None;
        } else if save {
            self.show_alias_dialog = false;
            self.save_device_alias(&serial, self.alias_input_value.clone());
            self.alias_input_serial = None;
        }
    }

    fn ui_logcat_args_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_logcat_args_dialog {
            return;
        }

        let Some(serial) = self.logcat_args_input_serial.clone() else {
            self.show_logcat_args_dialog = false;
            return;
        };
        let mut save = false;
        let mut clear = false;
        let mut cancel = false;
        egui::Window::new(self.tr("logcat_args.title"))
            .collapsible(false)
            .resizable(false)
            .fixed_size([520.0, 200.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(self.tr_args(
                    "logcat_args.target",
                    &[("serial", self.device_identity_label(&serial))],
                ));
                ui.add_space(10.0);
                ui.label(self.tr("logcat_args.input"));
                ui.text_edit_singleline(&mut self.logcat_args_input_value);
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(self.tr("logcat_args.hint"))
                        .small()
                        .color(ui.style().visuals.noninteractive().fg_stroke.color),
                );
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(self.tr("settings.cancel")).clicked() {
                        cancel = true;
                    }
                    if ui.button(self.tr("logcat_args.clear")).clicked() {
                        clear = true;
                    }
                    if ui.button(self.tr("logcat_args.save")).clicked() {
                        save = true;
                    }
                });
            });

        if cancel {
            self.show_logcat_args_dialog = false;
            self.logcat_args_input_serial = None;
            self.logcat_args_input_value.clear();
        } else if clear {
            self.show_logcat_args_dialog = false;
            self.save_device_logcat_args(&serial, String::new());
            self.logcat_args_input_serial = None;
        } else if save {
            self.show_logcat_args_dialog = false;
            self.save_device_logcat_args(&serial, self.logcat_args_input_value.clone());
            self.logcat_args_input_serial = None;
        }
    }

    fn ui_foreground_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_foreground_confirm.clone() else {
            return;
        };

        let action_text = self.foreground_action_text(pending.action);
        let warning_text = self.foreground_action_warning_text(pending.action);
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new(self.tr_args(
            "foreground.confirm.title",
            &[("action", action_text.clone())],
        ))
        .collapsible(false)
        .resizable(false)
        .fixed_size([460.0, 210.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(self.tr_args(
                "foreground.confirm.body",
                &[
                    ("action", action_text.clone()),
                    ("app", self.foreground_app_label(&pending.app)),
                ],
            ));
            ui.add_space(10.0);
            ui.label(self.tr_args(
                "foreground.confirm.device",
                &[("serial", self.device_identity_label(&pending.serial))],
            ));
            ui.label(self.tr_args(
                "foreground.confirm.package",
                &[("package", pending.app.package_name.clone())],
            ));
            if let Some(activity) = pending.app.activity_name.clone() {
                ui.label(self.tr_args("foreground.confirm.activity", &[("activity", activity)]));
            }
            ui.add_space(10.0);
            ui.colored_label(Color32::from_rgb(200, 90, 50), warning_text);
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let confirm_label = self.tr_args(
                    "foreground.confirm.confirm",
                    &[("action", action_text.clone())],
                );
                if ui.button(confirm_label).clicked() {
                    confirm = true;
                }
                if ui.button(self.tr("foreground.confirm.cancel")).clicked() {
                    cancel = true;
                }
            });
        });

        if cancel {
            self.pending_foreground_confirm = None;
        } else if confirm {
            self.pending_foreground_confirm = None;
            self.start_foreground_app_execution(pending.serial, pending.action, pending.app);
        }
    }

    fn ui_connect_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_connect_dialog {
            return;
        }

        let mut connect_target = None;
        egui::Window::new(self.tr("connect.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(self.tr("connect.intro"));
                ui.add_space(8.0);

                let connect_button_text = if self.connect_in_progress {
                    self.tr("connect.connecting")
                } else {
                    self.tr("connect.action")
                };
                let can_connect =
                    !self.connect_in_progress && !self.connect_target_input.trim().is_empty();

                ui.horizontal(|ui| {
                    let response = ui.text_edit_singleline(&mut self.connect_target_input);
                    let pressed_enter = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui
                        .add_enabled(can_connect, egui::Button::new(connect_button_text.clone()))
                        .clicked()
                        || (pressed_enter && can_connect)
                    {
                        connect_target = Some(self.connect_target_input.trim().to_owned());
                    }
                });

                if !self.config.recent_connections.is_empty() {
                    ui.add_space(8.0);
                    ui.label(self.tr("connect.recent"));
                    ui.horizontal_wrapped(|ui| {
                        for target in self.config.recent_connections.clone() {
                            if ui
                                .add_enabled(
                                    !self.connect_in_progress,
                                    egui::Button::new(target.as_str()),
                                )
                                .clicked()
                            {
                                connect_target = Some(target.clone());
                            }
                        }
                    });
                }

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.connect_in_progress,
                            egui::Button::new(self.tr("connect.cancel")),
                        )
                        .clicked()
                    {
                        self.show_connect_dialog = false;
                    }
                });
            });

        if let Some(target) = connect_target {
            self.start_device_connection(target);
        }
    }

    fn ui_drop_target_dialog(&mut self, ctx: &egui::Context) {
        let Some(payload) = self.pending_drop_payload.clone() else {
            return;
        };

        let ready_devices = self.ready_device_ids();
        if ready_devices.is_empty() {
            self.pending_drop_payload = None;
            self.pending_drop_target_serial = None;
            return;
        }

        if self
            .pending_drop_target_serial
            .as_deref()
            .map(|serial| !ready_devices.iter().any(|candidate| candidate == serial))
            .unwrap_or(true)
        {
            self.pending_drop_target_serial = ready_devices.first().cloned();
        }

        let mut start_target = None;
        let mut cancel = false;
        egui::Window::new(self.tr("drop.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(self.tr("drop.intro"));
                ui.add_space(8.0);
                ui.label(format!(
                    "{}: {}",
                    self.tr("drop.summary.apks"),
                    payload.apk_paths.len()
                ));
                ui.label(format!(
                    "{}: {}",
                    self.tr("drop.summary.files"),
                    payload.file_paths.len()
                ));
                ui.add_space(8.0);
                ui.label(self.tr("drop.target"));
                egui::ComboBox::from_id_salt("drop-target-device")
                    .selected_text(
                        self.pending_drop_target_serial
                            .as_deref()
                            .map(|serial| self.device_identity_label(serial))
                            .unwrap_or_default(),
                    )
                    .show_ui(ui, |ui| {
                        for serial in &ready_devices {
                            let label = self.device_identity_label(serial);
                            ui.selectable_value(
                                &mut self.pending_drop_target_serial,
                                Some(serial.clone()),
                                label,
                            );
                        }
                    });

                if let Some(serial) = self.pending_drop_target_serial.as_deref() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(self.tr_args(
                            "drop.confirm_target",
                            &[("device", self.device_identity_label(serial))],
                        ))
                        .strong(),
                    );
                }

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(self.tr("drop.cancel")).clicked() {
                        cancel = true;
                    }
                    if ui.button(self.tr("drop.start")).clicked() {
                        start_target = self.pending_drop_target_serial.clone();
                    }
                });
            });

        if cancel {
            self.pending_drop_payload = None;
            self.pending_drop_target_serial = None;
        }
        if let Some(serial) = start_target {
            let payload = self.pending_drop_payload.take();
            self.pending_drop_target_serial = None;
            if let Some(payload) = payload {
                self.start_drop_task(serial, payload);
            }
        }
    }

    fn ui_drag_overlay(&self, ctx: &egui::Context) {
        if self.require_initial_setup {
            return;
        }

        let hovered_count = ctx.input(|input| {
            input
                .raw
                .hovered_files
                .iter()
                .filter(|file| file.path.is_some())
                .count()
        });
        if hovered_count == 0 {
            return;
        }

        let ready_devices = self.ready_device_ids();
        let hover_target = if ready_devices.len() == 1 {
            ready_devices.first().cloned()
        } else {
            self.selected_serial
                .clone()
                .filter(|serial| ready_devices.iter().any(|candidate| candidate == serial))
        };

        egui::Area::new("drop_overlay".into())
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(self.tr("drop.hover_title"))
                                .strong()
                                .color(Color32::from_rgb(31, 37, 49)),
                        );
                        if let Some(serial) = hover_target.as_deref() {
                            ui.small(
                                RichText::new(self.tr_args(
                                    "drop.hover_target",
                                    &[("device", self.device_identity_label(serial))],
                                ))
                                .strong()
                                .color(Color32::from_rgb(29, 92, 196)),
                            );
                        }
                        ui.small(self.tr("drop.hover_hint"));
                        ui.small(
                            self.tr_args(
                                "drop.hover_count",
                                &[("count", hovered_count.to_string())],
                            ),
                        );
                    });
                });
            });
    }

    fn save_settings(&mut self) {
        self.settings_save_error = None;
        let candidate = AppConfig {
            adb_path: self.adb_path_input.trim().to_owned(),
            scrcpy_path: self.scrcpy_path_input.trim().to_owned(),
            log_dir: self.log_dir_input.trim().to_owned(),
            app_log_max_size_mb: self.config.app_log_max_size_mb,
            language: self.language_input.clone(),
            device_aliases: self.config.device_aliases.clone(),
            pinned_devices: self.config.pinned_devices.clone(),
            recent_connections: self.config.recent_connections.clone(),
            device_logcat_args: self.config.device_logcat_args.clone(),
            auto_check_updates: self.auto_update_input,
            update_proxy: self.update_proxy_input(),
        };

        if candidate.adb_path.is_empty() || candidate.log_dir.is_empty() {
            self.set_settings_save_error(self.tr("status.required_fields"));
            return;
        }

        if let Err(err) = adb::validate_adb_path(candidate.adb_path.as_str()) {
            self.set_settings_save_error(err);
            return;
        }
        if !candidate.scrcpy_path.is_empty()
            && let Err(err) = scrcpy::validate_scrcpy_path(candidate.scrcpy_path.as_str())
        {
            self.set_settings_save_error(err);
            return;
        }
        if let Err(error) = updater::validate_update_proxy(&candidate.update_proxy) {
            self.set_settings_save_error(self.update_proxy_validation_message(error));
            return;
        }

        let log_dir = PathBuf::from(candidate.log_dir.as_str());
        let resolved_log_dir = match config::ensure_log_dir(&log_dir) {
            Ok(path) => path,
            Err(err) => {
                self.set_settings_save_error(err);
                return;
            }
        };

        let saved = AppConfig {
            adb_path: fs_utils::display_path_string(&candidate.adb_path),
            scrcpy_path: fs_utils::display_path_string(&candidate.scrcpy_path),
            log_dir: fs_utils::display_path(&resolved_log_dir),
            app_log_max_size_mb: candidate.app_log_max_size_mb,
            language: self.language_input.clone(),
            device_aliases: candidate.device_aliases.clone(),
            pinned_devices: candidate.pinned_devices.clone(),
            recent_connections: candidate.recent_connections.clone(),
            device_logcat_args: candidate.device_logcat_args.clone(),
            auto_check_updates: candidate.auto_check_updates,
            update_proxy: candidate.update_proxy.clone(),
        };

        if let Err(err) = config::save_config(&self.app_paths.config_path, &saved) {
            self.set_settings_save_error(err);
            return;
        }

        self.config = saved.clone();
        self.adb_path_input = saved.adb_path.clone();
        self.scrcpy_path_input = saved.scrcpy_path.clone();
        self.log_dir_input = saved.log_dir.clone();
        self.language_input = saved.language.clone();
        self.update_proxy_mode_input = saved.update_proxy.mode;
        self.update_proxy_url_input = saved.update_proxy.url.clone();
        self.update_connection_test = UpdateConnectionTestPhase::Idle;
        self.i18n.set_language(&saved.language);
        self.show_settings = false;
        self.require_initial_setup = false;
        self.last_device_snapshot.clear();
        self.last_device_poll_at = None;
        self.last_auto_poll_error = None;
        self.set_info(self.tr("status.settings_saved"));
        self.refresh_devices();
        self.refresh_log_size();
    }

    fn reset_settings_inputs(&mut self) {
        self.adb_path_input = self.config.adb_path.clone();
        self.scrcpy_path_input = self.config.scrcpy_path.clone();
        self.log_dir_input = self.config.log_dir.clone();
        self.language_input = self.config.language.clone();
        self.auto_update_input = self.config.auto_check_updates;
        self.update_proxy_mode_input = self.config.update_proxy.mode;
        self.update_proxy_url_input = self.config.update_proxy.url.clone();
        self.update_connection_test = UpdateConnectionTestPhase::Idle;
        self.settings_save_error = None;
    }

    fn set_settings_save_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.set_error(text.clone());
        self.settings_save_error = Some(text);
    }

    fn update_proxy_input(&self) -> UpdateProxyConfig {
        UpdateProxyConfig {
            mode: self.update_proxy_mode_input,
            url: if self.update_proxy_mode_input == UpdateProxyMode::Custom {
                self.update_proxy_url_input.trim().to_owned()
            } else {
                String::new()
            },
        }
    }

    fn update_proxy_validation_message(
        &self,
        error: updater::UpdateProxyValidationError,
    ) -> String {
        self.tr(match error {
            updater::UpdateProxyValidationError::MissingUrl => "update.proxy_error_missing_url",
            updater::UpdateProxyValidationError::UnsupportedScheme => {
                "update.proxy_error_unsupported_scheme"
            }
            updater::UpdateProxyValidationError::MissingHostOrPort => {
                "update.proxy_error_missing_host_or_port"
            }
            updater::UpdateProxyValidationError::AuthenticationNotSupported => {
                "update.proxy_error_authentication"
            }
            updater::UpdateProxyValidationError::InvalidUrl => "update.proxy_error_invalid_url",
        })
    }

    fn update_proxy_status_label(&self) -> String {
        self.tr(match self.config.update_proxy.mode {
            UpdateProxyMode::Automatic => "update.proxy_status_automatic",
            UpdateProxyMode::Custom => "update.proxy_status_custom",
        })
    }

    fn request_update_connection_test(&mut self) {
        if matches!(
            self.update_connection_test,
            UpdateConnectionTestPhase::Testing
        ) {
            return;
        }

        let proxy = self.update_proxy_input();
        if updater::validate_update_proxy(&proxy).is_err() {
            self.update_connection_test =
                UpdateConnectionTestPhase::Failed(updater::UpdateConnectionTestError::InvalidProxy);
            self.scroll_to_update_proxy_settings = true;
            return;
        }
        let target_url = self.update_proxy_test_url_input.trim().to_owned();
        let target_url = if target_url.is_empty() {
            updater::DEFAULT_PROXY_TEST_URL.to_owned()
        } else {
            target_url
        };

        self.update_connection_test = UpdateConnectionTestPhase::Testing;
        self.scroll_to_update_proxy_settings = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(AppEvent::UpdateConnectionTestFinished(
                updater::test_update_connection(&proxy, &target_url),
            ));
        });
    }

    fn refresh_devices(&mut self) {
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        self.last_device_poll_at = Some(Instant::now());

        thread::spawn(move || {
            let result = adb::list_devices(&adb_path);
            let _ = tx.send(AppEvent::DevicesRefreshed(result));
        });
    }

    fn poll_devices(&mut self) {
        if self.require_initial_setup || self.device_poll_in_flight {
            return;
        }

        self.device_poll_in_flight = true;
        self.last_device_poll_at = Some(Instant::now());
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();

        thread::spawn(move || {
            let result = adb::list_devices(&adb_path);
            let _ = tx.send(AppEvent::DevicesPolled(result));
        });
    }

    fn poll_devices_if_due(&mut self) {
        if self.require_initial_setup || self.show_settings {
            return;
        }

        let should_poll = self
            .last_device_poll_at
            .map(|instant| instant.elapsed() >= DEVICE_POLL_INTERVAL)
            .unwrap_or(true);
        if should_poll {
            self.poll_devices();
        }
    }

    fn refresh_log_size(&mut self) {
        self.log_storage_loading = true;
        let tx = self.tx.clone();
        let log_dir = self.config.log_dir.clone();

        thread::spawn(move || {
            let result = fs_utils::scan_log_storage(PathBuf::from(log_dir).as_path());
            let _ = tx.send(AppEvent::LogStorageRefreshed(result));
        });
    }

    fn update_status_cache_path(&self) -> PathBuf {
        updater::status_cache_path(&self.app_paths.config_dir)
    }

    fn persist_update_cache(&mut self) {
        let path = self.update_status_cache_path();
        if let Err(err) = updater::write_status_cache(&path, &self.update_cache) {
            log::warn!("Failed to persist application update status: {err}");
        }
    }

    /// Starts a signed-manifest update check on a background thread. Automatic
    /// checks additionally mark today as done so a failing network does not
    /// retry on every focus event.
    fn request_update_check(&mut self, automatic: bool) {
        #[cfg(any(debug_assertions, feature = "update-preview"))]
        if updater::demo::requested() {
            if matches!(self.update_phase, UpdatePhase::Checking) {
                return;
            }
            log::info!("Starting demo application update check");
            self.update_phase = UpdatePhase::Checking;
            let tx = self.tx.clone();
            thread::spawn(move || {
                thread::sleep(updater::demo::DEMO_DELAY);
                let _ = tx.send(AppEvent::UpdateCheckFinished {
                    automatic,
                    result: Ok(Some(updater::demo::candidate())),
                });
            });
            return;
        }

        if !updater::updates_configured() {
            if automatic {
                self.update_cache.record_automatic_skipped(&self.version);
                self.persist_update_cache();
            }
            return;
        }
        if matches!(self.update_phase, UpdatePhase::Checking) {
            return;
        }

        self.update_phase = UpdatePhase::Checking;
        let config = match updater::update_config(&self.version, &self.config.update_proxy) {
            Ok(Some(config)) => config,
            Ok(None) => return,
            Err(error) => {
                self.handle_update_check_finished(
                    automatic,
                    Err(self.update_proxy_validation_message(error)),
                );
                return;
            }
        };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = desktop_updater::check(&config)
                .map(|result| match result {
                    CheckResult::UpToDate => None,
                    CheckResult::UpdateAvailable(candidate) => Some(candidate),
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::UpdateCheckFinished { automatic, result });
        });
    }

    /// Runs the daily automatic check when the window gains focus. A hidden
    /// window should not generate update traffic until the user returns.
    fn maybe_automatic_update_check(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|input| input.viewport().focused).unwrap_or(true);
        let gained_focus = match self.viewport_focused {
            None => focused,
            Some(previous) => focused && !previous,
        };
        self.viewport_focused = Some(focused);

        if !gained_focus || self.require_initial_setup || self.show_update_dialog {
            return;
        }

        #[cfg(any(debug_assertions, feature = "update-preview"))]
        if updater::demo::requested() {
            // Demo previews bypass the hour/config gate and never repeat once
            // a candidate is showing.
            if self.update_info.is_none() && !matches!(self.update_phase, UpdatePhase::Checking) {
                log::info!("Window focused with demo update preview enabled");
                self.request_update_check(true);
            }
            return;
        }

        if updater::automatic_check_is_due(
            self.config.auto_check_updates,
            updater::local_hour(),
            &updater::today_local(),
            self.update_cache.last_automatic_check_date.as_deref(),
        ) {
            log::info!("Starting daily automatic application update check");
            self.request_update_check(true);
        }
    }

    fn handle_update_check_finished(
        &mut self,
        automatic: bool,
        result: Result<Option<UpdateCandidate>, String>,
    ) {
        match result {
            Ok(candidate) => {
                // Demo previews stay in-memory so they never pollute the real
                // status cache or the once-per-day automatic gate.
                if !demo_update_active() {
                    self.update_cache
                        .record_check(&self.version, candidate.as_ref(), automatic);
                    self.persist_update_cache();
                }
                match candidate {
                    Some(candidate) => {
                        log::info!(
                            "Application update available: {} -> {}",
                            self.version,
                            candidate.version()
                        );
                        // A download may only target the candidate the user
                        // was shown; drop stale packages from older versions.
                        if self
                            .update_downloaded
                            .as_ref()
                            .map(|downloaded| downloaded.candidate.version() != candidate.version())
                            != Some(false)
                        {
                            self.update_downloaded = None;
                        }
                        self.update_info = Some(UpdateInfo {
                            version: candidate.version().to_owned(),
                            notes_url: candidate.notes_url().map(str::to_owned),
                        });
                        self.update_candidate = Some(candidate);
                        self.update_dismissed = false;
                        self.update_phase = UpdatePhase::Idle;
                        if automatic {
                            self.show_update_dialog = true;
                        }
                    }
                    None => {
                        log::info!("Application is up to date at {}", self.version);
                        self.update_info = None;
                        self.update_candidate = None;
                        self.update_downloaded = None;
                        self.update_phase = UpdatePhase::UpToDate;
                    }
                }
            }
            Err(err) => {
                log::warn!("Application update check failed: {err}");
                self.update_phase = UpdatePhase::Failed(err.clone());
                if !demo_update_active() {
                    self.update_cache
                        .record_failure(&self.version, err.clone(), automatic);
                    self.persist_update_cache();
                }
                if !automatic {
                    self.set_error(self.tr_args("update.check_failed", &[("error", err)]));
                }
            }
        }
    }

    /// Downloads the currently offered candidate into the updates directory.
    /// The archive hash and size are verified by desktop-updater before the
    /// result is accepted.
    fn request_update_download(&mut self) {
        if self.update_downloading || self.update_applying {
            return;
        }
        let Some(candidate) = self.update_candidate.clone() else {
            return;
        };

        #[cfg(any(debug_assertions, feature = "update-preview"))]
        if updater::demo::requested() {
            self.update_downloading = true;
            self.update_download_progress = None;
            let updates_dir = updater::updates_dir(&self.app_paths.config_dir);
            let tx = self.tx.clone();
            thread::spawn(move || {
                thread::sleep(updater::demo::DEMO_DELAY);
                let result = updater::install_dir_from_current_exe()
                    .and_then(|install_dir| {
                        updater::demo::build_package(&install_dir, &updates_dir)
                    })
                    .map(|package_path| DownloadedUpdate {
                        candidate,
                        package_path,
                    });
                let _ = tx.send(AppEvent::UpdateDownloadFinished(result));
            });
            return;
        }

        let config = match updater::update_config(&self.version, &self.config.update_proxy) {
            Ok(Some(config)) => config,
            Ok(None) => return,
            Err(error) => {
                let error = self.update_proxy_validation_message(error);
                self.set_error(self.tr_args("update.download_failed", &[("error", error)]));
                return;
            }
        };
        self.update_downloading = true;
        self.update_download_progress = None;
        let updates_dir = updater::updates_dir(&self.app_paths.config_dir);
        let (progress_tx, progress_rx) = mpsc::channel();
        self.update_progress_rx = Some(progress_rx);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result =
                desktop_updater::download(&config, candidate, &updates_dir, |done, total| {
                    let _ = progress_tx.send((done, total));
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::UpdateDownloadFinished(result));
        });
    }

    /// Hands the downloaded package to the update helper and exits. The
    /// helper waits for this process, replaces the allow-listed files, and
    /// restarts LogcatX with an acknowledgement request.
    fn request_update_apply(&mut self) {
        if self.update_applying || self.update_downloading {
            return;
        }
        let Some(downloaded) = self.update_downloaded.clone() else {
            return;
        };

        self.update_applying = true;
        let install_dir = match updater::install_dir_from_current_exe() {
            Ok(install_dir) => install_dir,
            Err(err) => {
                self.update_applying = false;
                self.set_error(self.tr_args("update.apply_failed", &[("error", err)]));
                return;
            }
        };
        let request = updater::apply_request(&install_dir);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = desktop_updater::apply_and_restart(&downloaded, &request)
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::UpdateApplyStarted(result));
        });
    }

    /// Silences the notice for the currently offered version; the sidebar
    /// indicator and manual checks keep working.
    fn dismiss_update_notice(&mut self) {
        if self.update_info.is_some() && !self.update_dismissed {
            self.update_cache.dismiss_available();
            if !demo_update_active() {
                self.persist_update_cache();
            }
            self.update_dismissed = true;
        }
    }

    fn open_cleanup_dialog(&mut self) {
        self.cleanup_all_devices = true;
        self.cleanup_selected_directories.clear();
        self.cleanup_time_filter = CleanupTimeFilter::AllHistory;
        self.cleanup_before_date_input = Local::now().format("%Y-%m-%d").to_string();
        self.cleanup_preview = None;
        self.cleanup_preview_filter = None;
        self.cleanup_preview_loading = false;
        self.show_clear_confirm = true;
        self.request_cleanup_preview();
    }

    fn cleanup_filter(&self) -> Option<fs_utils::CleanupFilter> {
        let device_directories = (!self.cleanup_all_devices).then(|| {
            let mut selected = self
                .cleanup_selected_directories
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            selected.sort();
            selected
        });
        let older_than = match self.cleanup_time_filter {
            CleanupTimeFilter::AllHistory => None,
            CleanupTimeFilter::OlderThan7Days => {
                Some((Local::now() - chrono::Duration::days(7)).into())
            }
            CleanupTimeFilter::OlderThan30Days => {
                Some((Local::now() - chrono::Duration::days(30)).into())
            }
            CleanupTimeFilter::OlderThan90Days => {
                Some((Local::now() - chrono::Duration::days(90)).into())
            }
            CleanupTimeFilter::BeforeDate => Some(self.cleanup_cutoff()?),
        };
        Some(fs_utils::CleanupFilter {
            device_directories,
            older_than,
        })
    }

    fn cleanup_cutoff(&self) -> Option<std::time::SystemTime> {
        let date =
            chrono::NaiveDate::parse_from_str(self.cleanup_before_date_input.trim(), "%Y-%m-%d")
                .ok()?;
        let date_time = date.and_hms_opt(0, 0, 0)?;
        Local
            .from_local_datetime(&date_time)
            .single()
            .map(Into::into)
    }

    fn protected_log_paths(&self) -> Vec<PathBuf> {
        let mut protected_paths: Vec<PathBuf> = self
            .devices
            .iter()
            .filter(|device| device.is_active())
            .filter_map(|device| device.output_path.clone())
            .collect();
        protected_paths.push(self.app_paths.app_log_path.clone());
        protected_paths
    }

    fn request_cleanup_preview(&mut self) {
        if self.cleanup_in_progress {
            return;
        }
        self.cleanup_preview_generation = self.cleanup_preview_generation.wrapping_add(1);
        let request_id = self.cleanup_preview_generation;
        let Some(filter) = self.cleanup_filter() else {
            self.cleanup_preview = None;
            self.cleanup_preview_filter = None;
            self.cleanup_preview_loading = false;
            return;
        };

        self.cleanup_preview = None;
        self.cleanup_preview_filter = Some(filter.clone());
        self.cleanup_preview_loading = true;
        let tx = self.tx.clone();
        let log_dir = PathBuf::from(self.config.log_dir.as_str());
        let protected_paths = self.protected_log_paths();
        let event_filter = filter.clone();
        thread::spawn(move || {
            let result = fs_utils::preview_log_cleanup(&log_dir, &filter, &protected_paths);
            let _ = tx.send(AppEvent::CleanupPreviewed {
                request_id,
                filter: event_filter,
                result,
            });
        });
    }

    fn clear_history_logs(&mut self, filter: fs_utils::CleanupFilter) {
        if self.cleanup_in_progress {
            return;
        }
        let tx = self.tx.clone();
        let log_dir = PathBuf::from(self.config.log_dir.as_str());
        let protected_paths = self.protected_log_paths();

        self.cleanup_in_progress = true;
        self.set_info(self.tr("status.clearing_history"));
        thread::spawn(move || {
            let result = fs_utils::cleanup_matching_logs(&log_dir, &filter, &protected_paths);
            let _ = tx.send(AppEvent::CleanupFinished(result));
        });
    }

    fn start_device_connection(&mut self, target: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        if self.connect_in_progress {
            return;
        }

        let trimmed = target.trim().to_owned();
        if trimmed.is_empty() {
            self.set_error(self.tr("status.connect_target_required"));
            return;
        }

        self.connect_in_progress = true;
        self.connect_target_input = trimmed.clone();
        self.set_info(self.tr_args("status.connecting_device", &[("target", trimmed.clone())]));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = adb::connect_device(&adb_path, &trimmed);
            let _ = tx.send(AppEvent::DeviceConnectFinished {
                target: trimmed,
                result,
            });
        });
    }

    fn start_disconnect(&mut self, serial: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        if self.disconnecting_serial.is_some() || !adb::is_network_device_serial(&serial) {
            return;
        }

        let device_name = self.device_identity_label(&serial);
        self.disconnecting_serial = Some(serial.clone());
        self.set_info(self.tr_args("status.device_disconnecting", &[("serial", device_name)]));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = adb::disconnect_device(&adb_path, &serial);
            let _ = tx.send(AppEvent::DeviceDisconnectFinished { serial, result });
        });
    }

    fn start_adb_server_restart(&mut self) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        if self.restarting_adb_server {
            return;
        }

        self.restarting_adb_server = true;
        self.set_info(self.tr("status.adb_server_restarting"));
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = adb::restart_server(&adb_path);
            let _ = tx.send(AppEvent::AdbServerRestartFinished(result));
        });
    }

    fn copy_serial_to_clipboard(&mut self, ctx: &egui::Context, serial: String) {
        let copied = self
            .device_primary_transport_serial(&serial)
            .unwrap_or_else(|| serial.clone());
        ctx.copy_text(copied);
        self.set_info(self.tr_args(
            "status.serial_copied",
            &[("serial", self.device_identity_label(&serial))],
        ));
    }

    fn copy_log_path_to_clipboard(&mut self, ctx: &egui::Context, device_id: &str) {
        let device_name = self.device_identity_label(device_id);
        let Some(device) = self.find_device(device_id) else {
            return;
        };
        if let Some(path) = &device.output_path {
            let display = fs_utils::display_path(path);
            ctx.copy_text(display);
            self.set_info(self.tr_args("status.log_path_copied", &[("serial", device_name)]));
        } else {
            self.set_info(
                self.tr_args("status.log_path_not_available", &[("serial", device_name)]),
            );
        }
    }

    fn start_screenshot(&mut self, device_id: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        if self.screenshot_in_progress {
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
        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            return;
        };

        self.screenshot_in_progress = true;
        self.set_info(self.tr_args(
            "status.screenshot_capturing",
            &[("serial", self.device_identity_label(&device_id))],
        ));
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = adb::capture_screenshot(&adb_path, &transport_serial);
            let _ = tx.send(AppEvent::ScreenshotFinished {
                serial: device_id,
                result,
            });
        });
    }

    fn copy_screenshot_to_clipboard(&self, screenshot: Screenshot) -> Result<(), String> {
        use std::borrow::Cow;

        let mut clipboard = arboard::Clipboard::new()
            .map_err(|err| format!("Failed to access the system clipboard: {err}"))?;
        clipboard
            .set_image(arboard::ImageData {
                width: screenshot.width,
                height: screenshot.height,
                bytes: Cow::Owned(screenshot.rgba_pixels),
            })
            .map_err(|err| format!("Failed to copy the screenshot to the system clipboard: {err}"))
    }

    fn start_scrcpy_mirror(&mut self, device_id: String) {
        let Some(transport_serial) = self.ready_scrcpy_transport(&device_id) else {
            return;
        };
        if self.configured_scrcpy_version().is_none() {
            return;
        }

        let result = scrcpy::launch(
            &self.config.scrcpy_path,
            &self.config.adb_path,
            &transport_serial,
            scrcpy::LaunchMode::Mirror,
        );
        match result {
            Ok(()) => self.set_info(self.tr_args(
                "status.scrcpy_mirror_opened",
                &[("serial", self.device_identity_label(&device_id))],
            )),
            Err(err) => self.set_error(err),
        }
    }

    fn open_new_display_dialog(&mut self, device_id: String) {
        let Some(transport_serial) = self.ready_scrcpy_transport(&device_id) else {
            return;
        };
        let Some(version) = self.configured_scrcpy_version() else {
            return;
        };
        if !version.supports_new_display() {
            self.set_error(self.tr_args(
                "status.scrcpy_new_display_requires_v3",
                &[("version", version.to_string())],
            ));
            return;
        }

        self.new_display_device_id = Some(device_id.clone());
        self.show_new_display_dialog = true;
        self.new_display_start_app_input.clear();
        self.new_display_app_filter_input.clear();
        self.new_display_apps.clear();
        self.new_display_apps_error = None;
        self.new_display_apps_loading = true;
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = adb::list_installed_packages(&adb_path, &transport_serial);
            let _ = tx.send(AppEvent::ScrcpyAppsLoaded { device_id, result });
        });
    }

    fn start_scrcpy_new_display(&mut self, device_id: String, options: scrcpy::NewDisplayOptions) {
        let Some(transport_serial) = self.ready_scrcpy_transport(&device_id) else {
            return;
        };
        let Some(version) = self.configured_scrcpy_version() else {
            return;
        };
        if !version.supports_new_display() {
            self.set_error(self.tr_args(
                "status.scrcpy_new_display_requires_v3",
                &[("version", version.to_string())],
            ));
            return;
        }

        let result = scrcpy::launch(
            &self.config.scrcpy_path,
            &self.config.adb_path,
            &transport_serial,
            scrcpy::LaunchMode::NewDisplay(options),
        );
        match result {
            Ok(()) => {
                self.show_new_display_dialog = false;
                self.new_display_device_id = None;
                self.set_info(self.tr_args(
                    "status.scrcpy_new_display_opened",
                    &[("serial", self.device_identity_label(&device_id))],
                ));
            }
            Err(err) => self.set_error(err),
        }
    }

    fn ready_scrcpy_transport(&mut self, device_id: &str) -> Option<String> {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return None;
        }

        let device = self.find_device(device_id).cloned()?;
        if device.info.state != "device" {
            self.set_error(self.tr_args(
                "status.device_invalid_state",
                &[
                    ("serial", self.device_identity_label(device_id)),
                    ("state", self.device_state_text(&device.info.state)),
                ],
            ));
            return None;
        }
        self.device_primary_transport_serial(device_id)
    }

    fn configured_scrcpy_version(&mut self) -> Option<scrcpy::ScrcpyVersion> {
        if self.config.scrcpy_path.trim().is_empty() {
            self.set_error(self.tr("status.scrcpy_not_configured"));
            self.active_page = NavigationPage::Settings;
            return None;
        }

        match scrcpy::validate_scrcpy_path(&self.config.scrcpy_path) {
            Ok(version) => Some(version),
            Err(err) => {
                self.set_error(err);
                self.active_page = NavigationPage::Settings;
                None
            }
        }
    }

    fn open_device_shell(&mut self, device_id: String) {
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

        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            return;
        };
        match fs_utils::open_device_shell(&self.config.adb_path, &transport_serial) {
            Ok(message) => self.set_info(self.tr_args(
                "status.device_shell_opened",
                &[
                    ("serial", self.device_identity_label(&device_id)),
                    ("message", message),
                ],
            )),
            Err(err) => self.set_error(err),
        }
    }

    fn start_foreground_app_action(&mut self, device_id: String, action: ForegroundAppAction) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }
        if self.foreground_task_in_progress {
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

        self.foreground_task_in_progress = true;
        self.set_info(self.tr_args(
            "status.foreground_app_resolving",
            &[("serial", self.device_identity_label(&device_id))],
        ));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            self.foreground_task_in_progress = false;
            return;
        };
        thread::spawn(move || {
            let result = adb::query_foreground_app(&adb_path, &transport_serial);
            let _ = tx.send(AppEvent::ForegroundAppResolved {
                serial: device_id,
                action,
                result,
            });
        });
    }

    fn start_foreground_app_execution(
        &mut self,
        device_id: String,
        action: ForegroundAppAction,
        app: ForegroundApp,
    ) {
        if self.foreground_task_in_progress {
            return;
        }

        self.foreground_task_in_progress = true;
        self.set_info(self.tr_args(
            "status.foreground_app_action_started",
            &[
                ("serial", self.device_identity_label(&device_id)),
                ("action", self.foreground_action_text(action)),
                ("app", self.foreground_app_label(&app)),
            ],
        ));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let app_for_thread = app.clone();
        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            self.foreground_task_in_progress = false;
            return;
        };
        thread::spawn(move || {
            let result = match action {
                ForegroundAppAction::ForceStop => adb::force_stop_package(
                    &adb_path,
                    &transport_serial,
                    &app_for_thread.package_name,
                ),
                ForegroundAppAction::ClearData => adb::clear_package_data(
                    &adb_path,
                    &transport_serial,
                    &app_for_thread.package_name,
                ),
                ForegroundAppAction::Uninstall => adb::uninstall_package(
                    &adb_path,
                    &transport_serial,
                    &app_for_thread.package_name,
                ),
                ForegroundAppAction::Inspect => Ok(app_for_thread.package_name.clone()),
            };
            let _ = tx.send(AppEvent::ForegroundAppActionFinished {
                serial: device_id,
                action,
                app: app_for_thread,
                result,
            });
        });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if self.require_initial_setup {
            return;
        }

        let dropped_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        if dropped_paths.is_empty() {
            return;
        }
        if self.drop_task_in_progress || self.pending_drop_payload.is_some() {
            self.set_error(self.tr("status.drop_busy"));
            return;
        }

        let payload = classify_dropped_paths(dropped_paths);
        if payload.is_empty() {
            self.set_error(self.tr("status.drop_no_supported_files"));
            return;
        }

        let ready_devices = self.ready_device_ids();
        if ready_devices.is_empty() {
            self.set_error(self.tr("status.drop_no_ready_device"));
            return;
        }

        if let Some(selected_serial) = self.selected_serial.clone()
            && ready_devices
                .iter()
                .any(|serial| serial == &selected_serial)
        {
            self.start_drop_task(selected_serial, payload);
            return;
        }

        self.pending_drop_target_serial = ready_devices.first().cloned();
        self.pending_drop_payload = Some(payload);
    }

    fn start_drop_task(&mut self, device_id: String, payload: DroppedPayload) {
        if payload.is_empty() || self.drop_task_in_progress {
            return;
        }

        self.drop_task_in_progress = true;
        self.set_info(self.tr_args(
            "status.drop_processing",
            &[
                ("count", payload.total_count().to_string()),
                ("serial", self.device_identity_label(&device_id)),
            ],
        ));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            self.drop_task_in_progress = false;
            return;
        };
        thread::spawn(move || {
            let result = process_dropped_payload(&adb_path, &transport_serial, payload);
            let _ = tx.send(AppEvent::DeviceDropFinished {
                serial: device_id,
                result,
            });
        });
    }

    fn start_collection(&mut self, device_id: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }

        let device_name = self.device_identity_label(&device_id);

        if let Some(device) = self.find_device(&device_id)
            && device.info.state != "device"
        {
            self.set_error(self.tr_args(
                "status.device_invalid_state",
                &[
                    ("serial", device_name.clone()),
                    ("state", self.device_state_text(&device.info.state)),
                ],
            ));
            return;
        }

        let Some(transport_serial) = self.device_primary_transport_serial(&device_id) else {
            return;
        };
        let config_key = self.device_identity_key(&device_id);
        let output_path = fs_utils::session_log_path(
            PathBuf::from(self.config.log_dir.as_str()).as_path(),
            &config_key,
            self.device_alias(&device_id).as_deref(),
        );
        if let Some(device) = self.find_device_mut(&device_id) {
            if device.is_active() {
                self.set_info(self.tr_args(
                    "status.device_already_collecting",
                    &[("serial", device_name.clone())],
                ));
                return;
            }
            device.run_state = DeviceRunState::Starting;
            device.output_path = Some(output_path.clone());
        }

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let device_id_for_thread = device_id.clone();
        let logcat_args = self
            .config
            .device_logcat_args
            .get(&config_key)
            .map(|s| adb::parse_logcat_args(s))
            .unwrap_or_default();

        thread::spawn(move || {
            let child_holder: SharedChild = std::sync::Arc::new(std::sync::Mutex::new(None));

            match adb::spawn_logcat(&adb_path, &transport_serial, &output_path, &logcat_args) {
                Ok(child) => {
                    if let Ok(mut guard) = child_holder.lock() {
                        *guard = Some(child);
                    } else {
                        let _ = tx.send(AppEvent::CollectionEnded {
                            serial: device_id_for_thread,
                            exit_code: None,
                            error: Some("Failed to store collector process handle.".to_owned()),
                        });
                        return;
                    }

                    let _ = tx.send(AppEvent::CollectionSpawned {
                        serial: device_id_for_thread.clone(),
                        output_path: output_path.clone(),
                        child: child_holder.clone(),
                    });

                    let (exit_code, error) = wait_for_process_exit(&child_holder);
                    let _ = tx.send(AppEvent::CollectionEnded {
                        serial: device_id_for_thread,
                        exit_code,
                        error,
                    });
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::CollectionEnded {
                        serial: device_id_for_thread,
                        exit_code: None,
                        error: Some(err),
                    });
                }
            }
        });
    }

    fn stop_collection(&mut self, device_id: &str) {
        let Some(index) = self
            .devices
            .iter()
            .position(|device| device.matches_serial(device_id))
        else {
            return;
        };

        let Some(child) = self.devices[index].child.clone() else {
            self.devices[index].run_state = DeviceRunState::Idle;
            return;
        };

        self.devices[index].run_state = DeviceRunState::Stopping;
        let device_name = self.device_identity_label(device_id);
        match child.lock() {
            Ok(mut guard) => {
                if let Some(process) = guard.as_mut() {
                    if let Err(err) = process.kill() {
                        let message = self.tr_args(
                            "status.stop_failed",
                            &[("serial", device_name.clone()), ("error", err.to_string())],
                        );
                        self.devices[index].run_state = DeviceRunState::Error(message.clone());
                        self.set_error(message);
                    } else {
                        self.set_info(self.tr_args(
                            "status.stopping_collection",
                            &[("serial", device_name.clone())],
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
                        ("serial", device_name),
                        ("error", "internal lock error".to_owned()),
                    ],
                ));
            }
        }
    }

    fn stop_all_collections_for_shutdown(&mut self) {
        for device in &mut self.devices {
            let device_label = device.info.serial.clone();
            if let Some(child) = device.child.take() {
                Self::stop_child_for_shutdown(&child, &device_label);
            }
            if device.is_active() {
                device.run_state = DeviceRunState::Idle;
            }
            device.started_at = None;
        }
    }

    fn stop_child_for_shutdown(child: &SharedChild, device_label: &str) {
        match child.lock() {
            Ok(mut guard) => {
                if let Some(process) = guard.as_mut()
                    && let Err(err) = process.kill()
                {
                    log::warn!(
                        "Failed to stop collector process for {device_label} during shutdown: {err}"
                    );
                }
                *guard = None;
            }
            Err(_) => {
                log::warn!(
                    "Collector process lock for {device_label} was poisoned during shutdown"
                );
            }
        }
    }

    fn merge_devices(&mut self, devices: Vec<DeviceInfo>) {
        let previous_selected_identity = self
            .selected_serial
            .as_deref()
            .and_then(|serial| self.find_device(serial))
            .map(|device| device.info.identity_key.clone());
        let mut existing: HashMap<String, DeviceEntry> = self
            .devices
            .drain(..)
            .map(|device| (device.info.identity_key.clone(), device))
            .collect();

        let mut grouped: HashMap<String, Vec<DeviceInfo>> = HashMap::new();
        for info in devices {
            grouped
                .entry(info.identity_key.clone())
                .or_default()
                .push(info);
        }

        let mut merged = Vec::with_capacity(grouped.len() + existing.len());
        for (identity_key, infos) in grouped {
            let primary = pick_primary_device_info(&infos);
            let transport_serials = infos
                .iter()
                .map(|info| info.serial.clone())
                .collect::<Vec<_>>();

            if let Some(mut current) = existing.remove(&identity_key) {
                current.info.serial = primary.serial;
                current.info.identity_key = primary.identity_key;
                current.info.state = primary.state;
                if primary.android_version.is_some() {
                    current.info.android_version = primary.android_version;
                }
                if primary.manufacturer.is_some() {
                    current.info.manufacturer = primary.manufacturer;
                }
                if primary.model.is_some() {
                    current.info.model = primary.model;
                }
                current.transport_serials = transport_serials;
                merged.push(current);
            } else {
                let mut entry = DeviceEntry::new(primary);
                entry.transport_serials = transport_serials;
                merged.push(entry);
            }
        }

        for (_, mut device) in existing {
            if device.is_active() {
                device.info.state = "disconnected".to_owned();
                merged.push(device);
            }
        }

        self.devices = merged;
        self.sort_devices();
        let selected_exists = self
            .selected_serial
            .as_deref()
            .map(|serial| self.find_device(serial).is_some())
            .unwrap_or(false);
        if !selected_exists {
            self.selected_serial = previous_selected_identity
                .as_deref()
                .and_then(|identity| self.find_device(identity))
                .map(|device| device.info.identity_key.clone());
        }
        if self.selected_serial.is_none() {
            self.selected_serial = self
                .devices
                .first()
                .map(|device| device.info.identity_key.clone());
        }
        if self.selected_serial.is_none() {
            self.alias_input_serial = None;
            self.alias_input_value.clear();
        }
    }

    fn cleanup_time_filter_label(&self) -> String {
        match self.cleanup_time_filter {
            CleanupTimeFilter::AllHistory => self.tr("clear.time.all"),
            CleanupTimeFilter::OlderThan7Days => self.tr("clear.time.7_days"),
            CleanupTimeFilter::OlderThan30Days => self.tr("clear.time.30_days"),
            CleanupTimeFilter::OlderThan90Days => self.tr("clear.time.90_days"),
            CleanupTimeFilter::BeforeDate => self.tr("clear.time.before_date"),
        }
    }

    fn log_directory_label(&self, directory_name: &str) -> String {
        if directory_name.is_empty() {
            return self.tr("log_files.root_files");
        }

        for (identity_key, alias) in &self.config.device_aliases {
            if fs_utils::sanitize_serial(alias) == directory_name {
                return format!("{alias} ({identity_key})");
            }
        }
        for device in &self.devices {
            if fs_utils::sanitize_serial(&device.info.identity_key) == directory_name {
                return self.device_identity_label(&device.info.identity_key);
            }
        }
        for identity_key in self.config.device_aliases.keys() {
            if fs_utils::sanitize_serial(identity_key) == directory_name {
                return identity_key.clone();
            }
        }
        directory_name.to_owned()
    }

    fn format_log_time(&self, time: Option<std::time::SystemTime>) -> String {
        time.map(|time| {
            chrono::DateTime::<Local>::from(time)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| self.tr("misc.never"))
    }

    fn persist_config(&mut self) -> Result<(), String> {
        config::save_config(&self.app_paths.config_path, &self.config)?;
        self.config = config::load_config(&self.app_paths.config_path, &self.app_paths)?;
        Ok(())
    }

    fn open_alias_editor(&mut self, serial: &str) {
        let identity_key = self.device_identity_key(serial);
        self.alias_input_serial = Some(identity_key.clone());
        self.alias_input_value = self.device_alias(&identity_key).unwrap_or_default();
        self.show_alias_dialog = true;
    }

    fn open_logcat_args_editor(&mut self, serial: &str) {
        let identity_key = self.device_identity_key(serial);
        self.logcat_args_input_serial = Some(identity_key.clone());
        self.logcat_args_input_value = self
            .config
            .device_logcat_args
            .get(&identity_key)
            .cloned()
            .unwrap_or_default();
        self.show_logcat_args_dialog = true;
    }

    fn device_alias(&self, serial: &str) -> Option<String> {
        let identity_key = self.device_identity_key(serial);
        self.config.device_aliases.get(&identity_key).cloned()
    }

    fn device_primary_name(&self, serial: &str) -> String {
        self.device_alias(serial)
            .or_else(|| {
                self.find_device(serial)
                    .and_then(|device| device_display_name(&device.info))
            })
            .unwrap_or_else(|| serial.to_owned())
    }

    fn device_identity_label(&self, serial: &str) -> String {
        let primary_serial = self
            .device_primary_transport_serial(serial)
            .unwrap_or_else(|| serial.to_owned());
        if let Some(alias) = self.device_alias(serial)
            && alias != primary_serial
        {
            return format!("{alias} ({primary_serial})");
        }

        match self
            .find_device(serial)
            .and_then(|device| device_display_name(&device.info))
        {
            Some(name) if name != primary_serial => format!("{name} ({primary_serial})"),
            _ => primary_serial,
        }
    }

    fn is_pinned_device(&self, serial: &str) -> bool {
        let identity_key = self.device_identity_key(serial);
        self.config
            .pinned_devices
            .iter()
            .any(|value| value == &identity_key)
    }

    fn ready_device_ids(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|device| device.info.state == "device")
            .map(|device| device.info.identity_key.clone())
            .collect()
    }

    fn device_identity_key(&self, serial_or_identity: &str) -> String {
        self.find_device(serial_or_identity)
            .map(|device| device.info.identity_key.clone())
            .unwrap_or_else(|| serial_or_identity.to_owned())
    }

    fn device_primary_transport_serial(&self, serial_or_identity: &str) -> Option<String> {
        self.find_device(serial_or_identity)
            .map(|device| device.info.serial.clone())
    }

    fn device_network_transport_serial(&self, serial_or_identity: &str) -> Option<String> {
        self.find_device(serial_or_identity).and_then(|device| {
            device
                .transport_serials
                .iter()
                .find(|serial| adb::is_network_device_serial(serial))
                .cloned()
        })
    }

    fn foreground_action_text(&self, action: ForegroundAppAction) -> String {
        match action {
            ForegroundAppAction::Inspect => self.tr("foreground.action.inspect"),
            ForegroundAppAction::ForceStop => self.tr("foreground.action.force_stop"),
            ForegroundAppAction::ClearData => self.tr("foreground.action.clear_data"),
            ForegroundAppAction::Uninstall => self.tr("foreground.action.uninstall"),
        }
    }

    fn foreground_action_warning_text(&self, action: ForegroundAppAction) -> String {
        match action {
            ForegroundAppAction::ForceStop => self.tr("foreground.confirm.warning.force_stop"),
            ForegroundAppAction::ClearData => self.tr("foreground.confirm.warning.clear_data"),
            ForegroundAppAction::Uninstall => self.tr("foreground.confirm.warning.uninstall"),
            ForegroundAppAction::Inspect => String::new(),
        }
    }

    fn foreground_app_label(&self, app: &ForegroundApp) -> String {
        match app.activity_name.as_deref() {
            Some(activity) => format!("{} ({activity})", app.package_name),
            None => app.package_name.clone(),
        }
    }

    fn remember_recent_connection(&mut self, target: &str) -> Result<(), String> {
        let previous = self.config.recent_connections.clone();
        self.config
            .recent_connections
            .retain(|value| value != target);
        self.config.recent_connections.insert(0, target.to_owned());
        if let Err(err) = self.persist_config() {
            self.config.recent_connections = previous;
            return Err(err);
        }
        Ok(())
    }

    fn toggle_pinned_device(&mut self, serial: &str) {
        let identity_key = self.device_identity_key(serial);
        let was_pinned = self.is_pinned_device(&identity_key);
        let previous = self.config.pinned_devices.clone();

        if was_pinned {
            self.config
                .pinned_devices
                .retain(|value| value != &identity_key);
        } else {
            self.config.pinned_devices.push(identity_key);
        }

        if let Err(err) = self.persist_config() {
            self.config.pinned_devices = previous;
            self.set_error(err);
            return;
        }

        self.sort_devices();
        let status_key = if was_pinned {
            "status.device_unpinned"
        } else {
            "status.device_pinned"
        };
        self.set_info(self.tr_args(
            status_key,
            &[("serial", self.device_identity_label(serial))],
        ));
    }

    fn save_device_alias(&mut self, serial: &str, alias: String) {
        let identity_key = self.device_identity_key(serial);
        let next_alias = alias.trim().to_owned();
        let previous_alias = self.device_alias(&identity_key);
        let previous_alias_text = previous_alias.clone().unwrap_or_default();

        if previous_alias.as_deref() == Some(next_alias.as_str())
            || (next_alias.is_empty() && previous_alias.is_none())
        {
            self.alias_input_value = next_alias;
            return;
        }

        if self
            .find_device(&identity_key)
            .map(|device| device.is_active())
            .unwrap_or(false)
        {
            self.set_error(self.tr_args(
                "status.alias_change_requires_idle",
                &[("serial", self.device_identity_label(&identity_key))],
            ));
            self.alias_input_value = previous_alias_text;
            return;
        }

        let base_dir = PathBuf::from(self.config.log_dir.as_str());
        let old_dir = fs_utils::device_log_dir(&base_dir, &identity_key, previous_alias.as_deref());
        let renamed_dir = match fs_utils::rename_device_log_dir(
            &base_dir,
            &identity_key,
            previous_alias.as_deref(),
            (!next_alias.is_empty()).then_some(next_alias.as_str()),
        ) {
            Ok(path) => path,
            Err(err) => {
                self.alias_input_value = previous_alias_text;
                self.set_error(err);
                return;
            }
        };

        let previous_aliases = self.config.device_aliases.clone();
        if next_alias.is_empty() {
            self.config.device_aliases.remove(&identity_key);
        } else {
            self.config
                .device_aliases
                .insert(identity_key.clone(), next_alias.clone());
        }

        if let Err(err) = self.persist_config() {
            self.config.device_aliases = previous_aliases;
            if let Some(new_dir) = &renamed_dir
                && &old_dir != new_dir
                && new_dir.exists()
            {
                let _ = std::fs::rename(new_dir, &old_dir);
            }
            self.alias_input_value = previous_alias_text;
            self.set_error(err);
            return;
        }

        if let Some(new_dir) = renamed_dir.as_deref()
            && old_dir != new_dir
        {
            self.update_device_output_dir(&identity_key, &old_dir, new_dir);
        }

        self.sort_devices();
        self.alias_input_value = self.device_alias(&identity_key).unwrap_or_default();
        let status_key = if next_alias.is_empty() {
            "status.alias_cleared"
        } else {
            "status.alias_saved"
        };
        self.set_info(self.tr_args(
            status_key,
            &[("serial", self.device_identity_label(&identity_key))],
        ));
    }

    fn save_device_logcat_args(&mut self, serial: &str, args: String) {
        let identity_key = self.device_identity_key(serial);
        let trimmed = args.trim().to_owned();
        let previous = self
            .config
            .device_logcat_args
            .get(&identity_key)
            .cloned()
            .unwrap_or_default();

        if trimmed == previous {
            return;
        }

        let previous_args = self.config.device_logcat_args.clone();
        if trimmed.is_empty() {
            self.config.device_logcat_args.remove(&identity_key);
        } else {
            self.config
                .device_logcat_args
                .insert(identity_key.clone(), trimmed.clone());
        }

        if let Err(err) = self.persist_config() {
            self.config.device_logcat_args = previous_args;
            self.set_error(err);
            return;
        }

        let status_key = if trimmed.is_empty() {
            "status.logcat_args_cleared"
        } else {
            "status.logcat_args_saved"
        };
        self.set_info(self.tr_args(
            status_key,
            &[("serial", self.device_identity_label(&identity_key))],
        ));
    }

    fn update_device_output_dir(&mut self, serial: &str, old_dir: &Path, new_dir: &Path) {
        if let Some(device) = self.find_device_mut(serial)
            && let Some(current_path) = device.output_path.clone()
            && current_path.parent() == Some(old_dir)
            && let Some(file_name) = current_path.file_name()
        {
            device.output_path = Some(new_dir.join(file_name));
        }
    }

    fn sort_devices(&mut self) {
        let aliases = self.config.device_aliases.clone();
        let pinned = self.config.pinned_devices.clone();
        self.devices.sort_by(|left, right| {
            let left_pinned = pinned.iter().any(|value| value == &left.info.identity_key);
            let right_pinned = pinned.iter().any(|value| value == &right.info.identity_key);
            let left_name = aliases
                .get(&left.info.identity_key)
                .cloned()
                .or_else(|| device_display_name(&left.info))
                .unwrap_or_else(|| left.info.serial.clone());
            let right_name = aliases
                .get(&right.info.identity_key)
                .cloned()
                .or_else(|| device_display_name(&right.info))
                .unwrap_or_else(|| right.info.serial.clone());
            right_pinned
                .cmp(&left_pinned)
                .then_with(|| {
                    left_name
                        .to_ascii_lowercase()
                        .cmp(&right_name.to_ascii_lowercase())
                })
                .then_with(|| left.info.serial.cmp(&right.info.serial))
        });
    }

    fn device_state_key(&self, raw_state: &str) -> &'static str {
        match raw_state {
            "device" => "device.state.ready",
            "offline" => "device.state.offline",
            "unauthorized" => "device.state.unauthorized",
            "disconnected" => "device.state.disconnected",
            _ => "device.state.unknown",
        }
    }

    fn device_state_text(&self, raw_state: &str) -> String {
        self.tr(self.device_state_key(raw_state))
    }

    fn device_state_color(&self, raw_state: &str) -> Color32 {
        match raw_state {
            "device" => Color32::from_rgb(70, 160, 90),
            "offline" => Color32::from_rgb(210, 150, 50),
            "unauthorized" => Color32::from_rgb(200, 90, 50),
            "disconnected" => Color32::from_rgb(140, 140, 140),
            _ => Color32::from_rgb(120, 120, 120),
        }
    }

    fn adb_download_url(&self) -> &'static str {
        if self.language_input == "zh-CN" {
            PLATFORM_TOOLS_URL_ZH_CN
        } else {
            PLATFORM_TOOLS_URL_EN
        }
    }

    fn find_device(&self, serial: &str) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|device| device.matches_serial(serial))
    }

    fn find_device_mut(&mut self, serial: &str) -> Option<&mut DeviceEntry> {
        self.devices
            .iter_mut()
            .find(|device| device.matches_serial(serial))
    }

    fn set_info(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("{text}");
        let msg = StatusMessage::info(text);
        self.status_log.push(msg.clone());
        self.status = Some(msg);
    }

    fn set_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::error!("{text}");
        let error = StatusMessage::error(text);
        self.status_log.push(error.clone());
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

    fn stat_card(
        &self,
        ui: &mut egui::Ui,
        title: &str,
        value: String,
        chip_fill: Color32,
        chip_color: Color32,
    ) {
        egui::Frame::new()
            .fill(Color32::from_rgb(255, 255, 255))
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(231, 235, 243)))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(chip_fill)
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::symmetric(12, 14))
                        .show(ui, |ui| {
                            ui.label(RichText::new("●").size(12.0).color(chip_color));
                        });
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(title)
                                .size(12.5)
                                .color(Color32::from_rgb(122, 128, 140)),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(value)
                                .size(16.0)
                                .strong()
                                .color(Color32::from_rgb(54, 60, 72)),
                        );
                    });
                });
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

fn apply_visual_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = Color32::from_rgb(250, 248, 244);
    style.visuals.window_fill = Color32::from_rgb(255, 255, 255);
    style.visuals.window_stroke = egui::Stroke::new(1.0, Color32::from_rgb(232, 236, 243));
    style.visuals.window_corner_radius = egui::CornerRadius::same(18);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(12);
    style.visuals.override_text_color = Some(Color32::from_rgb(55, 61, 72));
    style.visuals.selection.bg_fill = Color32::from_rgb(240, 245, 255);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::from_rgb(91, 138, 255));
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(255, 255, 255);
    style.visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(231, 235, 243));
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(249, 250, 252);
    style.visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(225, 230, 240));
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(240, 245, 255);
    style.visuals.widgets.active.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(214, 225, 249));
    style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(255, 255, 255);
    style.visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(231, 235, 243));
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(18.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(13.5, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.0, egui::FontFamily::Proportional),
    );
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.scroll.floating = false;
    style.spacing.scroll.foreground_color = true;
    style.spacing.scroll.bar_inner_margin = 14.0;
    style.spacing.scroll.bar_outer_margin = 4.0;
    // 滚动条 handle 颜色（foreground_color=true 时取 widgets.fg_stroke.color）
    let scrollbar_handle_color = Color32::from_rgb(212, 216, 227);
    style.visuals.widgets.inactive.fg_stroke.color = scrollbar_handle_color;
    style.visuals.widgets.hovered.fg_stroke.color = scrollbar_handle_color;
    style.visuals.widgets.active.fg_stroke.color = scrollbar_handle_color;
    ctx.set_style(style);
}

fn content_card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::from_rgb(255, 255, 255))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(229, 233, 241)))
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::symmetric(18, 18))
}

fn button_label(icon: &str, text: String) -> String {
    format!("{icon}  {text}")
}

fn content_view_width(available_width: f32, right_gutter: f32) -> f32 {
    (available_width - right_gutter).max(0.0)
}

fn settings_error_scroll_area() -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .id_salt("settings-save-error")
        .max_height(SETTINGS_FOOTER_ERROR_VIEWPORT_HEIGHT)
        .min_scrolled_height(0.0)
}

fn is_current_cleanup_preview_response(current_generation: u64, request_id: u64) -> bool {
    current_generation == request_id
}

fn settings_path_input_width(available_width: f32) -> f32 {
    (available_width * 0.5).clamp(160.0, 420.0)
}

fn should_reveal_proxy_settings_content(
    pending_scroll: bool,
    newly_revealed_content: bool,
    test_requested: bool,
) -> bool {
    pending_scroll || newly_revealed_content || test_requested
}
fn detail_label(ui: &mut egui::Ui, label: &str) {
    ui.label(RichText::new(label).color(Color32::from_rgb(122, 128, 142)));
}

fn device_display_name(info: &DeviceInfo) -> Option<String> {
    format_device_model_name(info.manufacturer.as_deref(), info.model.as_deref())
}

fn format_device_model_name(manufacturer: Option<&str>, model: Option<&str>) -> Option<String> {
    let manufacturer = manufacturer
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let model = model.map(str::trim).filter(|value| !value.is_empty());

    match (manufacturer, model) {
        (Some(manufacturer), Some(model))
            if manufacturer.eq_ignore_ascii_case(model)
                || model
                    .to_ascii_lowercase()
                    .starts_with(&manufacturer.to_ascii_lowercase()) =>
        {
            Some(model.to_owned())
        }
        (Some(manufacturer), Some(model)) => Some(format!("{manufacturer} {model}")),
        (Some(manufacturer), None) => Some(manufacturer.to_owned()),
        (None, Some(model)) => Some(model.to_owned()),
        (None, None) => None,
    }
}

fn pick_primary_device_info(infos: &[DeviceInfo]) -> DeviceInfo {
    infos
        .iter()
        .min_by_key(|info| device_transport_rank(info))
        .cloned()
        .unwrap_or_else(|| infos[0].clone())
}

fn device_transport_rank(info: &DeviceInfo) -> (u8, u8, String) {
    let state_rank = if info.state == "device" { 0 } else { 1 };
    let transport_rank = if adb::is_network_device_serial(&info.serial) {
        1
    } else {
        0
    };
    (state_rank, transport_rank, info.serial.clone())
}

/// Painter-based badge that is guaranteed to be centered within `cell_rect`.
fn draw_state_badge_centered(ui: &mut egui::Ui, cell_rect: egui::Rect, text: &str, color: Color32) {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(text.to_owned(), font_id, color);
    let text_size = galley.rect.size();
    // 8px horizontal inner margin + 4px vertical inner margin (matching draw_state_badge)
    let badge_size = text_size + egui::vec2(16.0, 8.0);
    let badge_rect = egui::Rect::from_center_size(cell_rect.center(), badge_size);
    painter.rect(
        badge_rect,
        egui::CornerRadius::same(8),
        color.gamma_multiply(0.18),
        egui::Stroke::new(1.0, color.gamma_multiply(0.45)),
        egui::epaint::StrokeKind::Middle,
    );
    painter.galley(badge_rect.min + egui::vec2(8.0, 4.0), galley, color);
}

impl eframe::App for AdbCollectorApp {
    /// eframe's default is a semi-transparent dark color, so any region egui
    /// momentarily leaves unpainted (for example a panel seam during a
    /// DPI/scale transition) composites as a black strip on the desktop.
    /// Clearing with the app background makes such transient seams invisible.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::from_rgb(250, 248, 244).to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ime_enter_guard.frame(ctx);
        apply_visual_style(ctx);
        self.handle_events();
        self.poll_update_download_progress();
        if self.exit_after_update_apply {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.poll_devices_if_due();
        self.handle_dropped_files(ctx);
        self.maybe_automatic_update_check(ctx);

        egui::SidePanel::left("sidebar_panel")
            .exact_width(248.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(255, 255, 255))
                    .inner_margin(egui::Margin::symmetric(16, 14)),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                self.ui_sidebar(ui);
            });

        // 底部日志面板（仅在设备页显示）
        if self.active_page == NavigationPage::Devices {
            egui::TopBottomPanel::bottom("devices_log_panel")
                .exact_height(178.0) // 14 上间距 + 150 卡片 + 14 下间距
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(250, 248, 244))
                        .inner_margin(egui::Margin::symmetric(22, 14))
                        .stroke(egui::Stroke::NONE),
                )
                .show_separator_line(false)
                .show(ctx, |ui| {
                    self.ui_status_panel(ui);
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(250, 248, 244))
                    .inner_margin(egui::Margin::symmetric(22, 18)),
            )
            .show(ctx, |ui| {
                self.ui_main_content(ui);
            });

        self.ui_settings_dialog(ctx);
        self.ui_new_display_dialog(ctx);
        self.ui_clear_confirm_dialog(ctx);
        self.ui_alias_dialog(ctx);
        self.ui_logcat_args_dialog(ctx);
        self.ui_foreground_confirm_dialog(ctx);
        self.ui_connect_dialog(ctx);
        self.ui_drop_target_dialog(ctx);
        self.ui_update_dialog(ctx);
        self.ui_drag_overlay(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all_collections_for_shutdown();
    }
}

impl Drop for AdbCollectorApp {
    fn drop(&mut self) {
        self.stop_all_collections_for_shutdown();
    }
}

/// Whether the local update-flow demo is active in this process. Release
/// builds without the `update-preview` feature compile this to a constant
/// `false`, removing the demo paths entirely.
#[cfg(any(debug_assertions, feature = "update-preview"))]
fn demo_update_active() -> bool {
    updater::demo::requested()
}

#[cfg(not(any(debug_assertions, feature = "update-preview")))]
fn demo_update_active() -> bool {
    false
}

fn classify_dropped_paths(paths: Vec<PathBuf>) -> DroppedPayload {
    let mut payload = DroppedPayload::default();
    for path in paths {
        if is_apk_path(&path) {
            payload.apk_paths.push(path);
        } else {
            payload.file_paths.push(path);
        }
    }
    payload
}

fn is_apk_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("apk"))
        .unwrap_or(false)
}

fn build_device_push_destination(source_path: &Path) -> Result<String, String> {
    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "Cannot determine a file name for {}.",
                source_path.display()
            )
        })?;
    Ok(format!("{DEFAULT_DEVICE_DROP_DIR}/{file_name}"))
}

fn process_dropped_payload(
    adb_path: &str,
    serial: &str,
    payload: DroppedPayload,
) -> Result<String, String> {
    let mut installed = 0usize;
    let mut pushed = 0usize;
    let mut failures = Vec::new();

    for apk_path in payload.apk_paths {
        match adb::install_apk(adb_path, serial, &apk_path) {
            Ok(_) => installed += 1,
            Err(err) => failures.push(err),
        }
    }

    for file_path in payload.file_paths {
        match build_device_push_destination(&file_path) {
            Ok(remote_path) => match adb::push_file(adb_path, serial, &file_path, &remote_path) {
                Ok(_) => pushed += 1,
                Err(err) => failures.push(err),
            },
            Err(err) => failures.push(err),
        }
    }

    let summary = format_drop_processing_summary(installed, pushed, failures.len());
    if failures.is_empty() {
        Ok(summary)
    } else {
        Err(format!("{summary}\n{}", failures.join("\n")))
    }
}

fn format_drop_processing_summary(installed: usize, pushed: usize, failed: usize) -> String {
    let mut parts = Vec::new();
    if installed > 0 {
        parts.push(format!("Installed {installed} APK(s)"));
    }
    if pushed > 0 {
        parts.push(format!("Transferred {pushed} file(s)"));
    }
    if failed > 0 {
        parts.push(format!("Failed {failed} item(s)"));
    }
    if parts.is_empty() {
        "No dropped files were processed.".to_owned()
    } else {
        parts.join("; ")
    }
}

fn filter_installed_packages<'a>(packages: &'a [String], query: &str) -> Vec<&'a str> {
    let query = query.trim();
    if query.is_empty() {
        return packages.iter().map(String::as_str).collect();
    }

    let query = query.to_ascii_lowercase();
    packages
        .iter()
        .map(String::as_str)
        .filter(|package| package.to_ascii_lowercase().contains(&query))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_NEW_DISPLAY_DPI, DEFAULT_NEW_DISPLAY_HEIGHT, DEFAULT_NEW_DISPLAY_WIDTH,
        SETTINGS_FOOTER_ERROR_VIEWPORT_HEIGHT, build_device_push_destination,
        classify_dropped_paths, content_view_width, device_transport_rank,
        filter_installed_packages, format_device_model_name, is_current_cleanup_preview_response,
        pick_primary_device_info, settings_error_scroll_area, settings_path_input_width,
        should_reveal_proxy_settings_content,
    };
    use crate::models::DeviceInfo;
    use eframe::egui;
    use std::path::{Path, PathBuf};

    #[test]
    fn classify_dropped_paths_separates_apks_from_other_files() {
        let payload = classify_dropped_paths(vec![
            PathBuf::from("demo.apk"),
            PathBuf::from("notes.txt"),
            PathBuf::from("PATCH.APK"),
        ]);

        assert_eq!(payload.apk_paths.len(), 2);
        assert_eq!(payload.file_paths.len(), 1);
    }

    #[test]
    fn build_device_push_destination_targets_download_directory() {
        let remote =
            build_device_push_destination(Path::new("/tmp/demo.txt")).expect("remote destination");
        assert_eq!(remote, "/sdcard/Download/demo.txt");
    }

    #[test]
    fn format_device_model_name_prefers_readable_combination() {
        assert_eq!(
            format_device_model_name(Some("Google"), Some("Pixel 8")),
            Some("Google Pixel 8".to_owned())
        );
        assert_eq!(
            format_device_model_name(Some("Google"), Some("Google Pixel 8")),
            Some("Google Pixel 8".to_owned())
        );
        assert_eq!(
            format_device_model_name(None, Some("Pixel 8")),
            Some("Pixel 8".to_owned())
        );
        assert_eq!(format_device_model_name(None, None), None);
    }

    #[test]
    fn pick_primary_device_info_prefers_ready_usb_over_ready_wifi() {
        let wifi = DeviceInfo {
            serial: "192.168.0.8:5555".to_owned(),
            identity_key: "ZY223JQ9K".to_owned(),
            state: "device".to_owned(),
            android_version: None,
            manufacturer: None,
            model: None,
        };
        let usb = DeviceInfo {
            serial: "ZY223JQ9K".to_owned(),
            identity_key: "ZY223JQ9K".to_owned(),
            state: "device".to_owned(),
            android_version: None,
            manufacturer: None,
            model: None,
        };

        let primary = pick_primary_device_info(&[wifi, usb.clone()]);
        assert_eq!(primary.serial, usb.serial);
    }

    #[test]
    fn device_transport_rank_prefers_ready_wifi_over_offline_usb() {
        let wifi_ready = DeviceInfo {
            serial: "192.168.0.8:5555".to_owned(),
            identity_key: "ZY223JQ9K".to_owned(),
            state: "device".to_owned(),
            android_version: None,
            manufacturer: None,
            model: None,
        };
        let usb_offline = DeviceInfo {
            serial: "ZY223JQ9K".to_owned(),
            identity_key: "ZY223JQ9K".to_owned(),
            state: "offline".to_owned(),
            android_version: None,
            manufacturer: None,
            model: None,
        };

        assert!(device_transport_rank(&wifi_ready) < device_transport_rank(&usb_offline));
    }

    #[test]
    fn content_view_width_reserves_right_gutter_without_going_negative() {
        assert_eq!(content_view_width(320.0, 28.0), 292.0);
        assert_eq!(content_view_width(20.0, 28.0), 0.0);
    }

    #[test]
    fn settings_error_scroll_area_stays_inside_the_reserved_footer_viewport() {
        let ctx = egui::Context::default();
        let mut viewport_height = 0.0;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let output = settings_error_scroll_area().show(ui, |ui| {
                    ui.add(egui::Label::new("error ".repeat(256)).wrap());
                });
                viewport_height = output.inner_rect.height();
            });
        });

        assert!(viewport_height > 0.0);
        assert!(viewport_height <= SETTINGS_FOOTER_ERROR_VIEWPORT_HEIGHT);
    }
    #[test]
    fn cleanup_preview_only_accepts_the_current_request_generation() {
        assert!(is_current_cleanup_preview_response(7, 7));
        assert!(!is_current_cleanup_preview_response(8, 7));
        assert!(!is_current_cleanup_preview_response(7, 8));
    }

    #[test]
    fn settings_path_input_width_scales_with_the_available_width() {
        assert_eq!(settings_path_input_width(120.0), 160.0);
        assert_eq!(settings_path_input_width(500.0), 250.0);
        assert_eq!(settings_path_input_width(1_000.0), 420.0);
    }

    #[test]
    fn proxy_settings_reveal_triggers_for_new_or_async_content() {
        assert!(should_reveal_proxy_settings_content(true, false, false));
        assert!(should_reveal_proxy_settings_content(false, true, false));
        assert!(should_reveal_proxy_settings_content(false, false, true));
        assert!(!should_reveal_proxy_settings_content(false, false, false));
    }

    #[test]
    fn custom_new_display_defaults_use_portrait_dimensions() {
        assert_eq!(DEFAULT_NEW_DISPLAY_WIDTH, "720");
        assert_eq!(DEFAULT_NEW_DISPLAY_HEIGHT, "1600");
        assert_eq!(DEFAULT_NEW_DISPLAY_DPI, "320");
    }

    #[test]
    fn filter_installed_packages_matches_trimmed_case_insensitive_substrings() {
        let packages = vec![
            "com.android.settings".to_owned(),
            "com.tencent.mm".to_owned(),
            "com.tencent.mobileqq".to_owned(),
        ];

        assert_eq!(
            filter_installed_packages(&packages, " TENCENT "),
            vec!["com.tencent.mm", "com.tencent.mobileqq"]
        );
        assert_eq!(
            filter_installed_packages(&packages, ""),
            vec![
                "com.android.settings",
                "com.tencent.mm",
                "com.tencent.mobileqq"
            ]
        );
    }
}
