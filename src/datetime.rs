//! 灵活日期时间解析：支持任意常见格式，包含中文（年月日 / 上午下午 / 点分秒）。
//! 解析结果统一为 `NaiveDateTime`（本地墙钟时间），时区偏移由调用方处理。

use chrono::{
    DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc,
};
use std::str::FromStr;

/// 本地墙钟时间 + 固定偏移 → 带时区的时间。
///
/// 不用 `dt.and_local_timezone(offset).expect(..)`：那个 `expect` 在 FixedOffset 下
/// 理论上不可达，但一旦触发就是整个后台处理线程 panic 掉（进度条永远停在半路，
/// 而且用户看不到任何报错）。这里改成纯算术换算，不产生 panic 分支。
///
/// FixedOffset 没有夏令时，等价关系很直接：UTC = 本地墙钟 − 偏移。
pub fn with_offset(dt: NaiveDateTime, offset: FixedOffset) -> DateTime<FixedOffset> {
    let utc_secs = dt.and_utc().timestamp() - offset.local_minus_utc() as i64;
    DateTime::from_timestamp(utc_secs, 0)
        .map(|t| t.with_timezone(&offset))
        // 只有超出 chrono 可表示范围（±26 万年）才走到这里，退回 1970 也好过崩掉线程
        .unwrap_or_else(|| DateTime::<Utc>::UNIX_EPOCH.with_timezone(&offset))
}

/// 将全角数字/标点转半角，避免中文输入导致解析失败。
fn normalize_fullwidth(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '０'..='９' => {
                let d = c as u32 - '０' as u32 + '0' as u32;
                char::from_u32(d).unwrap_or(c)
            }
            '：' => ':',
            '－' => '-',
            '　' => ' ',
            '．' => '.',
            '／' => '/',
            _ => c,
        })
        .collect()
}

fn strip_keywords(s: &str, kws: &[&str]) -> String {
    let mut out = s.to_string();
    for k in kws {
        out = out.replace(k, " ");
    }
    out
}

/// 将中文 “3点20分30秒” / “15点20” / “09时05分00秒” 统一转成 “H:M:S”。
fn convert_dian(s: &str) -> String {
    let re = regex::Regex::new(r"(\d+)\s*[点时]\s*(\d+)?\s*分?\s*(\d+)?\s*秒?").unwrap();
    re.replace_all(s, |caps: &regex::Captures| {
        let h = &caps[1];
        let m = caps.get(2).map_or("00", |x| x.as_str());
        let sec = caps.get(3).map_or("00", |x| x.as_str());
        format!("{}:{}:{}", h, m, sec)
    })
    .to_string()
}

fn apply_meridiem_time(t: NaiveTime, pm: bool, am: bool) -> NaiveTime {
    let mut h = t.hour();
    if pm && h < 12 {
        h += 12;
    }
    if am && h == 12 {
        h = 0;
    }
    NaiveTime::from_hms_opt(h, t.minute(), t.second()).unwrap_or(t)
}

fn apply_meridiem_dt(dt: NaiveDateTime, pm: bool, am: bool) -> NaiveDateTime {
    let t = apply_meridiem_time(dt.time(), pm, am);
    dt.date().and_time(t)
}

fn parse_time_only(s: &str, pm: bool, am: bool) -> Option<NaiveTime> {
    for fmt in [
        "%H:%M:%S",
        "%H:%M",
        "%H",
        "%H时%M分%S秒",
        "%H时%M分",
        "%H时",
    ] {
        if let Ok(t) = NaiveTime::parse_from_str(s.trim(), fmt) {
            return Some(apply_meridiem_time(t, pm, am));
        }
    }
    None
}

/// 尝试把输入解析为本地墙钟时间。匹配不到返回 None。
pub fn parse_flexible(input: &str) -> Option<NaiveDateTime> {
    let raw = normalize_fullwidth(input.trim());
    if raw.is_empty() {
        return None;
    }

    let lower = raw.to_lowercase();

    // 关键字：现在
    if lower.contains("now") || lower.contains("现在") {
        return Some(Local::now().naive_local());
    }

    // 关键字日期：今天/昨天/明天
    let today = Local::now().date_naive();
    let (base_date, time_part): (Option<NaiveDate>, String) =
        if lower.contains("今天") || lower.contains("今日") {
            (Some(today), strip_keywords(&lower, &["今天", "今日"]))
        } else if lower.contains("昨天") || lower.contains("昨日") {
            (
                Some(today - Duration::days(1)),
                strip_keywords(&lower, &["昨天", "昨日"]),
            )
        } else if lower.contains("明天") || lower.contains("明日") {
            (
                Some(today + Duration::days(1)),
                strip_keywords(&lower, &["明天", "明日"]),
            )
        } else {
            (None, raw.clone())
        };

    // 上下午标记
    let pm = lower.contains("下午")
        || lower.contains("晚上")
        || lower.contains("傍晚")
        || lower.contains("夜里")
        || lower.contains("pm");
    let am = lower.contains("上午")
        || lower.contains("早上")
        || lower.contains("早晨")
        || lower.contains("凌晨")
        || lower.contains("am");

    let mut s = strip_keywords(
        &time_part,
        &[
            "上午", "早上", "早晨", "凌晨", "下午", "晚上", "傍晚", "夜里", "am", "pm", "今天",
            "今日", "昨天", "昨日", "明天", "明日", "现在",
        ],
    );

    // 中文“点分秒” -> 冒号格式
    s = convert_dian(&s);
    // 年月日 -> 斜杠
    s = s.replace("年", "-").replace("月", "-").replace("日", "");
    // 折叠多余空白（中英文混排去除标记后可能留下双空格）
    let s: String = s.split_whitespace().collect::<Vec<&str>>().join(" ");

    // 已知基准日期（今天/昨天/明天）：只解析时间部分
    if let Some(d) = base_date {
        if s.is_empty() {
            return d.and_hms_opt(0, 0, 0);
        }
        if let Some(t) = parse_time_only(&s, pm, am) {
            return Some(d.and_time(t));
        }
        return None;
    }

    // 完整日期时间：尝试多种常见格式
    let candidates = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %H",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%Y/%m/%d %H",
        "%Y/%m/%d",
        "%Y.%m.%d %H:%M:%S",
        "%Y.%m.%d %H:%M",
        "%Y%m%d %H%M%S",
        "%Y%m%d%H%M%S",
        "%Y%m%d",
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y年%m月%d日 %H:%M:%S",
        "%Y年%m月%d日 %H时%M分%S秒",
        "%Y年%m月%d日 %H时%M分",
        "%Y年%m月%d日",
        "%m/%d/%Y %H:%M",
        "%d/%m/%Y %H:%M",
        "%d-%m-%Y %H:%M",
        "%d.%m.%Y %H:%M",
    ];
    for fmt in candidates {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Some(apply_meridiem_dt(dt, pm, am));
        }
    }
    // 仅日期
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d", "%Y%m%d", "%Y年%m月%d日"] {
        if let Ok(d) = NaiveDate::parse_from_str(&s, fmt) {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    // RFC3339 / RFC2822
    if let Ok(dt) = DateTime::<Utc>::from_str(&s) {
        return Some(dt.naive_local());
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(&s) {
        return Some(dt.naive_local());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(s: &str) -> Option<String> {
        parse_flexible(s).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
    }

    #[test]
    fn test_common_formats() {
        assert_eq!(fmt("2023-05-01 15:20"), Some("2023-05-01 15:20:00".into()));
        assert_eq!(fmt("2023/5/1 15:20:00"), Some("2023-05-01 15:20:00".into()));
        assert_eq!(fmt("2023.05.01 09:05"), Some("2023-05-01 09:05:00".into()));
        assert_eq!(fmt("20230501"), Some("2023-05-01 00:00:00".into()));
        assert_eq!(
            fmt("2023:05:01 15:20:00"),
            Some("2023-05-01 15:20:00".into())
        );
    }

    #[test]
    fn test_chinese() {
        // 下午 -> +12
        assert_eq!(
            fmt("2023/5/1 下午3点20"),
            Some("2023-05-01 15:20:00".into())
        );
        assert_eq!(
            fmt("2023年5月1日 09时05分00秒"),
            Some("2023-05-01 09:05:00".into())
        );
        // 上午保留
        assert_eq!(
            fmt("2023/5/1 上午9点5分"),
            Some("2023-05-01 09:05:00".into())
        );
    }

    #[test]
    fn test_invalid() {
        assert_eq!(parse_flexible("乱码xyz"), None);
        assert_eq!(parse_flexible(""), None);
    }

    /// 点号分隔的「仅日期」曾因格式串写成 `%Y.%m/%d` 而匹配不上。
    #[test]
    fn test_dotted_date_only() {
        assert_eq!(fmt("2023.05.01"), Some("2023-05-01 00:00:00".into()));
        assert_eq!(fmt("2023.5.1"), Some("2023-05-01 00:00:00".into()));
    }
}
