//! 修改前的备份与一键恢复。
//! 备份内容：每个文件原始文件系统时间（UNIX 秒）+ 原始拍摄时间字符串。
//! 恢复时据此回写。

use std::path::Path;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use chrono::{FixedOffset, Local};
use serde::{Deserialize, Serialize};

use crate::apply::PlanItem;
use crate::exiftool;
use crate::files;

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    /// UNIX 秒，-1 表示未知
    file_created: i64,
    file_modified: i64,
    taken: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Backup {
    created_at: String,
    offset_secs: i32,
    entries: Vec<Entry>,
}

fn sys_to_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

/// 根据计划写出备份文件，返回文件路径；失败返回 None。
///
/// `offset` 是本次写元数据用的时区，必须一并存进备份 —— 「恢复」要靠它
/// 还原成同样的墙钟时间。
pub fn write_backup(plan: &[PlanItem], offset: FixedOffset) -> Option<String> {
    let mut entries = Vec::new();
    for item in plan {
        let meta = std::fs::metadata(&item.file.path).ok();
        let fc = meta
            .as_ref()
            .and_then(|m| m.created().ok())
            .map(sys_to_secs)
            .unwrap_or(-1);
        let fm = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(sys_to_secs)
            .unwrap_or(-1);
        entries.push(Entry {
            path: item.file.path.to_string_lossy().to_string(),
            file_created: fc,
            file_modified: fm,
            taken: item.file.original_taken.clone(),
        });
    }
    let backup = Backup {
        created_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        offset_secs: offset.local_minus_utc(),
        entries,
    };
    let json = serde_json::to_string_pretty(&backup).ok()?;

    let dir = plan
        .first()
        .and_then(|i| i.file.path.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let fname = format!("undo_{}.json", Local::now().format("%Y%m%d_%H%M%S"));
    let path = dir.join(&fname);
    std::fs::write(&path, json).ok()?;
    Some(path.to_string_lossy().to_string())
}

/// 从备份文件恢复。返回每个文件名与其成功/失败信息。
pub fn restore(path: &str) -> Vec<(String, Result<(), String>)> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) => return vec![("<backup>".into(), Err(format!("读取备份失败: {e}")))],
    };
    let backup: Backup = match serde_json::from_str(&data) {
        Ok(b) => b,
        Err(e) => return vec![("<backup>".into(), Err(format!("解析备份失败: {e}")))],
    };
    let offset = FixedOffset::east_opt(backup.offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(8 * 3600).unwrap());

    let mut report = Vec::new();
    for e in &backup.entries {
        let p = Path::new(&e.path);
        let mut errs: Vec<String> = Vec::new();

        if e.file_created >= 0 || e.file_modified >= 0 {
            let created = if e.file_created >= 0 {
                UNIX_EPOCH + StdDuration::from_secs(e.file_created as u64)
            } else {
                std::fs::metadata(p)
                    .and_then(|m| m.created())
                    .unwrap_or(UNIX_EPOCH)
            };
            let modified = if e.file_modified >= 0 {
                UNIX_EPOCH + StdDuration::from_secs(e.file_modified as u64)
            } else {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH)
            };
            let accessed = modified;
            if let Err(err) = crate::apply::set_fs_times(p, created, accessed, modified) {
                errs.push(format!("文件系统时间: {err}"));
            }
        }

        if let Some(taken) = &e.taken {
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_lowercase();
            let is_video = files::is_video_ext(&ext);
            if let Some(dt) = crate::apply::parse_taken(taken) {
                if let Err(err) = exiftool::write_metadata(p, dt, offset, is_video) {
                    errs.push(err);
                }
            }
        }

        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        if errs.is_empty() {
            report.push((name, Ok(())));
        } else {
            report.push((name, Err(errs.join("; "))));
        }
    }
    report
}
