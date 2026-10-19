//! 预览计划计算与实际应用：
//! - 文件系统时间（创建/修改/访问）由本模块直接设置；
//! - 媒体元数据时间（拍摄时间）调用 exiftool。

use std::fs;
use std::path::Path;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use chrono::{Duration, FixedOffset, NaiveDateTime};
use rand::Rng;

use crate::exiftool;
use crate::files::{MediaFile, MediaKind};

/// 计算计划时的输入。
pub struct PlanInput {
    pub mode: PlanMode,
    /// 绝对模式下每张文件之间的固定间隔（分钟）
    pub interval_min: u32,
    /// 绝对模式下是否在每次间隔上附加一个 0~59 的随机秒
    pub random: bool,
    /// 偏移模式下的整体偏移量（可为负）
    pub offset: Duration,
}

pub enum PlanMode {
    Absolute(NaiveDateTime),
    Offset,
}

pub struct PlanItem {
    pub file: MediaFile,
    pub new_time: NaiveDateTime,
}

/// 解析 exiftool 读出的拍摄时间字符串。
pub fn parse_taken(s: &str) -> Option<NaiveDateTime> {
    for fmt in [
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y:%m:%d %H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s.trim(), fmt) {
            return Some(dt);
        }
    }
    None
}

/// 根据输入计算每张文件的新时间。
pub fn compute_plan(files: &[MediaFile], input: &PlanInput) -> Vec<PlanItem> {
    let mut out = Vec::new();
    match input.mode {
        PlanMode::Absolute(base) => {
            let mut cursor = base;
            let mut rng = rand::thread_rng();
            for f in files {
                out.push(PlanItem {
                    file: f.clone(),
                    new_time: cursor,
                });
                cursor = cursor + Duration::minutes(input.interval_min as i64);
                if input.random {
                    cursor = cursor + Duration::seconds(rng.gen_range(0..=59));
                }
            }
        }
        PlanMode::Offset => {
            for f in files {
                let base = f
                    .original_taken
                    .as_ref()
                    .and_then(|s| parse_taken(s))
                    .or(f.original_modified);
                if let Some(b) = base {
                    out.push(PlanItem {
                        file: f.clone(),
                        new_time: b + input.offset,
                    });
                }
            }
        }
    }
    out
}

/// 处理进度消息，由后台处理线程发往 UI 线程。
pub enum ProgressMsg {
    /// 进度更新：已处理 done / 共 total，当前正在处理的文件名 current。
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    /// 处理完成，附带每个文件的结果。
    Done(Vec<(String, Result<(), String>)>),
}

/// 处理单个文件的时间修改。逻辑与原 `apply` 逐字一致，仅供拆分复用。
pub fn apply_one(
    item: &PlanItem,
    offset: FixedOffset,
    set_created: bool,
    set_modified: bool,
    set_taken: bool,
) -> Result<(), String> {
    let path = &item.file.path;
    let is_video = matches!(item.file.kind, MediaKind::Video);
    let mut errs: Vec<String> = Vec::new();

    // 先写元数据；exiftool -overwrite_original 会改变文件系统修改时间，
    // 因此文件系统时间必须放在最后设置，才能保留用户指定的创建/修改时间。
    if set_taken {
        if let Err(e) = exiftool::write_metadata(path, item.new_time, offset, is_video) {
            errs.push(e);
        }
    }

    if set_created || set_modified {
        let dt = item
            .new_time
            .and_local_timezone(offset)
            .single()
            .expect("fixed offset is unambiguous");
        let secs = dt.timestamp().max(0) as u64;
        let instant = UNIX_EPOCH + StdDuration::from_secs(secs);

        let meta = fs::metadata(path).ok();
        let cur_created = meta
            .as_ref()
            .and_then(|m| m.created().ok())
            .unwrap_or(instant);
        let cur_modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .unwrap_or(instant);
        let new_created = if set_created { instant } else { cur_created };
        let new_modified = if set_modified { instant } else { cur_modified };
        let new_accessed = new_modified;

        if let Err(e) = set_fs_times(path, new_created, new_accessed, new_modified) {
            errs.push(format!("文件系统时间: {e}"));
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// 应用计划（带进度回传）。在后台线程中逐文件处理，并通过 `tx` 实时上报进度，
/// 全部完成后发送 `ProgressMsg::Done`。
pub fn apply_with_progress(
    plan: Vec<PlanItem>,
    offset: FixedOffset,
    set_created: bool,
    set_modified: bool,
    set_taken: bool,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
) {
    let total = plan.len();
    let mut report = Vec::with_capacity(total);
    for (i, item) in plan.into_iter().enumerate() {
        let name = item.file.name.clone();
        let current = item.file.name.clone();
        let res = apply_one(&item, offset, set_created, set_modified, set_taken);
        report.push((name, res));
        let _ = tx.send(ProgressMsg::Progress {
            done: i + 1,
            total,
            current,
        });
    }
    let _ = tx.send(ProgressMsg::Done(report));
}

// ---- 文件系统时间设置 ----

#[cfg(windows)]
pub fn set_fs_times(
    path: &Path,
    created: SystemTime,
    accessed: SystemTime,
    modified: SystemTime,
) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{SetFileTime, FILE_FLAG_BACKUP_SEMANTICS};

    fn to_filetime(t: SystemTime) -> FILETIME {
        // Windows FILETIME 起点是 1601-01-01 UTC，Unix 起点是 1970-01-01 UTC，
        // 两者相差 11644473600 秒。必须加上这个偏移，否则会变成 1657 年。
        const WINDOWS_EPOCH_DIFF_SECS: u64 = 11644473600;
        let dur = t.duration_since(UNIX_EPOCH).unwrap_or(StdDuration::ZERO);
        let intervals = (dur.as_secs() + WINDOWS_EPOCH_DIFF_SECS) * 10_000_000
            + (dur.subsec_nanos() as u64 / 100);
        FILETIME {
            dwLowDateTime: (intervals & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (intervals >> 32) as u32,
        }
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let handle = file.as_raw_handle() as HANDLE;
    let ct = to_filetime(created);
    let at = to_filetime(accessed);
    let mt = to_filetime(modified);
    let ok = unsafe { SetFileTime(handle, &ct, &at, &mt) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
pub fn set_fs_times(
    path: &Path,
    _created: SystemTime,
    accessed: SystemTime,
    modified: SystemTime,
) -> std::io::Result<()> {
    // Unix 没有“创建时间”概念，仅设置访问/修改时间。
    filetime::set_file_times(path, accessed.into(), modified.into())
}
