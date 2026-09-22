//! Files page: browse and manage device files (PRD §4-§6, §9-§12).
//!
//! All listings/mutations run on background threads through RemoteFs and
//! arrive as `AppEvent`s; the UI thread only renders state, so no operation
//! can freeze the page (PRD §40). File transfers enqueue on the shared
//! TransferManager — the page never spawns its own push/pull.

use super::AdbCollectorApp;
use crate::{
    models::AppEvent,
    remote_fs::{RemoteEntry, RemoteEntryKind, RemoteFs, RemotePath},
    transfer::{TransferState, TransferTask},
};
use eframe::egui::{self, Align, Color32, RichText};
use std::thread;

/// Quick locations offered on the Files page (PRD §6.1).
pub(crate) const QUICK_PATHS: &[&str] = &[
    "/sdcard",
    "/sdcard/Download",
    "/sdcard/DCIM",
    "/sdcard/Pictures",
    "/sdcard/Documents",
    "/data/local/tmp",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FilesSortField {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FilesSort {
    pub field: FilesSortField,
    pub ascending: bool,
}

impl Default for FilesSort {
    fn default() -> Self {
        Self {
            field: FilesSortField::Name,
            ascending: true,
        }
    }
}

/// Sort entries for display: directories first, then by the chosen field,
/// ties broken by name ascending (PRD §6.1). Unknown mtimes always sort
/// last, regardless of direction.
pub(crate) fn sort_entries(entries: &mut [RemoteEntry], sort: FilesSort) {
    entries.sort_by(|a, b| {
        let dir_rank = |entry: &RemoteEntry| !matches!(entry.kind, RemoteEntryKind::Directory);
        match dir_rank(a).cmp(&dir_rank(b)) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
        let order = match sort.field {
            FilesSortField::Name => {
                let by_name = a.name.to_lowercase().cmp(&b.name.to_lowercase());
                if sort.ascending {
                    by_name
                } else {
                    by_name.reverse()
                }
            }
            FilesSortField::Size => {
                let by_size = a.size.cmp(&b.size);
                if sort.ascending {
                    by_size
                } else {
                    by_size.reverse()
                }
            }
            FilesSortField::Modified => match (a.modified_unix_secs, b.modified_unix_secs) {
                (Some(x), Some(y)) => {
                    if sort.ascending {
                        x.cmp(&y)
                    } else {
                        y.cmp(&x)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        };
        match order {
            std::cmp::Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            other => other,
        }
    });
}

impl AdbCollectorApp {
    /// First entry to the Files page: pick a device and open /sdcard.
    /// Runs its auto-navigation exactly ONCE per page entry — a failed
    /// listing must surface its error instead of re-requesting every frame
    /// (the field-trial log showed ~8 retries/second hammering on errors).
    pub(crate) fn files_enter_page(&mut self) {
        if self.files_serial.is_none() {
            let serial = self
                .selected_serial
                .clone()
                .or_else(|| self.ready_device_ids().first().cloned())
                .and_then(|id| self.device_primary_transport_serial(&id));
            self.files_serial = serial;
        }
        if self.files_page_opened {
            return;
        }
        self.files_page_opened = true;
        // /sdcard rather than /: several devices restrict `cd /`, and the
        // PRD's normal-user paths all live under /sdcard anyway.
        let start = if self.files_cwd.as_str() == "/" {
            RemotePath::new("/sdcard").unwrap_or_else(|_| RemotePath::root())
        } else {
            self.files_cwd.clone()
        };
        self.files_navigate(start);
    }

    pub(crate) fn files_navigate(&mut self, path: RemotePath) {
        if path != self.files_cwd {
            log::info!(
                "Files: navigating {} -> {} (device {})",
                self.files_cwd,
                path,
                self.files_serial.as_deref().unwrap_or("-")
            );
            self.files_history.push(self.files_cwd.clone());
            if self.files_history.len() > 64 {
                self.files_history.remove(0);
            }
            self.files_cwd = path;
        }
        self.files_request_listing();
    }

    pub(crate) fn files_refresh(&mut self) {
        self.files_request_listing();
    }

    pub(crate) fn files_navigate_back(&mut self) {
        if let Some(previous) = self.files_history.pop() {
            self.files_cwd = previous;
            self.files_request_listing();
        }
    }

    fn files_request_listing(&mut self) {
        let Some(serial) = self.files_serial.clone() else {
            return;
        };
        self.files_list_generation += 1;
        let generation = self.files_list_generation;
        self.files_list_loading = true;
        self.files_error = None;
        // Entering a different directory drops the old listing immediately
        // so the user sees one loading state instead of stale rows that
        // suddenly swap (field-trial bug 5).
        if self.files_entries_path.as_ref() != Some(&self.files_cwd) {
            self.files_entries.clear();
            self.files_selected.clear();
        }
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let path = self.files_cwd.clone();
        let run_as = self.files_run_as_package.clone();
        thread::spawn(move || {
            let remote = match &run_as {
                Some(package) => RemoteFs::new_run_as(&adb_path, &serial, package),
                None => RemoteFs::new(&adb_path, &serial),
            };
            let result = remote.list(&path).map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::FilesListed { generation, result });
        });
    }

    /// Fire a short mutation (mkdir/rename/delete/move) and refresh after.
    pub(crate) fn files_spawn_op(
        &mut self,
        op: impl FnOnce(RemoteFs) -> Result<(), String> + Send + 'static,
    ) {
        let Some(serial) = self.files_serial.clone() else {
            return;
        };
        self.files_op_generation += 1;
        let generation = self.files_op_generation;
        let tx = self.tx.clone();
        let adb_path = self.config.adb_path.clone();
        let run_as = self.files_run_as_package.clone();
        thread::spawn(move || {
            let remote = match &run_as {
                Some(package) => RemoteFs::new_run_as(&adb_path, &serial, package),
                None => RemoteFs::new(&adb_path, &serial),
            };
            let result = op(remote).map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::FilesOpFinished { generation, result });
        });
    }

    pub(crate) fn files_handle_listed(
        &mut self,
        generation: u64,
        result: Result<Vec<RemoteEntry>, String>,
    ) {
        if generation != self.files_list_generation {
            return; // stale listing (PRD §18 discipline)
        }
        self.files_list_loading = false;
        match result {
            Ok(entries) => {
                let directories = entries
                    .iter()
                    .filter(|entry| matches!(entry.kind, RemoteEntryKind::Directory))
                    .count();
                log::info!(
                    "Files: listed {} ({} entries, {} directories)",
                    self.files_cwd,
                    entries.len(),
                    directories
                );
                self.files_error = None;
                self.files_entries_path = Some(self.files_cwd.clone());
                self.files_entries = entries;
                self.files_selected.clear();
            }
            Err(err) => {
                log::warn!("Files: listing {} failed: {err}", self.files_cwd);
                self.files_entries.clear();
                self.files_entries_path = None;
                self.files_selected.clear();
                self.files_error = Some(err);
            }
        }
    }

    pub(crate) fn files_handle_op_finished(&mut self, generation: u64, result: Result<(), String>) {
        if generation != self.files_op_generation {
            return;
        }
        match result {
            Ok(()) => self.files_refresh(),
            Err(err) => {
                self.set_error(err);
                self.files_refresh();
            }
        }
    }

    fn files_selected_paths(&self) -> Vec<RemotePath> {
        self.files_entries
            .iter()
            .filter(|entry| self.files_selected.contains(&entry.name))
            .filter_map(|entry| self.files_cwd.join(&entry.name).ok())
            .collect()
    }

    fn files_transfer_source_label(task: &TransferTask) -> String {
        let path = match &task.operation {
            crate::transfer::TransferOperation::Push { source, .. } => source.clone(),
            crate::transfer::TransferOperation::Pull { source, .. } => {
                std::path::PathBuf::from(source)
            }
        };
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }

    pub(crate) fn ui_files_page(&mut self, ui: &mut egui::Ui) {
        self.files_enter_page();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.ui_files_header(ui);
                ui.add_space(8.0);
                self.ui_files_toolbar(ui);
                ui.add_space(8.0);
                self.ui_files_quick_paths(ui);
                ui.add_space(10.0);
                self.ui_files_listing(ui);
                ui.add_space(12.0);
                self.ui_files_transfers(ui);
            });
        self.ui_files_dialogs(ui.ctx());
    }

    fn ui_files_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new(self.tr("files.title")).size(20.0).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                egui::ComboBox::from_id_salt("files_device")
                    .selected_text(self.files_device_label())
                    .width(ui.available_width().min(240.0))
                    .show_ui(ui, |ui| {
                        let ids = self.ready_device_ids();
                        for id in ids {
                            let label = self.device_identity_label(&id);
                            let transport = self.device_primary_transport_serial(&id);
                            let selected = transport
                                .as_ref()
                                .is_some_and(|serial| Some(serial) == self.files_serial.as_ref());
                            if ui.selectable_label(selected, label).clicked()
                                && let Some(serial) = transport
                            {
                                self.files_serial = Some(serial);
                                self.files_run_as_package = None;
                                let cwd = RemotePath::new("/sdcard")
                                    .unwrap_or_else(|_| RemotePath::root());
                                self.files_navigate(cwd);
                            }
                        }
                    });
            });
        });
    }

    fn files_device_label(&self) -> String {
        let Some(serial) = &self.files_serial else {
            return self.tr("files.no_device");
        };
        let label = self
            .devices
            .iter()
            .find(|device| device.transport_serials.iter().any(|t| t == serial))
            .map(|device| self.device_identity_label(&device.info.identity_key))
            .unwrap_or_else(|| serial.clone());
        if self.files_run_as_package.is_some() {
            format!("{label} · {}", self.tr("files.run_as_suffix"))
        } else {
            label
        }
    }

    /// Map a transport serial back to the merged device identity.
    pub(crate) fn device_id_for_transport(&self, transport: &str) -> Option<String> {
        self.devices
            .iter()
            .find(|device| device.transport_serials.iter().any(|t| t == transport))
            .map(|device| device.info.identity_key.clone())
    }

    fn ui_files_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let back = egui::Button::new("←").min_size(egui::vec2(30.0, 26.0));
            if ui
                .add_enabled(self.files_history.last().is_some(), back)
                .on_hover_text(self.tr("files.back"))
                .clicked()
            {
                self.files_navigate_back();
            }
            let up = egui::Button::new("↑").min_size(egui::vec2(30.0, 26.0));
            if ui
                .add_enabled(self.files_cwd.parent().is_some(), up)
                .on_hover_text(self.tr("files.parent"))
                .clicked()
                && let Some(parent) = self.files_cwd.parent()
            {
                let parent = parent.clone();
                self.files_navigate(parent);
            }
            if ui
                .add(egui::Button::new("⟳").min_size(egui::vec2(30.0, 26.0)))
                .on_hover_text(self.tr("files.refresh"))
                .clicked()
            {
                self.files_refresh();
            }

            // Breadcrumb: every component is a jump target (PRD §6.1).
            ui.separator();
            let components = self.files_cwd.components();
            let mut jump = RemotePath::root();
            if ui
                .selectable_label(self.files_cwd.as_str() == "/", "/")
                .clicked()
            {
                let root = RemotePath::root();
                self.files_navigate(root);
            }
            for component in components {
                if let Ok(next) = jump.join(&component) {
                    jump = next;
                }
                let target = jump.clone();
                if ui
                    .selectable_label(self.files_cwd == target, component)
                    .clicked()
                {
                    self.files_navigate(target);
                }
                ui.label("/");
            }

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let favorited = self.files_cwd_favorite().is_some();
                let star = if favorited { "★" } else { "☆" };
                if ui
                    .button(star)
                    .on_hover_text(self.tr("files.favorite_toggle"))
                    .clicked()
                {
                    if favorited {
                        self.files_remove_favorite();
                    } else {
                        self.files_add_favorite();
                    }
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label(self.tr("files.path_label"));
            let mut go = false;
            let response = egui::TextEdit::singleline(&mut self.files_path_input)
                .desired_width(ui.available_width() - 80.0)
                .show(ui)
                .response;
            if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if ui.button(self.tr("files.go")).clicked() {
                go = true;
            }
            if go {
                match RemotePath::new(self.files_path_input.trim()) {
                    Ok(path) => {
                        let path = path.clone();
                        self.files_path_input = path.as_str().to_owned();
                        self.files_navigate(path);
                    }
                    Err(err) => self.set_error(err.to_string()),
                }
            }
        });
        if self.files_path_input.trim().is_empty()
            || self.files_path_input.trim() == self.files_cwd.as_str()
        {
            self.files_path_input = self.files_cwd.as_str().to_owned();
        }
    }

    fn files_cwd_favorite(&self) -> Option<usize> {
        let serial = self.files_serial.as_deref()?;
        self.config.file_favorites.iter().position(|favorite| {
            favorite.path == self.files_cwd.as_str() && favorite.device.as_deref() == Some(serial)
        })
    }

    fn files_add_favorite(&mut self) {
        let Some(serial) = self.files_serial.clone() else {
            return;
        };
        let favorite = crate::config::FileFavorite {
            path: self.files_cwd.as_str().to_owned(),
            name: self
                .files_cwd
                .file_name()
                .map(str::to_owned)
                .unwrap_or_else(|| self.files_cwd.as_str().to_owned()),
            device: Some(serial),
        };
        self.config.file_favorites.push(favorite);
        if let Err(err) = self.persist_config() {
            self.set_error(err);
        }
    }

    fn files_remove_favorite(&mut self) {
        if let Some(index) = self.files_cwd_favorite() {
            self.config.file_favorites.remove(index);
            if let Err(err) = self.persist_config() {
                self.set_error(err);
            }
        }
    }

    fn ui_files_quick_paths(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(self.tr("files.quick_paths")).small().weak());
            for path in QUICK_PATHS {
                if ui
                    .add(
                        egui::Button::new(RichText::new(*path).small())
                            .small()
                            .fill(Color32::from_rgb(244, 246, 251)),
                    )
                    .clicked()
                    && let Ok(target) = RemotePath::new(path)
                {
                    self.files_run_as_package = None;
                    let target = target.clone();
                    self.files_navigate(target);
                }
            }
            ui.separator();
            ui.label(RichText::new(self.tr("files.run_as_label")).small().weak());
            let run_as_response = egui::TextEdit::singleline(&mut self.files_run_as_input)
                .hint_text("com.example.app")
                .desired_width(180.0)
                .show(ui)
                .response;
            let open_run_as = ui.button(self.tr("files.run_as_open")).clicked()
                || (run_as_response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if open_run_as {
                let package = self.files_run_as_input.trim().to_owned();
                if package.is_empty() {
                    self.set_error(self.tr("files.run_as_empty"));
                } else {
                    self.files_run_as_package = Some(package);
                    if let Ok(target) = RemotePath::new("/data/data") {
                        let target = target.clone();
                        self.files_navigate(target);
                    }
                }
            }
        });

        // Favorites chips (persisted; PRD §6.2).
        if !self.config.file_favorites.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(self.tr("files.favorites")).small().weak());
                let favorites = self.config.file_favorites.clone();
                for (index, favorite) in favorites.iter().enumerate() {
                    let chip = format!("★ {}", favorite.name);
                    let chip_response =
                        ui.add(egui::Button::new(RichText::new(chip).small()).small());
                    if chip_response.clicked() {
                        if let Some(serial) = &self.files_serial
                            && favorite.device.as_deref() != Some(serial.as_str())
                        {
                            // Device-level favorite for another device: switch.
                            if let Some(serial) = favorite.device.clone() {
                                self.files_serial = Some(serial);
                            }
                        }
                        if let Ok(target) = RemotePath::new(&favorite.path) {
                            let target = target.clone();
                            self.files_navigate(target);
                        }
                    }
                    if chip_response
                        .on_hover_text(format!(
                            "{} [{}]",
                            favorite.path,
                            favorite.device.as_deref().unwrap_or("-")
                        ))
                        .secondary_clicked()
                        && self.config.file_favorites.len() > index
                    {
                        self.config.file_favorites.remove(index);
                        if let Err(err) = self.persist_config() {
                            self.set_error(err);
                        }
                    }
                }
            });
        }
    }

    fn ui_files_listing(&mut self, ui: &mut egui::Ui) {
        if self.files_serial.is_none() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(self.tr("files.no_device_hint"));
            });
            return;
        }
        if let Some(error) = &self.files_error {
            ui.colored_label(Color32::from_rgb(190, 60, 60), format!("⚠ {error}"));
            if ui.button(self.tr("files.retry")).clicked() {
                self.files_refresh();
            }
            ui.add_space(6.0);
        }
        if self.files_list_loading {
            ui.label(RichText::new(self.tr("files.loading")).weak());
        }

        // Operation bar (PRD §5/§6.6).
        ui.horizontal(|ui| {
            if ui
                .button(format!("⬆ {}", self.tr("files.upload")))
                .clicked()
            {
                self.files_pick_upload();
            }
            let selected = self.files_selected_paths();
            let download_enabled = !selected.is_empty();
            if ui
                .add_enabled(
                    download_enabled,
                    egui::Button::new(format!("⬇ {}", self.tr("files.download"))),
                )
                .clicked()
            {
                self.files_pick_download();
            }
            if ui
                .button(format!("＋ {}", self.tr("files.mkdir")))
                .clicked()
            {
                self.files_mkdir_input.clear();
                self.files_show_mkdir = true;
            }
            let rename_target = self.files_single_selected_name();
            if ui
                .add_enabled(
                    rename_target.is_some(),
                    egui::Button::new(self.tr("files.rename")),
                )
                .clicked()
                && let Some(name) = rename_target.clone()
            {
                self.files_rename_input = name.clone();
                self.files_rename_target = Some(name);
            }
            if ui
                .add_enabled(
                    rename_target.is_some(),
                    egui::Button::new(self.tr("files.move")),
                )
                .clicked()
                && let Some(name) = rename_target
            {
                self.files_rename_input = self.files_cwd.as_str().to_owned();
                self.files_rename_target = Some(name);
                self.files_move_mode = true;
            }
            let selection_count = self.files_selected.len();
            let any_dir = self.files_selected_contains_directory();
            if ui
                .add_enabled(
                    selection_count > 0,
                    egui::Button::new(format!("🗑 {}", self.tr("files.delete"))),
                )
                .clicked()
            {
                self.files_delete_pending = Some(crate::models::FilesDeletePending {
                    names: self.files_selected.iter().cloned().collect(),
                    contains_directory: any_dir,
                });
            }
            if selection_count > 0 {
                ui.label(
                    RichText::new(self.tr_args(
                        "files.selected_count",
                        &[("count", selection_count.to_string())],
                    ))
                    .small()
                    .weak(),
                );
            }
        });

        ui.add_space(4.0);
        let mut sorted = self.files_entries.clone();
        sort_entries(&mut sorted, self.files_sort);

        // Fixed info-column widths shared by header and rows so the columns
        // line up one-to-one (field-trial bug 4): name is flexible, then
        // size / modified / kind in the same order everywhere.
        const COL_SIZE: f32 = 84.0;
        const COL_MODIFIED: f32 = 122.0;
        const COL_KIND: f32 = 46.0;

        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(233, 236, 242)))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let all_selected = !sorted.is_empty()
                        && sorted
                            .iter()
                            .all(|entry| self.files_selected.contains(&entry.name));
                    if ui.checkbox(&mut all_selected.clone(), "").changed() {
                        if all_selected {
                            self.files_selected =
                                sorted.iter().map(|entry| entry.name.clone()).collect();
                        } else {
                            self.files_selected.clear();
                        }
                    }
                    self.files_sort_button(ui, FilesSortField::Name, self.tr("files.col_name"));
                    // Flexible spacer pushes the info columns to the right.
                    let remaining = (ui.available_width()
                        - COL_SIZE
                        - COL_MODIFIED
                        - COL_KIND
                        - 3.0 * ui.spacing().item_spacing.x)
                        .max(0.0);
                    ui.add_space(remaining);
                    self.files_sort_button(ui, FilesSortField::Size, self.tr("files.col_size"));
                    self.files_sort_button_sized(
                        ui,
                        FilesSortField::Modified,
                        self.tr("files.col_modified"),
                        Some(COL_MODIFIED),
                    );
                    ui.add_sized(
                        [COL_KIND, ui.available_height()],
                        egui::Label::new(RichText::new(self.tr("files.col_kind")).small().strong())
                            .selectable(false),
                    );
                });
                ui.separator();

                if self.files_list_loading {
                    // Loading state instead of stale rows: entering a
                    // directory shows one centered spinner until the new
                    // listing lands (field-trial bug 5).
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(self.tr("files.loading")).weak());
                        });
                    });
                    ui.add_space(12.0);
                    return;
                }

                if sorted.is_empty() {
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(self.tr("files.empty_dir")).weak());
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 8.0)
                    .auto_shrink([false, true])
                    .show_rows(ui, 26.0, sorted.len(), |ui, range| {
                        for index in range {
                            let entry = &sorted[index];
                            let is_dir = matches!(entry.kind, RemoteEntryKind::Directory);
                            let mut selected = self.files_selected.contains(&entry.name);
                            let mut open = false;
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut selected, "").changed() {
                                    if selected {
                                        self.files_selected.insert(entry.name.clone());
                                    } else {
                                        self.files_selected.remove(&entry.name);
                                    }
                                }
                                let icon = match entry.kind {
                                    RemoteEntryKind::Directory => "📁",
                                    RemoteEntryKind::File => "📄",
                                    RemoteEntryKind::Symlink => "🔗",
                                    RemoteEntryKind::Other(_) => "❔",
                                };
                                ui.label(icon);
                                let name_response = ui.selectable_label(
                                    false,
                                    RichText::new(&entry.name)
                                        .strong()
                                        .color(Color32::from_rgb(45, 52, 66)),
                                );
                                if name_response.clicked() {
                                    if selected {
                                        self.files_selected.remove(&entry.name);
                                    } else {
                                        self.files_selected.insert(entry.name.clone());
                                    }
                                }
                                if is_dir && name_response.double_clicked() {
                                    open = true;
                                }
                                let remaining = (ui.available_width()
                                    - COL_SIZE
                                    - COL_MODIFIED
                                    - COL_KIND
                                    - 3.0 * ui.spacing().item_spacing.x)
                                    .max(0.0);
                                ui.add_space(remaining);
                                ui.add_sized(
                                    [COL_SIZE, ui.available_height()],
                                    egui::Label::new(
                                        RichText::new(if is_dir {
                                            "—".to_owned()
                                        } else {
                                            desktop_fs::format_bytes(entry.size)
                                        })
                                        .small()
                                        .weak(),
                                    )
                                    .selectable(false),
                                );
                                ui.add_sized(
                                    [COL_MODIFIED, ui.available_height()],
                                    egui::Label::new(
                                        RichText::new(Self::files_modified_label(
                                            entry.modified_unix_secs,
                                        ))
                                        .small()
                                        .weak(),
                                    )
                                    .selectable(false),
                                );
                                ui.add_sized(
                                    [COL_KIND, ui.available_height()],
                                    egui::Label::new(
                                        RichText::new(Self::files_kind_label(&entry.kind))
                                            .small()
                                            .weak(),
                                    )
                                    .selectable(false),
                                );
                            });
                            if open && let Ok(target) = self.files_cwd.join(&entry.name) {
                                let target = target.clone();
                                self.files_navigate(target);
                            }
                        }
                    });
            });
    }

    /// Sort header: first click selects the field ascending, clicking the
    /// active field toggles direction (PRD §6.1 sorting).
    fn files_sort_button(&mut self, ui: &mut egui::Ui, field: FilesSortField, label: String) {
        self.files_sort_button_sized(ui, field, label, None)
    }

    fn files_sort_button_sized(
        &mut self,
        ui: &mut egui::Ui,
        field: FilesSortField,
        label: String,
        width: Option<f32>,
    ) {
        let marker = if self.files_sort.field == field {
            if self.files_sort.ascending {
                " ▲"
            } else {
                " ▼"
            }
        } else {
            ""
        };
        let button = egui::Button::new(RichText::new(format!("{label}{marker}")).small().strong());
        let response = match width {
            Some(width) => ui.add_sized([width, ui.available_height()], button),
            None => ui.add(button),
        };
        if response.clicked() {
            if self.files_sort.field == field {
                self.files_sort.ascending = !self.files_sort.ascending;
            } else {
                self.files_sort = FilesSort {
                    field,
                    ascending: true,
                };
            }
        }
    }

    fn files_kind_label(kind: &RemoteEntryKind) -> String {
        match kind {
            RemoteEntryKind::Directory => "DIR".to_owned(),
            RemoteEntryKind::File => "FILE".to_owned(),
            RemoteEntryKind::Symlink => "LINK".to_owned(),
            RemoteEntryKind::Other(raw) => {
                if raw.is_empty() {
                    "?".to_owned()
                } else {
                    raw.to_uppercase()
                }
            }
        }
    }

    fn files_modified_label(secs: Option<u64>) -> String {
        secs.map(|secs| {
            use chrono::TimeZone;
            chrono::Local
                .timestamp_opt(secs as i64, 0)
                .single()
                .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".to_owned())
        })
        .unwrap_or_else(|| "-".to_owned())
    }

    fn files_single_selected_name(&self) -> Option<String> {
        if self.files_selected.len() != 1 {
            return None;
        }
        self.files_selected.iter().next().cloned()
    }

    fn files_selected_contains_directory(&self) -> bool {
        self.files_entries.iter().any(|entry| {
            self.files_selected.contains(&entry.name)
                && matches!(entry.kind, RemoteEntryKind::Directory)
        })
    }

    fn files_pick_upload(&mut self) {
        if self.files_serial.is_none() {
            return;
        }
        if let Some(paths) = rfd::FileDialog::new()
            .set_title(self.tr("files.upload_dialog_title"))
            .pick_files()
        {
            for path in paths {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".to_owned());
                if let Ok(destination) = self.files_cwd.join(&name) {
                    let serial = self.files_serial.clone().unwrap_or_default();
                    self.transfers
                        .enqueue_push(serial, path, destination.as_str().to_owned());
                }
            }
        }
    }

    fn files_pick_download(&mut self) {
        let Some(serial) = self.files_serial.clone() else {
            return;
        };
        let Some(folder) = rfd::FileDialog::new()
            .set_title(self.tr("files.download_dialog_title"))
            .pick_folder()
        else {
            return;
        };
        for path in self.files_selected_paths() {
            let name = path
                .file_name()
                .map(|name| name.to_owned())
                .unwrap_or_else(|| "download".to_owned());
            self.transfers.enqueue_pull(
                serial.clone(),
                path.as_str().to_owned(),
                folder.join(name),
            );
        }
    }

    fn ui_files_transfers(&mut self, ui: &mut egui::Ui) {
        let tasks = self.transfers.snapshot();
        if tasks.is_empty() {
            return;
        }
        let active = tasks.iter().any(|task| task.state.is_active());
        let failed = tasks.iter().any(|task| task.state == TransferState::Failed);
        let finished = tasks.iter().any(|task| task.state.is_terminal());

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.tr("files.transfers"))
                    .size(14.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if finished && ui.button(self.tr("files.transfers_clear")).clicked() {
                    self.transfers.clear_finished();
                }
                if failed && ui.button(self.tr("files.transfers_retry_failed")).clicked() {
                    self.transfers.retry_failed();
                }
                if active && ui.button(self.tr("files.transfers_cancel_all")).clicked() {
                    self.transfers.cancel_all();
                }
            });
        });
        for task in &tasks {
            ui.horizontal(|ui| {
                let direction = match task.operation {
                    crate::transfer::TransferOperation::Push { .. } => "↑",
                    crate::transfer::TransferOperation::Pull { .. } => "↓",
                };
                ui.label(direction);
                ui.label(Self::files_transfer_source_label(task));
                match task.state {
                    TransferState::Queued => {
                        let _ = ui.label(
                            RichText::new(self.tr("files.transfer_queued"))
                                .small()
                                .weak(),
                        );
                    }
                    TransferState::Preparing => {
                        let _ = ui.label(
                            RichText::new(self.tr("files.transfer_preparing"))
                                .small()
                                .weak(),
                        );
                    }
                    TransferState::Running => {
                        let fraction = task.progress_fraction();
                        let (bytes, speed) = (
                            desktop_fs::format_bytes(task.bytes_transferred),
                            task.speed_bps
                                .map(desktop_fs::format_bytes)
                                .unwrap_or_else(|| "-".to_owned()),
                        );
                        match fraction {
                            Some(fraction) => {
                                let total = task
                                    .bytes_total
                                    .map(desktop_fs::format_bytes)
                                    .unwrap_or_else(|| "?".to_owned());
                                let progress = egui::ProgressBar::new(fraction as f32)
                                    .desired_width(ui.available_width() * 0.4)
                                    .text(format!("{bytes} / {total} · {speed}/s"));
                                ui.add(progress);
                            }
                            None => {
                                let _ = ui.label(
                                    RichText::new(self.tr("files.transfer_indeterminate"))
                                        .small()
                                        .weak(),
                                );
                                let _ = ui.label(
                                    RichText::new(format!("{bytes} · {speed}/s")).small().weak(),
                                );
                            }
                        }
                    }
                    TransferState::Completed => {
                        let _ = ui.label(
                            RichText::new(self.tr("files.transfer_completed"))
                                .small()
                                .color(Color32::from_rgb(46, 125, 50)),
                        );
                    }
                    TransferState::Failed => {
                        let _ = ui.label(
                            RichText::new(format!(
                                "{} {}",
                                self.tr("files.transfer_failed"),
                                task.error.as_deref().unwrap_or("")
                            ))
                            .small()
                            .color(Color32::from_rgb(190, 60, 60)),
                        );
                    }
                    TransferState::Cancelled => {
                        let _ = ui.label(
                            RichText::new(self.tr("files.transfer_cancelled"))
                                .small()
                                .weak(),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if task.state.is_active()
                        && ui.button(self.tr("files.transfer_cancel")).clicked()
                    {
                        self.transfers.cancel(task.id);
                    }
                    if task.state == TransferState::Failed
                        && ui.button(self.tr("files.transfer_retry")).clicked()
                    {
                        self.transfers.retry(task.id);
                    }
                });
            });
        }
    }

    fn ui_files_dialogs(&mut self, ctx: &egui::Context) {
        if self.files_show_mkdir {
            let mut open = true;
            egui::Window::new(self.tr("files.mkdir"))
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(self.tr("files.mkdir_hint"));
                    ui.text_edit_singleline(&mut self.files_mkdir_input);
                    ui.horizontal(|ui| {
                        if ui.button(self.tr("files.create")).clicked() {
                            let name = self.files_mkdir_input.trim().to_owned();
                            if name.is_empty() {
                                self.set_error(self.tr("files.mkdir_empty"));
                            } else if let Ok(target) = self.files_cwd.join(&name) {
                                self.files_show_mkdir = false;
                                self.files_spawn_op(move |fs| {
                                    fs.mkdir(&target).map_err(|err| err.to_string())
                                });
                            }
                        }
                        if ui.button(self.tr("files.cancel")).clicked() {
                            self.files_show_mkdir = false;
                        }
                    });
                });
            if !open {
                self.files_show_mkdir = false;
            }
        }

        if let Some(target_name) = self.files_rename_target.clone() {
            let mut open = true;
            let title = if self.files_move_mode {
                self.tr("files.move")
            } else {
                self.tr("files.rename")
            };
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    if self.files_move_mode {
                        ui.label(self.tr_args("files.move_hint", &[("name", target_name.clone())]));
                    } else {
                        ui.label(
                            self.tr_args("files.rename_hint", &[("name", target_name.clone())]),
                        );
                    }
                    ui.text_edit_singleline(&mut self.files_rename_input);
                    ui.horizontal(|ui| {
                        if ui.button(self.tr("files.apply")).clicked() {
                            let input = self.files_rename_input.trim().to_owned();
                            let result = if self.files_move_mode {
                                RemotePath::new(&input).and_then(|dir| dir.join(&target_name))
                            } else {
                                self.files_cwd.join(&input)
                            };
                            match result {
                                Ok(destination) => {
                                    if let Ok(source) = self.files_cwd.join(&target_name) {
                                        self.files_spawn_op(move |fs| {
                                            fs.rename(&source, &destination)
                                                .map_err(|err| err.to_string())
                                        });
                                    }
                                    self.files_rename_target = None;
                                    self.files_move_mode = false;
                                }
                                Err(err) => self.set_error(err.to_string()),
                            }
                        }
                        if ui.button(self.tr("files.cancel")).clicked() {
                            self.files_rename_target = None;
                            self.files_move_mode = false;
                        }
                    });
                });
            if !open {
                self.files_rename_target = None;
                self.files_move_mode = false;
            }
        }

        if let Some(pending) = self.files_delete_pending.clone() {
            let mut open = true;
            egui::Window::new(self.tr("files.delete"))
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    // One confirmation for the whole batch, stating the count
                    // (PRD §6.6).
                    ui.label(self.tr_args(
                        "files.delete_confirm",
                        &[("count", pending.names.len().to_string())],
                    ));
                    if pending.contains_directory {
                        ui.label(self.tr("files.delete_confirm_dirs"));
                    }
                    let preview = pending
                        .names
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    ui.label(RichText::new(preview).small().weak());
                    ui.horizontal(|ui| {
                        if ui.button(self.tr("files.delete_confirm_button")).clicked() {
                            let targets = self.files_selected_paths();
                            self.files_delete_pending = None;
                            self.files_spawn_op(move |fs| {
                                for target in targets {
                                    fs.delete(&target).map_err(|err| err.to_string())?;
                                }
                                Ok(())
                            });
                        }
                        if ui.button(self.tr("files.cancel")).clicked() {
                            self.files_delete_pending = None;
                        }
                    });
                });
            if !open {
                self.files_delete_pending = None;
            }
        }

        // APK drop onto the Files page: install, upload here, or cancel
        // (PRD §6.5). The transfer/install both ride shared infrastructure.
        if let Some(paths) = self.files_pending_apk_drop.clone() {
            if paths.is_empty() {
                self.files_pending_apk_drop = None;
                return;
            }
            let mut open = true;
            egui::Window::new(self.tr("files.apk_drop_title"))
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(
                        self.tr_args("files.apk_drop_hint", &[("count", paths.len().to_string())]),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(self.tr("files.apk_drop_install")).clicked() {
                            let device_id = self
                                .files_serial
                                .as_deref()
                                .and_then(|serial| self.device_id_for_transport(serial));
                            let Some(device_id) = device_id else {
                                self.files_pending_apk_drop = None;
                                return;
                            };
                            let payload = crate::app::DroppedPayload {
                                apk_paths: paths.clone(),
                                install_apks: true,
                                ..Default::default()
                            };
                            self.files_pending_apk_drop = None;
                            self.start_drop_task(device_id, payload);
                        }
                        if ui.button(self.tr("files.apk_drop_upload")).clicked() {
                            for path in &paths {
                                let name = path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "app.apk".to_owned());
                                if let (Some(serial), Ok(destination)) =
                                    (self.files_serial.clone(), self.files_cwd.join(&name))
                                {
                                    self.transfers.enqueue_push(
                                        serial,
                                        path.clone(),
                                        destination.as_str().to_owned(),
                                    );
                                }
                            }
                            self.files_pending_apk_drop = None;
                        }
                        if ui.button(self.tr("files.cancel")).clicked() {
                            self.files_pending_apk_drop = None;
                        }
                    });
                });
            if !open {
                self.files_pending_apk_drop = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_fs::RemoteEntry;

    fn entry(name: &str, dir: bool, size: u64, mtime: Option<u64>) -> RemoteEntry {
        RemoteEntry {
            name: name.to_owned(),
            kind: if dir {
                RemoteEntryKind::Directory
            } else {
                RemoteEntryKind::File
            },
            size,
            modified_unix_secs: mtime,
        }
    }

    #[test]
    fn sorting_puts_directories_first_then_field() {
        let mut entries = vec![
            entry("b.txt", false, 30, Some(300)),
            entry("z-dir", true, 0, None),
            entry("a.txt", false, 10, Some(100)),
        ];
        sort_entries(
            &mut entries,
            FilesSort {
                field: FilesSortField::Name,
                ascending: true,
            },
        );
        assert_eq!(entries[0].name, "z-dir"); // directories first
        assert_eq!(entries[1].name, "a.txt");
        assert_eq!(entries[2].name, "b.txt");
    }

    /// Empirical reproduction of the Files-page double-click with synthetic
    /// pointer input (no window needed). Mirrors the exact widget structure
    /// of a listing row: `ScrollArea::show_rows` + `ui.horizontal` +
    /// `selectable_label`, with the app's widened double-click window.
    #[test]
    fn directory_name_double_click_registers_in_ui_harness() {
        use egui::{CentralPanel, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
        use std::sync::atomic::Ordering;

        let ctx = egui::Context::default();
        // Same option the app sets at startup (AdbCollectorApp::new).
        ctx.options_mut(|options| options.input_options.max_double_click_delay = 0.5);

        let label_rect = std::sync::Arc::new(std::sync::Mutex::new(Rect::NOTHING));
        let singles = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let double_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let run_frame = |time: f64, events: Vec<egui::Event>| {
            let rect_cell = label_rect.clone();
            let singles = singles.clone();
            let double_fired = double_fired.clone();
            let input = RawInput {
                time: Some(time),
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show_rows(ui, 26.0, 1, |ui, range| {
                            for _index in range {
                                ui.horizontal(|ui| {
                                    let mut checked = false;
                                    ui.checkbox(&mut checked, "");
                                    ui.label("📁");
                                    let response = ui.selectable_label(
                                        false,
                                        egui::RichText::new("Directory Name").strong(),
                                    );
                                    *rect_cell.lock().unwrap() = response.rect;
                                    if response.clicked() {
                                        *singles.lock().unwrap() += 1;
                                    }
                                    if response.double_clicked() {
                                        double_fired.store(true, Ordering::SeqCst);
                                    }
                                });
                            }
                        });
                });
            });
        };

        // Frame 0: discover the label position (no input).
        run_frame(0.0, Vec::new());
        let pos = label_rect.lock().unwrap().center();
        assert!(label_rect.lock().unwrap().is_positive(), "label rendered");

        let press = || egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        };
        let release = || egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        };
        let moved = |pos: Pos2| egui::Event::PointerMoved(pos);

        // First click: press at t=0.05, release at t=0.10.
        run_frame(0.05, vec![moved(pos), press()]);
        run_frame(0.10, vec![release()]);
        assert_eq!(*singles.lock().unwrap(), 1, "single click registers");
        assert!(!double_fired.load(Ordering::SeqCst));

        // Second click 250ms later — within the 0.5s window.
        run_frame(0.35, vec![press()]);
        run_frame(0.40, vec![release()]);

        assert_eq!(*singles.lock().unwrap(), 2, "second click registers");
        assert!(
            double_fired.load(Ordering::SeqCst),
            "double click must fire with the widened window"
        );
    }

    #[test]
    fn sorting_by_size_desc_and_modified() {
        let mut entries = vec![
            entry("small", false, 10, Some(1)),
            entry("big", false, 900, Some(2)),
        ];
        sort_entries(
            &mut entries,
            FilesSort {
                field: FilesSortField::Size,
                ascending: false,
            },
        );
        assert_eq!(entries[0].name, "big");

        sort_entries(
            &mut entries,
            FilesSort {
                field: FilesSortField::Modified,
                ascending: true,
            },
        );
        assert_eq!(entries[0].name, "small");

        // Unknown mtime always sorts last.
        let mut entries = vec![
            entry("known", false, 1, Some(5)),
            entry("unknown", false, 1, None),
        ];
        sort_entries(
            &mut entries,
            FilesSort {
                field: FilesSortField::Modified,
                ascending: false,
            },
        );
        assert_eq!(entries[0].name, "known");
    }
}
