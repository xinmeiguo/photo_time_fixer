//! egui 界面：状态定义、面板骨架与入口。
//!
//! 原先是一个 1200+ 行的单文件，轮询、动作、两个 Tab 的渲染全挤在一起。
//! 现在按职责拆开：
//! - `state`：状态类型、后台消息枚举、路径小工具
//! - `poll`：后台线程消息回收（唯一一处 `try_recv`）
//! - `actions`：用户动作的触发（扫描 / 应用 / 转换 / 下载）
//! - `tab_time`、`tab_convert`：两个 Tab 的渲染

mod actions;
mod poll;
mod state;
mod tab_convert;
mod tab_time;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use eframe::egui;

use crate::apply::{PlanItem, ProgressMsg};
use crate::exiftool;
use crate::ffmpeg;
use crate::ffmpeg::ConvertMode;
use crate::files::MediaFile;

use state::{ConvertMsg, DlState, DownloadMsg};
pub use state::{Mode, Tab};

pub struct PhotoTimeFixer {
    // ── 时间页 ──
    folder: String,
    recursive: bool,
    time_input: String,
    mode: Mode,
    interval_min: u32,
    random: bool,
    offset_hours: i32,
    offset_mins: i32,
    tz_hours: i32,
    set_taken: bool,
    set_created: bool,
    set_modified: bool,
    files: Vec<MediaFile>,
    plan: Option<Vec<PlanItem>>,
    log: Vec<String>,
    scanning: bool,
    scan_rx: Option<Receiver<Vec<MediaFile>>>,

    // ── 外部工具状态（exiftool / ffmpeg）──
    exiftool_installed: bool,
    exiftool_version: Option<String>,
    exiftool_dl: DlState,
    exiftool_rx: Option<Receiver<DownloadMsg>>,
    ffmpeg_installed: bool,
    ffmpeg_version: Option<String>,
    ffmpeg_exe: Option<PathBuf>,
    ffmpeg_dl: DlState,
    ffmpeg_rx: Option<Receiver<DownloadMsg>>,

    // ── 共用进度（应用 / 转换轮流使用，两者不会同时跑）──
    progress_done: usize,
    progress_total: usize,
    progress_current: String,

    applying: bool,
    apply_rx: Option<Receiver<ProgressMsg>>,
    apply_cancel: Arc<AtomicBool>,

    // ── 视频格式转换 ──
    tab: Tab,
    convert_ext: String,
    convert_mode: ConvertMode,
    convert_overwrite: bool,
    convert_out_dir: String,
    /// 是否使用指定视频码率（否则走 CRF 自动质量）
    convert_bitrate_enabled: bool,
    /// 指定视频码率（kbps）
    convert_bitrate_kbps: u32,
    /// 是否保留源文件全部元数据（EXIF / 拍摄时间 / 设备信息）
    convert_keep_metadata: bool,
    converting: bool,
    convert_rx: Option<Receiver<ConvertMsg>>,
    convert_cancel: Arc<AtomicBool>,

    // ── 备份失败确认 ──
    /// 备份写盘失败时为 true，界面弹确认框，由用户决定「取消 / 不备份继续」。
    /// 备份是「恢复原时间」的唯一退路，失败绝不能静默放过。
    backup_failed_pending: bool,
    /// 准备应用但因备份失败被挂起的计划（用户选择继续时直接复用）。
    pending_plan: Option<Vec<PlanItem>>,
}

impl PhotoTimeFixer {
    pub fn new() -> Self {
        let exe = exiftool::exiftool_path();
        let exiftool_installed = exe.is_some();
        let exiftool_version = exe.as_ref().and_then(|p| exiftool::version(p));

        let ff = ffmpeg::ffmpeg_path();
        let ff_installed = ff.is_some();
        let ff_version = ff.as_ref().and_then(|p| ffmpeg::version(p));

        Self {
            folder: String::new(),
            recursive: false,
            time_input: String::new(),
            mode: Mode::Absolute,
            interval_min: 2,
            random: true,
            offset_hours: 0,
            offset_mins: 0,
            tz_hours: 8,
            set_taken: true,
            set_created: true,
            set_modified: true,
            files: Vec::new(),
            plan: None,
            log: Vec::new(),
            scanning: false,
            scan_rx: None,

            exiftool_installed,
            exiftool_version,
            exiftool_dl: DlState::default(),
            exiftool_rx: None,
            ffmpeg_installed: ff_installed,
            ffmpeg_version: ff_version,
            ffmpeg_exe: ff,
            ffmpeg_dl: DlState::default(),
            ffmpeg_rx: None,

            progress_done: 0,
            progress_total: 0,
            progress_current: String::new(),
            applying: false,
            apply_rx: None,
            apply_cancel: Arc::new(AtomicBool::new(false)),

            tab: Tab::Time,
            convert_ext: "mp4".to_string(),
            convert_mode: ConvertMode::MaxCompat,
            convert_overwrite: false,
            convert_out_dir: String::new(),
            convert_bitrate_enabled: false,
            convert_bitrate_kbps: 1000,
            convert_keep_metadata: true,
            converting: false,
            convert_rx: None,
            convert_cancel: Arc::new(AtomicBool::new(false)),

            backup_failed_pending: false,
            pending_plan: None,
        }
    }

    /// 拖拽：拖文件夹进来 = 选中并扫描；拖文件 = 取它所在目录。
    fn handle_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        let Some(p) = dropped else { return };
        let dir = if p.is_dir() {
            Some(p)
        } else {
            p.parent().map(|q| q.to_path_buf())
        };
        if let Some(dir) = dir {
            self.folder = dir.to_string_lossy().to_string();
            self.rescan();
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // 左侧：ffmpeg 状态
            if self.ffmpeg_installed {
                let v = self.ffmpeg_version.clone().unwrap_or_default();
                ui.colored_label(
                    egui::Color32::from_rgb(31, 158, 117),
                    format!("ffmpeg 已安装 v{v}"),
                );
            } else if self.ffmpeg_dl.active {
                // 下载中：进度条 + 「12.3/28.9 MB」
                self.ffmpeg_dl.show(ui);
            } else {
                if ui.button("下载安装 ffmpeg").clicked() {
                    self.start_ffmpeg_download();
                }
                ui.label(&self.ffmpeg_dl.status);
            }

            // 右侧：exiftool 状态（右对齐布局里元素是倒序画的，
            // 所以先画文案再画进度条，视觉上才是「条在左、字在右」）
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.exiftool_installed {
                    let v = self.exiftool_version.clone().unwrap_or_default();
                    ui.colored_label(
                        egui::Color32::from_rgb(31, 158, 117),
                        format!("exiftool 已安装 v{v}"),
                    );
                } else if self.exiftool_dl.active {
                    ui.label(self.exiftool_dl.text());
                    ui.add(
                        egui::ProgressBar::new(self.exiftool_dl.fraction()).desired_width(120.0),
                    );
                } else {
                    if ui.button("下载安装 exiftool").clicked() {
                        self.start_download();
                    }
                    ui.label(&self.exiftool_dl.status);
                }
            });
        });
    }

    /// 备份失败确认框。必须最后画，才能压在所有面板之上。
    /// 闭包内不能同时借用 `self`，所以先收集决定、再改状态。
    fn backup_dialog(&mut self, ctx: &egui::Context) {
        if !self.backup_failed_pending {
            return;
        }
        let mut decision: Option<bool> = None; // Some(true) = 不备份继续
        egui::Window::new("备份失败，已暂停")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("备份文件写入失败，本次没有生成恢复点。");
                ui.label("常见原因：目标目录只读、网络共享无写权限、磁盘空间不足。");
                ui.add_space(4.0);
                ui.label("若继续应用，改完之后将无法用「恢复…」还原原时间。");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消（不改动任何文件）").clicked() {
                        decision = Some(false);
                    }
                    if ui
                        .button(egui::RichText::new("不备份，继续应用").strong())
                        .clicked()
                    {
                        decision = Some(true);
                    }
                });
            });
        match decision {
            Some(false) => {
                self.backup_failed_pending = false;
                self.pending_plan = None;
                self.log("已取消应用，文件未改动".into());
            }
            Some(true) => {
                self.backup_failed_pending = false;
                if let Some(plan) = self.pending_plan.take() {
                    let offset = self.tz_offset();
                    self.log("按用户确认：不备份，继续应用".into());
                    self.start_apply_thread(plan, offset);
                }
            }
            None => {}
        }
    }
}

impl eframe::App for PhotoTimeFixer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.34 的入口是 `ui`（`update` 已 deprecated），面板一律走
        // `show_inside(ui, …)`。先把 Context 取出来，供轮询/拖拽/弹窗使用。
        let ctx = ui.ctx().clone();
        self.poll_background(&ctx);
        self.handle_dropped(&ctx);

        egui::Panel::top("top").show_inside(ui, |ui| {
            self.top_bar(ui);
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            // 整体加大控件行距，让界面更宽松
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

            // 功能切换 Tab
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.tab == Tab::Time, "修改时间")
                    .clicked()
                {
                    self.tab = Tab::Time;
                }
                if ui
                    .selectable_label(self.tab == Tab::Convert, "视频格式转换")
                    .clicked()
                {
                    self.tab = Tab::Convert;
                }
            });
            ui.separator();

            match self.tab {
                Tab::Time => self.render_time(ui),
                Tab::Convert => self.render_convert(ui),
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label("Copyright © XINWEI. All rights reserved.");
            });
        });

        self.backup_dialog(&ctx);
    }

    /// 窗口清屏色：用设计系统主题背景色，避免面板之间缝隙露出纯黑。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        crate::ds::window_bg(true)
    }
}
