use super::AdbCollectorApp;
use crate::{
    i18n::I18n,
    wireless::{
        self, AdbKind, CancelToken, Candidate, Failure, ScanProgress, ScanUpdate, Service,
        ServiceKind,
    },
};
use eframe::egui::{self, Color32, RichText};
use std::{
    net::IpAddr,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Wireless,
    Scan,
    Manual,
}

enum Event {
    Discovery(u64, u64, Result<Vec<Service>, Failure>),
    Paired(u64, String, Result<(), Failure>),
    Scan(u64, u64, ScanUpdate),
}

pub(super) struct ConnectionDialog {
    active: bool,
    session: u64,
    lifetime: CancelToken,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    tab: Tab,
    services: Vec<Service>,
    discovery_busy: bool,
    discovery_id: u64,
    discovery_error: Option<String>,
    discovery_at: Option<Instant>,
    pair_target: String,
    pair_code: String,
    pairing: bool,
    pair_open: bool,
    waiting_host: Option<(IpAddr, Instant)>,
    auto_scan_host: Option<IpAddr>,
    scan_input: String,
    scan_id: u64,
    scan_cancel: CancelToken,
    scan_progress: Option<ScanProgress>,
    scan_results: Vec<Candidate>,
    manual_wireless: bool,
    pub(super) connecting_wireless: bool,
    pub(super) error: Option<String>,
    message: Option<&'static str>,
}

impl Default for ConnectionDialog {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            active: false,
            session: 0,
            lifetime: CancelToken::default(),
            tx,
            rx,
            tab: Tab::Wireless,
            services: Vec::new(),
            discovery_busy: false,
            discovery_id: 0,
            discovery_error: None,
            discovery_at: None,
            pair_target: String::new(),
            pair_code: String::new(),
            pairing: false,
            pair_open: false,
            waiting_host: None,
            auto_scan_host: None,
            scan_input: String::new(),
            scan_id: 0,
            scan_cancel: CancelToken::default(),
            scan_progress: None,
            scan_results: Vec::new(),
            manual_wireless: false,
            connecting_wireless: false,
            error: None,
            message: None,
        }
    }
}

struct ConnectRequest {
    target: String,
    wireless: bool,
}

struct DrawInputs<'a> {
    manual_input: &'a mut String,
    connecting: bool,
    composing: bool,
    recent: &'a [String],
    wireless_connections: &'a [String],
}

enum Action {
    Connect(ConnectRequest),
    Pair,
    Scan(bool),
    Refresh,
    Reconnect(String),
    Close,
}

impl ConnectionDialog {
    fn begin(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        self.session += 1;
        self.lifetime = CancelToken::default();
        self.discovery_busy = false;
        self.discovery_at = None;
        self.discovery_error = None;
        self.services.clear();
        self.scan_results.clear();
        self.scan_progress = None;
        self.error = None;
        self.message = None;
    }

    fn close(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.session += 1;
        self.lifetime.cancel();
        self.scan_cancel.cancel();
        self.pair_code.clear();
        self.pairing = false;
        self.waiting_host = None;
        self.auto_scan_host = None;
    }

    fn discovering(&mut self, adb: &str) {
        if self.discovery_busy {
            return;
        }
        self.discovery_busy = true;
        self.discovery_id += 1;
        self.discovery_at = Some(Instant::now());
        let (tx, session, id, adb, cancel) = (
            self.tx.clone(),
            self.session,
            self.discovery_id,
            adb.to_owned(),
            self.lifetime.clone(),
        );
        thread::spawn(move || {
            let _ = tx.send(Event::Discovery(
                session,
                id,
                wireless::discover(&adb, &cancel),
            ));
        });
    }

    fn scanning(&self) -> bool {
        self.scan_progress.as_ref().is_some_and(|p| !p.done)
    }

    fn start_scan(&mut self, full: bool, recent: &[String], i18n: &I18n) {
        let ip = match wireless::parse_scan_ip(&self.scan_input) {
            Ok(ip) => ip,
            Err(key) => {
                self.error = Some(i18n.tr(key));
                return;
            }
        };
        self.scan_cancel.cancel();
        self.scan_cancel = CancelToken::default();
        self.scan_id += 1;
        self.scan_input = ip.to_string();
        self.scan_results.clear();
        self.error = None;
        self.tab = Tab::Scan;
        let options = wireless::ScanOptions::for_host(ip, recent, full);
        self.scan_progress = Some(ScanProgress {
            total: options.ports.len(),
            ..Default::default()
        });
        let (tx, session, id, cancel) = (
            self.tx.clone(),
            self.session,
            self.scan_id,
            self.scan_cancel.clone(),
        );
        thread::spawn(move || {
            wireless::scan(ip, options, &cancel, |update| {
                let _ = tx.send(Event::Scan(session, id, update));
            })
        });
    }

    fn start_pair(&mut self, adb: &str, i18n: &I18n) {
        if self.pairing {
            return;
        }
        let target = match wireless::parse_endpoint(&self.pair_target, true) {
            Ok(endpoint) => endpoint.to_string(),
            Err(key) => {
                self.error = Some(i18n.tr(key));
                return;
            }
        };
        let code = match wireless::parse_pairing_code(&self.pair_code) {
            Ok(code) => code,
            Err(key) => {
                self.error = Some(i18n.tr(key));
                return;
            }
        };
        self.scan_cancel.cancel();
        self.waiting_host = None;
        self.auto_scan_host = None;
        self.pair_target = target.clone();
        self.pair_code.clear();
        self.pairing = true;
        self.error = None;
        self.message = Some("connect.pairing");
        let (tx, session, adb, cancel) = (
            self.tx.clone(),
            self.session,
            adb.to_owned(),
            self.lifetime.clone(),
        );
        thread::spawn(move || {
            let result = wireless::pair(&adb, &target, &code, &cancel);
            let _ = tx.send(Event::Paired(session, target, result));
        });
    }

    fn poll(&mut self, adb: &str, recent: &[String], i18n: &I18n) -> Option<ConnectRequest> {
        let mut request = None;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Discovery(session, id, result)
                    if session == self.session && id == self.discovery_id =>
                {
                    self.discovery_busy = false;
                    match result {
                        Ok(services) => {
                            if self.tab == Tab::Scan
                                && self.scanning()
                                && let Ok(ip) = wireless::parse_scan_ip(&self.scan_input)
                            {
                                for service in &services {
                                    if service.address.ip() == ip
                                        && service.kind != ServiceKind::Pairing
                                        && !self
                                            .scan_results
                                            .iter()
                                            .any(|c| c.address == service.address)
                                    {
                                        self.scan_results.push(Candidate {
                                            address: service.address,
                                            kind: if service.kind == ServiceKind::Connect {
                                                AdbKind::Wireless
                                            } else {
                                                AdbKind::Legacy
                                            },
                                        });
                                    }
                                }
                            }
                            self.services = services;
                            self.discovery_error = None;
                        }
                        Err(err) => {
                            self.services.clear();
                            self.discovery_error = Some(failure_text(i18n, &err));
                        }
                    }
                }
                Event::Paired(session, target, result) if session == self.session => {
                    self.pairing = false;
                    match result {
                        Ok(()) => {
                            self.message = Some("connect.paired_waiting");
                            // Require a snapshot requested after pairing succeeded.
                            self.services.clear();
                            self.discovery_id += 1;
                            self.discovery_busy = false;
                            self.discovery_at = None;
                            if let Ok(endpoint) = wireless::parse_endpoint(&target, true)
                                && let Ok(ip) = endpoint.host.parse()
                            {
                                self.waiting_host = Some((ip, Instant::now()));
                            } else {
                                self.message = Some("connect.paired_manual");
                            }
                        }
                        Err(err) => {
                            self.message = None;
                            self.error = Some(failure_text(i18n, &err));
                        }
                    }
                }
                Event::Scan(session, id, update)
                    if session == self.session && id == self.scan_id =>
                {
                    match update {
                        ScanUpdate::Found(candidate) => {
                            if !self
                                .scan_results
                                .iter()
                                .any(|c| c.address == candidate.address)
                            {
                                self.scan_results.push(candidate);
                            }
                        }
                        ScanUpdate::Progress(progress) => {
                            if progress.done && self.auto_scan_host.take().is_some() {
                                if progress.cancelled || self.scan_cancel.cancelled() {
                                    self.message = Some("connect.scan_cancelled");
                                } else if self.scan_results.len() == 1 {
                                    let candidate = &self.scan_results[0];
                                    request = Some(ConnectRequest {
                                        target: candidate.address.to_string(),
                                        wireless: candidate.kind == AdbKind::Wireless,
                                    });
                                } else {
                                    self.message = Some("connect.choose_or_manual");
                                }
                            }
                            self.scan_progress = Some(progress);
                        }
                    }
                }
                _ => {} // stale results from closed dialogs / cancelled scans
            }
        }
        if let Some((ip, since)) = self.waiting_host {
            let services: Vec<_> = self
                .services
                .iter()
                .filter(|s| s.address.ip() == ip && s.kind == ServiceKind::Connect)
                .collect();
            if services.len() == 1 {
                request = Some(ConnectRequest {
                    target: services[0].address.to_string(),
                    wireless: true,
                });
                self.waiting_host = None;
            } else if services.len() > 1 {
                self.waiting_host = None;
                self.message = Some("connect.choose_or_manual");
            } else if since.elapsed() > Duration::from_secs(6) {
                self.waiting_host = None;
                self.scan_input = ip.to_string();
                self.auto_scan_host = Some(ip);
                self.message = Some("connect.paired_scanning");
                self.start_scan(false, recent, i18n);
            }
        }
        if self
            .discovery_at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
        {
            self.discovering(adb);
        }
        request
    }

    fn draw(&mut self, ctx: &egui::Context, i18n: &I18n, inputs: DrawInputs<'_>) -> Option<Action> {
        let DrawInputs {
            manual_input,
            connecting,
            composing,
            recent,
            wireless_connections,
        } = inputs;
        let mut action = None;
        let busy = connecting || self.pairing || self.waiting_host.is_some();
        egui::Window::new(i18n.tr("connect.title"))
            .id(egui::Id::new("connect_device_window"))
            .collapsible(false)
            .resizable(false)
            .default_width(570.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, key) in [
                        (Tab::Wireless, "connect.tab.wireless"),
                        (Tab::Scan, "connect.tab.scan"),
                        (Tab::Manual, "connect.tab.manual"),
                    ] {
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(i18n.tr(key)).selected(self.tab == tab),
                            )
                            .clicked()
                        {
                            self.tab = tab;
                            self.error = None;
                        }
                    }
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .id_salt("connection_body")
                    .max_height((ctx.screen_rect().height() - 230.0).clamp(220.0, 550.0))
                    .show(ui, |ui| {
                        ui.set_min_width(550.0);
                        match self.tab {
                            Tab::Wireless => {
                                ui.label(i18n.tr("connect.wireless_hint"));
                                ui.horizontal(|ui| {
                                    ui.strong(i18n.tr("connect.discovered"));
                                    if self.discovery_busy {
                                        ui.spinner();
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.discovery_busy,
                                            egui::Button::new(i18n.tr("connect.refresh")),
                                        )
                                        .clicked()
                                    {
                                        action = Some(Action::Refresh);
                                    }
                                });
                                if let Some(error) = &self.discovery_error {
                                    ui.small(error);
                                }
                                if self.services.is_empty() {
                                    ui.label(i18n.tr("connect.discovery_empty"));
                                }
                                for service in self.services.clone() {
                                    egui::Frame::new()
                                        .fill(Color32::from_rgb(245, 248, 253))
                                        .inner_margin(8.0)
                                        .corner_radius(6.0)
                                        .show(ui, |ui| {
                                            ui.set_min_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                ui.strong(service.address.to_string());
                                                ui.small(i18n.tr(match service.kind {
                                                    ServiceKind::Pairing => {
                                                        "connect.service.pairing"
                                                    }
                                                    ServiceKind::Connect => {
                                                        "connect.service.wireless"
                                                    }
                                                    ServiceKind::Legacy => "connect.service.legacy",
                                                }));
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if service.kind == ServiceKind::Pairing {
                                                            if ui
                                                                .add_enabled(
                                                                    !busy,
                                                                    egui::Button::new(
                                                                        i18n.tr(
                                                                            "connect.select_pair",
                                                                        ),
                                                                    ),
                                                                )
                                                                .clicked()
                                                            {
                                                                self.pair_target =
                                                                    service.address.to_string();
                                                                self.pair_open = true;
                                                                self.error = None;
                                                            }
                                                        } else if ui
                                                            .add_enabled(
                                                                !busy,
                                                                egui::Button::new(
                                                                    i18n.tr("connect.action"),
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            action = Some(Action::Connect(
                                                                ConnectRequest {
                                                                    target: service
                                                                        .address
                                                                        .to_string(),
                                                                    wireless: service.kind
                                                                        == ServiceKind::Connect,
                                                                },
                                                            ));
                                                        }
                                                    },
                                                );
                                            });
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&service.name).small(),
                                                )
                                                .wrap(),
                                            );
                                        });
                                    ui.add_space(4.0);
                                }
                                ui.add_space(6.0);
                                if ui
                                    .selectable_label(self.pair_open, i18n.tr("connect.pair_new"))
                                    .clicked()
                                {
                                    self.pair_open = !self.pair_open;
                                }
                                if self.pair_open {
                                    ui.label(i18n.tr("connect.pair_hint"));
                                    ui.add_enabled_ui(!busy, |ui| {
                                        ui.label(i18n.tr("connect.pair_address"));
                                        normalized_edit(
                                            ui,
                                            &mut self.pair_target,
                                            "pair_address",
                                            "192.168.0.8:37001",
                                            composing,
                                            false,
                                        );
                                        endpoint_feedback(
                                            ui,
                                            i18n,
                                            &self.pair_target,
                                            true,
                                            composing,
                                        );
                                        ui.label(i18n.tr("connect.pair_code"));
                                        ui.horizontal(|ui| {
                                            let code_response = normalized_edit(
                                                ui,
                                                &mut self.pair_code,
                                                "pair_code",
                                                "123456",
                                                composing,
                                                true,
                                            );
                                            let valid =
                                                wireless::parse_endpoint(&self.pair_target, true)
                                                    .is_ok()
                                                    && wireless::parse_pairing_code(
                                                        &self.pair_code,
                                                    )
                                                    .is_ok();
                                            if ui
                                                .add_enabled(
                                                    valid && !composing,
                                                    egui::Button::new(
                                                        i18n.tr("connect.pair_action"),
                                                    ),
                                                )
                                                .clicked()
                                                || (valid && submitted(ui, &code_response))
                                            {
                                                action = Some(Action::Pair);
                                            }
                                        });
                                        if !composing
                                            && !self.pair_code.is_empty()
                                            && wireless::parse_pairing_code(&self.pair_code)
                                                .is_err()
                                        {
                                            inline_error(ui, &i18n.tr("connect.error.code"));
                                        }
                                    });
                                }
                            }
                            Tab::Scan => {
                                ui.label(i18n.tr("connect.scan_hint"));
                                ui.add_enabled_ui(!busy && !self.scanning(), |ui| {
                                    ui.label(i18n.tr("connect.device_ip"));
                                    let response = normalized_edit(
                                        ui,
                                        &mut self.scan_input,
                                        "scan_ip",
                                        "192.168.0.8",
                                        composing,
                                        false,
                                    );
                                    let valid = wireless::parse_scan_ip(&self.scan_input).is_ok();
                                    if !composing && !self.scan_input.is_empty() && !valid {
                                        inline_error(ui, &i18n.tr("connect.error.ip"));
                                    }
                                    ui.horizontal(|ui| {
                                        if ui
                                            .add_enabled(
                                                valid && !composing,
                                                egui::Button::new(i18n.tr("connect.scan_quick")),
                                            )
                                            .clicked()
                                            || (valid && submitted(ui, &response))
                                        {
                                            action = Some(Action::Scan(false));
                                        }
                                        if ui
                                            .add_enabled(
                                                valid && !composing,
                                                egui::Button::new(i18n.tr("connect.scan_full")),
                                            )
                                            .clicked()
                                        {
                                            action = Some(Action::Scan(true));
                                        }
                                    });
                                });
                                if self.scanning()
                                    && ui.button(i18n.tr("connect.scan_stop")).clicked()
                                {
                                    self.scan_cancel.cancel();
                                    self.auto_scan_host = None;
                                    self.message = None;
                                }
                                if let Some(progress) = &self.scan_progress {
                                    let fraction =
                                        progress.tested as f32 / progress.total.max(1) as f32;
                                    let text = i18n.tr_args(
                                        "connect.scan_progress",
                                        &[
                                            ("tested", progress.tested.to_string()),
                                            ("total", progress.total.to_string()),
                                        ],
                                    );
                                    ui.add(egui::ProgressBar::new(fraction).text(text));
                                    if progress.done {
                                        ui.label(i18n.tr(if progress.cancelled {
                                            "connect.scan_cancelled"
                                        } else if progress.deadline_reached {
                                            "connect.scan_partial"
                                        } else if self.scan_results.is_empty() {
                                            "connect.scan_empty"
                                        } else {
                                            "connect.scan_done"
                                        }));
                                        if progress.timed_out > 0 {
                                            ui.small(i18n.tr_args(
                                                "connect.scan_timeouts",
                                                &[("count", progress.timed_out.to_string())],
                                            ));
                                        }
                                    }
                                }
                                for candidate in &self.scan_results {
                                    ui.horizontal(|ui| {
                                        ui.strong(candidate.address.to_string());
                                        ui.label(i18n.tr(if candidate.kind == AdbKind::Wireless {
                                            "connect.service.wireless"
                                        } else {
                                            "connect.service.legacy"
                                        }));
                                        if ui
                                            .add_enabled(
                                                !busy,
                                                egui::Button::new(i18n.tr("connect.action")),
                                            )
                                            .clicked()
                                        {
                                            action = Some(Action::Connect(ConnectRequest {
                                                target: candidate.address.to_string(),
                                                wireless: candidate.kind == AdbKind::Wireless,
                                            }));
                                        }
                                    });
                                }
                                ui.small(i18n.tr("connect.scan_pair_hint"));
                            }
                            Tab::Manual => {
                                ui.label(i18n.tr("connect.intro"));
                                ui.add_enabled_ui(!busy, |ui| {
                                    super::styled_checkbox(
                                        ui,
                                        true,
                                        &mut self.manual_wireless,
                                        i18n.tr("connect.manual_wireless"),
                                    );
                                    let response = normalized_edit(
                                        ui,
                                        manual_input,
                                        "manual_address",
                                        "192.168.0.8:5555",
                                        composing,
                                        false,
                                    );
                                    let valid = endpoint_feedback(
                                        ui,
                                        i18n,
                                        manual_input,
                                        self.manual_wireless,
                                        composing,
                                    );
                                    if ui
                                        .add_enabled(
                                            valid && !composing,
                                            egui::Button::new(i18n.tr("connect.action")),
                                        )
                                        .clicked()
                                        || (valid && submitted(ui, &response))
                                    {
                                        action = Some(Action::Connect(ConnectRequest {
                                            target: manual_input.clone(),
                                            wireless: self.manual_wireless,
                                        }));
                                    }
                                });
                            }
                        }
                        if !recent.is_empty() {
                            ui.separator();
                            ui.strong(i18n.tr("connect.recent"));
                            ui.horizontal_wrapped(|ui| {
                                for target in recent {
                                    let wireless =
                                        wireless::parse_endpoint(target, false).ok().is_some_and(
                                            |e| wireless_connections.contains(&e.to_string()),
                                        );
                                    let label = if wireless {
                                        i18n.tr_args(
                                            "connect.recent_wireless",
                                            &[("target", target.clone())],
                                        )
                                    } else {
                                        target.clone()
                                    };
                                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                                        action = Some(if wireless {
                                            Action::Reconnect(target.clone())
                                        } else {
                                            Action::Connect(ConnectRequest {
                                                target: target.clone(),
                                                wireless: false,
                                            })
                                        });
                                    }
                                }
                            });
                        }
                    });
                ui.add_space(8.0);
                if connecting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(i18n.tr("connect.connecting"));
                    });
                } else if let Some(key) = self.message {
                    ui.label(i18n.tr(key));
                }
                if let Some(error) = &self.error {
                    egui::ScrollArea::vertical()
                        .id_salt("connect_error")
                        .max_height(65.0)
                        .show(ui, |ui| {
                            inline_error(ui, error);
                        });
                }
                ui.separator();
                if ui
                    .add_enabled(!connecting, egui::Button::new(i18n.tr("connect.close")))
                    .clicked()
                {
                    action = Some(Action::Close);
                }
            });
        action
    }
}

impl Drop for ConnectionDialog {
    fn drop(&mut self) {
        self.lifetime.cancel();
        self.scan_cancel.cancel();
    }
}

pub(super) fn failure_text(i18n: &I18n, failure: &Failure) -> String {
    let text = i18n.tr(failure.key);
    if failure.detail.is_empty() {
        text
    } else {
        format!("{text}\n{}", failure.detail)
    }
}

fn normalized_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    id: &str,
    hint: &str,
    composing: bool,
    compact: bool,
) -> egui::Response {
    let widget_id = ui.make_persistent_id(egui::Id::new(id));
    // egui 0.31 advertises UIA ValuePattern but does not apply SetValue to
    // TextEdit. Honor replacement from assistive technology on these fields.
    let replacement = if ui.is_enabled() && !composing {
        ui.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::SetValue,
                    target,
                    data: Some(egui::accesskit::ActionData::Value(value)),
                }) if target.0 == widget_id.value() => Some(value.to_string()),
                _ => None,
            })
        })
    } else {
        None
    };
    let replaced = replacement.is_some();
    if let Some(value) = replacement {
        *text = wireless::normalize_input(&value);
    }
    let mut response = ui.add(
        egui::TextEdit::singleline(text)
            .id(widget_id)
            .hint_text(hint)
            .desired_width(if compact { 160.0 } else { f32::INFINITY }),
    );
    if replaced {
        response.mark_changed();
    }
    if !composing && (response.changed() || response.lost_focus()) {
        *text = wireless::normalize_input(text);
    }
    response
}

fn submitted(ui: &egui::Ui, response: &egui::Response) -> bool {
    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

fn inline_error(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(Color32::from_rgb(185, 48, 48))).wrap());
}

fn endpoint_feedback(
    ui: &mut egui::Ui,
    i18n: &I18n,
    input: &str,
    require_port: bool,
    composing: bool,
) -> bool {
    if composing || input.trim().is_empty() {
        return false;
    }
    match wireless::parse_endpoint(input, require_port) {
        Ok(endpoint) => {
            ui.small(i18n.tr_args(
                "connect.target_preview",
                &[("target", endpoint.to_string())],
            ));
            true
        }
        Err(key) => {
            inline_error(ui, &i18n.tr(key));
            false
        }
    }
}

impl AdbCollectorApp {
    pub(super) fn ui_connect_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_connect_dialog {
            self.connection.close();
            return;
        }
        self.connection.begin();
        let request = self.connection.poll(
            &self.config.adb_path,
            &self.config.recent_connections,
            &self.i18n,
        );
        let action = self.connection.draw(
            ctx,
            &self.i18n,
            DrawInputs {
                manual_input: &mut self.connect_target_input,
                connecting: self.connect_in_progress,
                composing: self.ime_enter_guard.is_composing(),
                recent: &self.config.recent_connections,
                wireless_connections: &self.config.wireless_connections,
            },
        );
        // A user action in this frame takes priority over an automatic retry.
        let action = action.or_else(|| request.map(Action::Connect));
        match action {
            Some(Action::Connect(request)) => {
                self.connection.scan_cancel.cancel();
                self.connection.waiting_host = None;
                self.connection.auto_scan_host = None;
                self.connection.connecting_wireless = request.wireless;
                self.connection.error = None;
                self.connection.message = None;
                self.start_device_connection(request.target);
            }
            Some(Action::Pair) => self
                .connection
                .start_pair(&self.config.adb_path, &self.i18n),
            Some(Action::Scan(full)) => {
                self.connection.waiting_host = None;
                self.connection.auto_scan_host = None;
                self.connection.message = None;
                self.connection.discovering(&self.config.adb_path);
                self.connection
                    .start_scan(full, &self.config.recent_connections, &self.i18n);
            }
            Some(Action::Refresh) => self.connection.discovering(&self.config.adb_path),
            Some(Action::Reconnect(target)) => {
                if let Ok(endpoint) = wireless::parse_endpoint(&target, false) {
                    if let Ok(ip) = endpoint.host.parse() {
                        self.connection.waiting_host = Some((ip, Instant::now()));
                        self.connection.tab = Tab::Wireless;
                        self.connection.message = Some("connect.rediscovering");
                        self.connection.discovery_at = None;
                        self.connection.services.clear();
                    } else {
                        self.connect_target_input = target;
                        self.connection.manual_wireless = true;
                        self.connection.tab = Tab::Manual;
                    }
                }
            }
            Some(Action::Close) => {
                self.show_connect_dialog = false;
                self.connection.close();
            }
            None => {}
        }
    }
}
