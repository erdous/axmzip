// Axmzip GUI v5.3 — Professional archive manager (WinRAR-style)
//
// Layout:
//   ┌─ Title bar ──────────────────────────────────────────────┐
//   ├─ Toolbar ─────────────────────────────────────────────────┤
//   ├─ Path bar ─────────────────────────────────────────────────┤
//   ├─ Column headers ───────────────────────────────────────────┤
//   │  File list (scrollable)                                    │
//   ├─ Progress bar (while working) ─────────────────────────────┤
//   └─ Status bar ───────────────────────────────────────────────┘

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, Align, Color32, FontId, Frame, Layout, Margin, Rect, RichText,
    ScrollArea, Sense, Stroke, Vec2,
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};
use axmzip_core::{self as core, ArchiveEntry, ArchiveFileInfo};

// ─────────────────────────────────────────────────────────────────
// THEME — sharp industrial dark, like a pro desktop tool
// ─────────────────────────────────────────────────────────────────

const C_BG:         Color32 = Color32::from_rgb(14,  14,  16);   // window background
const C_PANEL:      Color32 = Color32::from_rgb(20,  20,  23);   // panels
const C_HEADER:     Color32 = Color32::from_rgb(26,  26,  30);   // toolbar / col headers
const C_ROW_EVEN:   Color32 = Color32::from_rgb(18,  18,  21);
const C_ROW_ODD:    Color32 = Color32::from_rgb(22,  22,  26);
const C_ROW_HOVER:  Color32 = Color32::from_rgb(28,  38,  55);
const C_ROW_SEL:    Color32 = Color32::from_rgb(24,  62,  110);
const C_BORDER:     Color32 = Color32::from_rgb(38,  38,  44);
const C_ACCENT:     Color32 = Color32::from_rgb(58,  130, 200);  // steel blue
const C_ACCENT_DIM: Color32 = Color32::from_rgb(38,  85,  138);
const C_TEXT:       Color32 = Color32::from_rgb(204, 204, 210);
const C_TEXT_DIM:   Color32 = Color32::from_rgb(110, 110, 122);
const C_TEXT_BRIGHT:Color32 = Color32::from_rgb(230, 230, 236);
const C_SUCCESS:    Color32 = Color32::from_rgb(78,  185, 130);
const C_WARN:       Color32 = Color32::from_rgb(210, 160, 50);
const C_ERROR:      Color32 = Color32::from_rgb(205, 70,  70);
const C_PROGRESS:   Color32 = Color32::from_rgb(58,  130, 200);

const ROW_H:   f32 = 22.0;
const TOOL_H:  f32 = 36.0;
const PATH_H:  f32 = 26.0;
const COL_H:   f32 = 22.0;
const STAT_H:  f32 = 22.0;

fn human(n: usize) -> String {
    if n < 1_024          { format!("{n} B") }
    else if n < 1_048_576 { format!("{:.1} KB", n as f64/1_024.0) }
    else                  { format!("{:.2} MB", n as f64/1_048_576.0) }
}

fn ratio_str(pct: f64) -> String {
    if pct < 0.0 { format!("{:.0}%▲", pct.abs()) }
    else         { format!("{:.0}%", pct) }
}

fn file_icon(ext: &str) -> &'static str {
    match ext {
        "rs"|"py"|"js"|"ts"|"go"|"c"|"cpp"|"h"|"java"|"kt" => "◈",  // code
        "txt"|"md"|"rst"|"log"                               => "≡",  // text
        "png"|"jpg"|"jpeg"|"gif"|"bmp"|"svg"|"webp"         => "⬡",  // image
        "mp3"|"wav"|"flac"|"ogg"|"aac"                       => "♪",  // audio
        "mp4"|"mkv"|"avi"|"mov"                              => "▶",  // video
        "pdf"                                                => "⊞",  // pdf
        "zip"|"tar"|"gz"|"7z"|"axm"                         => "⊟",  // archive
        "json"|"yaml"|"yml"|"toml"|"xml"|"csv"              => "{}",  // data
        _                                                    => "·",  // generic
    }
}

fn folder_depth(path: &str) -> usize {
    path.matches('/').count()
}

fn open_path(path: &Path) {
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("explorer").arg(path).spawn(); }
    #[cfg(target_os = "linux")]
    { let _ = std::process::Command::new("xdg-open").arg(path).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(path).spawn(); }
}

// ─────────────────────────────────────────────────────────────────
// DIRECTORY WALK
// ─────────────────────────────────────────────────────────────────

fn walk_dir(root: &Path) -> Result<Vec<ArchiveEntry>, String> {
    let mut out = Vec::new();
    walk_inner(root, root, &mut out)?;
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}
fn walk_inner(root: &Path, cur: &Path, out: &mut Vec<ArchiveEntry>) -> Result<(), String> {
    for item in fs::read_dir(cur).map_err(|e| format!("{e}"))? {
        let item = item.map_err(|e| e.to_string())?;
        let path = item.path();
        let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            walk_inner(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(root).map_err(|_| "prefix error".to_string())?;
            let rel_path = rel.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>().join("/");
            let data = fs::read(&path).map_err(|e| format!("{e}"))?;
            out.push(ArchiveEntry { rel_path, data });
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
// STATE
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum SortCol { Name, Size, Packed, Ratio, Type }

#[derive(Debug, Clone)]
struct ArchiveView {
    path:      PathBuf,
    root_name: String,
    files:     Vec<ArchiveFileInfo>,
    selected:  HashSet<usize>,
    sort_col:  SortCol,
    sort_asc:  bool,
    sorted_idx:Vec<usize>,   // indices into `files` after sort
}

impl ArchiveView {
    fn new(path: PathBuf, root_name: String, files: Vec<ArchiveFileInfo>) -> Self {
        let n = files.len();
        let sorted_idx: Vec<usize> = (0..n).collect();
        let mut v = Self { path, root_name, files, selected: HashSet::new(),
                           sort_col: SortCol::Name, sort_asc: true, sorted_idx };
        v.apply_sort();
        v
    }

    fn apply_sort(&mut self) {
        let files = &self.files;
        let asc   = self.sort_asc;
        self.sorted_idx.sort_by(|&a, &b| {
            let fa = &files[a]; let fb = &files[b];
            let ord = match self.sort_col {
                SortCol::Name   => fa.rel_path.cmp(&fb.rel_path),
                SortCol::Size   => fa.orig_size.cmp(&fb.orig_size),
                SortCol::Packed => fa.packed_size.cmp(&fb.packed_size),
                SortCol::Ratio  => fa.ratio_pct().partial_cmp(&fb.ratio_pct()).unwrap_or(std::cmp::Ordering::Equal),
                SortCol::Type   => fa.ext().cmp(&fb.ext()),
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    fn sort_by(&mut self, col: SortCol) {
        if self.sort_col == col { self.sort_asc = !self.sort_asc; }
        else { self.sort_col = col; self.sort_asc = true; }
        self.apply_sort();
    }

    fn total_orig(&self)   -> usize { self.files.iter().map(|f| f.orig_size).sum() }
    fn total_packed(&self) -> usize { self.files.iter().map(|f| f.packed_size).sum() }
    fn total_ratio(&self)  -> f64 {
        let o = self.total_orig();
        if o == 0 { return 0.0; }
        (1.0 - self.total_packed() as f64 / o as f64) * 100.0
    }

    fn selected_orig(&self)   -> usize {
        self.selected.iter().map(|&i| self.files[i].orig_size).sum()
    }
}

#[derive(Debug, Clone)]
struct JobResult {
    output_path:  PathBuf,
    input_bytes:  usize,
    output_bytes: usize,
    ratio_pct:    f64,
    elapsed_ms:   u64,
    file_count:   usize,
    operation:    String,
}

#[derive(Debug, Clone)]
enum JobState { Idle, Running(String), Done(JobResult), Error(String) }

impl JobState {
    fn is_running(&self) -> bool { matches!(self, JobState::Running(_)) }
    fn op_label(&self) -> &str {
        match self { JobState::Running(s) => s.as_str(), _ => "" }
    }
}

struct AxmzipApp {
    // Quality settings
    quality:  u8,
    channels: u8,

    // Archive browser (populated when an .axm is open)
    archive:  Option<ArchiveView>,

    // Worker thread comms
    job_state: Arc<Mutex<JobState>>,
    progress:  Arc<Mutex<f32>>,

    // Pending archive path to open after a job completes
    pending_open: Arc<Mutex<Option<PathBuf>>>,

    // UI state
    spinner_angle:  f32,
    show_settings:  bool,
    hover_row:      Option<usize>,  // sorted index being hovered
    last_error:     Option<String>,
    status_override:Option<String>,
}

impl Default for AxmzipApp {
    fn default() -> Self {
        Self {
            quality:        100,
            channels:       1,
            archive:        None,
            job_state:      Arc::new(Mutex::new(JobState::Idle)),
            progress:       Arc::new(Mutex::new(0.0f32)),
            pending_open:   Arc::new(Mutex::new(None)),
            spinner_angle:  0.0,
            show_settings:  false,
            hover_row:      None,
            last_error:     None,
            status_override:None,
        }
    }
}

impl AxmzipApp {
    fn prog(&self) -> f32 { *self.progress.lock().unwrap() }

    fn reset_job(&self) {
        *self.job_state.lock().unwrap() = JobState::Idle;
        *self.progress.lock().unwrap()  = 0.0;
    }

    fn open_archive_from_bytes(&mut self, path: PathBuf, blob: &[u8]) {
        match core::list_archive(blob) {
            Ok((root, files)) => {
                self.archive      = Some(ArchiveView::new(path, root, files));
                self.last_error   = None;
            }
            Err(e) => {
                self.last_error   = Some(e.to_string());
                self.archive      = None;
            }
        }
    }

    fn open_archive_file(&mut self, path: PathBuf) {
        match fs::read(&path) {
            Ok(blob) => self.open_archive_from_bytes(path, &blob),
            Err(e)   => { self.last_error = Some(e.to_string()); }
        }
    }

    fn reload_archive(&mut self) {
        if let Some(ref av) = self.archive.clone() {
            self.open_archive_file(av.path.clone());
        }
    }

    // ── COMPRESS FILE OR FOLDER ───────────────────────────────────
    fn compress_path(&self, input: PathBuf, quality: u8, channels: u8) {
        let state    = Arc::clone(&self.job_state);
        let progress = Arc::clone(&self.progress);
        let pending  = Arc::clone(&self.pending_open);
        *state.lock().unwrap()    = JobState::Running("Compressing…".into());
        *progress.lock().unwrap() = 0.0;

        thread::spawn(move || {
            let result: Result<JobResult, String> = (|| {
                if input.is_dir() {
                    let root = input.file_name().and_then(|n| n.to_str()).unwrap_or("archive");
                    let entries = walk_dir(&input)?;
                    let total_in: usize = entries.iter().map(|e| e.data.len()).sum();
                    let output = input.parent().unwrap_or(Path::new("."))
                        .join(format!("{root}.axm"));
                    let (blob, stats) = core::compress_archive(root, &entries, quality,
                        Some(Arc::clone(&progress)));
                    fs::write(&output, &blob).map_err(|e| e.to_string())?;
                    *pending.lock().unwrap() = Some(output.clone());
                    Ok(JobResult {
                        output_path: output, input_bytes: total_in,
                        output_bytes: stats.compressed_bytes, ratio_pct: stats.ratio_pct,
                        elapsed_ms: stats.elapsed_ms, file_count: stats.file_count,
                        operation: "Compressed".into(),
                    })
                } else {
                    let data  = fs::read(&input).map_err(|e| e.to_string())?;
                    let fname = input.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                    let output = input.with_extension("axm");
                    let (blob, stats) = core::compress(&data, quality, channels, fname,
                        Some(Arc::clone(&progress)));
                    fs::write(&output, &blob).map_err(|e| e.to_string())?;
                    *pending.lock().unwrap() = Some(output.clone());
                    Ok(JobResult {
                        output_path: output, input_bytes: stats.original_bytes,
                        output_bytes: stats.compressed_bytes, ratio_pct: stats.ratio_pct,
                        elapsed_ms: stats.elapsed_ms, file_count: 1,
                        operation: "Compressed".into(),
                    })
                }
            })();
            *progress.lock().unwrap() = 1.0;
            *state.lock().unwrap() = match result {
                Ok(r)  => JobState::Done(r),
                Err(e) => JobState::Error(e),
            };
        });
    }

    // ── EXTRACT ───────────────────────────────────────────────────
    fn extract_all(&self, archive_path: PathBuf, out_dir: PathBuf) {
        let state    = Arc::clone(&self.job_state);
        let progress = Arc::clone(&self.progress);
        *state.lock().unwrap()    = JobState::Running("Extracting…".into());
        *progress.lock().unwrap() = 0.05;

        thread::spawn(move || {
            let result: Result<JobResult, String> = (|| {
                let t0   = std::time::Instant::now();
                let blob = fs::read(&archive_path).map_err(|e| e.to_string())?;
                *progress.lock().unwrap() = 0.15;

                if core::is_axmzip_archive(&blob) {
                    let (_, entries) = core::decompress_archive(&blob).map_err(|e| e.to_string())?;
                    *progress.lock().unwrap() = 0.6;
                    let n = entries.len();
                    let mut total = 0usize;
                    for (i, entry) in entries.iter().enumerate() {
                        if entry.rel_path.contains("..") {
                            return Err(format!("Unsafe path: {}", entry.rel_path));
                        }
                        let rel: PathBuf = entry.rel_path.split('/').collect();
                        let dest = out_dir.join(rel);
                        if let Some(p) = dest.parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
                        fs::write(&dest, &entry.data).map_err(|e| e.to_string())?;
                        total += entry.data.len();
                        *progress.lock().unwrap() = 0.6 + (i+1) as f32/n as f32 * 0.38;
                    }
                    Ok(JobResult {
                        output_path: out_dir, input_bytes: blob.len(),
                        output_bytes: total,
                        ratio_pct: (1.0 - blob.len() as f64 / total.max(1) as f64)*100.0,
                        elapsed_ms: t0.elapsed().as_millis() as u64,
                        file_count: n, operation: "Extracted".into(),
                    })
                } else {
                    let result = core::decompress(&blob).map_err(|e| e.to_string())?;
                    *progress.lock().unwrap() = 0.9;
                    let output = if let Some(ref orig) = result.original_filename {
                        out_dir.join(orig)
                    } else { out_dir.join("output") };
                    if let Some(p) = output.parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
                    fs::write(&output, &result.data).map_err(|e| e.to_string())?;
                    Ok(JobResult {
                        output_path: output, input_bytes: blob.len(),
                        output_bytes: result.data.len(),
                        ratio_pct: (1.0 - blob.len() as f64 / result.data.len().max(1) as f64)*100.0,
                        elapsed_ms: t0.elapsed().as_millis() as u64,
                        file_count: 1, operation: "Extracted".into(),
                    })
                }
            })();
            *progress.lock().unwrap() = 1.0;
            *state.lock().unwrap() = match result {
                Ok(r)  => JobState::Done(r),
                Err(e) => JobState::Error(e),
            };
        });
    }

    // ── EXTRACT SELECTED ─────────────────────────────────────────
    fn extract_selected(&self, archive_path: PathBuf, selected: Vec<usize>, out_dir: PathBuf) {
        let state    = Arc::clone(&self.job_state);
        let progress = Arc::clone(&self.progress);
        *state.lock().unwrap()    = JobState::Running("Extracting…".into());
        *progress.lock().unwrap() = 0.1;

        thread::spawn(move || {
            let result: Result<JobResult, String> = (|| {
                let t0   = std::time::Instant::now();
                let blob = fs::read(&archive_path).map_err(|e| e.to_string())?;
                let (_, all_entries) = core::decompress_archive(&blob).map_err(|e| e.to_string())?;
                *progress.lock().unwrap() = 0.5;

                let n = selected.len();
                let mut total = 0usize;
                for (i, &idx) in selected.iter().enumerate() {
                    if idx >= all_entries.len() { continue; }
                    let entry = &all_entries[idx];
                    if entry.rel_path.contains("..") { continue; }
                    let rel: PathBuf = entry.rel_path.split('/').collect();
                    let dest = out_dir.join(rel);
                    if let Some(p) = dest.parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
                    fs::write(&dest, &entry.data).map_err(|e| e.to_string())?;
                    total += entry.data.len();
                    *progress.lock().unwrap() = 0.5 + (i+1) as f32/n as f32 * 0.48;
                }
                Ok(JobResult {
                    output_path: out_dir, input_bytes: blob.len(),
                    output_bytes: total,
                    ratio_pct: 0.0, elapsed_ms: t0.elapsed().as_millis() as u64,
                    file_count: n, operation: "Extracted".into(),
                })
            })();
            *progress.lock().unwrap() = 1.0;
            *state.lock().unwrap() = match result {
                Ok(r)  => JobState::Done(r),
                Err(e) => JobState::Error(e),
            };
        });
    }
}

// ─────────────────────────────────────────────────────────────────
// EGUI APP
// ─────────────────────────────────────────────────────────────────

impl eframe::App for AxmzipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Animations / repaint
        let running = self.job_state.lock().unwrap().is_running();
        if running {
            self.spinner_angle = (self.spinner_angle
                + ctx.input(|i| i.unstable_dt) * 5.0)
                % (2.0 * std::f32::consts::PI);
            ctx.request_repaint();
        }

        // Check if a compress job finished → auto-open the archive
        if !running {
            let pending = self.pending_open.lock().unwrap().take();
            if let Some(path) = pending {
                self.open_archive_file(path);
            }
            // Promote error from job
            let job = self.job_state.lock().unwrap().clone();
            if let JobState::Error(ref e) = job {
                if self.last_error.is_none() {
                    self.last_error = Some(e.clone());
                }
            }
        }

        // File drop — detect type and act
        ctx.input(|i| {
            for dropped in &i.raw.dropped_files {
                if let Some(p) = &dropped.path {
                    if running { return; }
                    if p.extension().and_then(|e| e.to_str()) == Some("axm") {
                        self.open_archive_file(p.clone());
                    } else {
                        // Dropped a file/folder to compress
                        self.compress_path(p.clone(), self.quality, self.channels);
                    }
                }
            }
        });

        // Global style
        let mut style = (*ctx.style()).clone();
        style.visuals.window_fill  = C_BG;
        style.visuals.panel_fill   = C_BG;
        style.visuals.override_text_color = Some(C_TEXT);
        style.visuals.widgets.noninteractive.bg_fill = C_PANEL;
        style.visuals.widgets.inactive.bg_fill       = C_PANEL;
        style.visuals.widgets.hovered.bg_fill        = C_HEADER;
        style.visuals.widgets.active.bg_fill         = C_ACCENT;
        style.visuals.widgets.noninteractive.rounding = egui::Rounding::ZERO;
        style.visuals.widgets.inactive.rounding       = egui::Rounding::ZERO;
        style.visuals.widgets.hovered.rounding        = egui::Rounding::ZERO;
        style.visuals.widgets.active.rounding         = egui::Rounding::ZERO;
        style.spacing.item_spacing  = Vec2::new(0.0, 0.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        ctx.set_style(style);

        // ── STATUS BAR (bottom) ───────────────────────────────────
        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(STAT_H)
            .frame(Frame::none().fill(C_HEADER).inner_margin(Margin::symmetric(8.0, 0.0)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let job = self.job_state.lock().unwrap().clone();
                    match job {
                        JobState::Running(ref op) => {
                            let pct = (self.prog() * 100.0) as u32;
                            ui.label(RichText::new(format!("{op}  {pct}%"))
                                .color(C_ACCENT).font(FontId::monospace(11.0)));
                        }
                        JobState::Done(ref r) => {
                            ui.label(RichText::new(format!(
                                "{}  ·  {} → {}  ·  {:.0}%  ·  {} ms",
                                r.operation,
                                human(r.input_bytes), human(r.output_bytes),
                                r.ratio_pct.abs(), r.elapsed_ms))
                                .color(C_SUCCESS).font(FontId::monospace(11.0)));
                        }
                        JobState::Error(ref e) => {
                            ui.label(RichText::new(format!("Error: {e}"))
                                .color(C_ERROR).font(FontId::monospace(11.0)));
                        }
                        JobState::Idle => {
                            if let Some(ref av) = self.archive {
                                let sel = av.selected.len();
                                let status = if sel > 0 {
                                    format!("{sel} selected  ({})  ·  {} files  ·  {} → {}  ·  {:.0}%",
                                        human(av.selected_orig()),
                                        av.files.len(), human(av.total_orig()),
                                        human(av.total_packed()), av.total_ratio())
                                } else {
                                    format!("{} files  ·  {} uncompressed  ·  {} packed  ·  {:.0}%",
                                        av.files.len(), human(av.total_orig()),
                                        human(av.total_packed()), av.total_ratio())
                                };
                                ui.label(RichText::new(status)
                                    .color(C_TEXT_DIM).font(FontId::monospace(11.0)));
                            } else if let Some(ref e) = self.last_error.clone() {
                                ui.label(RichText::new(format!("Error: {e}"))
                                    .color(C_ERROR).font(FontId::monospace(11.0)));
                            } else {
                                ui.label(RichText::new("Axmzip v0.5.3  ·  Drop .axm to open  ·  Drop folder/file to compress")
                                    .color(C_TEXT_DIM).font(FontId::monospace(11.0)));
                            }
                        }
                    }
                });
            });

        // ── PROGRESS BAR (above status bar, only while running) ───
        let prog = self.prog();
        if running || (prog > 0.0 && prog < 1.0) {
            egui::TopBottomPanel::bottom("progressbar")
                .exact_height(4.0)
                .frame(Frame::none().fill(C_PANEL))
                .show(ctx, |ui| {
                    let w = ui.available_width();
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 4.0), Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, C_PANEL);
                    let filled = Rect::from_min_size(rect.min, Vec2::new(w * prog, 4.0));
                    ui.painter().rect_filled(filled, 0.0, C_PROGRESS);
                    if running { ctx.request_repaint(); }
                });
        }

        // ── TOOLBAR (top) ─────────────────────────────────────────
        egui::TopBottomPanel::top("toolbar")
            .exact_height(TOOL_H)
            .frame(Frame::none()
                .fill(C_HEADER)
                .inner_margin(Margin::symmetric(4.0, 4.0)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let has_archive  = self.archive.is_some();
                    let has_selected = self.archive.as_ref().map_or(false, |a| !a.selected.is_empty());

                    // LOGO — left of toolbar
                    ui.add_space(6.0);
                    ui.label(RichText::new("AXMZIP")
                        .font(FontId::proportional(13.0))
                        .color(C_ACCENT).strong());
                    ui.add_space(14.0);
                    self.draw_divider(ui);
                    ui.add_space(4.0);

                    // Toolbar buttons
                    self.toolbar_btn(ui, "Add Files",   !running, |app| {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title("Select file or folder to compress")
                            .pick_file() { app.compress_path(p, app.quality, app.channels); }
                    });
                    self.toolbar_btn(ui, "Add Folder",  !running, |app| {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title("Select folder to compress")
                            .pick_folder() { app.compress_path(p, app.quality, app.channels); }
                    });
                    self.toolbar_btn(ui, "Open",  !running, |app| {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Axmzip archive", &["axm"])
                            .add_filter("All files", &["*"])
                            .pick_file() { app.open_archive_file(p); }
                    });

                    ui.add_space(4.0);
                    self.draw_divider(ui);
                    ui.add_space(4.0);

                    self.toolbar_btn_enabled(ui, "Extract All",      has_archive && !running, |app| {
                        if let Some(ref av) = app.archive.clone() {
                            let out = av.path.parent().unwrap_or(Path::new("."))
                                .join(&av.root_name);
                            app.extract_all(av.path.clone(), out);
                        }
                    });
                    self.toolbar_btn_enabled(ui, "Extract Selected", has_selected && !running, |app| {
                        if let Some(ref av) = app.archive.clone() {
                            // selected contains indices into files[], but sorted_idx may reorder them
                            let sel: Vec<usize> = av.selected.iter().copied().collect();
                            let out = av.path.parent().unwrap_or(Path::new("."))
                                .join(&av.root_name);
                            app.extract_selected(av.path.clone(), sel, out);
                        }
                    });
                    self.toolbar_btn_enabled(ui, "Extract To…",      has_archive && !running, |app| {
                        if let Some(ref av) = app.archive.clone() {
                            if let Some(dir) = rfd::FileDialog::new()
                                .set_title("Extract to…").pick_folder()
                            {
                                let sel: Vec<usize> = av.selected.iter().copied().collect();
                                if sel.is_empty() {
                                    app.extract_all(av.path.clone(), dir);
                                } else {
                                    app.extract_selected(av.path.clone(), sel, dir);
                                }
                            }
                        }
                    });

                    ui.add_space(4.0);
                    self.draw_divider(ui);
                    ui.add_space(4.0);

                    self.toolbar_btn_enabled(ui, "Show Folder",  has_archive && !running, |app| {
                        if let Some(ref av) = app.archive { open_path(&av.path); }
                    });

                    // Right-align settings
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        self.toolbar_btn(ui, "⚙", !running, |app| {
                            app.show_settings = !app.show_settings;
                        });

                        // Settings popover (quality slider)
                        if self.show_settings {
                            egui::Window::new("Settings")
                                .fixed_size([260.0, 100.0])
                                .collapsible(false)
                                .resizable(false)
                                .frame(Frame::none()
                                    .fill(C_PANEL)
                                    .stroke(Stroke::new(1.0, C_BORDER))
                                    .inner_margin(Margin::same(12.0)))
                                .show(ctx, |ui| {
                                    ui.label(RichText::new("Compression Quality")
                                        .font(FontId::proportional(11.5)).color(C_TEXT_DIM));
                                    ui.add_space(6.0);
                                    ui.add(egui::Slider::new(&mut self.quality, 0..=100)
                                        .show_value(true)
                                        .suffix("%"));
                                    ui.add_space(4.0);
                                    let desc = match self.quality {
                                        100     => ("Lossless — bit-perfect", C_SUCCESS),
                                        90..=99 => ("Excellent quality (±6 per byte)", C_ACCENT),
                                        75..=89 => ("Good quality (±16 per byte)", C_ACCENT),
                                        50..=74 => ("Acceptable (±32 per byte)", C_WARN),
                                        _       => ("High compression (lossy)", C_ERROR),
                                    };
                                    ui.label(RichText::new(desc.0)
                                        .font(FontId::proportional(10.5)).color(desc.1));
                                    ui.add_space(8.0);
                                    if ui.add(egui::Button::new(
                                        RichText::new("Close").font(FontId::proportional(11.0)))
                                        .fill(C_HEADER)).clicked()
                                    { self.show_settings = false; }
                                });
                        }
                    });
                });
            });

        // ── PATH BAR ─────────────────────────────────────────────
        egui::TopBottomPanel::top("pathbar")
            .exact_height(PATH_H)
            .frame(Frame::none()
                .fill(C_PANEL)
                .stroke(Stroke::new(0.0, C_BORDER))
                .inner_margin(Margin { left: 10.0, right: 10.0, top: 0.0, bottom: 0.0 }))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let path_str = self.archive.as_ref()
                        .map(|a| a.path.display().to_string())
                        .unwrap_or_else(|| "No archive open".to_string());
                    ui.label(RichText::new(&path_str)
                        .font(FontId::monospace(11.0))
                        .color(if self.archive.is_some() { C_TEXT } else { C_TEXT_DIM }));
                    // quality badge
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        let (qlabel, qcol) = if self.quality == 100 {
                            ("LOSSLESS", C_SUCCESS)
                        } else {
                            ("LOSSY", C_WARN)
                        };
                        ui.label(RichText::new(format!("q={}  {qlabel}", self.quality))
                            .font(FontId::monospace(10.0)).color(qcol));
                    });
                });
            });

        // ── COLUMN HEADERS ────────────────────────────────────────
        egui::TopBottomPanel::top("col_headers")
            .exact_height(COL_H)
            .frame(Frame::none()
                .fill(C_HEADER)
                .stroke(Stroke::new(1.0, C_BORDER))
                .inner_margin(Margin::symmetric(0.0, 0.0)))
            .show(ctx, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    // Select-all checkbox space
                    let (check_rect, check_resp) = ui.allocate_exact_size(
                        Vec2::new(28.0, COL_H), Sense::click());
                    let all_sel = self.archive.as_ref().map_or(false, |a|
                        !a.files.is_empty() && a.selected.len() == a.files.len());
                    let check_col = if all_sel { C_ACCENT } else { C_BORDER };
                    ui.painter().rect_stroke(
                        check_rect.shrink2(Vec2::new(8.0, 5.0)), 1.0,
                        Stroke::new(1.0, check_col));
                    if all_sel {
                        ui.painter().line_segment(
                            [check_rect.center() - Vec2::new(3.0, 0.0),
                             check_rect.center() + Vec2::new(3.0, 0.0)],
                            Stroke::new(1.5, C_ACCENT));
                    }
                    if check_resp.clicked() {
                        if let Some(ref mut av) = self.archive {
                            if all_sel { av.selected.clear(); }
                            else { av.selected = (0..av.files.len()).collect(); }
                        }
                    }

                    self.col_header(ui, "Name",   400.0, SortCol::Name);
                    self.col_header(ui, "Size",   80.0,  SortCol::Size);
                    self.col_header(ui, "Packed", 80.0,  SortCol::Packed);
                    self.col_header(ui, "Ratio",  60.0,  SortCol::Ratio);
                    self.col_header(ui, "Type",   60.0,  SortCol::Type);
                });
            });

        // ── CENTRAL — file list or welcome screen ─────────────────
        egui::CentralPanel::default()
            .frame(Frame::none().fill(C_BG))
            .show(ctx, |ui| {
                if let Some(ref av) = self.archive.clone() {
                    self.draw_file_list(ui, av.clone());
                } else {
                    self.draw_welcome(ui, running);
                }
            });
    }
}

// ─────────────────────────────────────────────────────────────────
// DRAW HELPERS
// ─────────────────────────────────────────────────────────────────

impl AxmzipApp {
    fn draw_divider(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, TOOL_H - 12.0), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, C_BORDER);
    }

    // Generic toolbar button that mutates self via closure
    fn toolbar_btn<F: FnOnce(&mut AxmzipApp)>(
        &mut self, ui: &mut egui::Ui, label: &str, enabled: bool, action: F,
    ) {
        self.toolbar_btn_enabled(ui, label, enabled, action);
    }

    fn toolbar_btn_enabled<F: FnOnce(&mut AxmzipApp)>(
        &mut self, ui: &mut egui::Ui, label: &str, enabled: bool, action: F,
    ) {
        let col = if enabled { C_TEXT } else { C_TEXT_DIM };
        let btn = ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).font(FontId::proportional(11.5)).color(col))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::NONE)
                .min_size(Vec2::new(0.0, TOOL_H - 8.0))
        );
        if btn.clicked() { action(self); }
    }

    fn col_header(&mut self, ui: &mut egui::Ui, label: &str, width: f32, col: SortCol) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, COL_H), Sense::click());
        let is_active = self.archive.as_ref().map_or(false, |a| a.sort_col == col);
        let bg = if resp.hovered() { C_ROW_HOVER } else { C_HEADER };
        ui.painter().rect_filled(rect, 0.0, bg);
        // Right border
        ui.painter().line_segment(
            [rect.right_top(), rect.right_bottom()],
            Stroke::new(1.0, C_BORDER));

        let text_col = if is_active { C_TEXT_BRIGHT } else { C_TEXT_DIM };
        let sort_indicator = if is_active {
            if self.archive.as_ref().map_or(true, |a| a.sort_asc) { " ▲" } else { " ▼" }
        } else { "" };

        ui.painter().text(
            rect.min + Vec2::new(8.0, COL_H/2.0),
            egui::Align2::LEFT_CENTER,
            format!("{label}{sort_indicator}"),
            FontId::proportional(11.0),
            text_col,
        );

        if resp.clicked() {
            if let Some(ref mut av) = self.archive { av.sort_by(col); }
        }
    }

    fn draw_file_list(&mut self, ui: &mut egui::Ui, av: ArchiveView) {
        let avail_h = ui.available_height();
        ScrollArea::vertical()
            .id_source("filelist")
            .max_height(avail_h)
            .show(ui, |ui| {
                let total = av.sorted_idx.len();
                let mut click_idx: Option<usize> = None;
                let mut ctrl_click: Option<usize> = None;
                let mut hover_sorted: Option<usize> = None;
                let ctrl_held = ui.input(|i| i.modifiers.ctrl);

                for (row_pos, &file_idx) in av.sorted_idx.iter().enumerate() {
                    let file = &av.files[file_idx];
                    let is_selected = av.selected.contains(&file_idx);
                    let is_hovered  = self.hover_row == Some(row_pos);

                    let row_bg = if is_selected { C_ROW_SEL }
                                 else if is_hovered { C_ROW_HOVER }
                                 else if row_pos % 2 == 0 { C_ROW_EVEN }
                                 else { C_ROW_ODD };

                    let (row_rect, row_resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_H), Sense::click());

                    if row_resp.hovered() { hover_sorted = Some(row_pos); }

                    ui.painter().rect_filled(row_rect, 0.0, row_bg);
                    // Bottom border
                    ui.painter().line_segment(
                        [row_rect.left_bottom(), row_rect.right_bottom()],
                        Stroke::new(0.5, C_BORDER));

                    // ── Checkbox ──────────────────────────────────
                    let ck_rect = Rect::from_min_size(
                        row_rect.min + Vec2::new(8.0, (ROW_H-10.0)/2.0),
                        Vec2::new(10.0, 10.0));
                    ui.painter().rect_stroke(ck_rect, 1.0, Stroke::new(1.0,
                        if is_selected { C_ACCENT } else { C_BORDER }));
                    if is_selected {
                        ui.painter().line_segment(
                            [ck_rect.min + Vec2::new(1.5, 5.0),
                             ck_rect.min + Vec2::new(4.0, 7.5)],
                            Stroke::new(1.5, C_ACCENT));
                        ui.painter().line_segment(
                            [ck_rect.min + Vec2::new(4.0, 7.5),
                             ck_rect.min + Vec2::new(8.5, 2.0)],
                            Stroke::new(1.5, C_ACCENT));
                    }

                    // ── Icon + Name ───────────────────────────────
                    let depth   = folder_depth(&file.rel_path);
                    let indent  = 28.0 + depth as f32 * 12.0;
                    let ext     = file.ext();
                    let icon    = file_icon(&ext);
                    let icon_x  = row_rect.min.x + indent;
                    let text_y  = row_rect.center().y;

                    ui.painter().text(
                        egui::pos2(icon_x, text_y), egui::Align2::LEFT_CENTER,
                        icon, FontId::monospace(11.0), C_TEXT_DIM);

                    let name = file.rel_path.rsplit('/').next().unwrap_or(&file.rel_path);
                    ui.painter().text(
                        egui::pos2(icon_x + 18.0, text_y),
                        egui::Align2::LEFT_CENTER,
                        name, FontId::proportional(11.5),
                        if is_selected { C_TEXT_BRIGHT } else { C_TEXT });

                    // Show full path as dim suffix if nested
                    if depth > 0 {
                        let parent = file.rel_path.rfind('/').map(|i| &file.rel_path[..i]).unwrap_or("");
                        ui.painter().text(
                            egui::pos2(icon_x + 18.0 + name.len() as f32 * 6.8, text_y),
                            egui::Align2::LEFT_CENTER,
                            format!("  /  {parent}"),
                            FontId::proportional(10.0), C_TEXT_DIM);
                    }

                    // ── Size ─────────────────────────────────────
                    let col_x2 = row_rect.min.x + 400.0;
                    ui.painter().text(
                        egui::pos2(col_x2 + 74.0, text_y),
                        egui::Align2::RIGHT_CENTER,
                        human(file.orig_size), FontId::monospace(11.0), C_TEXT_DIM);

                    // ── Packed ────────────────────────────────────
                    let col_x3 = col_x2 + 80.0;
                    ui.painter().text(
                        egui::pos2(col_x3 + 74.0, text_y),
                        egui::Align2::RIGHT_CENTER,
                        human(file.packed_size), FontId::monospace(11.0), C_TEXT_DIM);

                    // ── Ratio ─────────────────────────────────────
                    let col_x4 = col_x3 + 80.0;
                    let ratio_pct = file.ratio_pct();
                    let ratio_col = if ratio_pct > 50.0 { C_SUCCESS }
                                    else if ratio_pct > 0.0 { C_TEXT_DIM }
                                    else { C_WARN };
                    ui.painter().text(
                        egui::pos2(col_x4 + 54.0, text_y),
                        egui::Align2::RIGHT_CENTER,
                        ratio_str(ratio_pct), FontId::monospace(11.0), ratio_col);

                    // ── Type ──────────────────────────────────────
                    let col_x5 = col_x4 + 60.0;
                    ui.painter().text(
                        egui::pos2(col_x5 + 8.0, text_y),
                        egui::Align2::LEFT_CENTER,
                        if ext.is_empty() { "—".into() } else { ext.clone() },
                        FontId::monospace(10.5), C_TEXT_DIM);

                    if row_resp.clicked() {
                        if ctrl_held { ctrl_click = Some(file_idx); }
                        else         { click_idx  = Some(file_idx); }
                    }
                    if row_resp.double_clicked() {
                        // Double-click: extract just this file
                        if let Some(ref av2) = self.archive.clone() {
                            let out = av2.path.parent().unwrap_or(Path::new("."))
                                .join(&av2.root_name);
                            self.extract_selected(av2.path.clone(), vec![file_idx], out);
                        }
                    }
                }

                self.hover_row = hover_sorted;

                // Apply selection changes to real archive state
                if let Some(ref mut real_av) = self.archive {
                    if let Some(idx) = click_idx {
                        if real_av.selected.contains(&idx) && real_av.selected.len() == 1 {
                            real_av.selected.clear();
                        } else {
                            real_av.selected.clear();
                            real_av.selected.insert(idx);
                        }
                    } else if let Some(idx) = ctrl_click {
                        if real_av.selected.contains(&idx) { real_av.selected.remove(&idx); }
                        else { real_av.selected.insert(idx); }
                    }
                }

                // Empty archive
                if total == 0 {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Archive is empty")
                            .color(C_TEXT_DIM).font(FontId::proportional(13.0)));
                    });
                }
            });
    }

    fn draw_welcome(&self, ui: &mut egui::Ui, running: bool) {
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());

        // Large drop hint
        let center = rect.center();
        let painter = ui.painter();

        // Grid texture
        let step = 28.0f32;
        let grid_col = Color32::from_rgba_unmultiplied(255, 255, 255, 8);
        let mut x = rect.min.x + (step - rect.min.x % step);
        while x < rect.max.x {
            painter.line_segment([egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                Stroke::new(0.5, grid_col));
            x += step;
        }
        let mut y = rect.min.y + (step - rect.min.y % step);
        while y < rect.max.y {
            painter.line_segment([egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                Stroke::new(0.5, grid_col));
            y += step;
        }

        if running {
            // Show progress in center while compressing
            let prog = *self.progress.lock().unwrap();
            let op   = self.job_state.lock().unwrap().op_label().to_string();
            painter.text(center - Vec2::new(0.0, 24.0), egui::Align2::CENTER_CENTER,
                &op, FontId::proportional(16.0), C_ACCENT);
            let bar_w  = 320.0f32;
            let bar_h  = 8.0f32;
            let bar_r  = Rect::from_center_size(center, Vec2::new(bar_w, bar_h));
            painter.rect_filled(bar_r, 2.0, C_HEADER);
            painter.rect_filled(
                Rect::from_min_size(bar_r.min, Vec2::new(bar_w * prog, bar_h)),
                2.0, C_PROGRESS);
            painter.text(center + Vec2::new(0.0, 24.0), egui::Align2::CENTER_CENTER,
                format!("{:.0}%", prog * 100.0),
                FontId::monospace(13.0), C_TEXT_DIM);
        } else {
            // Drop zone hint
            let box_size = Vec2::new(360.0, 180.0);
            let box_rect = Rect::from_center_size(center, box_size);
            painter.rect_stroke(box_rect, 4.0,
                Stroke::new(1.5, Color32::from_rgba_unmultiplied(58, 130, 200, 60)));
            painter.rect_filled(box_rect, 4.0,
                Color32::from_rgba_unmultiplied(20, 30, 50, 80));

            painter.text(center - Vec2::new(0.0, 42.0), egui::Align2::CENTER_CENTER,
                "⊟", FontId::proportional(48.0),
                Color32::from_rgba_unmultiplied(58, 130, 200, 100));
            painter.text(center - Vec2::new(0.0, 4.0), egui::Align2::CENTER_CENTER,
                "Drop .axm archive to browse", FontId::proportional(14.0), C_TEXT);
            painter.text(center + Vec2::new(0.0, 22.0), egui::Align2::CENTER_CENTER,
                "or drop a file / folder to compress", FontId::proportional(11.5), C_TEXT_DIM);
            painter.text(center + Vec2::new(0.0, 52.0), egui::Align2::CENTER_CENTER,
                "Use toolbar: Open · Add Files · Add Folder",
                FontId::monospace(10.0), C_TEXT_DIM);
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// ENTRY POINT
// ─────────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Axmzip")
            .with_inner_size([720.0, 540.0])
            .with_min_inner_size([560.0, 380.0])
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native("Axmzip", options, Box::new(|cc| {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
         Box::new(AxmzipApp::default()) as Box<dyn eframe::App>
    }))
}
