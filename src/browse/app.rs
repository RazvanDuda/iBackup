//! The explorer window: a domain sidebar, a file list and a preview pane.
//!
//! Everything that touches the disk (opening a backup, decoding a preview,
//! reading a SQLite table, extracting files) runs on one worker thread; the
//! UI thread only draws state and posts [`Cmd`]s. Replies come back as
//! [`Msg`]s, so a slow HEIC decode never freezes the window.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Frame, Layout, Margin, RichText, Rounding, Sense, Stroke,
    Ui, Vec2,
};

use crate::ui::theme as t;
use crate::util;

use super::model::{
    self, Backup, BackupInfo, Entry, ExtractOutcome, Kind, KindFilter, OpenError, Sort,
};
use super::preview::{self, Preview, TableData};

// ---------------------------------------------------------------------------
// Worker protocol
// ---------------------------------------------------------------------------

enum Cmd {
    Open {
        path: PathBuf,
        generation: u64,
    },
    Preview {
        backup: Arc<Backup>,
        index: u32,
    },
    Table {
        backup: Arc<Backup>,
        index: u32,
        table: String,
    },
    Extract {
        backup: Arc<Backup>,
        indices: Vec<u32>,
        out: PathBuf,
        label: String,
    },
    OpenExternal {
        backup: Arc<Backup>,
        index: u32,
    },
}

enum Msg {
    /// Rows indexed so far while a backup is being opened.
    Progress(usize),
    Opened {
        generation: u64,
        result: Result<Backup, OpenError>,
    },
    PreviewReady {
        index: u32,
        preview: Preview,
    },
    TableReady {
        index: u32,
        table: String,
        data: Result<TableData, String>,
    },
    ExtractProgress {
        done: usize,
        total: usize,
        bytes: u64,
        label: String,
    },
    ExtractDone {
        outcome: ExtractOutcome,
        label: String,
    },
    ExternalDone {
        path: PathBuf,
        error: Option<String>,
    },
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Msg>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Open { path, generation } => {
                let mut last = Instant::now();
                let result = Backup::open_with_progress(&path, &mut |rows| {
                    if last.elapsed() > Duration::from_millis(70) {
                        last = Instant::now();
                        let _ = tx.send(Msg::Progress(rows));
                    }
                });
                let _ = tx.send(Msg::Opened { generation, result });
            }
            Cmd::Preview { backup, index } => {
                let preview = match backup.entries.get(index as usize) {
                    Some(entry) => preview::load(&backup, entry),
                    None => Preview::Error("That file is no longer part of this backup.".into()),
                };
                let _ = tx.send(Msg::PreviewReady { index, preview });
            }
            Cmd::Table {
                backup,
                index,
                table,
            } => {
                let data = match backup.entries.get(index as usize) {
                    Some(entry) => {
                        let path = backup.blob_path(entry);
                        preview::load_table(&path, &table).map_err(|e| format!("{e:#}"))
                    }
                    None => Err("That file is no longer part of this backup.".into()),
                };
                let _ = tx.send(Msg::TableReady {
                    index,
                    table,
                    data,
                });
            }
            Cmd::Extract {
                backup,
                indices,
                out,
                label,
            } => {
                let mut last = Instant::now();
                let outcome = backup.extract(&indices, &out, &mut |done, total, bytes| {
                    if done == total || last.elapsed() > Duration::from_millis(80) {
                        last = Instant::now();
                        let _ = tx.send(Msg::ExtractProgress {
                            done,
                            total,
                            bytes,
                            label: label.clone(),
                        });
                    }
                });
                let _ = tx.send(Msg::ExtractDone { outcome, label });
            }
            Cmd::OpenExternal { backup, index } => {
                let Some(entry) = backup.entries.get(index as usize) else {
                    continue;
                };
                let mut name = entry.file_id.to_string();
                let ext = entry.extension();
                if !ext.is_empty() {
                    name.push('.');
                    name.push_str(ext);
                }
                let dir = std::env::temp_dir().join("ibackup-explorer");
                let dest = dir.join(util::sanitize_component(&name));
                let result = fs::create_dir_all(&dir)
                    .and_then(|_| fs::copy(backup.blob_path(entry), &dest))
                    .and_then(|_| super::open_in_system(&dest));
                let _ = tx.send(Msg::ExternalDone {
                    path: dest,
                    error: result.err().map(|e| e.to_string()),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum FolderPurpose {
    Open,
    Extract { indices: Vec<u32>, label: String },
}

struct ExtractState {
    done: usize,
    total: usize,
    bytes: u64,
    label: String,
}

struct ExtractSummary {
    files: usize,
    bytes: u64,
    missing: usize,
    skipped: usize,
    out: PathBuf,
    errors: Vec<String>,
}

struct TableState {
    table: String,
    data: Option<TableData>,
    error: Option<String>,
}

pub struct ExploreApp {
    ctx: egui::Context,
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    icon: Option<egui::TextureHandle>,

    backup: Option<Arc<Backup>>,
    domain_labels: Vec<String>,
    domain_order: Vec<u32>,
    domain: Option<u32>,
    cwd: String,

    search: String,
    filter: KindFilter,
    sort: Sort,
    view: Vec<u32>,
    searching: bool,
    truncated: bool,
    view_dirty: bool,

    selected: Option<u32>,
    preview: Option<(u32, Arc<Preview>)>,
    preview_pending: Option<u32>,
    texture: Option<(u32, egui::TextureHandle)>,
    table: Option<TableState>,
    table_pending: Option<(u32, String)>,

    open_gen: u64,
    opening: Option<(PathBuf, usize)>,
    open_error: Option<OpenError>,

    extract: Option<ExtractState>,
    last_extract: Option<ExtractSummary>,
    folder_rx: Option<(Receiver<Option<PathBuf>>, FolderPurpose)>,

    recents: Vec<PathBuf>,
    toast: Option<(String, Instant)>,
    info: Option<(String, String)>,
    scroll_to_selected: bool,
}

impl ExploreApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        crate::ui::theme::install(&cc.egui_ctx);
        let (cmd_tx, cmd_rx) = channel();
        let (msg_tx, msg_rx) = channel();
        let _ = thread::Builder::new()
            .name("ibackup-explorer".into())
            .spawn(move || worker(cmd_rx, msg_tx));

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            tx: cmd_tx,
            rx: msg_rx,
            icon: crate::ui::load_icon(&cc.egui_ctx),
            backup: None,
            domain_labels: Vec::new(),
            domain_order: Vec::new(),
            domain: None,
            cwd: String::new(),
            search: String::new(),
            filter: KindFilter::All,
            sort: Sort::Name,
            view: Vec::new(),
            searching: false,
            truncated: false,
            view_dirty: false,
            selected: None,
            preview: None,
            preview_pending: None,
            texture: None,
            table: None,
            table_pending: None,
            open_gen: 0,
            opening: None,
            open_error: None,
            extract: None,
            last_extract: None,
            folder_rx: None,
            recents: load_recents(),
            toast: None,
            info: None,
            scroll_to_selected: false,
        };
        let initial = initial.or_else(default_backup_dir);
        if let Some(path) = initial {
            app.open_path(path);
        }
        app
    }

    // -- state transitions -------------------------------------------------

    fn open_path(&mut self, path: PathBuf) {
        self.open_gen = self.open_gen.wrapping_add(1);
        let generation = self.open_gen;
        self.backup = None;
        self.open_error = None;
        self.opening = Some((path.clone(), 0));
        self.selected = None;
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.extract = None;
        self.last_extract = None;
        self.view.clear();
        self.search.clear();
        self.domain = None;
        self.cwd.clear();
        let _ = self.tx.send(Cmd::Open { path, generation });
    }

    fn adopt(&mut self, backup: Backup) {
        self.recents = remember_recent(&backup.dir);
        self.domain_labels = backup
            .domains
            .iter()
            .map(|d| model::friendly_domain(&d.name))
            .collect();
        let mut order: Vec<u32> = (0..backup.domains.len() as u32).collect();
        let labels = &self.domain_labels;
        order.sort_by_key(|&i| labels[i as usize].to_lowercase());
        self.domain_order = order;
        self.backup = Some(Arc::new(backup));
        self.opening = None;
        self.open_error = None;
        self.domain = None;
        self.cwd.clear();
        self.search.clear();
        self.filter = KindFilter::All;
        self.sort = Sort::Name;
        self.selected = None;
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.view_dirty = true;
    }

    fn close_backup(&mut self) {
        self.backup = None;
        self.open_error = None;
        self.opening = None;
        self.view.clear();
        self.selected = None;
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.domain = None;
        self.cwd.clear();
        self.search.clear();
    }

    fn rebuild_view(&mut self, backup: &Backup) {
        self.view_dirty = false;
        let query = self.search.trim().to_string();
        if query.is_empty() {
            self.searching = false;
            self.truncated = false;
            self.view = match self.domain {
                Some(domain) => backup.listing(domain, &self.cwd, self.filter, self.sort),
                None => Vec::new(),
            };
        } else {
            self.searching = true;
            let (ids, truncated) = backup.search(&query, self.domain, self.filter, 5000);
            self.view = ids;
            self.truncated = truncated;
        }
    }

    fn open_domain(&mut self, domain: Option<u32>) {
        self.domain = domain;
        self.cwd.clear();
        self.selected = None;
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.view_dirty = true;
    }

    fn set_cwd(&mut self, cwd: String) {
        self.cwd = cwd;
        self.selected = None;
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.view_dirty = true;
    }

    fn go_up(&mut self) {
        let parent = self
            .cwd
            .rfind('/')
            .map(|i| self.cwd[..i].to_string())
            .unwrap_or_default();
        self.set_cwd(parent);
    }

    fn select(&mut self, index: u32) {
        if self.selected == Some(index) {
            return;
        }
        self.selected = Some(index);
        self.preview = None;
        self.preview_pending = None;
        self.texture = None;
        self.table = None;
        self.table_pending = None;
        self.request_preview(index);
    }

    fn request_preview(&mut self, index: u32) {
        let Some(backup) = self.backup.clone() else {
            return;
        };
        let Some(entry) = backup.entries.get(index as usize) else {
            return;
        };
        if !entry.is_file() || !backup.blob_exists(entry) {
            return;
        }
        self.preview_pending = Some(index);
        let _ = self.tx.send(Cmd::Preview { backup, index });
    }

    fn request_table(&mut self, index: u32, table: String) {
        let Some(backup) = self.backup.clone() else {
            return;
        };
        self.table = Some(TableState {
            table: table.clone(),
            data: None,
            error: None,
        });
        self.table_pending = Some((index, table.clone()));
        let _ = self.tx.send(Cmd::Table {
            backup,
            index,
            table,
        });
    }

    fn request_external(&mut self, index: u32) {
        let Some(backup) = self.backup.clone() else {
            return;
        };
        let _ = self.tx.send(Cmd::OpenExternal { backup, index });
        self.toast("Opening with the system viewer…");
    }

    fn begin_extract(&mut self, indices: Vec<u32>, label: String) {
        let Some(backup) = self.backup.clone() else {
            return;
        };
        let files: Vec<u32> = indices
            .into_iter()
            .filter(|&i| {
                backup
                    .entries
                    .get(i as usize)
                    .map(|e| e.is_file())
                    .unwrap_or(false)
            })
            .collect();
        if files.is_empty() {
            self.toast("Nothing to extract — only folders were selected.");
            return;
        }
        self.extract = Some(ExtractState {
            done: 0,
            total: files.len(),
            bytes: 0,
            label: label.clone(),
        });
        let start = self
            .backup
            .as_ref()
            .map(|b| b.dir.clone())
            .or_else(dirs::home_dir);
        let (tx, rx) = channel();
        let _ = thread::Builder::new()
            .name("ibackup-explorer-picker".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new().set_title("Choose a folder for the extracted files");
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                let _ = tx.send(dialog.pick_folder());
            });
        self.folder_rx = Some((rx, FolderPurpose::Extract { indices: files, label }));
    }

    fn start_extract(&mut self, indices: Vec<u32>, label: String, out: PathBuf) {
        let Some(backup) = self.backup.clone() else {
            return;
        };
        self.extract = Some(ExtractState {
            done: 0,
            total: indices.len(),
            bytes: 0,
            label: label.clone(),
        });
        let _ = self.tx.send(Cmd::Extract {
            backup,
            indices,
            out,
            label,
        });
    }

    fn pick_folder(&mut self, purpose: FolderPurpose) {
        let start = self
            .backup
            .as_ref()
            .map(|b| b.dir.clone())
            .or_else(dirs::home_dir);
        let (tx, rx) = channel();
        let _ = thread::Builder::new()
            .name("ibackup-explorer-picker".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new().set_title("Choose an iPhone backup folder");
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                let _ = tx.send(dialog.pick_folder());
            });
        self.folder_rx = Some((rx, purpose));
    }

    fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    // -- message pump ------------------------------------------------------

    fn pump(&mut self) {
        if let Some((rx, purpose)) = self.folder_rx.take() {
            match rx.try_recv() {
                Ok(Some(path)) => match purpose {
                    FolderPurpose::Open => self.open_path(path),
                    FolderPurpose::Extract { indices, label } => {
                        self.start_extract(indices, label, path)
                    }
                },
                Ok(None) => {
                    if matches!(purpose, FolderPurpose::Extract { .. }) {
                        self.extract = None;
                        self.toast("Extraction cancelled.");
                    }
                }
                Err(TryRecvError::Empty) => self.folder_rx = Some((rx, purpose)),
                Err(TryRecvError::Disconnected) => {
                    if matches!(purpose, FolderPurpose::Extract { .. }) {
                        self.extract = None;
                    }
                }
            }
        }

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(rows) => {
                    if let Some((_, count)) = &mut self.opening {
                        *count = rows;
                    }
                }
                Msg::Opened { generation, result } => {
                    if generation != self.open_gen {
                        continue;
                    }
                    match result {
                        Ok(backup) => self.adopt(backup),
                        Err(err) => {
                            self.opening = None;
                            self.backup = None;
                            self.open_error = Some(err);
                        }
                    }
                }
                Msg::PreviewReady { index, preview } => {
                    if self.preview_pending != Some(index) {
                        continue;
                    }
                    self.preview_pending = None;
                    if self.selected != Some(index) {
                        continue;
                    }
                    if let Preview::Image(image) = &preview {
                        let texture = self.ctx.load_texture(
                            format!("explorer-preview-{index}"),
                            image.egui_image(),
                            egui::TextureOptions::LINEAR,
                        );
                        self.texture = Some((index, texture));
                    }
                    self.preview = Some((index, Arc::new(preview)));
                }
                Msg::TableReady {
                    index,
                    table,
                    data,
                } => {
                    if self.table_pending.as_ref() != Some(&(index, table.clone())) {
                        continue;
                    }
                    self.table_pending = None;
                    if self.selected != Some(index) {
                        continue;
                    }
                    if let Some(state) = &mut self.table {
                        match data {
                            Ok(data) => state.data = Some(data),
                            Err(err) => state.error = Some(err),
                        }
                    }
                }
                Msg::ExtractProgress {
                    done,
                    total,
                    bytes,
                    label,
                } => {
                    if let Some(state) = &mut self.extract {
                        state.done = done;
                        state.total = total;
                        state.bytes = bytes;
                        state.label = label;
                    }
                }
                Msg::ExtractDone { outcome, label } => {
                    self.extract = None;
                    let _ = &label;
                    let summary = ExtractSummary {
                        files: outcome.files,
                        bytes: outcome.bytes,
                        missing: outcome.missing,
                        skipped: outcome.skipped,
                        out: outcome.out.clone(),
                        errors: outcome.errors.clone(),
                    };
                    let message = if outcome.files == 0 {
                        "Nothing was extracted — the matching files are missing from this backup."
                            .to_string()
                    } else {
                        format!(
                            "Extracted {} files ({}) to {}",
                            count(outcome.files),
                            util::human_bytes(outcome.bytes),
                            outcome.out.display()
                        )
                    };
                    self.last_extract = Some(summary);
                    self.toast(message);
                }
                Msg::ExternalDone { path, error } => match error {
                    Some(err) => self.toast(format!("Could not open the file: {err}")),
                    None => self.toast(format!("Opened {}", path.display())),
                },
            }
        }
    }

    fn take_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        if let Some(path) = dropped.into_iter().find_map(|f| f.path) {
            self.open_path(path);
        }
    }

    // -- chrome ------------------------------------------------------------

    fn header(&mut self, ctx: &egui::Context) {
        let mut pick = false;
        let mut close = false;
        let busy = self.folder_rx.is_some();
        egui::TopBottomPanel::top("explorer-header")
            .frame(
                Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(icon) = self.icon.clone() {
                        ui.add(
                            egui::Image::new(egui::load::SizedTexture::new(
                                icon.id(),
                                Vec2::splat(26.0),
                            ))
                            .rounding(Rounding::same(7.0)),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("Backup Explorer").size(14.5).strong());
                        ui.label(
                            RichText::new("Look inside an iPhone backup without restoring it")
                                .size(10.5)
                                .color(t::TEXT_DIM),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if t::accent_button(ui, "Open backup…", !busy).clicked() {
                            pick = true;
                        }
                        if self.backup.is_some() && t::ghost_button(ui, "Close").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if pick {
            self.pick_folder(FolderPurpose::Open);
        }
        if close {
            self.close_backup();
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let summary = self
            .backup
            .as_ref()
            .map(|b| {
                (
                    b.file_count,
                    b.dir_count,
                    b.link_count,
                    b.total_size,
                    b.domains.len(),
                )
            });
        let searching = self.searching;
        let truncated = self.truncated;
        let results = self.view.len();
        let pending = self.preview_pending.is_some()
            || self.table_pending.is_some()
            || self.opening.is_some();
        egui::TopBottomPanel::bottom("explorer-status")
            .frame(
                Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(16.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    if let Some((files, dirs, links, size, domains)) = summary {
                        let mut text = format!(
                            "{} files · {} folders",
                            count(files),
                            count(dirs)
                        );
                        if links > 0 {
                            text.push_str(&format!(" · {} links", count(links)));
                        }
                        text.push_str(&format!(
                            " · {} · {} domains",
                            util::human_bytes(size),
                            count(domains)
                        ));
                        ui.label(
                            RichText::new(text)
                                .size(10.5)
                                .color(t::TEXT_DIM),
                        );
                    } else {
                        ui.label(
                            RichText::new("No backup open")
                                .size(10.5)
                                .color(t::TEXT_DIM),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if pending {
                            ui.spinner();
                        }
                        if searching {
                            let note = if truncated {
                                format!("Showing the first {results} matches")
                            } else {
                                format!("{results} matches")
                            };
                            ui.label(RichText::new(note).size(10.5).color(t::TEXT_DIM));
                        }
                        if self.extract.is_some() {
                            ui.label(
                                RichText::new("Extracting…")
                                    .size(10.5)
                                    .color(t::ACCENT),
                            );
                        }
                    });
                });
            });
    }

    fn extract_bar(&mut self, ctx: &egui::Context) {
        if let Some(state) = &self.extract {
            let fraction = if state.total == 0 {
                0.0
            } else {
                state.done as f32 / state.total as f32
            };
            let text = format!(
                "{} / {} · {}",
                count(state.done),
                count(state.total),
                util::human_bytes(state.bytes)
            );
            let label = state.label.clone();
            egui::TopBottomPanel::bottom("explorer-extract")
                .frame(
                    Frame::none()
                        .fill(t::SURFACE)
                        .inner_margin(Margin::symmetric(16.0, 8.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Extracting {label}"))
                                .size(11.5)
                                .color(t::TEXT),
                        );
                        ui.add(
                            egui::ProgressBar::new(fraction)
                                .desired_width(180.0)
                                .text(RichText::new(text).size(10.0)),
                        );
                    });
                });
            return;
        }

        let Some(summary) = self.last_extract.take() else {
            return;
        };
        let mut dismiss = false;
        let mut open_folder = false;
        let mut details = false;
        egui::TopBottomPanel::bottom("explorer-extract")
            .frame(
                Frame::none()
                    .fill(t::SURFACE)
                    .inner_margin(Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Extracted {} files ({}) to {}",
                            count(summary.files),
                            util::human_bytes(summary.bytes),
                            short_path(&summary.out)
                        ))
                        .size(11.5)
                        .color(t::SUCCESS),
                    );
                    if summary.missing > 0 {
                        ui.label(
                            RichText::new(format!("{} missing", summary.missing))
                                .size(11.0)
                                .color(t::WARN),
                        );
                    }
                    if summary.skipped > 0 {
                        ui.label(
                            RichText::new(format!("{} skipped", summary.skipped))
                                .size(11.0)
                                .color(t::TEXT_DIM),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if t::ghost_button(ui, "Dismiss").clicked() {
                            dismiss = true;
                        }
                        if t::secondary_button(ui, "Open folder", true).clicked() {
                            open_folder = true;
                        }
                        if !summary.errors.is_empty() && t::ghost_button(ui, "Details").clicked() {
                            details = true;
                        }
                    });
                });
            });
        if open_folder {
            let _ = super::open_in_system(&summary.out);
        }
        if details {
            self.info = Some((
                "Extraction problems".to_string(),
                summary.errors.join("\n"),
            ));
        }
        if !dismiss {
            self.last_extract = Some(summary);
        }
    }

    // -- sidebar -----------------------------------------------------------

    fn sidebar(&mut self, ctx: &egui::Context) {
        let backup = self.backup.clone();
        egui::SidePanel::left("explorer-domains")
            .default_width(286.0)
            .width_range(240.0..=420.0)
            .resizable(true)
            .frame(
                Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(14.0, 14.0)),
            )
            .show(ctx, |ui| {
                let Some(backup) = &backup else {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Open a backup to see the domains it contains.")
                            .size(11.5)
                            .color(t::TEXT_DIM),
                    );
                    return;
                };

                self.device_card(ui, backup);
                ui.add_space(10.0);

                let changed = ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search all files…"),
                    )
                    .changed();
                if changed {
                    self.view_dirty = true;
                }

                ui.add_space(8.0);
                if let Some(domain) = self.domain {
                    let response = ui.add_sized(
                        [ui.available_width(), 26.0],
                        egui::Button::new(
                            RichText::new("←  All domains")
                                .size(11.5)
                                .color(t::TEXT_DIM),
                        )
                        .frame(false),
                    );
                    if response.clicked() {
                        self.open_domain(None);
                    }
                    ui.add_space(4.0);
                    let _ = domain;
                } else {
                    ui.label(
                        RichText::new("DOMAINS")
                            .size(9.5)
                            .color(t::TEXT_DIM),
                    );
                    ui.add_space(2.0);
                }

                t::divider(ui);
                let order = self.domain_order.clone();
                let row_h = 42.0;
                egui::ScrollArea::vertical()
                    .id_salt("explorer-domain-list")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, order.len(), |ui, range| {
                        for &domain in &order[range] {
                            self.domain_row(ui, backup, domain, row_h);
                        }
                    });
            });
    }

    fn device_card(&self, ui: &mut Ui, backup: &Backup) {
        let name = if !backup.info.device_name.is_empty() {
            backup.info.device_name.clone()
        } else if !backup.info.display_name.is_empty() {
            backup.info.display_name.clone()
        } else {
            "iPhone".to_string()
        };
        t::card(ui, |ui| {
            ui.label(RichText::new(name).size(15.0).strong().color(t::TEXT));
            let mut parts: Vec<String> = Vec::new();
            if !backup.info.product_name.is_empty() {
                parts.push(backup.info.product_name.clone());
            }
            if !backup.info.product_version.is_empty() {
                parts.push(format!("iOS {}", backup.info.product_version));
            }
            if let Some(date) = &backup.info.date {
                parts.push(format!("backed up {date}"));
            }
            if !parts.is_empty() {
                ui.add(
                    egui::Label::new(
                        RichText::new(parts.join(" · "))
                            .size(10.5)
                            .color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                t::pill(ui, "Not encrypted", t::SUCCESS);
                if backup.info.passcode_set {
                    t::pill(ui, "Passcode set", t::WARN);
                }
            });
        });
    }

    fn domain_row(&mut self, ui: &mut Ui, backup: &Backup, domain: u32, row_h: f32) {
        let Some(info) = backup.domains.get(domain as usize) else {
            return;
        };
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), row_h),
            Sense::click(),
        );
        let selected = self.domain == Some(domain);
        paint_row(ui, rect, selected, response.hovered());

        let friendly = self
            .domain_labels
            .get(domain as usize)
            .cloned()
            .unwrap_or_else(|| info.name.to_string());
        let color = domain_color(&info.name);
        paint_badge(ui, rect, &initials(&friendly), color);

        let painter = ui.painter();
        let left = rect.left() + 48.0;
        painter.text(
            egui::pos2(left, rect.center().y - 8.0),
            Align2::LEFT_CENTER,
            &friendly,
            FontId::proportional(12.0),
            t::TEXT,
        );
        let room = (rect.width() - 56.0).max(40.0);
        let max_chars = (room / 5.0) as usize;
        let raw = truncate(&info.name, max_chars.max(12));
        painter.text(
            egui::pos2(left, rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            raw,
            FontId::proportional(9.5),
            t::TEXT_DIM,
        );
        painter.text(
            egui::pos2(rect.right() - 8.0, rect.center().y - 8.0),
            Align2::RIGHT_CENTER,
            util::human_bytes(info.size),
            FontId::proportional(9.5),
            t::TEXT_DIM,
        );

        if response.clicked() {
            self.open_domain(Some(domain));
        }
        let name = info.name.to_string();
        let response = response.on_hover_text(name);
        response.context_menu(|ui| {
            if ui.button("Open").clicked() {
                self.open_domain(Some(domain));
                ui.close_menu();
            }
        });
    }

    // -- main list ---------------------------------------------------------

    fn browser(&mut self, ui: &mut Ui, backup: &Backup) {
        if self.view_dirty {
            self.rebuild_view(backup);
        }
        self.toolbar(ui, backup);
        ui.add_space(6.0);

        if self.domain.is_none() && !self.searching {
            self.domain_overview(ui, backup);
        } else {
            self.list(ui, backup);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, backup: &Backup) {
        let crumbs = self.breadcrumb_targets(backup);
        let mut go: Option<String> = None;
        let mut up = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if self.domain.is_some() && !self.cwd.is_empty() {
                let response = ui.add(
                    egui::Button::new(RichText::new("↑").size(13.0).color(t::TEXT_DIM))
                        .frame(false)
                        .min_size(Vec2::new(22.0, 22.0)),
                );
                if response.clicked() {
                    up = true;
                }
            }
            ui.horizontal_wrapped(|ui| {
                for (i, (label, path)) in crumbs.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("/").size(12.0).color(t::BORDER_HI));
                    }
                    let last = i + 1 == crumbs.len();
                    let text = RichText::new(label)
                        .size(12.5)
                        .color(if last { t::TEXT } else { t::TEXT_DIM });
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        go = Some(path.clone());
                    }
                }
            });
        });

        // The domain overview has its own rows and counts; sort/filter only make
        // sense once a domain (or a search) is in view.
        if self.domain.is_none() && !self.searching {
            if up {
                self.go_up();
            }
            if let Some(path) = go {
                self.set_cwd(path);
            }
            return;
        }

        let mut filter_changed = false;
        let mut sort_changed = false;
        let mut reset = false;
        ui.horizontal(|ui| {
            let count_text = if self.searching {
                format!("{} result{}", count(self.view.len()), plural(self.view.len()))
            } else {
                format!("{} item{}", count(self.view.len()), plural(self.view.len()))
            };
            ui.label(RichText::new(count_text).size(10.5).color(t::TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                egui::ComboBox::from_id_salt("explorer-sort")
                    .selected_text(RichText::new(self.sort.label()).size(11.0))
                    .width(92.0)
                    .show_ui(ui, |ui| {
                        for option in Sort::ALL {
                            if ui
                                .selectable_value(&mut self.sort, option, option.label())
                                .changed()
                            {
                                sort_changed = true;
                            }
                        }
                    });
                ui.add_space(4.0);
                egui::ComboBox::from_id_salt("explorer-filter")
                    .selected_text(RichText::new(self.filter.label()).size(11.0))
                    .width(104.0)
                    .show_ui(ui, |ui| {
                        for option in KindFilter::ALL {
                            if ui
                                .selectable_value(&mut self.filter, option, option.label())
                                .changed()
                            {
                                filter_changed = true;
                            }
                        }
                    });
                if self.filter != KindFilter::All || self.sort != Sort::Name {
                    if t::ghost_button(ui, "Reset").clicked() {
                        reset = true;
                    }
                }
            });
        });

        if up {
            self.go_up();
        }
        if let Some(path) = go {
            self.set_cwd(path);
        }
        if reset {
            self.filter = KindFilter::All;
            self.sort = Sort::Name;
            self.view_dirty = true;
        }
        if filter_changed || sort_changed {
            self.view_dirty = true;
        }
    }

    fn breadcrumb_targets(&self, backup: &Backup) -> Vec<(String, String)> {
        let mut crumbs: Vec<(String, String)> = Vec::new();
        match self.domain {
            Some(domain) => {
                let label = self
                    .domain_labels
                    .get(domain as usize)
                    .cloned()
                    .unwrap_or_else(|| backup.domain_name(domain).to_string());
                crumbs.push((label, String::new()));
            }
            None => crumbs.push(("All domains".to_string(), String::new())),
        }
        let mut acc = String::new();
        for part in self.cwd.split('/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            crumbs.push((part.to_string(), acc.clone()));
        }
        crumbs
    }

    fn domain_overview(&mut self, ui: &mut Ui, backup: &Backup) {
        let order = self.domain_order.clone();
        if order.is_empty() {
            empty_note(ui, "This backup has no files.");
            return;
        }
        let row_h = 46.0;
        let mut open: Option<u32> = None;
        egui::ScrollArea::vertical()
            .id_salt("explorer-domain-overview")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, order.len(), |ui, range| {
                for &domain in &order[range] {
                    let Some(info) = backup.domains.get(domain as usize) else {
                        continue;
                    };
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), row_h),
                        Sense::click(),
                    );
                    let selected = self.domain == Some(domain);
                    paint_row(ui, rect, selected, response.hovered());
                    let friendly = self
                        .domain_labels
                        .get(domain as usize)
                        .cloned()
                        .unwrap_or_else(|| info.name.to_string());
                    let color = domain_color(&info.name);
                    paint_badge(ui, rect, &initials(&friendly), color);
                    let painter = ui.painter();
                    painter.text(
                        egui::pos2(rect.left() + 48.0, rect.center().y - 8.0),
                        Align2::LEFT_CENTER,
                        &friendly,
                        FontId::proportional(12.5),
                        t::TEXT,
                    );
                    painter.text(
                        egui::pos2(rect.left() + 48.0, rect.center().y + 8.0),
                        Align2::LEFT_CENTER,
                        truncate(&info.name, 48),
                        FontId::proportional(9.5),
                        t::TEXT_DIM,
                    );
                    painter.text(
                        egui::pos2(rect.right() - 8.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        format!(
                            "{} files · {} folders · {}",
                            count(info.files),
                            count(info.dirs),
                            util::human_bytes(info.size)
                        ),
                        FontId::proportional(9.5),
                        t::TEXT_DIM,
                    );
                    if response.clicked() {
                        open = Some(domain);
                    }
                }
            });
        if let Some(domain) = open {
            self.open_domain(Some(domain));
        }
    }

    fn list(&mut self, ui: &mut Ui, backup: &Backup) {
        if self.view.is_empty() {
            if self.searching {
                empty_note(
                    ui,
                    &format!("No files match “{}”.", self.search.trim()),
                );
            } else {
                empty_note(ui, "This folder is empty in the backup.");
            }
            return;
        }
        let ids = self.view.clone();
        let searching = self.searching;
        let row_h = if searching { 46.0 } else { 32.0 };
        let total = ids.len();
        egui::ScrollArea::vertical()
            .id_salt("explorer-file-list")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total, |ui, range| {
                for &index in &ids[range] {
                    self.file_row(ui, backup, index, row_h, searching);
                }
            });
    }

    fn file_row(
        &mut self,
        ui: &mut Ui,
        backup: &Backup,
        index: u32,
        row_h: f32,
        searching: bool,
    ) {
        let Some(entry) = backup.entries.get(index as usize) else {
            return;
        };
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), row_h),
            Sense::click(),
        );
        let selected = self.selected == Some(index);
        paint_row(ui, rect, selected, response.hovered());
        if self.scroll_to_selected && selected {
            ui.scroll_to_rect(rect, Some(Align::Center));
            self.scroll_to_selected = false;
        }

        let kind = entry.kind();
        let color = kind_color(kind);
        paint_badge(ui, rect, &badge_text(entry), color);

        let name = entry.name().to_string();
        let name_x = rect.left() + 48.0;
        let right = rect.right() - 8.0;
        let clip = egui::Rect::from_min_max(
            egui::pos2(name_x, rect.top()),
            egui::pos2((right - 130.0).max(name_x + 10.0), rect.bottom()),
        );
        let painter = ui.painter().with_clip_rect(clip);
        let name_y = if searching {
            rect.center().y - 7.0
        } else {
            rect.center().y
        };
        painter.text(
            egui::pos2(name_x, name_y),
            Align2::LEFT_CENTER,
            &name,
            FontId::proportional(12.5),
            t::TEXT,
        );

        if searching {
            let subtitle = format!(
                "{} · {}",
                self.domain_labels
                    .get(entry.domain as usize)
                    .cloned()
                    .unwrap_or_else(|| backup.domain_name(entry.domain).to_string()),
                entry.parent().unwrap_or("")
            );
            painter.text(
                egui::pos2(name_x, rect.center().y + 8.0),
                Align2::LEFT_CENTER,
                subtitle,
                FontId::proportional(9.5),
                t::TEXT_DIM,
            );
        }

        let meta = if entry.is_dir() {
            let items = backup.direct_count(entry.domain, &entry.path);
            format!("{} item{}", count(items), plural(items))
        } else if searching {
            format!(
                "{} · {}",
                util::human_bytes(entry.size),
                model::format_time(entry.modified)
            )
        } else {
            util::human_bytes(entry.size)
        };
        ui.painter().text(
            egui::pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            meta,
            FontId::proportional(10.0),
            t::TEXT_DIM,
        );

        let full_path = format!("{}/{}", backup.domain_name(entry.domain), entry.path);
        let is_dir = entry.is_dir();
        let is_file = entry.is_file();
        let present = is_file && backup.blob_exists(entry);
        let dir_path = entry.path.to_string();
        let response = response.on_hover_text(&full_path);

        let mut open_dir: Option<String> = None;
        response.context_menu(|ui| {
            if ui.button("Preview").clicked() {
                self.select(index);
                ui.close_menu();
            }
            if is_dir && ui.button("Open folder").clicked() {
                open_dir = Some(dir_path.clone());
                ui.close_menu();
            }
            if ui.button("Extract…").clicked() {
                self.begin_extract(vec![index], name.clone());
                ui.close_menu();
            }
            if is_file && present && ui.button("Open externally").clicked() {
                self.request_external(index);
                ui.close_menu();
            }
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(full_path.clone());
                ui.close_menu();
            }
        });
        if let Some(path) = open_dir {
            self.set_cwd(path);
            return;
        }

        if response.double_clicked() {
            if is_dir {
                self.set_cwd(dir_path);
            } else {
                self.request_external(index);
            }
        } else if response.clicked() {
            self.select(index);
        }
    }

    // -- preview -----------------------------------------------------------

    fn preview_column(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("explorer-preview")
            .default_width(352.0)
            .width_range(280.0..=640.0)
            .resizable(true)
            .frame(
                Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(Margin::symmetric(14.0, 14.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("explorer-preview-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.preview_panel(ui);
                    });
            });
    }

    fn preview_panel(&mut self, ui: &mut Ui) {
        let Some(backup) = self.backup.clone() else {
            empty_note(ui, "Open a backup to preview its files.");
            return;
        };
        let Some(index) = self.selected else {
            empty_note(
                ui,
                "Select a file to see its details and a preview of its contents.",
            );
            return;
        };
        let Some(entry) = backup.entries.get(index as usize) else {
            return;
        };

        let kind = entry.kind();
        let color = kind_color(kind);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
            ui.painter()
                .rect_filled(rect, Rounding::same(10.0), t::tint(color, 28));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                badge_text(entry),
                FontId::proportional(10.0),
                color,
            );
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(
                    egui::Label::new(
                        RichText::new(entry.name())
                            .size(13.0)
                            .strong()
                            .color(t::TEXT),
                    )
                    .wrap(),
                );
                let mut meta = kind.label().to_string();
                if entry.is_file() {
                    meta.push_str(" · ");
                    meta.push_str(&util::human_bytes(entry.size));
                }
                if entry.modified > 0 {
                    meta.push_str(" · ");
                    meta.push_str(&model::format_time(entry.modified));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(meta).size(10.0).color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
            });
        });
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "{}/{}",
                    backup.domain_name(entry.domain),
                    entry.path
                ))
                .monospace()
                .size(9.5)
                .color(t::TEXT_DIM),
            )
            .wrap(),
        );
        if entry.synthetic {
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "This folder is implied by the files inside it — the backup has no row of its own for it.",
                )
                .size(10.0)
                .color(t::TEXT_DIM),
            );
        }
        ui.add_space(8.0);

        let mut extract_clicked = false;
        let mut open_external = false;
        let mut copy_path = false;
        ui.horizontal_wrapped(|ui| {
            if entry.is_dir() {
                if t::accent_button(ui, "Extract folder…", true).clicked() {
                    extract_clicked = true;
                }
            } else if entry.is_file() {
                let present = backup.blob_exists(entry);
                if t::accent_button(ui, "Extract…", present).clicked() {
                    extract_clicked = true;
                }
                if t::secondary_button(ui, "Open", present).clicked() {
                    open_external = true;
                }
            }
            if t::ghost_button(ui, "Copy path").clicked() {
                copy_path = true;
            }
        });
        if extract_clicked {
            if entry.is_dir() {
                let files = backup.files_under(entry.domain, &entry.path);
                self.begin_extract(files, entry.path.to_string());
            } else {
                self.begin_extract(vec![index], entry.name().to_string());
            }
        }
        if open_external {
            self.request_external(index);
        }
        if copy_path {
            ui.ctx().copy_text(format!(
                "{}/{}",
                backup.domain_name(entry.domain),
                entry.path
            ));
            self.toast("Path copied to the clipboard.");
        }

        t::divider(ui);
        self.preview_body(ui, entry, index);
    }

    fn preview_body(&mut self, ui: &mut Ui, entry: &Entry, index: u32) {
        if entry.is_dir() {
            let Some(backup) = self.backup.clone() else {
                return;
            };
            let (files, folders) = backup.folder_counts(entry.domain, &entry.path);
            let (nested_files, bytes) = backup.folder_totals(entry.domain, &entry.path);
            t::card(ui, |ui| {
                t::field(ui, "FILES (DIRECT)", &count(files), t::TEXT);
                t::field(ui, "FOLDERS (DIRECT)", &count(folders), t::TEXT);
                t::field(
                    ui,
                    "EVERYTHING BELOW",
                    &format!(
                        "{} files · {}",
                        count(nested_files),
                        util::human_bytes(bytes)
                    ),
                    t::TEXT,
                );
            });
            return;
        }

        if entry.is_link() {
            ui.label(
                RichText::new("This entry is a symbolic link; its target is not previewed.")
                    .size(11.5)
                    .color(t::TEXT_DIM),
            );
            return;
        }

        let Some(backup) = self.backup.clone() else {
            return;
        };
        if !backup.blob_exists(entry) {
            t::banner(
                ui,
                t::WARN,
                "File data is missing",
                "This backup has a record of the file, but its bytes are not in the backup folder any more. That usually means the backup was interrupted or its blob folder was pruned.",
                |_ui| {},
            );
            return;
        }

        if self.preview_pending == Some(index) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading preview…").size(11.5).color(t::TEXT_DIM));
            });
            return;
        }

        let loaded = self
            .preview
            .as_ref()
            .filter(|(i, _)| *i == index)
            .map(|(_, preview)| preview.clone());
        let Some(preview) = loaded else {
            ui.label(
                RichText::new("No preview available.")
                    .size(11.5)
                    .color(t::TEXT_DIM),
            );
            return;
        };

        match &*preview {
            Preview::Image(image) => {
                let texture = self
                    .texture
                    .as_ref()
                    .filter(|(i, _)| *i == index)
                    .map(|(_, texture)| texture.clone());
                match texture {
                    Some(texture) => {
                        let max_w = ui.available_width().max(120.0);
                        let max_h = 440.0_f32;
                        let scale = (max_w / image.width.max(1) as f32)
                            .min(max_h / image.height.max(1) as f32)
                            .min(3.0);
                        ui.vertical_centered(|ui| {
                            ui.add(
                                egui::Image::new(&texture)
                                    .fit_to_exact_size(Vec2::new(
                                        image.width as f32 * scale,
                                        image.height as f32 * scale,
                                    ))
                                    .rounding(Rounding::same(10.0)),
                            );
                        });
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(&image.note)
                                .size(10.0)
                                .color(t::TEXT_DIM),
                        );
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Decoding image…")
                                    .size(11.5)
                                    .color(t::TEXT_DIM),
                            );
                        });
                    }
                }
            }
            Preview::Text { text, note } => {
                if !note.is_empty() {
                    ui.label(
                        RichText::new(note).size(10.0).color(t::TEXT_DIM),
                    );
                }
                mono_block(ui, text, "explorer-text-preview");
            }
            Preview::Plist(text) => {
                mono_block(ui, text, "explorer-plist-preview");
            }
            Preview::Hex { text, note } => {
                if !note.is_empty() {
                    ui.label(
                        RichText::new(note).size(10.0).color(t::TEXT_DIM),
                    );
                }
                mono_block(ui, text, "explorer-hex-preview");
            }
            Preview::Sqlite { tables } => {
                if tables.is_empty() {
                    ui.label(
                        RichText::new("This database has no tables.")
                            .size(11.5)
                            .color(t::TEXT_DIM),
                    );
                } else {
                    ui.label(
                        RichText::new("TABLES")
                            .size(9.5)
                            .color(t::TEXT_DIM),
                    );
                    let mut chosen: Option<String> = None;
                    ui.horizontal_wrapped(|ui| {
                        for table in tables {
                            let selected = self
                                .table
                                .as_ref()
                                .map(|state| state.table == *table)
                                .unwrap_or(false);
                            if ui.selectable_label(selected, table).clicked() {
                                chosen = Some(table.clone());
                            }
                        }
                    });
                    if let Some(table) = chosen {
                        self.request_table(index, table);
                    }
                    if let Some(state) = &self.table {
                        if let Some(error) = &state.error {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(error).size(11.0).color(t::DANGER),
                            );
                        }
                        if let Some(data) = &state.data {
                            ui.add_space(4.0);
                            mono_block(ui, &table_text(data), "explorer-table-preview");
                        }
                    } else if self
                        .table_pending
                        .as_ref()
                        .map(|(i, _)| *i == index)
                        .unwrap_or(false)
                    {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("Reading table…")
                                    .size(11.5)
                                    .color(t::TEXT_DIM),
                            );
                        });
                    }
                }
            }
            Preview::Media { kind, note } => {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(kind.label())
                            .size(20.0)
                            .color(t::TEXT_DIM),
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(note).size(11.0).color(t::TEXT_DIM),
                        )
                        .wrap(),
                    );
                    ui.add_space(10.0);
                    if t::accent_button(ui, "Open in system viewer", true).clicked() {
                        self.request_external(index);
                    }
                });
            }
            Preview::Missing => {
                t::banner(
                    ui,
                    t::WARN,
                    "File data is missing",
                    "The backup lists this file, but its bytes are not in the backup folder.",
                    |_ui| {},
                );
            }
            Preview::Error(message) => {
                t::banner(ui, t::DANGER, "Preview failed", message, |_ui| {});
            }
        }
    }

    // -- central screens ---------------------------------------------------

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                Frame::none()
                    .fill(t::BG)
                    .inner_margin(Margin::same(16.0)),
            )
            .show(ctx, |ui| {
                if let Some(backup) = self.backup.clone() {
                    self.browser(ui, &backup);
                } else if let Some(error) = self.open_error.clone() {
                    self.error_screen(ui, &error);
                } else if let Some((path, rows)) = self.opening.clone() {
                    self.loading_screen(ui, &path, rows);
                } else {
                    self.welcome(ui);
                }
            });
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let mut pick = false;
        let mut open: Option<PathBuf> = None;
        let recents = self.recents.clone();
        let default_dir = default_backup_dir();
        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(660.0);
            ui.label(
                RichText::new("Look inside an iPhone backup")
                    .size(24.0)
                    .strong()
                    .color(t::TEXT),
            );
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Browse every file the backup contains — photos, messages, app data and databases — without restoring it to a device. The backup is opened read-only.",
                    )
                    .size(12.5)
                    .color(t::TEXT_DIM),
                )
                .wrap(),
            );
            ui.add_space(18.0);
            let clicked = ui
                .allocate_ui(Vec2::new(280.0, 42.0), |ui| {
                    ui.set_width(280.0);
                    t::primary_button(ui, "Open backup folder…", true)
                })
                .inner
                .clicked();
            if clicked {
                pick = true;
            }
            if let Some(dir) = &default_dir {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "Looking in {} — or drop a backup folder here.",
                        short_path(dir)
                    ))
                    .size(10.5)
                    .color(t::TEXT_DIM),
                );
            } else {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Tip: you can also drop a backup folder onto this window.")
                        .size(10.5)
                        .color(t::TEXT_DIM),
                );
            }

            if !recents.is_empty() {
                ui.add_space(22.0);
                ui.label(
                    RichText::new("RECENT BACKUPS")
                        .size(9.5)
                        .color(t::TEXT_DIM),
                );
                ui.add_space(6.0);
                for path in &recents {
                    let response = ui.add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new(
                            RichText::new(short_path(path))
                                .size(11.5)
                                .monospace(),
                        )
                        .fill(t::SURFACE)
                        .rounding(Rounding::same(9.0)),
                    );
                    if response.clicked() {
                        open = Some(path.clone());
                    }
                }
            }

            ui.add_space(22.0);
            t::banner(
                ui,
                t::ACCENT,
                "What you can do here",
                "Pick a domain on the left to walk the folder tree, search across the whole backup, preview photos, text, property lists and SQLite tables, then extract the files you need to a normal folder.",
                |_ui| {},
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "Encrypted backups are not supported yet — turn off “Encrypt local backup” and make a fresh backup first.",
                )
                .size(10.0)
                .color(t::TEXT_DIM),
            );
        });
        if pick {
            self.pick_folder(FolderPurpose::Open);
        }
        if let Some(path) = open {
            self.open_path(path);
        }
    }

    fn loading_screen(&mut self, ui: &mut Ui, path: &Path, rows: usize) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.spinner();
            ui.add_space(10.0);
            ui.label(
                RichText::new("Reading the backup index…")
                    .size(14.0)
                    .color(t::TEXT),
            );
            ui.label(
                RichText::new(format!("{} rows indexed", count(rows)))
                    .size(11.0)
                    .color(t::TEXT_DIM),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(short_path(path))
                    .monospace()
                    .size(10.0)
                    .color(t::TEXT_DIM),
            );
        });
    }

    fn error_screen(&mut self, ui: &mut Ui, error: &OpenError) {
        let mut pick = false;
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(660.0);
            t::banner(
                ui,
                t::DANGER,
                "Could not open this backup",
                &error.message,
                |ui| {
                    ui.add_space(6.0);
                    if t::accent_button(ui, "Choose another folder…", true).clicked() {
                        pick = true;
                    }
                },
            );
            if let Some(dir) = &error.dir {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(short_path(dir))
                        .monospace()
                        .size(10.0)
                        .color(t::TEXT_DIM),
                );
            }
            if let Some(info) = &error.info {
                ui.add_space(12.0);
                self.info_card(ui, info);
            }
        });
        if pick {
            self.pick_folder(FolderPurpose::Open);
        }
    }

    fn info_card(&self, ui: &mut Ui, info: &BackupInfo) {
        let name = if !info.device_name.is_empty() {
            info.device_name.clone()
        } else if !info.display_name.is_empty() {
            info.display_name.clone()
        } else {
            "iPhone".to_string()
        };
        t::card(ui, |ui| {
            ui.label(RichText::new(name).size(15.0).strong().color(t::TEXT));
            if !info.product_type.is_empty() {
                ui.label(
                    RichText::new(&info.product_type)
                        .size(11.0)
                        .color(t::TEXT_DIM),
                );
            }
            if let Some(date) = &info.date {
                ui.label(
                    RichText::new(format!("Last backup {date}"))
                        .size(11.0)
                        .color(t::TEXT_DIM),
                );
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if info.encrypted {
                    t::pill(ui, "Encrypted", t::WARN);
                } else {
                    t::pill(ui, "Not encrypted", t::SUCCESS);
                }
                if info.passcode_set {
                    t::pill(ui, "Passcode set", t::WARN);
                }
            });
        });
    }

    // -- keyboard, overlays ------------------------------------------------

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let (down, up, enter, back, escape) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if escape {
            if !self.search.is_empty() {
                self.search.clear();
                self.view_dirty = true;
            } else {
                self.selected = None;
                self.preview = None;
                self.preview_pending = None;
                self.texture = None;
            }
        }
        if back && !self.cwd.is_empty() {
            self.go_up();
        }
        if (down || up) && !self.view.is_empty() {
            let ids = self.view.clone();
            let position = self
                .selected
                .and_then(|selected| ids.iter().position(|&i| i == selected));
            let next = if down {
                match position {
                    Some(p) if p + 1 < ids.len() => p + 1,
                    Some(p) => p,
                    None => 0,
                }
            } else {
                match position {
                    Some(p) => p.saturating_sub(1),
                    None => 0,
                }
            };
            self.select(ids[next]);
            self.scroll_to_selected = true;
        }
        if enter {
            if let Some(index) = self.selected {
                if let Some(backup) = self.backup.clone() {
                    if let Some(entry) = backup.entries.get(index as usize) {
                        if entry.is_dir() {
                            let path = entry.path.to_string();
                            self.set_cwd(path);
                        }
                    }
                }
            }
        }
    }

    fn info_window(&mut self, ctx: &egui::Context) {
        let Some((title, body)) = self.info.clone() else {
            return;
        };
        let mut close = false;
        egui::Window::new(title)
            .id(egui::Id::new("explorer-info"))
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&body)
                                    .monospace()
                                    .size(10.5)
                                    .color(t::TEXT_DIM),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                ui.add_space(6.0);
                if t::secondary_button(ui, "Close", true).clicked() {
                    close = true;
                }
            });
        if close {
            self.info = None;
        }
    }

    fn toast_window(&mut self, ctx: &egui::Context) {
        let Some((message, at)) = &self.toast else {
            return;
        };
        if at.elapsed() > Duration::from_secs(8) {
            self.toast = None;
            return;
        }
        let message = message.clone();
        egui::Area::new(egui::Id::new("explorer-toast"))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -54.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                Frame::none()
                    .fill(t::SURFACE_HI)
                    .stroke(Stroke::new(1.0_f32, t::BORDER_HI))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(Margin::symmetric(14.0, 9.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(message).size(11.5).color(t::TEXT),
                            )
                            .wrap(),
                        );
                    });
            });
    }

    fn needs_repaint(&self) -> bool {
        self.opening.is_some()
            || self.folder_rx.is_some()
            || self.preview_pending.is_some()
            || self.table_pending.is_some()
            || self.extract.is_some()
            || self.toast.is_some()
    }
}

impl eframe::App for ExploreApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.take_dropped(ctx);
        self.header(ctx);
        self.status_bar(ctx);
        self.extract_bar(ctx);
        self.sidebar(ctx);
        self.preview_column(ctx);
        self.central(ctx);
        self.keys(ctx);
        self.info_window(ctx);
        self.toast_window(ctx);
        if self.needs_repaint() {
            ctx.request_repaint_after(Duration::from_millis(90));
        }
    }
}

// ---------------------------------------------------------------------------
// Small UI helpers
// ---------------------------------------------------------------------------

fn paint_row(ui: &Ui, rect: egui::Rect, selected: bool, hovered: bool) {
    let rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + 1.0),
        egui::pos2(rect.right(), rect.bottom() - 1.0),
    );
    let fill = if selected {
        t::ACCENT_SOFT
    } else if hovered {
        t::SURFACE_HOVER
    } else {
        t::SURFACE
    };
    ui.painter().rect_filled(rect, Rounding::same(8.0), fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            Rounding::same(8.0),
            Stroke::new(1.0_f32, t::tint(t::ACCENT, 90)),
        );
    }
}

fn paint_badge(ui: &Ui, row: egui::Rect, label: &str, color: Color32) {
    let badge = egui::Rect::from_center_size(
        egui::pos2(row.left() + 26.0, row.center().y),
        Vec2::new(34.0, 20.0),
    );
    ui.painter()
        .rect_filled(badge, Rounding::same(6.0), t::tint(color, 30));
    ui.painter().text(
        badge.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(9.0),
        color,
    );
}

fn mono_block(ui: &mut Ui, text: &str, id: &str) {
    Frame::none()
        .fill(t::BG)
        .stroke(Stroke::new(1.0_f32, t::BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::same(10.0))
        .show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt(id)
                .max_height(520.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text)
                                .monospace()
                                .size(10.5)
                                .color(t::TEXT),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
        });
}

fn empty_note(ui: &mut Ui, text: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Label::new(RichText::new(text).size(12.0).color(t::TEXT_DIM)).wrap(),
        );
    });
}

fn kind_color(kind: Kind) -> Color32 {
    match kind {
        Kind::Dir => t::ACCENT,
        Kind::Image => Color32::from_rgb(0x6B, 0xA2, 0xFF),
        Kind::Video => t::WARN,
        Kind::Audio => t::SUCCESS,
        Kind::Text => Color32::from_rgb(0x8A, 0xB4, 0xD8),
        Kind::Plist => Color32::from_rgb(0xC9, 0xA6, 0xFF),
        Kind::Database => Color32::from_rgb(0xF2, 0x8F, 0x5B),
        Kind::Archive => Color32::from_rgb(0xC7, 0x8A, 0xF0),
        Kind::Other => t::TEXT_DIM,
    }
}

fn domain_color(name: &str) -> Color32 {
    let mut hash: u32 = 0;
    for byte in name.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u32);
    }
    match hash % 6 {
        0 => Color32::from_rgb(0x6B, 0xA2, 0xFF),
        1 => Color32::from_rgb(0x3E, 0xD5, 0x8F),
        2 => Color32::from_rgb(0xF2, 0xB8, 0x4B),
        3 => Color32::from_rgb(0xC9, 0xA6, 0xFF),
        4 => Color32::from_rgb(0xF2, 0x8F, 0x5B),
        _ => Color32::from_rgb(0x7F, 0xD4, 0xE8),
    }
}

fn badge_text(entry: &Entry) -> String {
    if entry.is_dir() {
        return "DIR".to_string();
    }
    let ext = entry.extension();
    let cleaned: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();
    if cleaned.is_empty() {
        "FILE".to_string()
    } else {
        cleaned.to_uppercase()
    }
}

fn initials(label: &str) -> String {
    let mut out = String::new();
    for word in label.split_whitespace() {
        if let Some(ch) = word.chars().next() {
            if ch.is_alphabetic() {
                out.extend(ch.to_uppercase());
            }
        }
        if out.chars().count() == 2 {
            return out;
        }
    }
    if out.is_empty() {
        out.push('·');
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn table_text(data: &TableData) -> String {
    let mut out = String::new();
    out.push_str(&data.columns.join("  |  "));
    out.push('\n');
    out.push_str(&"-".repeat(data.columns.join("  |  ").len().min(120)));
    for row in &data.rows {
        out.push('\n');
        out.push_str(&row.join("  |  "));
    }
    if data.truncated {
        out.push_str("\n… more rows in the database");
    }
    out
}

fn short_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn default_backup_dir() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join("iPhoneBackups");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Recent backups
// ---------------------------------------------------------------------------

fn recents_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ibackup").join("explorer-recents.txt"))
}

fn load_recents() -> Vec<PathBuf> {
    let Some(file) = recents_file() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    let mut recents: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if !recents.contains(&path) {
            recents.push(path);
        }
        if recents.len() == 6 {
            break;
        }
    }
    recents
}

fn remember_recent(dir: &Path) -> Vec<PathBuf> {
    let mut recents = load_recents();
    let path = dir.to_path_buf();
    recents.retain(|existing| existing != &path);
    recents.insert(0, path);
    recents.truncate(6);
    if let Some(file) = recents_file() {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = recents
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(file, body);
    }
    recents
}
