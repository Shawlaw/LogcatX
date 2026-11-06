use super::AdbCollectorApp;
use crate::{clipboard, text_input};
use eframe::egui;
use std::thread;

impl AdbCollectorApp {
    pub(super) fn open_text_dialog(&mut self, device_id: String) {
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

        self.text_device_id = Some(device_id);
        self.text_draft.clear();
        self.text_error = None;
        self.show_text_dialog = true;
    }

    pub(super) fn ui_text_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_text_dialog {
            return;
        }
        let Some(device_id) = self.text_device_id.clone() else {
            self.show_text_dialog = false;
            return;
        };
        let mut send = false;
        let mut cancel = false;

        egui::Window::new(self.tr("text.title"))
            .collapsible(false)
            .resizable(false)
            .fixed_size([560.0, 430.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label(self.tr_args(
                    "text.target",
                    &[("device", self.device_identity_label(&device_id))],
                ));
                ui.add_space(8.0);
                ui.label(self.tr("text.input"));
                let placeholder = self.tr("text.placeholder");
                let response = ui.add_sized(
                    [ui.available_width(), 170.0],
                    egui::TextEdit::multiline(&mut self.text_draft)
                        .desired_rows(7)
                        .hint_text(placeholder),
                );
                super::text_menu::text_edit_menu(&response, &mut self.text_draft, &self.i18n);
                if response.changed() {
                    self.text_error = None;
                }
                ui.add_space(4.0);
                ui.small(self.tr("text.hint"));
                if let Some(error) = &self.text_error {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(error).color(egui::Color32::from_rgb(192, 57, 43)),
                    );
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(self.tr("text.paste")).clicked() {
                        match self.read_pc_clipboard_text() {
                            Ok(Some(text)) => {
                                self.text_draft = text;
                                self.text_error = None;
                            }
                            Ok(None) => {
                                self.text_error = Some(self.tr("clipboard.error.no_pc_text"))
                            }
                            Err(err) => self.text_error = Some(err),
                        }
                    }
                });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(self.tr("text.send")).clicked() {
                        send = true;
                    }
                    if ui.button(self.tr("text.cancel")).clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel {
            self.show_text_dialog = false;
            self.text_device_id = None;
            self.text_error = None;
        } else if send {
            let text = self.text_draft.clone();
            if text.is_empty() {
                self.text_error = Some(self.tr("text.empty"));
            } else {
                match self.start_text_task(&device_id, text) {
                    Ok(()) => {
                        self.show_text_dialog = false;
                        self.text_device_id = None;
                        self.text_error = None;
                    }
                    Err(err) => self.text_error = Some(err),
                }
            }
        }
    }

    /// Read the PC clipboard now and type it into the device's focused field.
    pub(super) fn send_pc_clipboard_text(&mut self, device_id: String) {
        match self.read_pc_clipboard_text() {
            Ok(Some(text)) => {
                if let Err(err) = self.start_text_task(&device_id, text) {
                    self.set_error(err);
                }
            }
            Ok(None) => self.set_error(self.tr("clipboard.error.no_pc_text")),
            Err(err) => self.set_error(err),
        }
    }

    /// Put the PC clipboard text into the device clipboard (true clipboard
    /// write; the user still chooses when to paste).
    pub(super) fn write_pc_clipboard(&mut self, device_id: String) {
        let text = match self.read_pc_clipboard_text() {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.set_error(self.tr("clipboard.error.no_pc_text"));
                return;
            }
            Err(err) => {
                self.set_error(err);
                return;
            }
        };
        let serial = match self.claim_tool_task(&device_id) {
            Ok(serial) => serial,
            Err(err) => {
                self.set_error(err);
                return;
            }
        };
        self.set_info(self.tr_args(
            "clipboard.status.writing",
            &[("device", self.transport_identity_label(&serial))],
        ));
        let adb_path = self.config.adb_path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = clipboard::write_text(&adb_path, &serial, &text);
            let _ = tx.send(crate::models::AppEvent::ClipboardWriteFinished { serial, result });
        });
    }

    /// Read the device clipboard text into the PC clipboard.
    pub(super) fn read_device_clipboard(&mut self, device_id: String) {
        let serial = match self.claim_tool_task(&device_id) {
            Ok(serial) => serial,
            Err(err) => {
                self.set_error(err);
                return;
            }
        };
        self.set_info(self.tr_args(
            "clipboard.status.reading",
            &[("device", self.transport_identity_label(&serial))],
        ));
        let adb_path = self.config.adb_path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = clipboard::read_text(&adb_path, &serial);
            let _ = tx.send(crate::models::AppEvent::ClipboardReadFinished { serial, result });
        });
    }

    pub(super) fn handle_text_input_finished(
        &mut self,
        serial: String,
        result: Result<text_input::TextInputOutcome, String>,
    ) {
        self.finish_tool_task(&serial);
        let device = self.transport_identity_label(&serial);
        match result {
            Ok(outcome) => {
                let key = if outcome.clipboard_pastes > 0 {
                    "text.status.sent_fallback"
                } else {
                    "text.status.sent"
                };
                self.set_info(self.tr_args(
                    key,
                    &[
                        ("device", device),
                        ("chars", outcome.chars.to_string()),
                        ("fallbacks", outcome.clipboard_pastes.to_string()),
                    ],
                ));
            }
            Err(err) => self.set_error(
                self.tr_args("text.status.failed", &[("device", device), ("error", err)]),
            ),
        }
    }

    pub(super) fn handle_clipboard_read_finished(
        &mut self,
        serial: String,
        result: Result<Option<String>, String>,
    ) {
        self.finish_tool_task(&serial);
        let device = self.transport_identity_label(&serial);
        match result {
            Ok(Some(text)) => match self.write_pc_clipboard_text(&text) {
                Ok(()) => self.set_info(self.tr_args(
                    "clipboard.status.read",
                    &[
                        ("device", device),
                        ("chars", text.chars().count().to_string()),
                    ],
                )),
                Err(err) => self.set_error(err),
            },
            Ok(None) => {
                self.set_info(self.tr_args("clipboard.status.empty", &[("device", device)]))
            }
            Err(err) => self.set_error(self.tr_args(
                "clipboard.status.failed",
                &[("device", device), ("error", err)],
            )),
        }
    }

    pub(super) fn handle_clipboard_write_finished(
        &mut self,
        serial: String,
        result: Result<(), String>,
    ) {
        self.finish_tool_task(&serial);
        let device = self.transport_identity_label(&serial);
        match result {
            Ok(()) => {
                self.set_info(self.tr_args("clipboard.status.written", &[("device", device)]))
            }
            Err(err) => self.set_error(self.tr_args(
                "clipboard.status.failed",
                &[("device", device), ("error", err)],
            )),
        }
    }

    fn start_text_task(&mut self, device_id: &str, text: String) -> Result<(), String> {
        let serial = self.claim_tool_task(device_id)?;
        self.set_info(self.tr_args(
            "text.status.sending",
            &[("device", self.transport_identity_label(&serial))],
        ));
        let adb_path = self.config.adb_path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = text_input::send_text(&adb_path, &serial, &text, true);
            let _ = tx.send(crate::models::AppEvent::TextInputFinished { serial, result });
        });
        Ok(())
    }

    fn claim_tool_task(&mut self, device_id: &str) -> Result<String, String> {
        if self.tool_task_serial.is_some() {
            return Err(self.tr("device_tool.busy"));
        }
        let serial = self
            .device_primary_transport_serial(device_id)
            .ok_or_else(|| "The selected device has no usable ADB transport.".to_owned())?;
        self.tool_task_serial = Some(serial.clone());
        Ok(serial)
    }

    fn finish_tool_task(&mut self, serial: &str) {
        if self.tool_task_serial.as_deref() == Some(serial) {
            self.tool_task_serial = None;
        }
    }

    pub(super) fn read_pc_clipboard_text(&self) -> Result<Option<String>, String> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|err| format!("Failed to open the PC clipboard: {err}"))?;
        match clipboard.get_text() {
            Ok(text) if !text.is_empty() => Ok(Some(text)),
            Ok(_) => Ok(None),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(format!("Failed to read the PC clipboard: {err}")),
        }
    }

    pub(super) fn write_pc_clipboard_text(&self, text: &str) -> Result<(), String> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|err| format!("Failed to open the PC clipboard: {err}"))?;
        clipboard
            .set_text(text)
            .map_err(|err| format!("Failed to write the PC clipboard: {err}"))
    }
}
