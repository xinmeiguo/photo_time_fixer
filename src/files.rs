//! 文件扫描、照片/视频分类，以及读取原始时间（文件系统时间 + 拍摄时间）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime, Utc};

use crate::exiftool;

#[derive(Clone, Copy, PartialEq, Eq)]
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

const PHOTO_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "avif",
];
const VIDEO_EXT: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "wmv", "flv", "m4v", "mpg", "mpeg", "3gp",
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
