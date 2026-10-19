//! 后台线程消息回收。
//!
//! 全工程只有这里调用 `try_recv`：界面线程每帧调一次，把线程结果搬进状态。
//! 拆出来之后，`mod.rs` 只剩下面板骨架，不再被五个轮询分支淹没。
//!
//! 统一的取消息姿势：先把 `Receiver` 从 `Option` 里 **take** 出来（拿到所有权），
//! 处理完还没结束时再放回去。这样循环体内可以随便调 `&mut self` 的方法，
//! 不会和「借着一个 self 字段」打架。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui;

use crate::apply::ProgressMsg;
use crate::exiftool;
use crate::ffmpeg;

use super::state::{ConvertMsg, DlState, DownloadMsg};
use super::PhotoTimeFixer;

impl PhotoTimeFixer {
    pub(super) fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_downloads();
        self.poll_scan();
        let applied = self.poll_apply();
        self.poll_convert();
        if applied {
            // 重新扫描以刷新「原时间」列（应用/恢复都会改动文件时间）
            self.rescan();
        }
        if self.applying || self.converting {
            // 处理中持续重绘，保证进度条实时刷新
            ctx.request_repaint();
        } else if self.exiftool_dl.active || self.ffmpeg_dl.active {
            // 下载只在字节流变化时更新，10 fps 足够，不必空转满帧
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn poll_downloads(&mut self) {
        if let Some(res) = drain_download(&mut self.exiftool_rx, &mut self.exiftool_dl) {
            match res {
                Ok(p) => {
                    self.exiftool_installed = true;
                    self.exiftool_version = exiftool::version(&p);
                    self.exiftool_dl.finish("安装成功".into());
                }
                Err(e) => self.exiftool_dl.finish(format!("下载失败: {e}")),
            }
        }
        if let Some(res) = drain_download(&mut self.ffmpeg_rx, &mut self.ffmpeg_dl) {
            match res {
                Ok(p) => {
                    self.ffmpeg_version = ffmpeg::version(&p);
                    self.ffmpeg_exe = Some(p);
                    self.ffmpeg_installed = true;
                    self.ffmpeg_dl.finish("安装成功".into());
                }
                Err(e) => self.ffmpeg_dl.finish(format!("下载失败: {e}")),
            }
        }
    }

    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(files) => {
                self.scanning = false;
                self.files = files;
                self.recompute();
                self.log(format!("扫描到 {} 个媒体文件", self.files.len()));
            }
            // 还没扫完：放回去，下一帧再看
            Err(_) => self.scan_rx = Some(rx),
        }
    }

    /// 返回 true 表示本帧刚处理完一批（需要重新扫描刷新显示）。
    fn poll_apply(&mut self) -> bool {
        let Some(rx) = self.apply_rx.take() else {
            return false;
        };
        let mut finished = false;
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
                    self.report_apply("应用完成", "成功", "失败", &report);
                    self.end_apply();
                    // 处理过的文件时间已变，必须刷新预览表里的「原时间」
                    finished = true;
                    break;
                }
                Ok(ProgressMsg::Cancelled {
                    done,
                    total,
                    report,
                }) => {
                    self.report_cancelled("应用", done, total, &report);
                    self.end_apply();
                    // 已取消但前面的文件确实被改过，同样要刷新
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => {
                    self.apply_rx = Some(rx);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.end_apply();
                    break;
                }
            }
        }
        finished
    }

    fn report_apply(
        &mut self,
        title: &str,
        ok_word: &str,
        fail_word: &str,
        report: &[(String, Result<(), String>)],
    ) {
        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
        let fail = report.len() - ok;
        self.log(format!("{title}：{ok_word} {ok} / {fail_word} {fail}"));
        for (name, r) in report {
            if let Err(e) = r {
                self.log(format!("  [失败] {name}: {e}"));
            }
        }
    }

    /// 取消时统一格式：`已取消：完成 3/10（成功 3 / 失败 0）`。
    fn report_cancelled(
        &mut self,
        label: &str,
        done: usize,
        total: usize,
        report: &[(String, Result<(), String>)],
    ) {
        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
        let fail = report.len() - ok;
        self.log(format!(
            "已取消{label}：完成 {done}/{total}（成功 {ok} / 失败 {fail}）"
        ));
        for (name, r) in report {
            if let Err(e) = r {
                self.log(format!("  [失败] {name}: {e}"));
            }
        }
    }

    fn end_apply(&mut self) {
        self.applying = false;
        self.apply_rx = None;
        self.progress_done = 0;
        self.progress_total = 0;
        self.progress_current = String::new();
    }

    fn poll_convert(&mut self) {
        let Some(rx) = self.convert_rx.take() else {
            return;
        };
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
                    self.report_convert("转换完成", &report);
                    self.end_convert();
                    break;
                }
                Ok(ConvertMsg::Cancelled {
                    done,
                    total,
                    report,
                }) => {
                    self.report_convert_cancelled(done, total, &report);
                    self.end_convert();
                    break;
                }
                Err(TryRecvError::Empty) => {
                    self.convert_rx = Some(rx);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.end_convert();
                    break;
                }
            }
        }
    }

    fn report_convert(&mut self, title: &str, report: &[(String, Result<String, String>)]) {
        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
        let fail = report.len() - ok;
        self.log(format!("{title}：成功 {ok} / 失败 {fail}"));
        for (name, r) in report {
            if let Err(e) = r {
                self.log(format!("  [失败] {name}: {e}"));
            }
        }
        // 元数据/文件时间状态每批只报一次，避免几百个文件刷屏
        if let Some(note) = report.iter().find_map(|(_, r)| match r {
            Ok(m) if !m.is_empty() => Some(m.clone()),
            _ => None,
        }) {
            self.log(format!("  {note}"));
        }
    }

    fn report_convert_cancelled(
        &mut self,
        done: usize,
        total: usize,
        report: &[(String, Result<String, String>)],
    ) {
        let ok = report.iter().filter(|(_, r)| r.is_ok()).count();
        let fail = report.len() - ok;
        self.log(format!(
            "已取消转换：完成 {done}/{total}（成功 {ok} / 失败 {fail}）"
        ));
        for (name, r) in report {
            if let Err(e) = r {
                self.log(format!("  [失败] {name}: {e}"));
            }
        }
    }

    fn end_convert(&mut self) {
        self.converting = false;
        self.convert_rx = None;
        self.progress_done = 0;
        self.progress_total = 0;
        self.progress_current = String::new();
    }
}

/// 把一个下载 channel 里积压的消息全部消费掉。
/// 返回 `Some(结果)` 表示下载已结束（此时 `rx` 被清空）。
fn drain_download(
    rx: &mut Option<Receiver<DownloadMsg>>,
    dl: &mut DlState,
) -> Option<Result<PathBuf, String>> {
    let slot = rx.as_mut()?;
    loop {
        match slot.try_recv() {
            Ok(DownloadMsg::Progress {
                got,
                total,
                extracting,
                attempt,
            }) => dl.update(got, total, extracting, attempt),
            Ok(DownloadMsg::Done(res)) => {
                *rx = None;
                return Some(res);
            }
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                *rx = None;
                return Some(Err("下载线程异常结束".into()));
            }
        }
    }
}
