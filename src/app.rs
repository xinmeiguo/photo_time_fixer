//! egui 界面与主状态。

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;

use chrono::{Duration, FixedOffset, Local};

use eframe::egui;

use crate::apply::{apply_with_progress, compute_plan, PlanInput, PlanItem, PlanMode, ProgressMsg};
use crate::backup;
use crate::datetime;
use crate::exiftool;
use crate::ffmpeg;
use crate::ffmpeg::ConvertMode;
use crate::files::{self, MediaFile, MediaKind};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Absolute,
    Offset,
}

/// 是否网络共享路径（UNC，如 `\\192.168.1.216\share\...`）。
/// 排除 `\\?\` 开头的 verbatim 路径，那不是共享。
fn is_network_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.starts_with(r"\\") && !s.starts_with(r"\\?\")
}

/// 确保目录存在且**真的可写**：建目录 + 写一个探针文件再删掉。
/// 只判断 `exists()` 不够——网络共享/只读目录能创建句柄却写不进去。
fn ensure_writable_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let probe = dir.join(".ptf_write_test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("写入测试失败: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

enum DownloadMsg {
    Done(Result<PathBuf, String>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Time,
    Convert,
}

enum FFDownloadMsg {
    Done(Result<PathBuf, String>),
}

enum ConvertMsg {
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    Done(Vec<(String, Result<String, String>)>),
}

pub struct PhotoTimeFixer {
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
    exiftool_installed: bool,
    exiftool_version: Option<String>,
    download_status: String,
    download_rx: Option<std::sync::mpsc::Receiver<DownloadMsg>>,
    scanning: bool,
    scan_rx: Option<std::sync::mpsc::Receiver<Vec<MediaFile>>>,
    applying: bool,
    progress_done: usize,
    progress_total: usize,
    progress_current: String,
    // ffmpeg 状态
    tab: Tab,
    ffmpeg_installed: bool,
    ffmpeg_version: Option<String>,
    ffmpeg_exe: Option<PathBuf>,
    ffmpeg_download_status: String,
    ffmpeg_download_rx: Option<std::sync::mpsc::Receiver<FFDownloadMsg>>,

    // 视频格式转换
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
    convert_rx: Option<std::sync::mpsc::Receiver<ConvertMsg>>,

    apply_rx: Option<std::sync::mpsc::Receiver<ProgressMsg>>,
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
            exiftool_installed,
            exiftool_version,
            download_status: String::new(),
            download_rx: None,
            scanning: false,
            scan_rx: None,
            applying: false,
            progress_done: 0,
            progress_total: 0,
            progress_current: String::new(),
            apply_rx: None,

            tab: Tab::Time,
            ffmpeg_installed: ff_installed,
            ffmpeg_version: ff_version,
            ffmpeg_exe: ff,
            ffmpeg_download_status: String::new(),
            ffmpeg_download_rx: None,

            convert_ext: "mp4".to_string(),
            convert_mode: ConvertMode::MaxCompat,
            convert_overwrite: false,
            convert_out_dir: String::new(),
            convert_bitrate_enabled: false,
            convert_bitrate_kbps: 1000,
            convert_keep_metadata: true,
            converting: false,
            convert_rx: None,
        }
    }

    fn tz_offset(&self) -> FixedOffset {
        let secs = if self.tz_hours == i32::MIN {
            Local::now().offset().local_minus_utc()
        } else {
            self.tz_hours * 3600
        };
        FixedOffset::east_opt(secs).unwrap_or_else(|| FixedOffset::east_opt(8 * 3600).unwrap())
    }

    fn tz_label(&self) -> String {
        match self.tz_hours {
            8 => "UTC+8 北京".into(),
            9 => "UTC+9 东京".into(),
            0 => "UTC+0".into(),
            -5 => "UTC-5 纽约".into(),
            -8 => "UTC-8 洛杉矶".into(),
            i32::MIN => "跟随系统".into(),
            h => format!("UTC{:+}", h),
        }
    }

    fn log(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 300 {
            self.log.drain(0..self.log.len() - 300);
        }
    }

    fn rescan(&mut self) {
        if self.folder.is_empty() {
            self.files.clear();
            self.plan = None;
            return;
        }
        let p = Path::new(&self.folder).to_path_buf();
        if !p.is_dir() {
            self.log("文件夹路径不存在".into());
            return;
        }
        let recursive = self.recursive;
        self.scanning = true;
        self.plan = None;
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        std::thread::spawn(move || {
            let files = files::scan(&p, recursive);
            let _ = tx.send(files);
        });
        self.log("正在扫描…".into());
    }

    fn recompute(&mut self) {
        if self.files.is_empty() {
            self.plan = None;
            return;
        }
        let input = match self.mode {
            Mode::Absolute => {
                let base = match datetime::parse_flexible(&self.time_input) {
                    Some(b) => b,
                    None => {
                        // 这里静默失败：recompute 在「扫描完成」「切换标签」时都会被动调用，
                        // 而「视频格式转换」页根本不需要起始时间。若在此打日志，
                        // 转换时只要扫描过文件就会刷出“无法解析起始时间”的噪音。
                        // 真正的提示放在用户主动触发的地方（预览/应用按钮）。
                        self.plan = None;
                        return;
                    }
                };
                PlanInput {
                    mode: PlanMode::Absolute(base),
                    interval_min: self.interval_min,
                    random: self.random,
                    offset: Duration::zero(),
                }
            }
            Mode::Offset => {
                let off = Duration::hours(self.offset_hours as i64)
                    + Duration::minutes(self.offset_mins as i64);
                PlanInput {
                    mode: PlanMode::Offset,
                    interval_min: 0,
                    random: false,
                    offset: off,
                }
            }
        };
        self.plan = Some(compute_plan(&self.files, &input));
    }

    /// 用户主动触发（预览/应用）但起始时间不可用时，给出明确提示。
    fn log_time_parse_hint(&mut self) {
        let t = self.time_input.trim().to_string();
        if t.is_empty() {
            self.log("请先填写起始日期时间（示例：2026-10-19 16:30:00）".into());
        } else {
            self.log(format!(
                "起始时间「{t}」无法解析，请检查格式（示例：2026-10-19 16:30:00）"
            ));
        }
    }

    fn do_apply(&mut self) {
        if !self.exiftool_installed && self.set_taken {
            self.log("未安装 exiftool，无法修改拍摄时间(元数据)。请先安装，或取消勾选“拍摄时间(元数据)”。".into());
            return;
        }
        if self.files.is_empty() {
            self.log("请先选择文件夹并扫描".into());
            return;
        }
        if self.plan.is_none() {
            self.recompute();
        }
        let plan = match self.plan.take() {
            Some(p) => p,
            None => {
                if self.mode == Mode::Absolute {
                    self.log_time_parse_hint();
                } else {
                    self.log("没有可应用的时间计划".into());
                }
                return;
            }
        };
        let offset = self.tz_offset();
        // 备份（同步、快速，纯 JSON 写入）。时区必须存**实际选择的**那一个，
        // 否则「恢复」会按写死的 UTC+8 回写元数据，跟原设置对不上。
        if let Some(bp) = backup::write_backup(&plan, offset) {
            self.log(format!("已生成备份: {bp}"));
        } else {
            self.log("警告：备份生成失败，仍继续应用".into());
        }
        let set_created = self.set_created;
        let set_modified = self.set_modified;
        let set_taken = self.set_taken;
        let total = plan.len();
        let (tx, rx) = channel();
        self.apply_rx = Some(rx);
        self.applying = true;
        self.progress_done = 0;
        self.progress_total = total;
        self.progress_current = String::new();
        self.log(format!("开始应用修改，共 {total} 个文件…"));
        std::thread::spawn(move || {
            apply_with_progress(plan, offset, set_created, set_modified, set_taken, tx);
        });
    }

    fn start_download(&mut self) {
        self.download_status = "下载中…".into();
        let (tx, rx) = channel();
        self.download_rx = Some(rx);
        std::thread::spawn(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."));
            let res = exiftool::download_and_install(&dir);
            let _ = tx.send(DownloadMsg::Done(res));
        });
    }

    fn orig_display(f: &MediaFile) -> String {
        if let Some(t) = &f.original_taken {
            t.clone()
        } else if let Some(d) = f.original_modified {
            d.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            "—".into()
        }
    }

    fn start_ffmpeg_download(&mut self) {
        self.ffmpeg_download_status = "下载中…".into();
        let (tx, rx) = channel();
        self.ffmpeg_download_rx = Some(rx);
        std::thread::spawn(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."));
            let res = ffmpeg::download_and_install(&dir);
            let _ = tx.send(FFDownloadMsg::Done(res));
        });
    }

    /// 开始批量视频格式转换（后台线程 + 进度回调）。
    fn start_convert(&mut self) {
        if !self.ffmpeg_installed {
            self.log("未安装 ffmpeg，无法转换视频格式。请先点击顶部“下载安装 ffmpeg”。".into());
            return;
        }
        // 先确定输出目录，再收集视频：输出目录里的文件不能再当作输入，
        // 否则重复转换会把上次的产物再转一遍（_converted_converted…）。
        let out_dir = {
            let manual = self.convert_out_dir.trim();
            if manual.is_empty() {
                // 默认输出到「所选文件夹/out」，不污染原目录
                let root = self.folder.trim();
                if root.is_empty() {
                    self.log("请先选择文件夹".into());
                    return;
                }
                PathBuf::from(root).join("out")
            } else {
                PathBuf::from(manual)
            }
        };
        // 处理前先确认输出目录真的可写。网络共享（\\server\share）或只读目录
        // 在这里就要拦住，否则几百个文件会逐个失败，白白等很久。
        if let Err(e) = ensure_writable_dir(&out_dir) {
            self.log(format!("输出目录不可用：{}", out_dir.display()));
            self.log(format!("  原因：{e}"));
            if is_network_path(&out_dir) {
                self.log(
                    "  这是网络共享路径，通常是写权限不足导致。请点『选择目录…』改到本地目录，或把文件复制到本地后再转换。"
                        .into(),
                );
            } else {
                self.log("  请点『选择目录…』指定一个有写权限的目录。".into());
            }
            return;
        }
        // 收集当前扫描结果中的视频文件（跳过输出目录内的文件）
        let videos: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|f| f.kind == MediaKind::Video)
            .filter(|f| !f.path.starts_with(&out_dir))
            .map(|f| f.path.clone())
            .collect();
        if videos.is_empty() {
            self.log(
                "未找到视频文件。请先在“修改时间”页选择文件夹并扫描，或确认文件夹包含视频。".into(),
            );
            return;
        }
        let ff = match self.ffmpeg_exe.clone() {
            Some(p) => p,
            None => {
                self.log("ffmpeg 路径缺失".into());
                return;
            }
        };
        let ext = self.convert_ext.trim().to_string();
        if ext.is_empty() {
            self.log("目标格式不能为空".into());
            return;
        }
        // 一次构造，线程内每个文件共用（含码率与元数据保留策略）
        let spec = ffmpeg::ConvertSpec {
            target_ext: ext.clone(),
            mode: self.convert_mode,
            video_bitrate_kbps: if self.convert_bitrate_enabled {
                Some(self.convert_bitrate_kbps)
            } else {
                None
            },
            keep_metadata: self.convert_keep_metadata,
            overwrite: self.convert_overwrite,
        };

        let total = videos.len();
        let (tx, rx) = channel();
        self.convert_rx = Some(rx);
        self.converting = true;
        self.progress_done = 0;
        self.progress_total = total;
        self.progress_current = String::new();
        self.log(format!("开始转换，共 {total} 个视频…"));

        std::thread::spawn(move || {
            let mut report: Vec<(String, Result<String, String>)> = Vec::with_capacity(total);
            // 递归扫描时不同子目录可能存在同名文件，统统落到 out/ 会互相覆盖，
            // 这里对批内重名加 _2/_3 序号。
            let mut used: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for (idx, input) in videos.iter().enumerate() {
                let name = input
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let _ = tx.send(ConvertMsg::Progress {
                    done: idx,
                    total,
                    current: name.clone(),
                });
                let mut out = ffmpeg::output_path(input, &ext, Some(out_dir.as_path()));
                if !used.insert(out.clone()) {
                    let stem = input
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "output".into());
                    let mut n = 2u32;
                    loop {
                        let cand = out_dir.join(format!("{stem}_{n}.{ext}"));
                        if used.insert(cand.clone()) {
                            out = cand;
                            break;
                        }
                        n += 1;
                    }
                }
                let r = ffmpeg::convert_one(&ff, input, &out, &spec);
                report.push((name, r));
            }
            let _ = tx.send(ConvertMsg::Progress {
                done: total,
                total,
                current: String::new(),
            });
            let _ = tx.send(ConvertMsg::Done(report));
        });
    }

    /// 渲染“视频格式转换”Tab 的界面。
    fn render_convert(&mut self, ui: &mut egui::Ui) {
        // 文件夹选择（与时间页共享 folder/recursive，可独立触发扫描）
        ui.horizontal(|ui| {
            ui.label("文件夹:");
            ui.text_edit_singleline(&mut self.folder);
            if ui.button("选择文件夹…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.folder = dir.to_string_lossy().to_string();
                    self.rescan();
                }
            }
            ui.checkbox(&mut self.recursive, "包含子文件夹");
            if ui
                .add_enabled(!self.scanning, egui::Button::new("刷新扫描"))
                .clicked()
            {
                self.rescan();
            }
            if self.scanning {
                ui.label("扫描中…");
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("目标格式:");
            egui::ComboBox::from_id_salt("convert_ext_combo")
                .selected_text(self.convert_ext.clone())
                .width(160.0)
                .show_ui(ui, |ui| {
                    for ext in ["mp4", "mkv", "mov", "avi", "webm", "flv", "m4v"] {
                        ui.selectable_value(&mut self.convert_ext, ext.to_string(), ext);
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("转换方式:");
            egui::ComboBox::from_id_salt("convert_mode_combo")
                .selected_text(self.convert_mode.label().to_string())
                .width(240.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.convert_mode,
                        ConvertMode::MaxCompat,
                        ConvertMode::MaxCompat.label(),
                    );
                    ui.selectable_value(
                        &mut self.convert_mode,
                        ConvertMode::Reencode,
                        ConvertMode::Reencode.label(),
                    );
                    ui.selectable_value(
                        &mut self.convert_mode,
                        ConvertMode::Copy,
                        ConvertMode::Copy.label(),
                    );
                });
        });
        // 视频码率：指定码率可大幅压小文件（内窥镜录像常见码率虚高）
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.convert_bitrate_enabled, "指定视频码率");
            ui.add_enabled_ui(self.convert_bitrate_enabled, |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.convert_bitrate_kbps)
                        .speed(50.0)
                        .range(100u32..=50000)
                        .suffix(" kbps"),
                );
            });
        });
        // 元数据：默认开启，转换后不丢 EXIF / 拍摄时间 / 设备信息
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.convert_keep_metadata,
                "保留元数据（EXIF / 拍摄时间 / 设备信息，不丢失）",
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.convert_overwrite, "覆盖已存在的输出文件");
        });
        ui.horizontal(|ui| {
            ui.label("输出目录(留空=视频文件夹\\out):");
            ui.text_edit_singleline(&mut self.convert_out_dir);
            if ui.button("选择目录…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.convert_out_dir = dir.to_string_lossy().to_string();
                }
            }
        });

        let video_count = self
            .files
            .iter()
            .filter(|f| f.kind == MediaKind::Video)
            .count();
        ui.label(format!("当前文件夹视频数量: {video_count}"));

        ui.horizontal(|ui| {
            let btn_size = egui::vec2(140.0, 36.0);
            let convert_btn =
                egui::Button::new(egui::RichText::new("开始转换").size(15.0).strong())
                    .min_size(btn_size);
            if ui.add_enabled(!self.converting, convert_btn).clicked() {
                self.start_convert();
            }
            if !self.ffmpeg_installed {
                ui.colored_label(egui::Color32::from_rgb(200, 80, 60), "未安装 ffmpeg");
            }
        });

        // 共享进度条显示
        if self.converting || self.progress_total > 0 {
            let total = self.progress_total.max(1);
            let frac = self.progress_done as f32 / total as f32;
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                ui.label(format!("{}/{}", self.progress_done, self.progress_total));
            });
            if !self.progress_current.is_empty() {
                ui.label(format!("正在转换: {}", self.progress_current));
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("日志:");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    self.log.clear();
                }
            });
        });
        egui::ScrollArea::vertical()
            .max_height(40.0)
            .show(ui, |ui| {
                for l in &self.log {
                    ui.label(l);
                }
            });
    }
}

impl eframe::App for PhotoTimeFixer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.34 的入口是 `ui`（`update` 已 deprecated），面板一律走
        // `show_inside(ui, …)`。这里先把 Context 取出来，供轮询/拖拽/重绘
        // 这几处非面板代码继续使用。
        let ctx = ui.ctx().clone();
        // 轮询下载结果
        if let Some(rx) = &mut self.download_rx {
            if let Ok(DownloadMsg::Done(res)) = rx.try_recv() {
                match res {
                    Ok(p) => {
                        self.exiftool_installed = true;
                        self.exiftool_version = exiftool::version(&p);
                        self.download_status = "安装成功".into();
                    }
                    Err(e) => {
                        self.download_status = format!("下载失败: {e}");
                    }
                }
                self.download_rx = None;
            }
        }

        // 轮询 ffmpeg 下载结果
        if let Some(rx) = &mut self.ffmpeg_download_rx {
            if let Ok(FFDownloadMsg::Done(res)) = rx.try_recv() {
                match res {
                    Ok(p) => {
                        self.ffmpeg_exe = Some(p.clone());
                        self.ffmpeg_installed = true;
                        self.ffmpeg_version = ffmpeg::version(&p);
                        self.ffmpeg_download_status = "安装成功".into();
                    }
                    Err(e) => {
                        self.ffmpeg_download_status = format!("下载失败: {e}");
                    }
                }
                self.ffmpeg_download_rx = None;
            }
        }

        // 轮询扫描结果（后台线程）
        if let Some(rx) = &mut self.scan_rx {
            if let Ok(files) = rx.try_recv() {
                self.scanning = false;
                self.scan_rx = None;
                self.files = files;
                self.recompute();
                self.log(format!("扫描到 {} 个媒体文件", self.files.len()));
            }
        }

        // 轮询应用进度（后台线程）
        let mut apply_finished = false;
        if let Some(rx) = &mut self.apply_rx {
            loop {
                match rx.try_recv() {
                    Ok(ProgressMsg::Progress {
                        done,
                        total,
                        current,
                    }) => {
                        self.progress_done = done;
                        self.progress_total = total;
                        self.progress_current = current;
                    }
                    Ok(ProgressMsg::Done(report)) => {
                        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
                        let fail = report.len() - ok;
                        self.log(format!("应用完成：成功 {ok} / 失败 {fail}"));
                        for (name, r) in &report {
                            if let Err(e) = r {
                                self.log(format!("  [失败] {name}: {e}"));
                            }
                        }
                        self.applying = false;
                        self.apply_rx = None;
                        self.progress_done = 0;
                        self.progress_total = 0;
                        self.progress_current = String::new();
                        apply_finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.applying = false;
                        self.apply_rx = None;
                        self.progress_done = 0;
                        self.progress_total = 0;
                        self.progress_current = String::new();
                        break;
                    }
                }
            }
        }
        // 轮询转换进度
        if let Some(rx) = &mut self.convert_rx {
            loop {
                match rx.try_recv() {
                    Ok(ConvertMsg::Progress {
                        done,
                        total,
                        current,
                    }) => {
                        self.progress_done = done;
                        self.progress_total = total;
                        self.progress_current = current;
                    }
                    Ok(ConvertMsg::Done(report)) => {
                        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
                        let fail = report.len() - ok;
                        self.log(format!("转换完成：成功 {ok} / 失败 {fail}"));
                        for (name, r) in &report {
                            if let Err(e) = r {
                                self.log(format!("  [失败] {name}: {e}"));
                            }
                        }
                        // 元数据状态只报一次，避免几百个文件刷屏
                        if let Some(note) = report.iter().find_map(|(_, r)| match r {
                            Ok(m) if !m.is_empty() => Some(m.clone()),
                            _ => None,
                        }) {
                            self.log(format!("  {note}"));
                        }
                        self.converting = false;
                        self.convert_rx = None;
                        self.progress_done = 0;
                        self.progress_total = 0;
                        self.progress_current = String::new();
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.converting = false;
                        self.convert_rx = None;
                        self.progress_done = 0;
                        self.progress_total = 0;
                        self.progress_current = String::new();
                        break;
                    }
                }
            }
        }

        if apply_finished {
            // 重新扫描以刷新原时间显示
            self.rescan();
        }
        if self.applying {
            // 处理中持续重绘，保证进度条实时刷新
            ctx.request_repaint();
        }

        // 拖拽
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(f) = dropped.first() {
            if let Some(p) = &f.path {
                if p.is_dir() {
                    self.folder = p.to_string_lossy().to_string();
                    self.rescan();
                } else if let Some(parent) = p.parent() {
                    self.folder = parent.to_string_lossy().to_string();
                    self.rescan();
                }
            }
        }

        egui::Panel::top("top").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                // 左侧：ffmpeg 状态
                if self.ffmpeg_installed {
                    let v = self.ffmpeg_version.clone().unwrap_or_default();
                    ui.colored_label(
                        egui::Color32::from_rgb(31, 158, 117),
                        format!("ffmpeg 已安装 v{v}"),
                    );
                } else {
                    if ui.button("下载安装 ffmpeg").clicked() {
                        self.start_ffmpeg_download();
                    }
                    ui.label(&self.ffmpeg_download_status);
                }

                // 右侧：exiftool 状态
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.exiftool_installed {
                        let v = self.exiftool_version.clone().unwrap_or_default();
                        ui.colored_label(
                            egui::Color32::from_rgb(31, 158, 117),
                            format!("exiftool 已安装 v{v}"),
                        );
                    } else {
                        if ui.button("下载安装 exiftool").clicked() {
                            self.start_download();
                        }
                        ui.label(&self.download_status);
                    }
                });
            });
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            // 整体加大控件行距，让界面更宽松
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

            // 功能切换 Tab
            ui.horizontal(|ui| {
                if ui.selectable_label(self.tab == Tab::Time, "修改时间").clicked() {
                    self.tab = Tab::Time;
                }
                if ui.selectable_label(self.tab == Tab::Convert, "视频格式转换").clicked() {
                    self.tab = Tab::Convert;
                }
            });
            ui.separator();

            if self.tab == Tab::Time {
            // 文件夹
            ui.horizontal(|ui| {
                ui.label("文件夹:");
                ui.text_edit_singleline(&mut self.folder);
                if ui.button("选择文件夹…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.folder = dir.to_string_lossy().to_string();
                        self.rescan();
                    }
                }
                ui.checkbox(&mut self.recursive, "包含子文件夹");
                if ui
                    .add_enabled(!self.scanning, egui::Button::new("刷新扫描"))
                    .clicked()
                {
                    self.rescan();
                }
                if self.scanning {
                    ui.label("扫描中…");
                }
            });
            ui.separator();

            // 模式
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode, Mode::Absolute, "绝对起始时间");
                ui.radio_value(&mut self.mode, Mode::Offset, "整体偏移 (校正时差)");
            });
            if self.mode == Mode::Absolute {
                ui.horizontal(|ui| {
                    ui.label("起始日期时间(任意格式):");
                    ui.text_edit_singleline(&mut self.time_input);
                });
                let parsed = datetime::parse_flexible(&self.time_input);
                ui.label(format!(
                    "识别结果: {}",
                    parsed
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "无法识别".into())
                ));
                ui.horizontal(|ui| {
                    ui.label("每张间隔(分钟):");
                    if ui.button("−").clicked() {
                        self.interval_min = self.interval_min.saturating_sub(1);
                    }
                    ui.add(
                        egui::DragValue::new(&mut self.interval_min)
                            .speed(1.0)
                            .range(0u32..=59)
                            .fixed_decimals(0),
                    );
                    if ui.button("+").clicked() {
                        self.interval_min = (self.interval_min + 1).min(59);
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.random, "随机秒 (0~59)");
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("整体偏移:");
                    ui.add(egui::Slider::new(&mut self.offset_hours, -24..=24i32).step_by(1.0));
                    ui.label("时");
                    ui.add(egui::Slider::new(&mut self.offset_mins, -59..=59i32).step_by(1.0));
                    ui.label("分 (负值为提前)");
                });
                ui.label("说明：对每张文件“原始拍摄时间/修改时间”整体加减该偏移，常用于整批时差校正。");
            }
            ui.separator();

            // 时区 + 范围
            ui.horizontal(|ui| {
                egui::ComboBox::from_label("时区")
                    .selected_text(self.tz_label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.tz_hours, 8, "UTC+8 北京");
                        ui.selectable_value(&mut self.tz_hours, 9, "UTC+9 东京");
                        ui.selectable_value(&mut self.tz_hours, 0, "UTC+0");
                        ui.selectable_value(&mut self.tz_hours, -5, "UTC-5 纽约");
                        ui.selectable_value(&mut self.tz_hours, -8, "UTC-8 洛杉矶");
                        ui.selectable_value(&mut self.tz_hours, i32::MIN, "跟随系统");
                    });
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.set_taken, "拍摄时间(元数据)");
                ui.checkbox(&mut self.set_created, "文件创建时间");
                ui.checkbox(&mut self.set_modified, "修改时间");
            });
            ui.separator();

            ui.horizontal(|ui| {
                let btn_size = egui::vec2(140.0, 36.0);
                let preview_btn = egui::Button::new(
                    egui::RichText::new("预览新时间").size(15.0).strong(),
                )
                .min_size(btn_size);
                if ui.add(preview_btn).clicked() {
                    if self.files.is_empty() {
                        self.log("请先选择文件夹并扫描".into());
                    } else {
                        self.recompute();
                        if self.plan.is_none() && self.mode == Mode::Absolute {
                            self.log_time_parse_hint();
                        }
                    }
                }

                let apply_btn = egui::Button::new(
                    egui::RichText::new("应用修改").size(15.0).strong(),
                )
                .min_size(btn_size);
                if ui.add_enabled(!self.applying, apply_btn).clicked() {
                    self.do_apply();
                }

                let restore_btn = egui::Button::new(
                    egui::RichText::new("恢复…").size(15.0).strong(),
                )
                .min_size(btn_size);
                if ui.add(restore_btn).clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("备份", &["json"])
                        .pick_file()
                    {
                        let report = backup::restore(&file.to_string_lossy());
                        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
                        self.log(format!("恢复完成：成功 {}/{}", ok, report.len()));
                        for (n, r) in &report {
                            if let Err(e) = r {
                                self.log(format!("  [失败] {n}: {e}"));
                            }
                        }
                    }
                }
            });

            // 预览表
            if let Some(plan) = &self.plan {
                ui.label(format!("共 {} 个文件（已按文件名排序）", plan.len()));
                egui::ScrollArea::vertical()
                    .max_height(120.0)
                    .show(ui, |ui| {
                        egui::Grid::new("preview")
                            .striped(true)
                            .min_col_width(120.0)
                            .spacing([8.0, 8.0])
                            .show(ui, |ui| {
                                ui.strong("文件名");
                                ui.strong("原时间");
                                ui.strong("新时间(预览)");
                                ui.strong("类型");
                                ui.end_row();
                                for item in plan {
                                    let orig = Self::orig_display(&item.file);
                                    let new = item
                                        .new_time
                                        .format("%Y-%m-%d %H:%M:%S")
                                        .to_string();
                                    ui.label(&item.file.name);
                                    ui.label(orig);
                                    ui.label(new);
                                    ui.label(item.file.kind.label());
                                    ui.end_row();
                                }
                            });
                    });
            } else {
                ui.label("（点击“预览新时间”查看计划，或先选择文件夹）");
            }

            ui.separator();

            // 处理进度条
            if self.applying || self.progress_total > 0 {
                let total = self.progress_total.max(1);
                let frac = self.progress_done as f32 / total as f32;
                ui.horizontal(|ui| {
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    ui.label(format!("{}/{}", self.progress_done, self.progress_total));
                });
                if !self.progress_current.is_empty() {
                    ui.label(format!("正在处理: {}", self.progress_current));
                }
            }

            ui.horizontal(|ui| {
                ui.label("日志:");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("清空").clicked() {
                        self.log.clear();
                    }
                });
            });
            egui::ScrollArea::vertical().max_height(40.0).show(ui, |ui| {
                for l in &self.log {
                    ui.label(l);
                }
            });

            } // end Tab::Time

            ui.separator();

            if self.tab == Tab::Convert {
                self.render_convert(ui);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label("Copyright © XINWEI. All rights reserved.");
            });
        });
    }

    /// 窗口清屏色：用设计系统主题背景色，避免面板之间缝隙露出纯黑。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        crate::ds::window_bg(true)
    }
}
