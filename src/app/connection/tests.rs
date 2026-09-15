use super::*;
use crate::{ime::ImeEnterGuard, wireless::tests::Fixture};

struct Harness {
    ctx: egui::Context,
    dialog: ConnectionDialog,
    i18n: I18n,
    manual: String,
    guard: ImeEnterGuard,
    text: Vec<(String, egui::Rect)>,
}

impl Harness {
    fn new() -> Self {
        let mut h = Self {
            ctx: Default::default(),
            dialog: Default::default(),
            i18n: I18n::new("en"),
            manual: String::new(),
            guard: Default::default(),
            text: Vec::new(),
        };
        h.dialog.begin();
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        h
    }

    fn frame(&mut self, events: Vec<egui::Event>) -> Option<Action> {
        let mut action = None;
        let output = self.ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1100.0, 720.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                self.guard.frame(ctx);
                action = self.dialog.draw(
                    ctx,
                    &self.i18n,
                    DrawInputs {
                        manual_input: &mut self.manual,
                        connecting: false,
                        composing: self.guard.is_composing(),
                        recent: &[],
                        wireless_connections: &[],
                    },
                );
            },
        );
        self.text.clear();
        fn collect(shape: &egui::Shape, text: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => text.push((
                    t.galley.text().to_owned(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        collect(s, text);
                    }
                }
                _ => {}
            }
        }
        for shape in &output.shapes {
            collect(&shape.shape, &mut self.text);
        }
        action
    }

    fn click(&mut self, text: &str) -> Option<Action> {
        // Layout can change size after changing tabs; let the anchored window settle.
        for _ in 0..3 {
            self.frame(Vec::new());
        }
        let pos = self
            .text
            .iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| {
                panic!(
                    "missing {text}; available {:?}",
                    self.text.iter().map(|(t, _)| t).collect::<Vec<_>>()
                )
            })
            .1
            .center();
        self.frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        self.frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }])
    }

    fn enter() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }
    }
}

#[test]
fn ui_e2e_full_width_ime_commit_then_enter_connects_without_losing_input() {
    let mut h = Harness::new();
    h.click("Manual connection");
    h.click("192.168.0.8:5555");
    let action = h.frame(vec![
        Harness::enter(),
        egui::Event::Ime(egui::ImeEvent::Commit(
            "１２７。０。０。１：５５５５".into(),
        )),
    ]);
    assert!(action.is_none(), "IME Enter must not submit");
    assert_eq!(h.manual, "127.0.0.1:5555");
    let action = h.frame(vec![Harness::enter()]);
    let Some(Action::Connect(request)) = action else {
        panic!("regular Enter must connect");
    };
    assert_eq!(request.target, "127.0.0.1:5555");
    assert!(!request.wireless);
    let fixture = Fixture::new("");
    crate::adb::connect_device(&fixture.adb, &request.target).unwrap();
}

#[test]
fn ui_e2e_invalid_pasted_port_cannot_submit() {
    let mut h = Harness::new();
    h.click("Manual connection");
    h.click("192.168.0.8:5555");
    h.frame(vec![egui::Event::Paste(
        "１９２。１６８。０。８：６５５３６".into(),
    )]);
    assert_eq!(h.manual, "192.168.0.8:65536");
    assert!(h.frame(vec![Harness::enter()]).is_none());
    assert!(
        h.text
            .iter()
            .any(|(t, _)| t.contains("between 1 and 65535"))
    );
    assert!(h.click("Connect").is_none());
}

#[test]
fn ui_e2e_pairing_code_stdin_then_fresh_dynamic_port() {
    let fixture = Fixture::new("before_pair=127.0.0.1:39000\nconnect=127.0.0.1:39001");
    let mut h = Harness::new();
    h.dialog.services = wireless::discover(&fixture.adb, &CancelToken::default()).unwrap();
    h.click("Enter code");
    assert_eq!(h.dialog.pair_target, "127.0.0.1:37001");
    h.click("123456");
    h.frame(vec![egui::Event::Paste("０１２３４５".into())]);
    assert_eq!(h.dialog.pair_code, "012345");
    assert!(matches!(h.click("Pair and connect"), Some(Action::Pair)));
    h.dialog.start_pair(&fixture.adb, &h.i18n);
    assert!(h.dialog.pair_code.is_empty());
    let start = Instant::now();
    let request = loop {
        if let Some(request) = h.dialog.poll(&fixture.adb, &[], &h.i18n) {
            break request;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "pairing did not finish: {:?}",
            h.dialog.error
        );
        thread::sleep(Duration::from_millis(20));
        h.frame(Vec::new());
    };
    assert_eq!(request.target, "127.0.0.1:39001");
    assert!(request.wireless);
    crate::adb::connect_device(&fixture.adb, &request.target).unwrap();
}

#[test]
fn cancelled_dialog_ignores_all_previous_results() {
    let mut dialog = ConnectionDialog::default();
    dialog.begin();
    let session = dialog.session;
    let lifetime = dialog.lifetime.clone();
    dialog.pair_code = "012345".into();
    dialog.close();
    dialog.begin();
    assert!(lifetime.cancelled());
    assert!(dialog.pair_code.is_empty());
    dialog.discovery_at = Some(Instant::now());
    dialog
        .tx
        .send(Event::Paired(session, "127.0.0.1:37001".into(), Ok(())))
        .unwrap();
    dialog
        .tx
        .send(Event::Discovery(
            session,
            dialog.discovery_id,
            Ok(wireless::parse_mdns(
                "x _adb-tls-connect._tcp 127.0.0.1:37002",
            )),
        ))
        .unwrap();
    dialog
        .tx
        .send(Event::Scan(
            session,
            dialog.scan_id,
            ScanUpdate::Found(Candidate {
                address: "127.0.0.1:37002".parse().unwrap(),
                kind: AdbKind::Wireless,
            }),
        ))
        .unwrap();
    assert!(dialog.poll("unused", &[], &I18n::new("en")).is_none());
    assert!(
        dialog.services.is_empty()
            && dialog.scan_results.is_empty()
            && dialog.waiting_host.is_none()
    );
}

#[test]
fn multiple_connection_services_require_selection() {
    let mut dialog = ConnectionDialog::default();
    dialog.begin();
    dialog.discovery_at = Some(Instant::now());
    dialog.waiting_host = Some(("127.0.0.1".parse().unwrap(), Instant::now()));
    dialog.services = wireless::parse_mdns(
        "a _adb-tls-connect._tcp 127.0.0.1:37002\nb _adb-tls-connect._tcp 127.0.0.1:37003",
    );
    assert!(dialog.poll("unused", &[], &I18n::new("en")).is_none());
    assert!(dialog.waiting_host.is_none());
    assert_eq!(dialog.message, Some("connect.choose_or_manual"));
}

#[test]
fn accessibility_value_replacement_normalizes_and_respects_disabled_fields() {
    let ctx = egui::Context::default();
    let mut text = String::new();
    let mut id = egui::Id::NULL;
    let _ = ctx.run(Default::default(), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            id = normalized_edit(ui, &mut text, "address", "", false, false).id;
        });
    });
    let event = || {
        egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
            action: egui::accesskit::Action::SetValue,
            target: id.value().into(),
            data: Some(egui::accesskit::ActionData::Value(
                "１２７。０。０。１：５５５５".into(),
            )),
        })
    };
    for enabled in [false, true] {
        let _ = ctx.run(
            egui::RawInput {
                events: vec![event()],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if !enabled {
                        ui.disable();
                    }
                    let response = normalized_edit(ui, &mut text, "address", "", false, false);
                    assert_eq!(response.changed(), enabled);
                });
            },
        );
        assert_eq!(text, if enabled { "127.0.0.1:5555" } else { "" });
    }
}

#[test]
fn stopping_automatic_scan_clears_stale_message_and_does_not_auto_connect() {
    let mut h = Harness::new();
    h.dialog.tab = Tab::Scan;
    h.dialog.discovery_at = Some(Instant::now());
    h.dialog.auto_scan_host = Some("127.0.0.1".parse().unwrap());
    h.dialog.message = Some("connect.paired_scanning");
    h.dialog.scan_progress = Some(ScanProgress {
        total: 100,
        ..Default::default()
    });
    h.dialog.scan_results.push(Candidate {
        address: "127.0.0.1:39001".parse().unwrap(),
        kind: AdbKind::Wireless,
    });
    h.click("Stop scan");
    assert!(h.dialog.scan_cancel.cancelled());
    assert!(h.dialog.auto_scan_host.is_none());
    assert!(h.dialog.message.is_none());
    h.dialog
        .tx
        .send(Event::Scan(
            h.dialog.session,
            h.dialog.scan_id,
            ScanUpdate::Progress(ScanProgress {
                total: 100,
                done: true,
                cancelled: true,
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(h.dialog.poll("unused", &[], &h.i18n).is_none());
    h.frame(Vec::new());
    assert!(
        h.text
            .iter()
            .any(|(text, _)| text == &h.i18n.tr("connect.scan_cancelled"))
    );
    assert!(
        !h.text
            .iter()
            .any(|(text, _)| text == &h.i18n.tr("connect.paired_scanning"))
    );
    assert_eq!(h.dialog.scan_results.len(), 1);
}

#[test]
fn cancelled_automatic_scan_callback_updates_message_without_auto_connecting() {
    let mut dialog = ConnectionDialog::default();
    dialog.begin();
    dialog.discovery_at = Some(Instant::now());
    dialog.auto_scan_host = Some("127.0.0.1".parse().unwrap());
    dialog.message = Some("connect.paired_scanning");
    dialog
        .tx
        .send(Event::Scan(
            dialog.session,
            dialog.scan_id,
            ScanUpdate::Progress(ScanProgress {
                done: true,
                cancelled: true,
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(dialog.poll("unused", &[], &I18n::new("en")).is_none());
    assert_eq!(dialog.message, Some("connect.scan_cancelled"));
    assert!(dialog.auto_scan_host.is_none());
}

#[test]
fn adb_failures_keep_localization_keys_and_raw_diagnostics() {
    let fixture = Fixture::new("mode=hang");
    let timeout = wireless::run_adb(
        &fixture.adb,
        &["connect", "127.0.0.1:5555"],
        None,
        Duration::from_millis(150),
        &CancelToken::default(),
    )
    .unwrap_err();
    assert_eq!(timeout.key, "connect.error.timeout");
    let cancel = CancelToken::default();
    cancel.cancel();
    let cancelled = wireless::run_adb(
        &fixture.adb,
        &["connect", "127.0.0.1:5555"],
        None,
        Duration::from_secs(1),
        &cancel,
    )
    .unwrap_err();
    assert_eq!(cancelled.key, "connect.error.cancelled");
    for language in ["zh-CN", "en"] {
        let i18n = I18n::new(language);
        assert_eq!(
            failure_text(&i18n, &timeout),
            i18n.tr("connect.error.timeout")
        );
        assert_eq!(
            failure_text(&i18n, &cancelled),
            i18n.tr("connect.error.cancelled")
        );
    }
    let fixture = Fixture::new("mode=connect-fail");
    let error = crate::adb::connect_device(&fixture.adb, "127.0.0.1:5555").unwrap_err();
    assert_eq!(error.key, "connect.error.connection");
    let text = failure_text(&I18n::new("zh-CN"), &error);
    assert!(text.starts_with("连接失败"));
    assert!(text.contains("failed to connect to 127.0.0.1:5555"));
}
