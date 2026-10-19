//! 文件扫描、照片/视频分类，以及读取原始时间（文件系统时间 + 拍摄时间）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime, Utc};

use crate::exiftool;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    Photo,
    Video,
}

impl MediaKind {
    pub fn label(&self) -> &'static str {
        match self {
            MediaKind::Photo => "照片",
            MediaKind::Video => "视频",
        }
    }
}

#[derive(Clone)]
pub struct MediaFile {
    pub path: PathBuf,
    pub name: String,
    pub kind: MediaKind,
    /// exiftool 读取到的原始拍摄时间字符串（如 "2020:01:01 10:00:00"）
    pub original_taken: Option<String>,
    pub original_modified: Option<NaiveDateTime>,
}

impl MediaFile {
    /// 预览表里「原时间」列的显示文本：
    /// 优先拍摄时间（元数据），其次文件修改时间，都没有就显示破折号。
    pub fn original_display(&self) -> String {
        if let Some(t) = &self.original_taken {
            t.clone()
        } else if let Some(d) = self.original_modified {
            d.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            "—".into()
        }
    }
}

const PHOTO_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "avif",
];
const VIDEO_EXT: &[&str] = &[
    // 注意：这里必须覆盖「视频格式转换」页所有可选的目标格式，
    // 否则能选出来的格式却扫不到同类输入（webm 曾经就是这样漏掉的）。
    "mp4", "mov", "avi", "mkv", "webm", "wmv", "flv", "m4v", "mpg", "mpeg", "3gp",
];

fn kind_of(ext: &str) -> Option<MediaKind> {
    let e = ext.to_lowercase();
    if PHOTO_EXT.contains(&e.as_str()) {
        Some(MediaKind::Photo)
    } else if VIDEO_EXT.contains(&e.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

/// 扩展名是否为视频。备份恢复时也要按视频规则写元数据，
/// 这里共用同一份列表，避免两处各维护一份而走样。
pub fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXT.contains(&ext.to_lowercase().as_str())
}

fn sys_to_naive(t: SystemTime) -> NaiveDateTime {
    DateTime::<Utc>::from(t).with_timezone(&Local).naive_local()
}

/// 扫描目录，返回按文件名排序的媒体文件列表。
/// 拍摄时间（exiftool 元数据）一次性批量读取，避免逐文件启动进程导致卡死。
pub fn scan(dir: &Path, recursive: bool) -> Vec<MediaFile> {
    let mut candidates: Vec<(PathBuf, String, MediaKind)> = Vec::new();
    collect_candidates(dir, recursive, &mut candidates);

    // 所有候选路径一次性交给 exiftool 批量读取（仅一次进程启动）
    let all_paths: Vec<PathBuf> = candidates.iter().map(|(p, _, _)| p.clone()).collect();
    let taken_map = if exiftool::is_installed() {
        exiftool::read_taken_batch(&all_paths)
    } else {
        HashMap::new()
    };

    let mut out = Vec::new();
    for (path, name, kind) in candidates {
        let om = fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(sys_to_naive);
        let original_taken = taken_map.get(&path.to_string_lossy().to_string()).cloned();
        out.push(MediaFile {
            path,
            name,
            kind,
            original_taken,
            original_modified: om,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn collect_candidates(dir: &Path, recursive: bool, out: &mut Vec<(PathBuf, String, MediaKind)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_candidates(&path, recursive, out);
            }
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some(kind) = kind_of(ext) {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push((path, name, kind));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转换页下拉框里能选的目标格式，扫描时必须都能识别。
    /// 这条测试就是为 webm 曾经漏在 VIDEO_EXT 外而加的。
    #[test]
    fn convert_targets_are_all_recognized() {
        for ext in ["mp4", "mkv", "mov", "avi", "webm", "flv", "m4v"] {
            assert_eq!(
                kind_of(ext),
                Some(MediaKind::Video),
                "转换目标格式 {ext} 未被识别为视频"
            );
        }
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(kind_of("JPG"), Some(MediaKind::Photo));
        assert_eq!(kind_of("HeIc"), Some(MediaKind::Photo));
        assert!(is_video_ext("WEBM"));
        assert!(is_video_ext("Mp4"));
    }

    #[test]
    fn irrelevant_extension_is_ignored() {
        assert_eq!(kind_of("txt"), None);
        assert_eq!(kind_of(""), None);
        assert!(!is_video_ext("jpg"));
    }

    /// 预览表「原时间」列：拍摄时间 > 文件修改时间 > 破折号。
    #[test]
    fn original_display_prefers_taken_time() {
        let mut f = MediaFile {
            path: PathBuf::from("a.jpg"),
            name: "a.jpg".into(),
            kind: MediaKind::Photo,
            original_taken: Some("2020:01:01 10:00:00".into()),
            original_modified: None,
        };
        assert_eq!(f.original_display(), "2020:01:01 10:00:00");

        f.original_taken = None;
        f.original_modified = Some(
            NaiveDateTime::parse_from_str("2021-02-02 11:00:00", "%Y-%m-%d %H:%M:%S").unwrap(),
        );
        assert_eq!(f.original_display(), "2021-02-02 11:00:00");

        f.original_modified = None;
        assert_eq!(f.original_display(), "—");
    }
}
