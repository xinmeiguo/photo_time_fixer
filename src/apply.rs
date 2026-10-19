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
                cursor += Duration::minutes(input.interval_min as i64);
                if input.random {
                    cursor += Duration::seconds(rng.gen_range(0..=59));
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
    /// 用户取消：已完成 done 个，附带已完成部分的结果。
    Cancelled {
        done: usize,
        total: usize,
        report: Vec<(String, Result<(), String>)>,
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
///
/// `cancel` 每处理完一个文件检查一次；置位则停止开工并发送 `ProgressMsg::Cancelled`。
/// 已改过的文件**不回滚**（回滚请用备份「恢复…」），只是不再继续。
pub fn apply_with_progress(
    plan: Vec<PlanItem>,
    offset: FixedOffset,
    set_created: bool,
    set_modified: bool,
    set_taken: bool,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
) {
    use std::sync::atomic::Ordering;

    let total = plan.len();
    let mut report = Vec::with_capacity(total);
    for (i, item) in plan.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(ProgressMsg::Cancelled {
                done: i,
                total,
                report,
            });
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::path::PathBuf;

    fn media(name: &str, taken: Option<&str>, modified: Option<&str>) -> MediaFile {
        MediaFile {
            path: PathBuf::from(name),
            name: name.to_string(),
            kind: MediaKind::Photo,
            original_taken: taken.map(|s| s.to_string()),
            original_modified: modified.and_then(parse_taken),
        }
    }

    fn base() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap()
    }

    fn text(dt: NaiveDateTime) -> String {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// exiftool 回传的格式是冒号分隔（2020:01:01），但我们自己的备份/预览
    /// 是连字符格式，两种都必须认。
    #[test]
    fn parse_taken_accepts_both_separators() {
        assert!(parse_taken("2020:01:01 10:00:00").is_some());
        assert!(parse_taken("2020-01-01 10:00:00").is_some());
        assert!(parse_taken("2020-01-01 10:00").is_some());
        // 前后空白要容忍（JSON 回传偶尔带空格）
        assert!(parse_taken("  2020:01:01 10:00:00  ").is_some());
        assert!(parse_taken("not a date").is_none());
        assert!(parse_taken("").is_none());
    }

    /// 绝对模式：从起始时间开始，按固定间隔依次排下去。
    #[test]
    fn absolute_plan_is_sequential() {
        let files = vec![
            media("a.jpg", None, None),
            media("b.jpg", None, None),
            media("c.jpg", None, None),
        ];
        let plan = compute_plan(
            &files,
            &PlanInput {
                mode: PlanMode::Absolute(base()),
                interval_min: 2,
                random: false,
                offset: Duration::zero(),
            },
        );
        assert_eq!(plan.len(), 3);
        assert_eq!(text(plan[0].new_time), "2026-01-01 09:00:00");
        assert_eq!(text(plan[1].new_time), "2026-01-01 09:02:00");
        assert_eq!(text(plan[2].new_time), "2026-01-01 09:04:00");
    }

    /// 偏移模式：优先用拍摄时间，没有就退回文件修改时间；
    /// 两者都没有的文件跳过（无处可偏移，硬套偏移会凭空造时间）。
    #[test]
    fn offset_plan_uses_taken_then_modified() {
        let files = vec![
            media("a.jpg", Some("2020:01:01 10:00:00"), None),
            media("b.jpg", None, Some("2021-02-02 11:00:00")),
            media("c.jpg", None, None),
        ];
        let plan = compute_plan(
            &files,
            &PlanInput {
                mode: PlanMode::Offset,
                interval_min: 0,
                random: false,
                offset: Duration::hours(8),
            },
        );
        assert_eq!(plan.len(), 2, "没有任何时间的文件必须跳过");
        assert_eq!(text(plan[0].new_time), "2020-01-01 18:00:00");
        assert_eq!(text(plan[1].new_time), "2021-02-02 19:00:00");
    }

    /// 负偏移（提前）也要算对，且跨日不能溢出。
    #[test]
    fn offset_can_go_backwards_across_midnight() {
        let files = vec![media("a.jpg", Some("2020:01:01 02:00:00"), None)];
        let plan = compute_plan(
            &files,
            &PlanInput {
                mode: PlanMode::Offset,
                interval_min: 0,
                random: false,
                offset: Duration::hours(-5),
            },
        );
        assert_eq!(text(plan[0].new_time), "2019-12-31 21:00:00");
    }
}
