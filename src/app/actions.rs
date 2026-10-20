//! 用户动作：扫描目录、重算计划、应用修改、批量转换、下载工具。
//!
//! 所有耗时动作都在这里「校验前置条件 → 起后台线程 → 立刻返回」，
//! 结果一律由 `poll` 从 channel 里回收，界面线程不做阻塞操作。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::channel;

use chrono::{Duration, FixedOffset, Local};

use crate::apply::{apply_with_progress, compute_plan, PlanInput, PlanItem, PlanMode};
use crate::backup;
use crate::datetime;
use crate::download::{DlProgress, DlStage};
use crate::exiftool;
use crate::ffmpeg;
use crate::files::{self, MediaKind};

use super::state::{ensure_writable_dir, is_network_path, ConvertMsg, DownloadMsg, Mode};
use super::PhotoTimeFixer;

/// 工具安装目录：与 exe 同级（便携模式下就是程序自己所在目录）。
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 视频转换的并发度。
///
/// 实测（720p / 8 个 4 秒片段）：串行 9.90 s，并发 2 为 6.01 s，加速 1.65x；
/// 而并发 4 与并发 2 基本持平 —— 说明单个 ffmpeg 的重编码已接近吃满多核，
/// 再加进程只是互相抢核。所以固定 2 就是这条曲线的拐点。
///
/// 每个进程限流到「核数 / 并发数」，否则两个 ffmpeg 各自按核数开线程，
/// 上下文切换的代价会把并发的收益吃掉。
///
/// 少于 4 核的机器退回单线程：并行只会让每个任务更慢，总时间不变甚至变长。
fn convert_workers(total: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let w = if cores >= 4 { 2 } else { 1 };
    w.min(total.max(1))
}

impl PhotoTimeFixer {
    pub(super) fn tz_offset(&self) -> FixedOffset {
        let secs = if self.tz_hours == i32::MIN {
            Local::now().offset().local_minus_utc()
        } else {
            self.tz_hours * 3600
        };
        FixedOffset::east_opt(secs).unwrap_or_else(|| FixedOffset::east_opt(8 * 3600).unwrap())
    }

    pub(super) fn tz_label(&self) -> String {
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

    pub(super) fn log(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 300 {
            self.log.drain(0..self.log.len() - 300);
        }
    }

    /// 后台扫描目录。扫描完成后由 `poll` 回收并调用 `recompute`。
    pub(super) fn rescan(&mut self) {
        if self.folder.is_empty() {
            self.files.clear();
            self.plan = None;
            return;
        }
        let p = std::path::Path::new(&self.folder).to_path_buf();
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

    /// 重算预览计划。**纯计算、无副作用**：不写日志、不动线程。
    /// 原因见 `log_time_parse_hint` 的说明。
    pub(super) fn recompute(&mut self) {
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
    pub(super) fn log_time_parse_hint(&mut self) {
        let t = self.time_input.trim().to_string();
        if t.is_empty() {
            self.log("请先填写起始日期时间（示例：2026-10-19 16:30:00）".into());
        } else {
            self.log(format!(
                "起始时间「{t}」无法解析，请检查格式（示例：2026-10-19 16:30:00）"
            ));
        }
    }

    pub(super) fn do_apply(&mut self) {
        // 界面按钮已经互斥，这里再挡一道：防止将来加快捷键/托盘入口绕过按钮层。
        if self.converting {
            self.log("视频转换进行中，请等待完成或先取消，再修改时间。".into());
            return;
        }
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
        match backup::write_backup(&plan, offset) {
            Some(bp) => self.log(format!("已生成备份: {bp}")),
            None => {
                // 备份是唯一退路：写不出来就不许静默改文件。
                // 先把计划挂起，由界面弹窗让用户决定「取消」还是「不备份继续」。
                self.log("已中止：备份文件写入失败，未改动任何文件".into());
                self.log(format!(
                    "  备份目录：{}",
                    plan.first()
                        .and_then(|i| i.file.path.parent())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ".".into())
                ));
                self.backup_failed_pending = true;
                self.pending_plan = Some(plan);
                return;
            }
        }
        self.start_apply_thread(plan, offset);
    }

    /// 真正启动应用线程（备份已确认或用户已同意不备份）。
    pub(super) fn start_apply_thread(&mut self, plan: Vec<PlanItem>, offset: FixedOffset) {
        let set_created = self.set_created;
        let set_modified = self.set_modified;
        let set_taken = self.set_taken;
        let total = plan.len();
        let (tx, rx) = channel();
        self.apply_rx = Some(rx);
        self.applying = true;
        self.apply_cancel.store(false, Ordering::Relaxed);
        let cancel = self.apply_cancel.clone();
        self.progress_done = 0;
        self.progress_total = total;
        self.progress_current = String::new();
        self.log(format!("开始应用修改，共 {total} 个文件…"));
        std::thread::spawn(move || {
            apply_with_progress(
                plan,
                offset,
                set_created,
                set_modified,
                set_taken,
                cancel,
                tx,
            );
        });
    }

    /// 从备份 JSON 恢复原时间。恢复会改动文件时间，完成后必须重新扫描，
    /// 否则预览表里的「原时间」还是旧值，用户会误以为没生效。
    pub(super) fn do_restore(&mut self, backup_file: &Path) {
        if self.applying || self.converting {
            self.log("有任务正在运行，请等待完成或先取消，再执行恢复。".into());
            return;
        }
        let report = backup::restore(&backup_file.to_string_lossy());
        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
        self.log(format!("恢复完成：成功 {}/{}", ok, report.len()));
        for (n, r) in &report {
            if let Err(e) = r {
                self.log(format!("  [失败] {n}: {e}"));
            }
        }
        if self.folder.trim().is_empty() {
            self.log("提示：当前未选择文件夹，预览未自动刷新".into());
        } else {
            self.rescan();
        }
    }

    pub(super) fn start_download(&mut self) {
        if self.exiftool_dl.active {
            return;
        }
        self.exiftool_dl.begin();
        let (tx, rx) = channel();
        self.exiftool_rx = Some(rx);
        std::thread::spawn(move || {
            let dir = exe_dir();
            let pt = tx.clone();
            let mut sink = |p: DlProgress| {
                let _ = pt.send(DownloadMsg::Progress {
                    got: p.got,
                    total: p.total,
                    extracting: p.stage == DlStage::Extracting,
                    attempt: p.attempt,
                });
            };
            let res = exiftool::download_and_install(&dir, &mut sink);
            let _ = tx.send(DownloadMsg::Done(res));
        });
    }

    pub(super) fn start_ffmpeg_download(&mut self) {
        if self.ffmpeg_dl.active {
            return;
        }
        self.ffmpeg_dl.begin();
        let (tx, rx) = channel();
        self.ffmpeg_rx = Some(rx);
        std::thread::spawn(move || {
            let dir = exe_dir();
            let pt = tx.clone();
            let mut sink = |p: DlProgress| {
                let _ = pt.send(DownloadMsg::Progress {
                    got: p.got,
                    total: p.total,
                    extracting: p.stage == DlStage::Extracting,
                    attempt: p.attempt,
                });
            };
            let res = ffmpeg::download_and_install(&dir, &mut sink);
            let _ = tx.send(DownloadMsg::Done(res));
        });
    }

    /// 开始批量视频格式转换（后台线程 + 进度回调）。
    pub(super) fn start_convert(&mut self) {
        if self.applying {
            self.log("时间修改进行中，请等待完成或先取消，再开始转换。".into());
            return;
        }
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
        // ── 派发前串行算好所有输出路径 ──
        // 递归扫描时不同子目录可能有同名文件，统统落到 out/ 会互相覆盖，
        // 所以批内重名要加 _2/_3 序号。这一步必须在并发之前做完：它依赖一个
        // 共享的「已占用名字」集合，多线程同时算会抢到同一个输出名。
        let total = videos.len();
        let mut used: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::with_capacity(total);
        let mut jobs: Vec<ffmpeg::Encoded> = Vec::with_capacity(total);
        for input in &videos {
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
            jobs.push(ffmpeg::Encoded {
                input: input.clone(),
                output: out,
            });
        }

        let workers = convert_workers(total);
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);

        // 一次构造，所有 worker 共用（含码率、元数据保留策略、线程上限）
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
            // 只在并发时限流：单进程让编码器自己按核数决定（x264 默认就够聪明）
            threads: (workers > 1).then(|| (cores / workers).max(1) as u32),
        };

        let (tx, rx) = channel();
        self.convert_rx = Some(rx);
        self.converting = true;
        self.convert_cancel.store(false, Ordering::Relaxed);
        let cancel = self.convert_cancel.clone();
        self.progress_done = 0;
        self.progress_total = total;
        self.progress_current = String::new();
        if workers > 1 {
            self.log(format!(
                "开始转换，共 {total} 个视频（并发 {workers}，每个 {cores} / {workers} 线程）…"
            ));
        } else {
            self.log(format!("开始转换，共 {total} 个视频…"));
        }

        std::thread::spawn(move || {
            let names: Vec<String> = jobs
                .iter()
                .map(|j| {
                    j.input
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .collect();

            // 并发干活：`next` 是任务派发计数（谁空谁取下一个，天然是负载均衡），
            // `completed` 是真正的完成数（成功 + 失败，不含被取消的）。
            // 进度按完成数上报，不再用「已开工序号」——并发下后者会跳着涨。
            let next = std::sync::atomic::AtomicUsize::new(0);
            let completed = std::sync::atomic::AtomicUsize::new(0);
            let results: std::sync::Mutex<Vec<(usize, Result<ffmpeg::Encoded, String>)>> =
                std::sync::Mutex::new(Vec::with_capacity(total));

            let jobs_ref = &jobs;
            let names_ref = &names;
            let spec_ref = &spec;
            let ff_ref = &ff;
            let cancel_ref = &cancel;
            std::thread::scope(|s| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    let next = &next;
                    let completed = &completed;
                    let results = &results;
                    s.spawn(move || {
                        loop {
                            // 取消后立刻停手：正在跑的 ffmpeg 由 encode_one 内部杀掉
                            if cancel_ref.load(Ordering::Relaxed) {
                                break;
                            }
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            if idx >= jobs_ref.len() {
                                break;
                            }
                            let job = &jobs_ref[idx];
                            let res = ffmpeg::encode_one(
                                ff_ref,
                                &job.input,
                                &job.output,
                                spec_ref,
                                Some(cancel_ref),
                            );
                            // 被打断的那个文件不算失败，也不计入完成数
                            let entry = match res {
                                Ok(()) => Ok(job.clone()),
                                Err(e) if e == "已取消" => continue,
                                Err(e) => Err(e),
                            };
                            let n = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            let _ = tx.send(ConvertMsg::Progress {
                                done: n,
                                total,
                                current: names_ref[idx].clone(),
                            });
                            if let Ok(mut v) = results.lock() {
                                v.push((idx, entry));
                            }
                        }
                    });
                }
            });

            // 每个文件一个槽位：转码阶段填错误，收尾阶段填成功说明。
            // 用槽位而非直接 push，报告顺序才和文件顺序一致。
            let mut slots: Vec<Option<Result<String, String>>> = (0..total).map(|_| None).collect();
            let mut encoded: Vec<(usize, ffmpeg::Encoded)> = Vec::new();
            for (idx, r) in results.into_inner().unwrap_or_default() {
                match r {
                    Ok(e) => encoded.push((idx, e)),
                    Err(e) => slots[idx] = Some(Err(e)),
                }
            }
            // 收尾按文件顺序来，日志与报告才稳定可读
            encoded.sort_by_key(|(i, _)| *i);

            // ── 收尾：整批一次性复制元数据，再逐文件还原文件时间 ──
            // 放在循环外是本次优化的核心：exiftool 只启动一次。
            let items: Vec<ffmpeg::Encoded> = encoded.iter().map(|(_, e)| e.clone()).collect();
            let (meta_note, times) = ffmpeg::finalize_batch(&items, spec.keep_metadata);
            for (k, (idx, _)) in encoded.iter().enumerate() {
                let mut parts: Vec<String> = Vec::new();
                if !meta_note.is_empty() {
                    parts.push(meta_note.clone());
                }
                match &times[k] {
                    Ok(()) => parts.push("文件时间：已还原为原文件时间".into()),
                    Err(e) => parts.push(format!("文件时间还原失败({e})")),
                }
                slots[*idx] = Some(Ok(parts.join("；")));
            }

            let report: Vec<(String, Result<String, String>)> = slots
                .into_iter()
                .enumerate()
                .filter_map(|(i, s)| s.map(|r| (names[i].clone(), r)))
                .collect();

            let done = completed.load(Ordering::Relaxed);
            if done < total {
                let _ = tx.send(ConvertMsg::Cancelled {
                    done,
                    total,
                    report,
                });
                return;
            }
            let _ = tx.send(ConvertMsg::Progress {
                done: total,
                total,
                current: String::new(),
            });
            let _ = tx.send(ConvertMsg::Done(report));
        });
    }
}
