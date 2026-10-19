//! 界面状态类型、后台消息枚举、路径小工具。
//!
//! 这里只放「数据与纯函数」，不放流程：动作在 `actions`，消息回收在 `poll`。

use std::path::{Path, PathBuf};

use eframe::egui;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Absolute,
    Offset,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Time,
    Convert,
}

/// 下载线程发往界面的消息（exiftool 与 ffmpeg 共用）。
pub enum DownloadMsg {
    /// 进度：已收字节 / 总字节（服务器未给长度时为 None）
    Progress {
        got: u64,
        total: Option<u64>,
        extracting: bool,
        attempt: usize,
    },
    /// 完成：成功返回可执行文件路径，失败返回原因
    Done(Result<PathBuf, String>),
}

/// 转换线程发往界面的消息。
pub enum ConvertMsg {
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    /// 用户取消：已完成 `done` 个，附带已完成部分的结果。
    Cancelled {
        done: usize,
        total: usize,
        report: Vec<(String, Result<String, String>)>,
    },
    Done(Vec<(String, Result<String, String>)>),
}

/// 一次下载的界面状态（进度条 + 文案）。exiftool 与 ffmpeg 各持一份。
#[derive(Default)]
pub struct DlState {
    /// 结束后的结果文案（成功/失败）
    pub status: String,
    pub got: u64,
    pub total: Option<u64>,
    pub extracting: bool,
    /// 第几次尝试（>1 时界面提示正在重试）
    pub attempt: usize,
    /// 下载进行中：按钮禁用、进度条显示、持续重绘
    pub active: bool,
}

impl DlState {
    pub fn begin(&mut self) {
        self.status.clear();
        self.got = 0;
        self.total = None;
        self.extracting = false;
        self.attempt = 1;
        self.active = true;
    }

    pub fn update(&mut self, got: u64, total: Option<u64>, extracting: bool, attempt: usize) {
        self.got = got;
        self.total = total;
        self.extracting = extracting;
        self.attempt = attempt;
    }

    /// 结束（成功或失败），保留结果文案。
    pub fn finish(&mut self, msg: String) {
        self.active = false;
        self.status = msg;
        self.got = 0;
        self.total = None;
        self.extracting = false;
    }

    /// 进度比例。没有 Content-Length 时返回 0（进度条停在起点，
    /// 但文案里仍有已下载体积，用户能看出在动）。
    pub fn fraction(&self) -> f32 {
        match self.total {
            Some(t) if t > 0 => (self.got as f32 / t as f32).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    pub fn text(&self) -> String {
        if self.extracting {
            return "解压中…".into();
        }
        let mut s = match self.total {
            Some(t) if t > 0 => format!("下载 {:.1}/{:.1} MB", mb(self.got), mb(t)),
            _ => format!("下载 {:.1} MB", mb(self.got)),
        };
        if self.attempt > 1 {
            s.push_str(&format!("（第 {} 次尝试）", self.attempt));
        }
        s
    }

    /// 在横向布局里画「进度条 + 文案」。
    pub fn show(&self, ui: &mut egui::Ui) {
        ui.add(egui::ProgressBar::new(self.fraction()).desired_width(120.0));
        ui.label(self.text());
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// 是否网络共享路径（UNC，如 `\\192.168.1.216\share\...`）。
/// 排除 `\\?\`（verbatim）与 `\\.\`（设备命名空间），那两个不是共享。
pub fn is_network_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.starts_with(r"\\") && !s.starts_with(r"\\?\") && !s.starts_with(r"\\.\")
}

/// 确保目录存在且**真的可写**：建目录 + 写一个探针文件再删掉。
/// 只判断 `exists()` 不够——网络共享/只读目录能创建句柄却写不进去。
pub fn ensure_writable_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let probe = dir.join(".ptf_write_test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("写入测试失败: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_path_is_network_but_verbatim_is_not() {
        assert!(is_network_path(Path::new(r"\\192.168.1.216\share")));
        // verbatim / 设备命名空间不是共享路径，不能误报「网络权限不足」
        assert!(!is_network_path(Path::new(r"\\?\D:\photos")));
        assert!(!is_network_path(Path::new(r"\\.\C:")));
        assert!(!is_network_path(Path::new(r"D:\photos")));
    }

    #[test]
    fn progress_text_shows_percent_source() {
        let mut d = DlState::default();
        d.begin();
        d.update(512 * 1024, Some(1024 * 1024), false, 1);
        assert!((d.fraction() - 0.5).abs() < 1e-6);
        assert!(d.text().contains("0.5/1.0 MB"), "{}", d.text());

        // 没有 Content-Length 时比例退回 0，但文案仍有体积
        d.update(3 * 1024 * 1024, None, false, 2);
        assert_eq!(d.fraction(), 0.0);
        assert!(d.text().contains("3.0 MB"));
        assert!(d.text().contains("第 2 次"), "{}", d.text());

        d.update(0, None, true, 1);
        assert_eq!(d.text(), "解压中…");
    }
}
