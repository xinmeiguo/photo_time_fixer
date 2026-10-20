//! 预览计划计算与实际应用：
//! - 文件系统时间（创建/修改/访问）由本模块直接设置；
//! - 媒体元数据时间（拍摄时间）调用 exiftool。

use std::fs;
use std::path::Path;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use chrono::{Duration, FixedOffset, NaiveDateTime};
use rand::Rng;

use crate::datetime;
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

/// 只设文件系统时间（创建/修改/访问）。
///
/// ★ 必须在元数据写入**之后**调用：`-overwrite_original` 会重写整个文件，
/// 把修改时间刷成“现在”，先设会被覆盖掉。
fn set_times_one(
    item: &PlanItem,
    offset: FixedOffset,
    set_created: bool,
    set_modified: bool,
) -> Result<(), String> {
    let path = &item.file.path;
    let instant = UNIX_EPOCH
        + StdDuration::from_secs(
            datetime::with_offset(item.new_time, offset)
                .timestamp()
                .max(0) as u64,
        );

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

    set_fs_times(path, new_created, new_accessed, new_modified).map_err(|e| e.to_string())
}

/// 每批处理多少个文件。
///
/// 为什么不「一次全批」：
/// - 一个坏文件会让整批退出码非 0，回落到逐文件重试时等于白跑一遍；
/// - 分块后「取消」最迟在块边界生效，进度条也有机会往前走。
///
/// 为什么是 32（实测，200 张 1.6 MB JPEG）：
/// - 逐文件：1.35 s/张 → 200 张约 4.5 分钟
/// - 一次全批：约 49 s（只有 1 次启动开销，但取消要等 49 秒）
/// - 32/块：57 s（7 次启动开销多花 8 s，换来取消延迟 ≤ 8 秒）
///
/// 也就是说启动开销只占三成，块大小对总时间影响很小 —— 用 17% 的时间
/// 换 6 倍的取消响应速度，是划算的。块再小就没意义了（16/块 反而慢 15%）。
const APPLY_CHUNK: usize = 32;

/// 应用计划（带进度回传）。在后台线程中按块处理，并通过 `tx` 实时上报进度，
/// 全部完成后发送 `ProgressMsg::Done`。
///
/// 处理顺序：**整块先写元数据（一次 exiftool）→ 再逐文件设文件系统时间**。
/// 顺序反了会丢时间（见 `set_times_one` 的说明）。
///
/// `cancel` 每块检查一次；置位则停止开工并发送 `ProgressMsg::Cancelled`。
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
    let mut report: Vec<(String, Result<(), String>)> = Vec::with_capacity(total);
    let mut done = 0usize;

    for chunk in plan.chunks(APPLY_CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(ProgressMsg::Cancelled {
                done,
                total,
                report,
            });
            return;
        }
        // 块开工前报一次：一整批要跑好几秒，界面得知道在动。
        // current 写「首个文件名 等 N 个」而不是只写第一个：整批是一次 exiftool
        // 调用同时处理的，只显示一个名字会让用户以为在逐个处理。
        let current = match chunk.first() {
            Some(first) if chunk.len() > 1 => {
                format!("{} 等 {} 个", first.file.name, chunk.len())
            }
            Some(first) => first.file.name.clone(),
            None => String::new(),
        };
        let _ = tx.send(ProgressMsg::Progress {
            done,
            total,
            current,
        });

        // ① 元数据：整块只启动一次 exiftool；失败时由它内部回落逐文件重试。
        let meta: Vec<Option<Result<(), String>>> = if set_taken {
            let items: Vec<exiftool::MetaWrite<'_>> = chunk
                .iter()
                .map(|i| exiftool::MetaWrite {
                    path: &i.file.path,
                    dt: i.new_time,
                    is_video: matches!(i.file.kind, MediaKind::Video),
                })
                .collect();
            exiftool::write_metadata_batch(&items, offset)
                .into_iter()
                .map(Some)
                .collect()
        } else {
            vec![None; chunk.len()]
        };

        // ② 文件系统时间：逐文件调 Win32 SetFileTime（微秒级，无需批量）
        for (item, meta_res) in chunk.iter().zip(meta) {
            let mut errs: Vec<String> = Vec::new();
            if let Some(Err(e)) = meta_res {
                errs.push(e);
            }
            if set_created || set_modified {
                if let Err(e) = set_times_one(item, offset, set_created, set_modified) {
                    errs.push(format!("文件系统时间: {e}"));
                }
            }
            report.push((
                item.file.name.clone(),
                if errs.is_empty() {
                    Ok(())
                } else {
                    Err(errs.join("; "))
                },
            ));
            done += 1;
        }
    }

    let _ = tx.send(ProgressMsg::Progress {
        done,
        total,
        current: String::new(),
    });
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
