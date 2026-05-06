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
const DEFAULT_DEVICE_DROP_DIR: &str = "/sdcard/Download";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationPage {
    Devices,
    Logs,
    LogFiles,
    Settings,
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
    total_log_bytes: u64,
    status: Option<StatusMessage>,
    last_error: Option<StatusMessage>,
    status_log: Vec<StatusMessage>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    adb_path_input: String,
    log_dir_input: String,
    language_input: String,
    i18n: I18n,
    show_settings: bool,
    require_initial_setup: bool,
    active_page: NavigationPage,
    show_clear_confirm: bool,
    show_connect_dialog: bool,
    selected_serial: Option<String>,
    version: String,
    connect_target_input: String,
    connect_in_progress: bool,
    restarting_adb_server: bool,
    disconnecting_serial: Option<String>,
    alias_input_serial: Option<String>,
    alias_input_value: String,
    pending_drop_payload: Option<DroppedPayload>,
    pending_drop_target_serial: Option<String>,
    drop_task_in_progress: bool,
    device_poll_in_flight: bool,
    last_device_poll_at: Option<Instant>,
    last_device_snapshot: Vec<DeviceInfo>,
    last_auto_poll_error: Option<String>,
    sidebar_icon: Option<egui::TextureHandle>,
}

impl AdbCollectorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, bootstrap: AppBootstrap) -> Self {
        let config = bootstrap.config;
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

        let mut app = Self {
            adb_path_input: config.adb_path.clone(),
            log_dir_input: config.log_dir.clone(),
            app_paths: bootstrap.app_paths,
            config,
            devices: Vec::new(),
            total_log_bytes: 0,
            status: None,
            last_error: None,
            status_log: Vec::new(),
            tx,
            rx,
            language_input: String::new(),
            i18n: I18n::new("en"),
            show_settings: require_initial_setup,
            require_initial_setup,
            active_page: NavigationPage::Devices,
            show_clear_confirm: false,
            show_connect_dialog: false,
            selected_serial: None,
            version: bootstrap.version.to_owned(),
            connect_target_input: String::new(),
            connect_in_progress: false,
            restarting_adb_server: false,
            disconnecting_serial: None,
            alias_input_serial: None,
            alias_input_value: String::new(),
            pending_drop_payload: None,
            pending_drop_target_serial: None,
            drop_task_in_progress: false,
            device_poll_in_flight: false,
            last_device_poll_at: None,
            last_device_snapshot: Vec::new(),
            last_auto_poll_error: None,
            sidebar_icon,
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
                AppEvent::LogSizeRefreshed(result) => match result {
                    Ok(size) => {
                        self.total_log_bytes = size;
                    }
                    Err(err) => self.set_error(err),
                },
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
                    egui::Frame::new()
                        .fill(Color32::from_rgb(49, 106, 255))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(7, 3))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("v{}", self.version))
                                    .color(Color32::WHITE)
                                    .size(11.5)
                                    .strong(),
                            );
                        });
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
            if ui.add(github_button).clicked() {
                if let Err(err) = fs_utils::open_url(PROJECT_URL) {
                    self.set_error(err);
                }
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
                        fs_utils::format_bytes(self.total_log_bytes),
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
                    {
                        if let Err(err) =
                            fs_utils::open_path(PathBuf::from(&self.config.log_dir).as_path())
                        {
                            self.set_error(err);
                        }
                    }
                    if ui
                        .add(secondary_button("⌘", self.tr("toolbar.open_app_log")))
                        .clicked()
                    {
                        if let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path())
                        {
                            self.set_error(err);
                        }
                    }
                    if ui
                        .add(secondary_button("⚙", self.tr("toolbar.settings")))
                        .clicked()
                    {
                        if self.require_initial_setup {
                            self.show_settings = true;
                        } else {
                            self.active_page = NavigationPage::Settings;
                        }
                    }
                    let danger = egui::Button::new(
                        RichText::new(button_label("⌫", self.tr("toolbar.clear_history")))
                            .color(Color32::from_rgb(230, 85, 77))
                            .size(13.5)
                            .strong(),
                    )
                    .fill(Color32::from_rgb(255, 244, 243))
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(255, 221, 218)))
                    .corner_radius(egui::CornerRadius::same(10))
                    .min_size(egui::vec2(120.0, 38.0));
                    if ui.add(danger).clicked() {
                        self.show_clear_confirm = true;
                    }
                });
            });
    }

    fn ui_main_content(&mut self, ui: &mut egui::Ui) {
        match self.active_page {
            NavigationPage::Devices => {
                // 上部内容区域：BottomPanel 已从底部扣除日志面板空间，直接使用全部可用高度
                let available_width = ui.available_width();
                let upper_height = ui.available_height().max(80.0);

                let (upper_rect, _) = ui.allocate_exact_size(
                    egui::vec2(available_width, upper_height),
                    egui::Sense::hover(),
                );
                let mut upper_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(upper_rect)
                        .layout(egui::Layout::top_down(Align::LEFT)),
                );
                upper_ui.set_clip_rect(upper_rect);
                // 让垂直滚动条不浮动，占据独立空间，避免遮盖内容
                upper_ui.spacing_mut().scroll.floating = false;
                egui::ScrollArea::vertical()
                    .max_height(upper_rect.height())
                    .auto_shrink([false, false])
                    .show(&mut upper_ui, |ui| {
                        let content_width = ui.available_width();
                        ui.set_min_width(content_width);
                        ui.set_max_width(content_width);
                        self.ui_overview_cards(ui);
                        ui.add_space(14.0);
                        self.ui_action_row(ui);
                        ui.add_space(14.0);
                        self.ui_devices(ui);
                        ui.add_space(14.0);
                        self.ui_selected_device(ui);
                    });
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
                    ui.colored_label(
                        color,
                        format!("[{}] {}", entry.timestamp, entry.text),
                    );
                }
            });
    }

    fn ui_selected_device(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let selected = self
            .selected_serial
            .as_deref()
            .and_then(|serial| self.find_device(serial).cloned());
        let heading = self.tr("device.selected");
        let alias_label = self.tr("device.alias");
        let display_label = self.tr("device.display_name");
        let serial_label = self.tr("device.serial");
        let android_version_label = self.tr("device.android_version");
        let state_label = self.tr("device.state");
        let session_label = self.tr("device.session");
        let started_label = self.tr("device.started");
        let latest_file_label = self.tr("device.latest_file");
        let never_text = self.tr("misc.never");
        let no_file_text = self.tr("device.no_file");
        let open_file_text = self.tr("device.action.open_file");
        let open_folder_text = self.tr("device.action.open_folder");
        let save_alias_text = self.tr("device.action.save_alias");
        let clear_alias_text = self.tr("device.action.clear_alias");
        let pin_text = self.tr("device.action.pin");
        let unpin_text = self.tr("device.action.unpin");
        let copy_serial_text = self.tr("device.action.copy_serial");
        let open_shell_text = self.tr("device.action.open_shell");
        let disconnect_text = self.tr("device.action.disconnect");
        let none_selected_text = self.tr("device.none_selected");
        let pinned_text = self.tr("device.pinned");
        let mut alias_save_serial = None;
        let mut alias_clear_serial = None;
        let mut toggle_pin_serial = None;
        let mut copy_serial = None;
        let mut open_shell_serial = None;
        let mut disconnect_serial = None;

        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(heading);
            ui.add_space(12.0);

            if let Some(device) = selected {
                let serial = device.info.serial.clone();
                self.sync_alias_editor(&serial);
                let display_name = self.device_identity_label(&serial);
                let alias_text = self
                    .device_alias(&serial)
                    .unwrap_or_else(|| self.tr("device.alias.empty"));
                let android_version_text = self.device_android_version_text(&device.info);
                let run_state_text = self.run_state_text(&device.run_state);
                let started_at_text = device
                    .started_at
                    .map(format_system_time)
                    .unwrap_or_else(|| never_text.clone());
                let latest_file_text = device
                    .output_path
                    .as_ref()
                    .and_then(|path| path.file_name().and_then(|name| name.to_str()))
                    .map(str::to_owned)
                    .unwrap_or_else(|| no_file_text.clone());
                let state_text_value = self.device_state_text(&device.info.state);
                let state_color = self.device_state_color(&device.info.state);
                let is_pinned = self.is_pinned_device(&device.info.serial);
                let visual_ready = device.info.state == "device";
                let status_card_title = self.tr("device.status_card.title");
                let status_card_subtitle = self.tr("device.status_card.subtitle");
                let secondary_button = |text: String| {
                    egui::Button::new(text)
                        .corner_radius(egui::CornerRadius::same(10))
                        .min_size(egui::vec2(82.0, 34.0))
                };
                let side_by_side = ui.available_width() >= 560.0;
                let visual_col_w = 216.0;
                let gutter = 10.0 * 2.0 + ui.spacing().item_spacing.x;
                let summary_width = (ui.available_width() - visual_col_w - gutter).max(0.0);

                // 提取 alias_input_value 到局部变量，避免闭包借用冲突
                let mut alias_input_value = std::mem::take(&mut self.alias_input_value);
                let mut alias_save_serial_local: Option<String> = None;
                let mut alias_clear_serial_local: Option<String> = None;

                let render_summary = |ui: &mut egui::Ui| {
                    selected_device_text_row(ui, &display_label, display_name);
                    selected_device_text_row(ui, &alias_label, alias_text.clone());
                    selected_device_text_row(ui, &serial_label, serial.clone());
                    selected_device_text_row(ui, &android_version_label, android_version_text);
                    selected_device_detail_row(ui, &state_label, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            draw_state_badge(ui, &state_text_value, state_color);
                            if is_pinned {
                                ui.small(RichText::new(pinned_text.clone()).strong());
                            }
                        });
                    });
                    ui.add_space(12.0);
                    selected_device_text_row(ui, &session_label, run_state_text);
                    selected_device_text_row(ui, &started_label, started_at_text);
                    selected_device_text_row(ui, &latest_file_label, latest_file_text);

                    // 别名编辑区（3 行形式：标签、输入框、按钮）
                    ui.add_space(14.0);
                    selected_device_detail_row(ui, &alias_label, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut alias_input_value)
                                .vertical_align(Align::Center)
                                .return_key(None),
                        );
                    });
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        let primary = egui::Button::new(
                            RichText::new(save_alias_text.clone())
                                .color(Color32::WHITE)
                                .strong(),
                        )
                        .fill(Color32::from_rgb(56, 116, 255))
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(egui::CornerRadius::same(10))
                        .min_size(egui::vec2(62.0, 34.0));
                        if ui.add(primary).clicked() {
                            alias_save_serial_local = Some(serial.clone());
                        }
                        if ui.add(secondary_button(clear_alias_text.clone())).clicked() {
                            alias_clear_serial_local = Some(serial.clone());
                        }
                    });
                };

                if side_by_side {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(summary_width, 0.0),
                            egui::Layout::top_down(Align::LEFT),
                            render_summary,
                        );
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width().max(0.0), 0.0),
                            egui::Layout::top_down(Align::Center),
                            |ui| {
                                draw_device_visual(
                                    ui,
                                    &status_card_title,
                                    &status_card_subtitle,
                                    visual_ready,
                                );
                            },
                        );
                    });
                } else {
                    render_summary(ui);
                    ui.add_space(16.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 0.0),
                        egui::Layout::top_down(Align::Center),
                        |ui| {
                            draw_device_visual(
                                ui,
                                &status_card_title,
                                &status_card_subtitle,
                                visual_ready,
                            );
                        },
                    );
                }

                // 写回 alias 状态
                self.alias_input_value = alias_input_value;
                alias_save_serial = alias_save_serial_local;
                alias_clear_serial = alias_clear_serial_local;

                ui.add_space(14.0);
                ui.horizontal_wrapped(|ui| {
                    let pin_button_text = if self.is_pinned_device(&device.info.serial) {
                        unpin_text.clone()
                    } else {
                        pin_text.clone()
                    };
                    if ui.add(secondary_button(pin_button_text)).clicked() {
                        toggle_pin_serial = Some(device.info.serial.clone());
                    }
                    if ui.add(secondary_button(copy_serial_text.clone())).clicked() {
                        copy_serial = Some(device.info.serial.clone());
                    }
                    if ui
                        .add_enabled(
                            device.info.state == "device",
                            secondary_button(open_shell_text.clone()),
                        )
                        .clicked()
                    {
                        open_shell_serial = Some(device.info.serial.clone());
                    }
                    if adb::is_network_device_serial(&device.info.serial)
                        && ui
                            .add_enabled(
                                self.disconnecting_serial.as_deref()
                                    != Some(device.info.serial.as_str()),
                                secondary_button(disconnect_text.clone()),
                            )
                            .clicked()
                    {
                        disconnect_serial = Some(device.info.serial.clone());
                    }
                    if let Some(path) = &device.output_path {
                        if ui.add(secondary_button(open_file_text.clone())).clicked() {
                            if let Err(err) = fs_utils::open_path(path) {
                                self.set_error(err);
                            }
                        }
                        if let Some(parent) = path.parent() {
                            if ui.add(secondary_button(open_folder_text.clone())).clicked() {
                                if let Err(err) = fs_utils::open_path(parent) {
                                    self.set_error(err);
                                }
                            }
                        }
                    }
                });
            } else {
                let side_by_side = ui.available_width() >= 560.0;
                let summary_width = (ui.available_width() - 216.0).max(260.0);
                let status_card_title = self.tr("device.status_card.title");
                let status_card_subtitle = self.tr("device.status_card.subtitle");

                if side_by_side {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(summary_width, 220.0),
                            egui::Layout::top_down(Align::LEFT),
                            |ui| {
                                ui.add_space(34.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(none_selected_text.clone())
                                            .size(16.0)
                                            .strong()
                                            .color(Color32::from_rgb(54, 60, 72)),
                                    )
                                    .wrap(),
                                );
                            },
                        );
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width().max(0.0), 0.0),
                            egui::Layout::top_down(Align::Center),
                            |ui| {
                                draw_device_visual(
                                    ui,
                                    &status_card_title,
                                    &status_card_subtitle,
                                    false,
                                );
                            },
                        );
                    });
                } else {
                    ui.add(
                        egui::Label::new(
                            RichText::new(none_selected_text)
                                .size(16.0)
                                .strong()
                                .color(Color32::from_rgb(54, 60, 72)),
                        )
                        .wrap(),
                    );
                    ui.add_space(16.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 0.0),
                        egui::Layout::top_down(Align::Center),
                        |ui| {
                            draw_device_visual(
                                ui,
                                &status_card_title,
                                &status_card_subtitle,
                                false,
                            );
                        },
                    );
                }
            }
        });

        if let Some(serial) = alias_save_serial {
            self.save_device_alias(&serial, self.alias_input_value.clone());
        }
        if let Some(serial) = alias_clear_serial {
            self.save_device_alias(&serial, String::new());
        }
        if let Some(serial) = toggle_pin_serial {
            self.toggle_pinned_device(&serial);
        }
        if let Some(serial) = copy_serial {
            self.copy_serial_to_clipboard(&ctx, serial);
        }
        if let Some(serial) = open_shell_serial {
            self.open_device_shell(serial);
        }
        if let Some(serial) = disconnect_serial {
            self.start_disconnect(serial);
        }
    }

    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("devices.title"));
            ui.label(self.tr("devices.hint"));
            ui.add_space(10.0);

            if self.devices.is_empty() {
                ui.set_min_height(190.0);
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
            let mut open_shell_serial: Option<String> = None;
            let mut disconnect_serial: Option<String> = None;
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
            let shell_text = self.tr("device.action.open_shell");
            let disconnect_text = self.tr("device.action.disconnect");
            let more_text = self.tr("device.action.more");
            let stopping_text = self.tr("run_state.stopping");

            // Fixed widths for secondary columns (reduced for better fit).
            // First column is flexible: fills remaining space, min 180 px.
            let fixed_cols: f32 = 100.0 + 90.0 + 90.0 + 120.0 + 140.0 + 120.0;
            let col_spacing: f32 = 10.0 * 6.0; // item_spacing.x * gaps
            let row_inner_margin: f32 = 12.0 * 2.0;
            let name_col_w =
                (ui.available_width() - row_inner_margin - col_spacing - fixed_cols).max(180.0);
            let widths = [name_col_w, 100.0, 90.0, 90.0, 120.0, 140.0, 120.0];
            let total_min_w = row_inner_margin + 180.0 + fixed_cols + col_spacing;

            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.set_min_width(total_min_w);
                    // Header row: 12px left indent to align with row frame inner margin
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        for (idx, title) in [
                            serial_text.clone(),
                            android_version_text.clone(),
                            state_text.clone(),
                            session_text.clone(),
                            started_text.clone(),
                            output_text.clone(),
                            actions_text.clone(),
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
                        let state = self.devices[index].info.state.clone();
                        let android_version = self.devices[index].info.android_version.clone();
                        let run_state = self.devices[index].run_state.clone();
                        let started_at = self.devices[index].started_at;
                        let output_path = self.devices[index].output_path.clone();
                        let selected = self.selected_serial.as_deref() == Some(serial.as_str());
                        let primary_name = self.device_primary_name(&serial);
                        let is_pinned = self.is_pinned_device(&serial);
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
                                        format!(
                                            "{} ({})",
                                            primary_name,
                                            self.tr("device.pinned_short")
                                        )
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
                                            self.alias_input_serial = None;
                                            self.alias_input_value.clear();
                                        } else {
                                            self.selected_serial = Some(serial.clone());
                                            self.sync_alias_editor(&serial);
                                        }
                                    }
                                    if name_response.double_clicked() {
                                        start_serial = Some(serial.clone());
                                    }

                                    // Helper closure for centered cell (painter-based for reliable H+V centering)
                                    let centered_cell =
                                        |ui: &mut egui::Ui, w: f32, text: String| {
                                            let (rect, _) = ui.allocate_exact_size(
                                                egui::vec2(w, 44.0),
                                                egui::Sense::hover(),
                                            );
                                            if ui.is_rect_visible(rect) {
                                                let font_id =
                                                    egui::TextStyle::Body.resolve(ui.style());
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

                                    centered_cell(
                                        ui,
                                        widths[1],
                                        android_version.unwrap_or_else(|| {
                                            self.tr("device.android_version.unknown")
                                        }),
                                    );
                                    {
                                        let (badge_cell_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(widths[2], 44.0),
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
                                    centered_cell(
                                        ui,
                                        widths[3],
                                        run_state_text_with(&i18n, &run_state),
                                    );
                                    centered_cell(
                                        ui,
                                        widths[4],
                                        started_at
                                            .map(format_system_time)
                                            .unwrap_or_else(|| never_text.clone()),
                                    );
                                    let output_name = output_path
                                        .as_ref()
                                        .map(|path| {
                                            path.file_name()
                                                .and_then(|name| name.to_str())
                                                .map(str::to_owned)
                                                .unwrap_or_else(|| {
                                                    path.to_string_lossy().into_owned()
                                                })
                                        })
                                        .unwrap_or_else(|| "-".to_owned());
                                    centered_cell(ui, widths[5], output_name);

                                    // Actions column: vertical centering via top_down(Center) wrapper,
                                    // horizontal centering via narrower group_w allocation for
                                    // Idle/Running states so the button group aligns with the header.
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(widths[6], 44.0),
                                        egui::Layout::top_down(Align::Center),
                                        |ui| {
                                            // Vertical center: button height ~30px, cell 44px → pad 7px
                                            ui.add_space(7.0);
                                            // group_w = Stopping uses full column (may overflow as before);
                                            // Idle/Error = Start(52)+gap(10)+More(44) ≈ 106;
                                            // Starting/Running = Stop(56)+gap(10)+More(44) ≈ 110.
                                            let group_w = match &run_state {
                                                DeviceRunState::Stopping => widths[6],
                                                DeviceRunState::Idle | DeviceRunState::Error(_) => {
                                                    106.0
                                                }
                                                _ => 110.0,
                                            };
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(group_w, 30.0),
                                                egui::Layout::left_to_right(Align::Center),
                                                |ui| {
                                                    match run_state {
                                                        DeviceRunState::Idle
                                                        | DeviceRunState::Error(_) => {
                                                            let can_start = state == "device";
                                                            let start_button = egui::Button::new(
                                                                RichText::new(start_text.clone())
                                                                    .color(Color32::WHITE)
                                                                    .strong(),
                                                            )
                                                            .fill(Color32::from_rgb(56, 116, 255))
                                                            .stroke(egui::Stroke::NONE)
                                                            .corner_radius(
                                                                egui::CornerRadius::same(8),
                                                            )
                                                            .min_size(egui::vec2(52.0, 30.0));
                                                            if ui
                                                                .add_enabled(
                                                                    can_start,
                                                                    start_button,
                                                                )
                                                                .clicked()
                                                            {
                                                                start_serial = Some(serial.clone());
                                                            }
                                                        }
                                                        DeviceRunState::Starting
                                                        | DeviceRunState::Running => {
                                                            if ui
                                                                .add(rounded_secondary(
                                                                    stop_text.clone(),
                                                                ))
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
                                                        if let Some(path) = &output_path {
                                                            if ui
                                                                .add(rounded_secondary(
                                                                    open_text.clone(),
                                                                ))
                                                                .clicked()
                                                            {
                                                                open_output = Some(path.clone());
                                                                ui.close_menu();
                                                            }
                                                        }
                                                        if ui
                                                            .add(rounded_secondary(
                                                                copy_text.clone(),
                                                            ))
                                                            .clicked()
                                                        {
                                                            copy_serial = Some(serial.clone());
                                                            ui.close_menu();
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                state == "device",
                                                                rounded_secondary(
                                                                    shell_text.clone(),
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            open_shell_serial =
                                                                Some(serial.clone());
                                                            ui.close_menu();
                                                        }
                                                        if adb::is_network_device_serial(&serial)
                                                            && ui
                                                                .add_enabled(
                                                                    self.disconnecting_serial
                                                                        .as_deref()
                                                                        != Some(serial.as_str()),
                                                                    rounded_secondary(
                                                                        disconnect_text.clone(),
                                                                    ),
                                                                )
                                                                .clicked()
                                                        {
                                                            disconnect_serial =
                                                                Some(serial.clone());
                                                            ui.close_menu();
                                                        }
                                                    });
                                                },
                                            );
                                        },
                                    );
                                });
                            });
                        ui.add_space(8.0);
                    }
                }); // end ScrollArea::horizontal

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
            if let Some(serial) = copy_serial {
                self.copy_serial_to_clipboard(&ctx, serial);
            }
            if let Some(serial) = open_shell_serial {
                self.open_device_shell(serial);
            }
            if let Some(serial) = disconnect_serial {
                self.start_disconnect(serial);
            }
        });
    }

    fn ui_logs_page(&mut self, ui: &mut egui::Ui) {
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("status.panel_title"));
            ui.label(self.tr("logs.hint"));
            ui.add_space(10.0);
            self.ui_status_content(ui, None);
        });
    }

    fn ui_log_files_page(&mut self, ui: &mut egui::Ui) {
        let log_entries = self.collect_log_entries();
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("log_files.title"));
            ui.label(self.tr("log_files.hint"));
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button(self.tr("toolbar.open_logs")).clicked() {
                    if let Err(err) =
                        fs_utils::open_path(PathBuf::from(&self.config.log_dir).as_path())
                    {
                        self.set_error(err);
                    }
                }
                if ui.button(self.tr("toolbar.open_app_log")).clicked() {
                    if let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path()) {
                        self.set_error(err);
                    }
                }
                if ui.button(self.tr("toolbar.refresh_size")).clicked() {
                    self.refresh_log_size();
                }
                if ui.button(self.tr("toolbar.clear_history")).clicked() {
                    self.show_clear_confirm = true;
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
                    ui.label(fs_utils::format_bytes(self.total_log_bytes));
                    ui.end_row();
                });
            ui.add_space(14.0);
            ui.label(RichText::new(self.tr("log_files.recent_entries")).strong());
            ui.add_space(6.0);
            if log_entries.is_empty() {
                ui.small(self.tr("log_files.empty"));
            } else {
                for entry in log_entries {
                    ui.label(entry);
                }
            }
        });
    }

    fn ui_settings_page(&mut self, ui: &mut egui::Ui) {
        content_card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(self.tr("settings.title"));
            ui.label(self.tr("settings.page_hint"));
            ui.add_space(10.0);
            self.ui_settings_form(ui, true);
        });
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

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                self.ui_settings_form(ui, false);
            });
    }

    fn ui_settings_form(&mut self, ui: &mut egui::Ui, inline_page: bool) {
        ui.label(self.tr("settings.intro"));
        ui.small(self.tr("settings.explainer"));
        ui.horizontal(|ui| {
            if ui.button(self.tr("settings.open_config_dir")).clicked() {
                if let Err(err) = fs_utils::open_path(self.app_paths.config_dir.as_path()) {
                    self.set_error(err);
                }
            }
            if ui.button(self.tr("settings.open_app_log")).clicked() {
                if let Err(err) = fs_utils::open_path(self.app_paths.app_log_path.as_path()) {
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
        if self.adb_path_input.trim().is_empty() {
            ui.add_space(4.0);
            ui.small(self.tr("settings.adb.download_hint"));
            ui.hyperlink_to(
                self.tr("settings.adb.download_link"),
                self.adb_download_url(),
            );
        }

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

        ui.add_space(12.0);
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

    fn ui_clear_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_clear_confirm {
            return;
        }

        egui::Window::new(self.tr("clear.title"))
            .collapsible(false)
            .resizable(false)
            .fixed_size([420.0, 150.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
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

        let ready_devices = self.ready_device_serials();
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

        egui::Area::new("drop_overlay".into())
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(self.tr("drop.hover_title")).strong());
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
        let candidate = AppConfig {
            adb_path: self.adb_path_input.trim().to_owned(),
            log_dir: self.log_dir_input.trim().to_owned(),
            app_log_max_size_mb: self.config.app_log_max_size_mb,
            language: self.language_input.clone(),
            device_aliases: self.config.device_aliases.clone(),
            pinned_devices: self.config.pinned_devices.clone(),
            recent_connections: self.config.recent_connections.clone(),
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
            device_aliases: candidate.device_aliases.clone(),
            pinned_devices: candidate.pinned_devices.clone(),
            recent_connections: candidate.recent_connections.clone(),
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
        self.last_device_snapshot.clear();
        self.last_device_poll_at = None;
        self.last_auto_poll_error = None;
        self.set_info(self.tr("status.settings_saved"));
        self.refresh_devices();
        self.refresh_log_size();
    }

    fn reset_settings_inputs(&mut self) {
        self.adb_path_input = self.config.adb_path.clone();
        self.log_dir_input = self.config.log_dir.clone();
        self.language_input = self.config.language.clone();
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
        ctx.copy_text(serial.clone());
        self.set_info(self.tr_args(
            "status.serial_copied",
            &[("serial", self.device_identity_label(&serial))],
        ));
    }

    fn open_device_shell(&mut self, serial: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }

        let Some(device) = self.find_device(&serial).cloned() else {
            return;
        };
        if device.info.state != "device" {
            self.set_error(self.tr_args(
                "status.device_invalid_state",
                &[
                    ("serial", self.device_identity_label(&serial)),
                    ("state", self.device_state_text(&device.info.state)),
                ],
            ));
            return;
        }

        match fs_utils::open_device_shell(&self.config.adb_path, &serial) {
            Ok(message) => self.set_info(self.tr_args(
                "status.device_shell_opened",
                &[
                    ("serial", self.device_identity_label(&serial)),
                    ("message", message),
                ],
            )),
            Err(err) => self.set_error(err),
        }
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

        let ready_devices = self.ready_device_serials();
        if ready_devices.is_empty() {
            self.set_error(self.tr("status.drop_no_ready_device"));
            return;
        }

        if let Some(selected_serial) = self.selected_serial.clone() {
            if ready_devices
                .iter()
                .any(|serial| serial == &selected_serial)
            {
                self.start_drop_task(selected_serial, payload);
                return;
            }
        }

        self.pending_drop_target_serial = ready_devices.first().cloned();
        self.pending_drop_payload = Some(payload);
    }

    fn start_drop_task(&mut self, serial: String, payload: DroppedPayload) {
        if payload.is_empty() || self.drop_task_in_progress {
            return;
        }

        self.drop_task_in_progress = true;
        self.set_info(self.tr_args(
            "status.drop_processing",
            &[
                ("count", payload.total_count().to_string()),
                ("serial", self.device_identity_label(&serial)),
            ],
        ));

        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        thread::spawn(move || {
            let result = process_dropped_payload(&adb_path, &serial, payload);
            let _ = tx.send(AppEvent::DeviceDropFinished { serial, result });
        });
    }

    fn start_collection(&mut self, serial: String) {
        if self.require_initial_setup {
            self.set_error(self.tr("status.finish_initial_setup"));
            self.show_settings = true;
            return;
        }

        let device_name = self.device_identity_label(&serial);

        if let Some(device) = self.find_device(&serial) {
            if device.info.state != "device" {
                self.set_error(self.tr_args(
                    "status.device_invalid_state",
                    &[
                        ("serial", device_name.clone()),
                        ("state", self.device_state_text(&device.info.state)),
                    ],
                ));
                return;
            }
        }

        let output_path = fs_utils::session_log_path(
            PathBuf::from(self.config.log_dir.as_str()).as_path(),
            &serial,
            self.device_alias(&serial).as_deref(),
        );
        if let Some(device) = self.find_device_mut(&serial) {
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
        let device_name = self.device_identity_label(serial);
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
                current.info.android_version = info.android_version;
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

        self.devices = merged;
        self.sort_devices();
        let selected_exists = self
            .selected_serial
            .as_deref()
            .map(|serial| self.find_device(serial).is_some())
            .unwrap_or(false);
        if !selected_exists {
            self.selected_serial = self
                .devices
                .first()
                .map(|device| device.info.serial.clone());
            if let Some(serial) = self.selected_serial.clone() {
                self.sync_alias_editor(&serial);
            } else {
                self.alias_input_serial = None;
                self.alias_input_value.clear();
            }
        }
    }

    fn device_android_version_text(&self, info: &DeviceInfo) -> String {
        info.android_version
            .clone()
            .unwrap_or_else(|| self.tr("device.android_version.unknown"))
    }

    fn collect_log_entries(&self) -> Vec<String> {
        let mut entries = std::fs::read_dir(&self.config.log_dir)
            .ok()
            .into_iter()
            .flat_map(|iter| iter.flatten())
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if file_type.is_dir() {
                    name.push('/');
                }
                Some(name)
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries.truncate(8);
        entries
    }

    fn persist_config(&mut self) -> Result<(), String> {
        config::save_config(&self.app_paths.config_path, &self.config)?;
        self.config = config::load_config(&self.app_paths.config_path, &self.app_paths)?;
        Ok(())
    }

    fn sync_alias_editor(&mut self, serial: &str) {
        if self.alias_input_serial.as_deref() == Some(serial) {
            return;
        }

        self.alias_input_serial = Some(serial.to_owned());
        self.alias_input_value = self.device_alias(serial).unwrap_or_default();
    }

    fn device_alias(&self, serial: &str) -> Option<String> {
        self.config.device_aliases.get(serial).cloned()
    }

    fn device_primary_name(&self, serial: &str) -> String {
        self.device_alias(serial)
            .unwrap_or_else(|| serial.to_owned())
    }

    fn device_identity_label(&self, serial: &str) -> String {
        match self.device_alias(serial) {
            Some(alias) if alias != serial => format!("{alias} ({serial})"),
            _ => serial.to_owned(),
        }
    }

    fn is_pinned_device(&self, serial: &str) -> bool {
        self.config
            .pinned_devices
            .iter()
            .any(|value| value == serial)
    }

    fn ready_device_serials(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|device| device.info.state == "device")
            .map(|device| device.info.serial.clone())
            .collect()
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
        let was_pinned = self.is_pinned_device(serial);
        let previous = self.config.pinned_devices.clone();

        if was_pinned {
            self.config.pinned_devices.retain(|value| value != serial);
        } else {
            self.config.pinned_devices.push(serial.to_owned());
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
        let next_alias = alias.trim().to_owned();
        let previous_alias = self.device_alias(serial);
        let previous_alias_text = previous_alias.clone().unwrap_or_default();

        if previous_alias.as_deref() == Some(next_alias.as_str())
            || (next_alias.is_empty() && previous_alias.is_none())
        {
            self.alias_input_value = next_alias;
            return;
        }

        if self
            .find_device(serial)
            .map(|device| device.is_active())
            .unwrap_or(false)
        {
            self.set_error(self.tr_args(
                "status.alias_change_requires_idle",
                &[("serial", self.device_identity_label(serial))],
            ));
            self.alias_input_value = previous_alias_text;
            return;
        }

        let base_dir = PathBuf::from(self.config.log_dir.as_str());
        let old_dir = fs_utils::device_log_dir(&base_dir, serial, previous_alias.as_deref());
        let renamed_dir = match fs_utils::rename_device_log_dir(
            &base_dir,
            serial,
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
            self.config.device_aliases.remove(serial);
        } else {
            self.config
                .device_aliases
                .insert(serial.to_owned(), next_alias.clone());
        }

        if let Err(err) = self.persist_config() {
            self.config.device_aliases = previous_aliases;
            if let Some(new_dir) = &renamed_dir {
                if &old_dir != new_dir && new_dir.exists() {
                    let _ = std::fs::rename(new_dir, &old_dir);
                }
            }
            self.alias_input_value = previous_alias_text;
            self.set_error(err);
            return;
        }

        if let Some(new_dir) = renamed_dir.as_deref() {
            if old_dir != new_dir {
                self.update_device_output_dir(serial, &old_dir, new_dir);
            }
        }

        self.sort_devices();
        self.alias_input_value = self.device_alias(serial).unwrap_or_default();
        let status_key = if next_alias.is_empty() {
            "status.alias_cleared"
        } else {
            "status.alias_saved"
        };
        self.set_info(self.tr_args(
            status_key,
            &[("serial", self.device_identity_label(serial))],
        ));
    }

    fn update_device_output_dir(&mut self, serial: &str, old_dir: &Path, new_dir: &Path) {
        if let Some(device) = self.find_device_mut(serial) {
            if let Some(current_path) = device.output_path.clone() {
                if current_path.parent() == Some(old_dir) {
                    if let Some(file_name) = current_path.file_name() {
                        device.output_path = Some(new_dir.join(file_name));
                    }
                }
            }
        }
    }

    fn sort_devices(&mut self) {
        let aliases = self.config.device_aliases.clone();
        let pinned = self.config.pinned_devices.clone();
        self.devices.sort_by(|left, right| {
            let left_pinned = pinned.iter().any(|value| value == &left.info.serial);
            let right_pinned = pinned.iter().any(|value| value == &right.info.serial);
            right_pinned
                .cmp(&left_pinned)
                .then_with(|| {
                    aliases
                        .get(&left.info.serial)
                        .unwrap_or(&left.info.serial)
                        .to_ascii_lowercase()
                        .cmp(
                            &aliases
                                .get(&right.info.serial)
                                .unwrap_or(&right.info.serial)
                                .to_ascii_lowercase(),
                        )
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

    fn run_state_text(&self, run_state: &DeviceRunState) -> String {
        run_state_text_with(&self.i18n, run_state)
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

fn detail_label(ui: &mut egui::Ui, label: &str) {
    ui.label(RichText::new(label).color(Color32::from_rgb(122, 128, 142)));
}

fn selected_device_detail_row<R>(
    ui: &mut egui::Ui,
    label: &str,
    add_value: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    const LABEL_WIDTH: f32 = 112.0;

    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_WIDTH, 0.0),
            egui::Layout::top_down(Align::LEFT),
            |ui| {
                ui.add(
                    egui::Label::new(RichText::new(label).color(Color32::from_rgb(122, 128, 142)))
                        .wrap(),
                );
            },
        );
        let value_width = ui.available_width().max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(value_width, 0.0),
            egui::Layout::top_down(Align::LEFT),
            add_value,
        )
        .inner
    })
    .inner
}

fn selected_device_text_row(ui: &mut egui::Ui, label: &str, value: impl Into<egui::WidgetText>) {
    selected_device_detail_row(ui, label, |ui| {
        ui.add(egui::Label::new(value).wrap());
    });
    ui.add_space(12.0);
}

fn draw_state_badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).strong());
        });
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

fn draw_device_visual(ui: &mut egui::Ui, title: &str, subtitle: &str, ready: bool) {
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        egui::Frame::new()
            .fill(Color32::from_rgb(251, 252, 255))
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(234, 237, 245)))
            .corner_radius(egui::CornerRadius::same(16))
            .inner_margin(egui::Margin::symmetric(24, 20))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let phone_rect_size = egui::vec2(78.0, 124.0);
                    let (rect, _) = ui.allocate_exact_size(phone_rect_size, egui::Sense::hover());
                    ui.painter().rect(
                        rect,
                        egui::CornerRadius::same(16),
                        Color32::from_rgb(245, 246, 250),
                        egui::Stroke::new(1.0, Color32::from_rgb(223, 228, 237)),
                        egui::epaint::StrokeKind::Middle,
                    );
                    ui.painter().rect_filled(
                        egui::Rect::from_center_size(
                            rect.center_top() + egui::vec2(0.0, 10.0),
                            egui::vec2(18.0, 4.0),
                        ),
                        egui::CornerRadius::same(2),
                        Color32::from_rgb(217, 221, 229),
                    );
                    ui.painter().rect_filled(
                        rect.shrink2(egui::vec2(7.0, 9.0)),
                        egui::CornerRadius::same(12),
                        Color32::WHITE,
                    );
                    let badge_center = rect.center_bottom() - egui::vec2(0.0, 22.0);
                    let badge_color = if ready {
                        Color32::from_rgb(51, 162, 94)
                    } else {
                        Color32::from_rgb(126, 133, 149)
                    };
                    ui.painter().circle_filled(
                        badge_center,
                        14.0,
                        badge_color.gamma_multiply(0.14),
                    );
                    ui.painter().circle_filled(badge_center, 12.0, badge_color);
                    ui.painter().text(
                        badge_center,
                        egui::Align2::CENTER_CENTER,
                        if ready { "✓" } else { "…" },
                        egui::FontId::proportional(17.0),
                        Color32::WHITE,
                    );
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new(title)
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(44, 49, 59)),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(subtitle)
                            .size(13.0)
                            .color(Color32::from_rgb(116, 123, 136)),
                    );
                });
            });
    });
}

impl eframe::App for AdbCollectorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_visual_style(ctx);
        self.handle_events();
        self.poll_devices_if_due();
        self.handle_dropped_files(ctx);

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
        self.ui_clear_confirm_dialog(ctx);
        self.ui_connect_dialog(ctx);
        self.ui_drop_target_dialog(ctx);
        self.ui_drag_overlay(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
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
    use super::{build_device_push_destination, classify_dropped_paths};
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
}
