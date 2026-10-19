//! exiftool 检测、自动下载安装与调用。
//! 元数据时间（拍摄时间/CreateDate/ModifyDate）统一走 exiftool；
//! 文件系统时间由 apply 模块直接设置，不在此处理。

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{FixedOffset, NaiveDateTime};

/// 返回 exiftool 可执行文件路径（可能是命令名 "exiftool" 或具体文件路径）。
pub fn exiftool_path() -> Option<PathBuf> {
    // 1. 工具自带（与 exe 同级的 tools 目录）
    if let Some(dir) = exe_dir() {
        let p = dir.join("tools").join("exiftool.exe");
        if p.exists() {
            return Some(p);
        }
    }
    // 2. PATH 中可直接调用
    if Command::new("exiftool")
        .arg("-ver")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("exiftool"));
    }
    // 3. 常见安装位置
    for cand in [
        r"C:\Program Files\ExifTool\exiftool.exe",
        r"C:\Program Files\ExifTool\exiftool(-k).exe",
        r"C:\Program Files (x86)\ExifTool\exiftool.exe",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn is_installed() -> bool {
    exiftool_path().is_some()
}

/// 读取 exiftool 版本号（用于界面状态灯展示）。
pub fn version(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("-ver").output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !e.is_empty() {
            return Some(e);
        }
    }
    None
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
}

/// 从 exiftool.org 下载 Windows 版并解压到 <dir>/tools/exiftool.exe。
/// 返回解压后的 exe 路径。
///
/// `sink` 用于把下载进度回传给界面（zip 约 6 MB，走的是流式读取）。
pub fn download_and_install(
    dir: &Path,
    sink: crate::download::ProgressSink,
) -> Result<PathBuf, String> {
    // 取官网首页解析最新版本号（页面很小，但仍然走同一套重试逻辑）
    let index_bytes = crate::download::fetch("https://exiftool.org/", &mut *sink)
        .map_err(|e| format!("无法访问 exiftool.org: {e}"))?;
    let index = String::from_utf8_lossy(&index_bytes).to_string();
    let re = regex::Regex::new(r"exiftool-(\d+\.\d+)_64\.zip").unwrap();
    let ver = re
        .captures(&index)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| "无法在官网解析 exiftool 版本".to_string())?;
    let url = format!("https://exiftool.org/exiftool-{ver}_64.zip");

    let bytes = crate::download::fetch(&url, &mut *sink)?;
    crate::download::notify_extracting(sink, 1);

    let tools_dir = dir.join("tools");
    std::fs::create_dir_all(&tools_dir).map_err(|e| format!("创建目录失败: {e}"))?;

    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("解压失败: {e}"))?;
    let mut exe_written = false;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = file.name().to_string();
        let rel = name.rsplit('/').next().unwrap_or(&name).to_string();
        if rel.is_empty() {
            continue;
        }
        let out_path = tools_dir.join(&rel);
        if file.is_dir() {
            let _ = std::fs::create_dir_all(&out_path);
            continue;
        }
        // 主程序 exe 统一命名为 exiftool.exe
        let final_name = if rel == "exiftool(-k).exe" || rel == "exiftool.exe" {
            exe_written = true;
            "exiftool.exe".to_string()
        } else {
            rel.clone()
        };
        let final_path = tools_dir.join(final_name);
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(|e| e.to_string())?;
        std::fs::write(&final_path, &content).map_err(|e| e.to_string())?;
    }

    let exe = tools_dir.join("exiftool.exe");
    if !exe.exists() || !exe_written {
        return Err("解压后未找到 exiftool.exe".into());
    }
    Ok(exe)
}

/// 写入媒体元数据时间。
/// 照片：DateTimeOriginal / CreateDate / ModifyDate（本地墙钟，无时区）。
/// 视频：CreateDate / ModifyDate（QuickTime 按 UTC 存储，用 -api QuickTimeUTC 转写）。
pub fn write_metadata(
    path: &Path,
    dt: NaiveDateTime,
    offset: FixedOffset,
    is_video: bool,
) -> Result<(), String> {
    let exep = exiftool_path().ok_or_else(|| "未找到 exiftool".to_string())?;
    let mut lines: Vec<String> = vec!["-overwrite_original".into()];

    let dt_str = if is_video {
        lines.push("-api".into());
        lines.push("QuickTimeUTC".into());
        let dt = dt
            .and_local_timezone(offset)
            .single()
            .expect("fixed offset is unambiguous");
        dt.format("%Y:%m:%d %H:%M:%S%:z").to_string()
    } else {
        dt.format("%Y:%m:%d %H:%M:%S").to_string()
    };

    // 注意：-TAG=VALUE 必须作为单个参数传给 exiftool，否则 VALUE 会被当成文件路径。
    if is_video {
        lines.push(format!("-CreateDate={dt_str}"));
        lines.push(format!("-ModifyDate={dt_str}"));
    } else {
        lines.push(format!("-DateTimeOriginal={dt_str}"));
        lines.push(format!("-CreateDate={dt_str}"));
        lines.push(format!("-ModifyDate={dt_str}"));
    }
    lines.push(path.to_string_lossy().to_string());

    // 通过 UTF-8 参数文件 + -charset filename=UTF8 绕过 Windows 命令行编码问题
    let argfile = temp_argfile_path("ptf_write");
    std::fs::write(&argfile, lines.join("\n")).map_err(|e| format!("写入参数文件失败: {e}"))?;
    let out = Command::new(&exep)
        .arg("-charset")
        .arg("filename=UTF8")
        .arg("-@")
        .arg(&argfile)
        .output()
        .map_err(|e| format!("执行 exiftool 失败: {e}"))?;
    let _ = std::fs::remove_file(&argfile);
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("exiftool 报错: {}", err.trim()))
    }
}

/// 一次性批量读取多个文件的“拍摄时间”，只启动一个 exiftool 进程。
/// 返回 `SourceFile(路径字符串) -> 拍摄时间字符串` 的映射。
pub fn read_taken_batch(paths: &[PathBuf]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if paths.is_empty() {
        return map;
    }
    let exep = match exiftool_path() {
        Some(p) => p,
        None => return map,
    };

    // 用 UTF-8 参数文件传路径，避免 Windows 命令行对 Unicode 文件名编码错误。
    let argfile = temp_argfile_path("ptf_read");
    let content = paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if std::fs::write(&argfile, content).is_err() {
        return map;
    }

    let out = Command::new(&exep)
        .arg("-j")
        .arg("-DateTimeOriginal")
        .arg("-CreateDate")
        .arg("-FileModifyDate")
        .arg("-charset")
        .arg("filename=UTF8")
        .arg("-@")
        .arg(&argfile)
        .output();
    let _ = std::fs::remove_file(&argfile);

    let out = match out {
        Ok(o) => o,
        Err(_) => return map,
    };
    if !out.status.success() {
        return map;
    }
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(x) => x,
        Err(_) => return map,
    };
    if let Some(arr) = v.as_array() {
        for obj in arr {
            let src = match obj.get("SourceFile").and_then(|x| x.as_str()) {
                Some(s) => s,
                None => continue,
            };
            for key in ["DateTimeOriginal", "CreateDate", "FileModifyDate"] {
                if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                    map.insert(src.to_string(), s.to_string());
                    break;
                }
            }
        }
    }
    map
}

/// 一批文件的元数据深度复制：整批**只启动一次** exiftool。
///
/// 参数文件里每个文件一盏指令块：
/// ```text
/// -TagsFromFile <src> -all:all -unsafe <dst>
/// ```
/// exiftool 按出现顺序逐块执行，效果与「一个文件调一次」完全一致，
/// 但省掉 N-1 次进程启动与 N-1 次参数文件读写（转换几百个视频时很明显）。
///
/// 整批失败会退回逐文件重试——避免「一个坏文件连累整批」。
pub fn copy_metadata_batch(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    if pairs.is_empty() {
        return Ok(());
    }
    let exep = exiftool_path().ok_or_else(|| "未找到 exiftool".to_string())?;
    if let Err(first) = run_copy_once(&exep, pairs) {
        if pairs.len() == 1 {
            return Err(first);
        }
        let mut bad = 0;
        for p in pairs {
            if run_copy_once(&exep, std::slice::from_ref(p)).is_err() {
                bad += 1;
            }
        }
        if bad > 0 {
            return Err(format!(
                "{first}；逐文件重试后仍有 {bad}/{} 个失败",
                pairs.len()
            ));
        }
        // 逐个都过了：批量那次只是被个别文件拖累，结果已经修好
    }
    Ok(())
}

/// 执行一批 `-TagsFromFile` 复制（一个 exiftool 进程）。
///
/// ★ 必须用 `-execute` 分隔每个文件，不能把多个 `-TagsFromFile` 块直接串起来。
///
/// exiftool 是「先把整条命令行的选项全部解析完，再统一处理所有 FILE 参数」，
/// 所以这种写法：
/// ```text
/// -TagsFromFile a.mp4 -all:all ao.mp4 -TagsFromFile b.mp4 -all:all bo.mp4
/// ```
/// 会让 `ao.mp4` 和 `bo.mp4` **都用最后一个源（b.mp4）** 的元数据 —— 实测
/// 两个输出都拿到了 b 的时间，而且退出码是 0，属于静默串味。
/// `-execute` 才是官方给「一条命令行跑多个独立命令」的手段，进程数仍然是 1。
fn run_copy_once(exep: &Path, pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let lines = batch_argfile_lines(pairs);
    // 走 UTF-8 参数文件，避免 Windows 命令行对 Unicode 路径的编码问题。
    // -charset 放在命令行上，对每个 -execute 分段都生效（已用中文名实测）。
    let argfile = temp_argfile_path("ptf_meta");
    std::fs::write(&argfile, lines.join("\n")).map_err(|e| format!("写入参数文件失败: {e}"))?;
    let out = Command::new(exep)
        .arg("-charset")
        .arg("filename=UTF8")
        .arg("-@")
        .arg(&argfile)
        .output();
    let _ = std::fs::remove_file(&argfile);
    let out = out.map_err(|e| format!("执行 exiftool 失败: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(last_line(&out.stderr))
    }
}

/// 拼出批量复制的参数文件内容（纯字符串，便于单测锁死 `-execute`）。
fn batch_argfile_lines(pairs: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (i, (src, dst)) in pairs.iter().enumerate() {
        if i > 0 {
            lines.push("-execute".into());
        }
        lines.push("-m".into()); // 忽略次要错误（少数 tag 不可写）
        lines.push("-overwrite_original".into());
        lines.push("-TagsFromFile".into());
        lines.push(src.to_string_lossy().to_string());
        lines.push("-all:all".into());
        lines.push("-unsafe".into());
        lines.push(dst.to_string_lossy().to_string());
    }
    lines
}

/// 取 stderr 最后一条非空行：exiftool 的诊断信息在末尾，前面多是进度噪音。
fn last_line(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("未知错误")
        .trim()
        .to_string()
}

fn temp_argfile_path(prefix: &str) -> PathBuf {
    // pid + 毫秒：同一进程里连续调用不会撞名（读、写、批量复制都用这一个函数）。
    // 只用 pid 时，并行化或连续调用会互相覆盖参数文件。
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{}_{}_{}.args", prefix, std::process::id(), ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// ★ 回归锁：每个文件必须用 `-execute` 单独分段。
    ///
    /// 曾经把多个 `-TagsFromFile` 块直接串在一条命令里 —— exiftool 先解析完全部
    /// 选项、再统一处理所有文件参数，结果**所有输出都用了最后一个源的元数据**，
    /// 而且退出码是 0。真机实测踩过这个坑（两个输出拿到同一个时间），
    /// 这条测试防止有人把它「简化」回去。
    #[test]
    fn batch_argfile_separates_each_file_with_execute() {
        let lines = batch_argfile_lines(&[
            (p(r"D:\a.mp4"), p(r"D:\out\a.mp4")),
            (p(r"D:\b.mp4"), p(r"D:\out\b.mp4")),
        ]);
        let n_exec = lines.iter().filter(|l| l.as_str() == "-execute").count();
        assert_eq!(n_exec, 1, "两个文件之间必须恰好一个 -execute");

        let pos = lines.iter().position(|l| l == "-execute").unwrap();
        assert_eq!(
            lines[pos - 1],
            r"D:\out\a.mp4",
            "-execute 要跟在上一个目标后面"
        );
        assert_eq!(lines[pos + 1], "-m", "-execute 后面是下一段的选项");

        // 每段都有自己的 -TagsFromFile，且源不能串
        let tags: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| lines[*i].as_str() == "-TagsFromFile")
            .map(|(i, _)| lines[i + 1].as_str())
            .collect();
        assert_eq!(tags, vec![r"D:\a.mp4", r"D:\b.mp4"]);
    }

    #[test]
    fn single_file_batch_has_no_execute() {
        let lines = batch_argfile_lines(&[(p("a.mp4"), p("b.mp4"))]);
        assert!(!lines.iter().any(|l| l == "-execute"));
        assert_eq!(lines.first().map(|s| s.as_str()), Some("-m"));
        assert_eq!(lines.last().map(|s| s.as_str()), Some("b.mp4"));
    }

    /// 参数文件名带毫秒：同进程内连续两次调用不能撞名。
    #[test]
    fn argfile_names_are_unique_enough() {
        let a = temp_argfile_path("ptf_t");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = temp_argfile_path("ptf_t");
        assert_ne!(a, b);
    }
}
