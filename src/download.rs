//! HTTP 下载：流式读取 + 进度回调 + 失败重试。
//!
//! 为什么不用 `resp.bytes()`：它一次把整个响应读进内存，中途没有任何反馈。
//! ffmpeg 的 essentials 包有几十 MB，慢网络下界面只会停在「下载中…」，
//! 用户以为卡死；而且一旦中途断开，前面收的字节全白费。
//! 这里改成 `Read` 分块读取，边读边上报已收字节，并在收不够 Content-Length
//! 时判为截断（截断的 zip 解压时才会报「不可预料的压缩文件末端」，报错点太远）。

use std::io::Read;
use std::time::Duration;

/// 总尝试次数（含首次）。gyan.dev 偶发 502、exiftool.org 偶尔超时，
/// 这类失败重试一次基本就过。
const ATTEMPTS: usize = 3;

/// 分块大小：64 KB 足够平滑，又不至于让回调被调用得过于频繁。
const CHUNK: usize = 64 * 1024;

/// 下载所处阶段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DlStage {
    /// 正在接收字节
    Fetching,
    /// 已下载完，正在解压
    Extracting,
}

/// 一次进度上报。
#[derive(Clone, Copy)]
pub struct DlProgress {
    pub stage: DlStage,
    /// 已接收字节数
    pub got: u64,
    /// 总字节数；服务器没给 Content-Length 时为 None
    pub total: Option<u64>,
    /// 当前是第几次尝试（从 1 起），用于提示「正在重试」
    pub attempt: usize,
}

/// 进度回调。用 `&mut dyn FnMut` 而不是泛型，避免两个下载入口都要泛型化。
pub type ProgressSink<'a> = &'a mut dyn FnMut(DlProgress);

/// 下载 URL 到内存，带进度回调与自动重试。
pub fn fetch(url: &str, sink: ProgressSink) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            // 退避 0.8s / 1.6s：给对端一点恢复时间，又不至于让用户觉得卡住
            std::thread::sleep(Duration::from_millis(800 * (attempt as u64 - 1)));
        }
        match fetch_once(url, attempt, &mut *sink) {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(format!("{last}（已尝试 {ATTEMPTS} 次）"))
}

fn fetch_once(url: &str, attempt: usize, sink: ProgressSink) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        // 连接超时要短（失败快、重试才有意义），整体超时给足（几十 MB 的包）
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("连接失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 HTTP {}", resp.status()));
    }
    let total = resp.content_length();
    let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut chunk = [0u8; CHUNK];
    let mut got: u64 = 0;
    emit(sink, DlStage::Fetching, got, total, attempt);
    loop {
        let n = resp
            .read(&mut chunk)
            .map_err(|e| format!("读取下载内容失败: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        got += n as u64;
        emit(sink, DlStage::Fetching, got, total, attempt);
    }
    // 服务器声明了长度就必须收够，否则是半截数据
    if let Some(t) = total {
        if got < t {
            return Err(format!("下载不完整：{got}/{t} 字节"));
        }
    }
    Ok(buf)
}

fn emit(sink: ProgressSink, stage: DlStage, got: u64, total: Option<u64>, attempt: usize) {
    sink(DlProgress {
        stage,
        got,
        total,
        attempt,
    });
}

/// 通知界面「进入解压阶段」。
pub fn notify_extracting(sink: ProgressSink, attempt: usize) {
    emit(sink, DlStage::Extracting, 0, None, attempt);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解压阶段必须发一条独立的进度事件，否则界面会一直停在最后一个百分比。
    #[test]
    fn extracting_stage_is_reported() {
        let mut events: Vec<DlProgress> = Vec::new();
        let mut sink = |p: DlProgress| events.push(p);
        notify_extracting(&mut sink, 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, DlStage::Extracting);
        assert_eq!(events[0].attempt, 2);
        assert_eq!(events[0].total, None);
    }

    /// 重试次数写在常量里，改小会让弱网下极易失败，改大则卡很久。
    #[test]
    fn retry_policy_is_sane() {
        assert!((2..=5).contains(&ATTEMPTS));
    }
}
