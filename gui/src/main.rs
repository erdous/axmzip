// Axmzip GUI — egui/eframe application
//
// Simple mode  : drag-drop file, big Compress/Decompress buttons, progress, result
// Advanced mode: quality slider, channels selector, stats panel, mode breakdown

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]  // hide console on Windows release

use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};
use std::{path::PathBuf, sync::{Arc, Mutex}, thread};
use axmzip_core as core;

// ─────────────────────────────────────────────────────────────────
// APP STATE
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum AppMode { Compress, Decompress }

#[derive(Debug, Clone, PartialEq)]
enum JobState {
    Idle,
    Running(f32),   // 0.0 – 1.0 progress
    Done(JobResult),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
struct JobResult {
    input_path:   PathBuf,
    output_path:  PathBuf,
    input_bytes:  usize,
    output_bytes: usize,
    ratio_pct:    f64,
    mode_str:     String,
    lossless:     bool,
    psnr_db:      f64,
    max_error:    u8,
    elapsed_ms:   u64,
}

struct AxmzipApp {
    // UI state
    app_mode:      AppMode,
    advanced_open: bool,
    quality:       u8,
    channels:      u8,
    dropped_file:  Option<PathBuf>,
    selected_file: Option<PathBuf>,

    // Job state (shared with worker thread)
    job_state: Arc<Mutex<JobState>>,

    // Animation
    ring_angle: f32,
}

impl Default for AxmzipApp {
    fn default() -> Self {
        Self {
            app_mode:      AppMode::Compress,
            advanced_open: false,
            quality:       100,
            channels:      1,
            dropped_file:  None,
            selected_file: None,
            job_state:     Arc::new(Mutex::new(JobState::Idle)),
            ring_angle:    0.0,
        }
    }
}

impl AxmzipApp {
    fn active_file(&self) -> Option<&PathBuf> {
        self.dropped_file.as_ref().or(self.selected_file.as_ref())
    }

    fn run_compress(&self, input: PathBuf, quality: u8, channels: u8) {
        let state = Arc::clone(&self.job_state);
        *state.lock().unwrap() = JobState::Running(0.0);
        thread::spawn(move || {
            let result = (|| -> Result<JobResult, String> {
                // 0 % — starting read
                *state.lock().unwrap() = JobState::Running(0.05);
                let data = std::fs::read(&input).map_err(|e| e.to_string())?;

                // 30 % — file read, beginning compression
                *state.lock().unwrap() = JobState::Running(0.30);
                let (blob, stats) = core::compress(&data, quality, channels);

                // 85 % — compression done, writing output
                *state.lock().unwrap() = JobState::Running(0.85);
                let mut output = input.clone();
                output.set_extension("axm");
                std::fs::write(&output, &blob).map_err(|e| e.to_string())?;

                // 100 % — done
                *state.lock().unwrap() = JobState::Running(1.0);

                Ok(JobResult {
                    input_path:   input,
                    output_path:  output,
                    input_bytes:  stats.original_bytes,
                    output_bytes: stats.compressed_bytes,
                    ratio_pct:    stats.ratio_pct,
                    mode_str:     stats.mode,
                    lossless:     stats.lossless,
                    psnr_db:      stats.psnr_db,
                    max_error:    stats.max_error,
                    elapsed_ms:   stats.elapsed_ms,
                })
            })();
            *state.lock().unwrap() = match result {
                Ok(r)  => JobState::Done(r),
                Err(e) => JobState::Error(e),
            };
        });
    }

    fn run_decompress(&self, input: PathBuf) {
        let state = Arc::clone(&self.job_state);
        *state.lock().unwrap() = JobState::Running(0.0);
        thread::spawn(move || {
            let result = (|| -> Result<JobResult, String> {
                // 5 % — starting read
                *state.lock().unwrap() = JobState::Running(0.05);
                let blob = std::fs::read(&input).map_err(|e| e.to_string())?;
                let t0   = std::time::Instant::now();

                // 20 % — file read, beginning decompression
                *state.lock().unwrap() = JobState::Running(0.20);
                let data = core::decompress(&blob).map_err(|e| e.to_string())?;

                // 90 % — decompression done, writing output
                *state.lock().unwrap() = JobState::Running(0.90);
                let mut output = input.clone();
                let stem = output.file_stem()
                    .and_then(|s| s.to_str()).unwrap_or("output").to_string();
                output.set_file_name(format!("{stem}_decoded.bin"));
                std::fs::write(&output, &data).map_err(|e| e.to_string())?;

                // 100 % — done
                *state.lock().unwrap() = JobState::Running(1.0);

                Ok(JobResult {
                    input_path:   input,
                    output_path:  output,
                    input_bytes:  blob.len(),
                    output_bytes: data.len(),
                    ratio_pct:    (1.0 - blob.len() as f64 / data.len() as f64) * 100.0,
                    mode_str:     "decompressed".into(),
                    lossless:     true,
                    psnr_db:      f64::INFINITY,
                    max_error:    0,
                    elapsed_ms:   t0.elapsed().as_millis() as u64,
                })
            })();
            *state.lock().unwrap() = match result {
                Ok(r)  => JobState::Done(r),
                Err(e) => JobState::Error(e),
            };
        });
    }
}

// ─────────────────────────────────────────────────────────────────
// COLOURS & STYLE
// ─────────────────────────────────────────────────────────────────

const BG:        Color32 = Color32::from_rgb(13,  13,  20);
const SURFACE:   Color32 = Color32::from_rgb(22,  22,  35);
const SURFACE2:  Color32 = Color32::from_rgb(30,  30,  48);
const ACCENT:    Color32 = Color32::from_rgb(120, 80,  255);
const ACCENT2:   Color32 = Color32::from_rgb(80,  200, 180);
const TEXT:      Color32 = Color32::from_rgb(220, 220, 235);
const TEXT_DIM:  Color32 = Color32::from_rgb(120, 120, 145);
const SUCCESS:   Color32 = Color32::from_rgb(80,  220, 120);
const ERROR_COL: Color32 = Color32::from_rgb(240, 80,  80);
const WARNING:   Color32 = Color32::from_rgb(240, 180, 60);

fn human(n: usize) -> String {
    if n < 1024           { format!("{n} B") }
    else if n < 1024*1024 { format!("{:.1} KB", n as f64 / 1024.0) }
    else                  { format!("{:.2} MB", n as f64 / 1024.0 / 1024.0) }
}

// ─────────────────────────────────────────────────────────────────
// MAIN UI
// ─────────────────────────────────────────────────────────────────

impl eframe::App for AxmzipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Check running state ───────────────────────────
        let job_snapshot = self.job_state.lock().unwrap().clone();
        let running = matches!(job_snapshot, JobState::Running(_));

        // Keep animating & repainting while a job is in flight
        if running {
            self.ring_angle = (self.ring_angle + ctx.input(|i| i.unstable_dt) * 3.0)
                % (2.0 * std::f32::consts::PI);
            ctx.request_repaint();
        }

        // Handle file drops
        ctx.input(|i| {
            if let Some(f) = i.raw.dropped_files.first() {
                if let Some(p) = &f.path {
                    self.dropped_file = Some(p.clone());
                }
            }
        });

        // Apply visual style
        let mut style = (*ctx.style()).clone();
        style.visuals.window_fill                        = BG;
        style.visuals.panel_fill                         = BG;
        style.visuals.override_text_color                = Some(TEXT);
        style.visuals.widgets.noninteractive.bg_fill     = SURFACE;
        style.visuals.widgets.inactive.bg_fill           = SURFACE;
        style.visuals.widgets.hovered.bg_fill            = SURFACE2;
        style.visuals.widgets.active.bg_fill             = ACCENT;
        style.visuals.widgets.noninteractive.rounding    = egui::Rounding::same(8.0);
        style.visuals.widgets.inactive.rounding          = egui::Rounding::same(8.0);
        ctx.set_style(style);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.set_min_size(Vec2::new(520.0, 600.0));

            // ── Header ───────────────────────────────────
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("⬡ AXMZIP")
                    .font(FontId::proportional(32.0))
                    .color(ACCENT)
                    .strong());
                ui.label(RichText::new("Axiom-Based Binary Compression")
                    .font(FontId::proportional(13.0))
                    .color(TEXT_DIM));
            });
            ui.add_space(16.0);

            // ── Mode toggle ───────────────────────────────
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.add_space(ui.available_width() / 2.0 - 110.0);
                    let compress_col   = if self.app_mode == AppMode::Compress   { ACCENT } else { SURFACE2 };
                    let decompress_col = if self.app_mode == AppMode::Decompress { ACCENT } else { SURFACE2 };

                    if ui.add(egui::Button::new(RichText::new("Compress").color(TEXT).strong())
                        .fill(compress_col).min_size(Vec2::new(100.0, 32.0))).clicked()
                    {
                        self.app_mode = AppMode::Compress;
                        *self.job_state.lock().unwrap() = JobState::Idle;
                    }
                    if ui.add(egui::Button::new(RichText::new("Decompress").color(TEXT).strong())
                        .fill(decompress_col).min_size(Vec2::new(100.0, 32.0))).clicked()
                    {
                        self.app_mode = AppMode::Decompress;
                        *self.job_state.lock().unwrap() = JobState::Idle;
                    }
                });
            });
            ui.add_space(16.0);

            // ── Drop zone ─────────────────────────────────
            let file_label = self.active_file()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("Drop file here or click Browse");

            let drop_color = if self.active_file().is_some() { ACCENT2 } else { TEXT_DIM };

            let drop_resp = ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width() - 40.0, 90.0),
                    egui::Sense::click(),
                );
                let painter = ui.painter();
                painter.rect_stroke(rect, 12.0, Stroke::new(2.0, drop_color));
                painter.rect_filled(rect, 12.0, Color32::from_rgba_unmultiplied(30, 30, 50, 120));
                painter.text(rect.center() - Vec2::new(0.0, 10.0),
                    egui::Align2::CENTER_CENTER, "⬆", FontId::proportional(28.0), drop_color);
                painter.text(rect.center() + Vec2::new(0.0, 16.0),
                    egui::Align2::CENTER_CENTER, file_label, FontId::proportional(12.0), drop_color);
                resp
            }).inner;

            if drop_resp.clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.selected_file = Some(path);
                    *self.job_state.lock().unwrap() = JobState::Idle;
                }
            }
            ui.add_space(12.0);

            // ── Advanced toggle ───────────────────────────
            ui.vertical_centered(|ui| {
                if ui.add(egui::Button::new(
                    RichText::new(if self.advanced_open { "▲ Simple mode" } else { "▼ Advanced options" })
                        .color(TEXT_DIM).font(FontId::proportional(12.0)))
                    .fill(Color32::TRANSPARENT).stroke(Stroke::NONE)).clicked()
                {
                    self.advanced_open = !self.advanced_open;
                }
            });

            // ── Advanced panel ────────────────────────────
            if self.advanced_open {
                egui::Frame::none()
                    .fill(SURFACE)
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                    .show(ui, |ui| {
                        ui.add_space(4.0);

                        // Quality slider
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Quality").color(TEXT_DIM).font(FontId::proportional(12.0)));
                            ui.add_space(8.0);
                            let q_col = if self.quality == 100 { SUCCESS }
                                        else if self.quality >= 75 { ACCENT2 }
                                        else { WARNING };
                            ui.add(egui::Slider::new(&mut self.quality, 0..=100)
                                .text("").show_value(false));
                            let label = if self.quality == 100 { "Lossless".into() }
                                        else { format!("q={} (lossy)", self.quality) };
                            ui.label(RichText::new(label).color(q_col).font(FontId::proportional(12.0)));
                        });

                        // Quality description
                        let desc = match self.quality {
                            100     => "Perfect reconstruction guaranteed",
                            90..=99 => "Excellent quality — ±6 per byte max",
                            75..=89 => "Good quality — ±16 per byte max",
                            50..=74 => "Acceptable quality — ±32 per byte max",
                            _       => "High compression — visible quality loss",
                        };
                        ui.label(RichText::new(desc).color(TEXT_DIM).font(FontId::proportional(11.0)));
                        ui.add_space(8.0);

                        // Channels
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Channels").color(TEXT_DIM).font(FontId::proportional(12.0)));
                            ui.add_space(8.0);
                            for (label, val) in [("Auto (1)", 1u8), ("RGB (3)", 3), ("RGBA (4)", 4)] {
                                let col = if self.channels == val { ACCENT } else { SURFACE2 };
                                if ui.add(egui::Button::new(
                                    RichText::new(label).font(FontId::proportional(11.0)))
                                    .fill(col).min_size(Vec2::new(70.0, 24.0))).clicked()
                                {
                                    self.channels = val;
                                }
                            }
                        });
                    });
                ui.add_space(12.0);
            }

            // ── Action button ─────────────────────────────
            let has_file   = self.active_file().is_some();
            let is_running = running;

            ui.vertical_centered(|ui| {
                let btn_text = if is_running {
                    "Working…"
                } else if self.app_mode == AppMode::Compress {
                    "Compress"
                } else {
                    "Decompress"
                };

                let btn = ui.add_enabled(
                    has_file && !is_running,
                    egui::Button::new(
                        RichText::new(btn_text)
                            .font(FontId::proportional(16.0)).strong().color(TEXT))
                        .fill(if has_file && !is_running { ACCENT } else { SURFACE2 })
                        .min_size(Vec2::new(200.0, 48.0)),
                );

                if btn.clicked() {
                    if let Some(path) = self.active_file().cloned() {
                        if self.app_mode == AppMode::Compress {
                            self.run_compress(path, self.quality, self.channels);
                        } else {
                            self.run_decompress(path);
                        }
                    }
                }
            });

            ui.add_space(14.0);

            // ── Progress bar (shown only while running) ───
            if let JobState::Running(pct) = job_snapshot.clone() {
                ui.vertical_centered(|ui| {
                    let bar_w = ui.available_width() - 40.0;

                    // Label above bar
                    let stage_label = match pct {
                        p if p < 0.10 => "Reading file…",
                        p if p < 0.50 => if self.app_mode == AppMode::Compress {
                            "Compressing…"
                        } else {
                            "Decompressing…"
                        },
                        p if p < 0.95 => "Writing output…",
                        _             => "Finishing…",
                    };
                    ui.label(RichText::new(stage_label)
                        .color(TEXT_DIM).font(FontId::proportional(11.0)));
                    ui.add_space(4.0);

                    // Track
                    let (track_rect, _) = ui.allocate_exact_size(
                        Vec2::new(bar_w, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(track_rect, 5.0, SURFACE2);

                    // Fill — animated leading edge using ring_angle for a subtle pulse
                    let pulse  = 1.0 + 0.03 * self.ring_angle.sin();
                    let fill_w = (bar_w * pct * pulse).min(bar_w);
                    let fill_rect = egui::Rect::from_min_size(
                        track_rect.min, Vec2::new(fill_w, 10.0));

                    // Gradient-ish: blend ACCENT → ACCENT2 based on progress
                    let bar_col = Color32::from_rgb(
                        lerp_u8(ACCENT.r(), ACCENT2.r(), pct),
                        lerp_u8(ACCENT.g(), ACCENT2.g(), pct),
                        lerp_u8(ACCENT.b(), ACCENT2.b(), pct),
                    );
                    ui.painter().rect_filled(fill_rect, 5.0, bar_col);

                    // Spinning dot at the leading edge
                    let dot_x = track_rect.min.x + fill_w;
                    let dot_y = track_rect.center().y;
                    ui.painter().circle_filled(
                        egui::pos2(dot_x, dot_y), 5.0, ACCENT2);

                    ui.add_space(4.0);

                    // Percentage label
                    ui.label(RichText::new(format!("{:.0}%", pct * 100.0))
                        .color(ACCENT2).font(FontId::proportional(11.0)));
                });
                ui.add_space(14.0);
            }

            // ── Result panel ──────────────────────────────
            match &job_snapshot {
                JobState::Done(r) => {
                    egui::Frame::none()
                        .fill(SURFACE)
                        .rounding(egui::Rounding::same(12.0))
                        .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                        .show(ui, |ui| {
                            // Big ratio display
                            ui.horizontal(|ui| {
                                let arrow = if r.ratio_pct >= 0.0 { "▼" } else { "▲" };
                                let col   = if r.ratio_pct >= 0.0 { SUCCESS } else { ERROR_COL };
                                ui.label(RichText::new(format!("{arrow}{:.1}%", r.ratio_pct.abs()))
                                    .font(FontId::proportional(36.0)).color(col).strong());
                                ui.add_space(12.0);
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&r.mode_str)
                                        .color(ACCENT2).font(FontId::proportional(12.0)));
                                    if !r.lossless {
                                        let psnr_col = if r.psnr_db > 40.0 { SUCCESS }
                                                       else if r.psnr_db > 30.0 { ACCENT2 }
                                                       else { WARNING };
                                        ui.label(RichText::new(
                                            format!("PSNR: {:.1} dB  max err ±{}", r.psnr_db, r.max_error))
                                            .color(psnr_col).font(FontId::proportional(11.0)));
                                    } else {
                                        ui.label(RichText::new("Lossless ✓")
                                            .color(SUCCESS).font(FontId::proportional(11.0)));
                                    }
                                });
                            });

                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(6.0);

                            // Stats grid
                            egui::Grid::new("stats").num_columns(2).spacing([20.0, 4.0]).show(ui, |ui| {
                                let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                                    ui.label(RichText::new(k).color(TEXT_DIM).font(FontId::proportional(11.0)));
                                    ui.label(RichText::new(v).color(TEXT).font(FontId::proportional(11.0)));
                                    ui.end_row();
                                };
                                kv(ui, "Input",  format!("{} ({})",
                                    r.input_path.file_name().unwrap_or_default().to_string_lossy(),
                                    human(r.input_bytes)));
                                kv(ui, "Output", format!("{} ({})",
                                    r.output_path.file_name().unwrap_or_default().to_string_lossy(),
                                    human(r.output_bytes)));
                                kv(ui, "Saved",  human(r.input_bytes.saturating_sub(r.output_bytes)));
                                kv(ui, "Time",   format!("{} ms", r.elapsed_ms));
                            });

                            ui.add_space(10.0);

                            // Ratio bar
                            let ratio_clamped = (r.ratio_pct / 100.0).clamp(0.0, 1.0) as f32;
                            let bar_w = ui.available_width();
                            let (bar_rect, _) = ui.allocate_exact_size(Vec2::new(bar_w, 8.0), egui::Sense::hover());
                            ui.painter().rect_filled(bar_rect, 4.0, SURFACE2);
                            let fill_rect = egui::Rect::from_min_size(
                                bar_rect.min, Vec2::new(bar_w * ratio_clamped, 8.0));
                            ui.painter().rect_filled(fill_rect, 4.0,
                                if r.lossless { ACCENT } else { ACCENT2 });

                            ui.add_space(10.0);

                            // Open output folder
                            ui.vertical_centered(|ui| {
                                if ui.add(egui::Button::new(
                                    RichText::new("📂  Open output folder")
                                        .font(FontId::proportional(12.0)))
                                    .fill(SURFACE2)).clicked()
                                {
                                    if let Some(parent) = r.output_path.parent() {
                                        let _ = open::that(parent);
                                    }
                                }
                            });
                        });
                }

                JobState::Error(e) => {
                    egui::Frame::none()
                        .fill(Color32::from_rgb(50, 20, 20))
                        .rounding(egui::Rounding::same(10.0))
                        .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(format!("✗  {e}")).color(ERROR_COL));
                        });
                }

                _ => {}
            }

            ui.add_space(16.0);

            // ── Footer ────────────────────────────────────
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Axmzip v0.5 · Apache 2.0 · github.com/erdous/axmzip")
                    .color(TEXT_DIM).font(FontId::proportional(10.0)));
            });
        });
    }
}

// ─────────────────────────────────────────────────────────────────
// HELPERS
// ─────────────────────────────────────────────────────────────────

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

// ─────────────────────────────────────────────────────────────────
// ENTRY POINT
// ─────────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Axmzip")
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([440.0, 560.0])
            .with_resizable(true)
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "Axmzip",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Box::new(AxmzipApp::default())
        }),
    )
}