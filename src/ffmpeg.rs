//! ffmpeg 检测、自动下载安装与视频格式批量转换。
//! 检测/下载模式与 exiftool 保持一致：优先工具自带 tools 目录，其次 PATH，再次常见安装位置。

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 转换模式：仅封装(不重编码) 或 重新编码(H.264/AAC)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConvertMode {
    /// -c copy：最快，仅改容器，不损画质；部分格式组合不支持。
    Copy,
    /// -c:v libx264 -c:a aac：通用重编码，兼容性好。
    Reencode,
    /// 最大兼容：H.264 High + yuv420p 8bit + 偶数宽高 + AAC 48kHz 立体声。
    /// 专治「Windows 自带播放器放不了」——内窥镜设备常见成因是
    /// HEVC、4:2:2、10bit、奇数分辨率、怪异声道/采样率。
    MaxCompat,
}

impl ConvertMode {
    pub fn label(&self) -> &'static str {
        match self {
            ConvertMode::Copy => "仅封装(不重编码, 最快)",
            ConvertMode::Reencode => "重编码(H.264/AAC, 兼容好)",
            ConvertMode::MaxCompat => "最大兼容(修复 Windows 播放器放不了)",
        }
    }
}

/// 一次转换的全部参数。
#[derive(Clone)]
pub struct ConvertSpec {
    /// 目标扩展名，如 "mp4"
    pub target_ext: String,
    pub mode: ConvertMode,
    /// 指定视频码率（kbps）。None = 用 CRF 自动质量模式。
    pub video_bitrate_kbps: Option<u32>,
    /// 是否保留源文件全部元数据（EXIF / 拍摄时间 / 设备信息 / GPS 等）。
    pub keep_metadata: bool,
    /// 输出已存在时是否覆盖。
    pub overwrite: bool,
}

/// 返回 ffmpeg 可执行文件路径（可能是命令名 "ffmpeg" 或具体路径）。
pub fn ffmpeg_path() -> Option<PathBuf> {
    // 1. 工具自带（与 exe 同级的 tools 目录）
    if let Some(dir) = exe_dir() {
        let p = dir.join("tools").join("ffmpeg.exe");
        if p.exists() {
            return Some(p);
        }
    }
    // 2. PATH 中可直接调用
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("ffmpeg"));
    }
    // 3. 常见安装位置
    for cand in [
        r"C:\ffmpeg\bin\ffmpeg.exe",
        r"C:\Program Files\ffmpeg\bin\ffmpeg.exe",
        r"C:\Program Files (x86)\ffmpeg\bin\ffmpeg.exe",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// 读取 ffmpeg 版本号（用于界面状态灯展示）。
pub fn version(path: &Path) -> Option<String> {
    let out = Command::new(path)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = s.lines().next() {
            // 首行形如：ffmpeg version 9.0.1-essentials_build-www.gyan.dev Copyright (c) ...
            // 只要版本号本身：取 "version" 后的第一个 token，再去掉构建后缀。
            if let Some(idx) = line.find("version") {
                let rest = line[idx + "version".len()..].trim();
                let tok = rest.split_whitespace().next().unwrap_or("");
                let ver = tok.split("-essentials_build").next().unwrap_or(tok);
                let ver = ver.trim();
                if !ver.is_empty() {
                    return Some(ver.to_string());
                }
            }
            // 兜底：退化为第一个 token（如 "N-109000-g..." 这类自制构建）
            return line
                .split_whitespace()
                .nth(2)
                .map(|t| t.to_string())
                .or_else(|| Some(line.trim().to_string()));
        }
    }
    None
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
}

/// 从 gyan.dev 下载 Windows 版 essentials 构建并解压到 <dir>/tools/。
/// ffmpeg 是动态链接版，运行时依赖同目录的 av*.dll 等，因此需把整个 bin/ 目录
/// （ffmpeg.exe + 依赖 dll）一并解压到 tools/，否则运行会缺库失败。
/// 返回解压后的 exe 路径。
pub fn download_and_install(dir: &Path) -> Result<PathBuf, String> {
    let url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
    let bytes = reqwest::blocking::get(url)
        .map_err(|e| format!("下载失败: {e}"))?
        .bytes()
        .map_err(|e| format!("读取下载内容失败: {e}"))?;

    let tools_dir = dir.join("tools");
    std::fs::create_dir_all(&tools_dir).map_err(|e| format!("创建目录失败: {e}"))?;

    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("解压失败: {e}"))?;

    let mut found = false;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = file.name().to_string();
        // 只提取 bin/ 目录下的文件（ffmpeg.exe 及其依赖 dll）
        if !name.contains("/bin/") {
            continue;
        }
        let rel = name.rsplit('/').next().unwrap_or(&name).to_string();
        if rel.is_empty() || file.is_dir() {
            continue;
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(|e| e.to_string())?;
        let out = tools_dir.join(&rel);
        std::fs::write(&out, &content).map_err(|e| e.to_string())?;
        if rel.eq_ignore_ascii_case("ffmpeg.exe") {
            found = true;
        }
    }

    if !found {
        return Err("解压后未找到 ffmpeg.exe".into());
    }
    Ok(tools_dir.join("ffmpeg.exe"))
}

/// 归一化为可比较的路径字符串（绝对化 + 统一分隔符 + 小写）。
/// Windows 路径大小写不敏感，且 exe 里可能出现 `\` 与 `/` 混用，故统一处理。
fn norm_path(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    abs.to_string_lossy()
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 判断两个路径是否指向同一个文件。
fn is_same_path(a: &Path, b: &Path) -> bool {
    norm_path(a) == norm_path(b)
}

/// 计算单个文件的输出路径（同目录或指定输出目录下、换扩展名）。
///
/// 若目标扩展名与源文件相同（如 mp4 → mp4，只想改码率），换扩展名得到的路径
/// 就是输入自身。ffmpeg 拒绝“输出写到输入自身”，且中途失败会毁掉原文件，
/// 因此这种情况自动改名为 `<原名>_converted<扩展名>`。
pub fn output_path(input: &Path, target_ext: &str, out_dir: Option<&Path>) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    let dir = match out_dir {
        Some(d) => d.to_path_buf(),
        None => input.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
    };
    let mut out = dir.join(format!("{stem}.{target_ext}"));
    if is_same_path(&out, input) {
        out = dir.join(format!("{stem}_converted.{target_ext}"));
    }
    out
}

/// 转换单个文件。
/// 返回 `Ok(元数据状态描述)` / `Err(错误)`。
///
/// 元数据保留做**两道**：
/// 1. ffmpeg 侧 `-map_metadata 0`（全局+流级元数据、章节整体搬运）；
/// 2. 转换后用 exiftool `-TagsFromFile` 把源文件**全部 tag** 深度复制过去。
///    第 2 道是“EXIF 一点都不丢”的关键——ffmpeg 的 mp4 muxer 会丢掉一部分非标准 tag。
pub fn convert_one(
    ff: &Path,
    input: &Path,
    output: &Path,
    spec: &ConvertSpec,
) -> Result<String, String> {
    // 兜底：绝不允许把输出写到输入自身。ffmpeg 会拒绝执行，
    // 更糟的是若中途失败会留下被截断的原文件。
    if is_same_path(output, input) {
        return Err(
            "输出路径与原文件相同（目标格式与原格式一致），请换一个目标格式或指定输出目录".into(),
        );
    }
    if output.exists() && !spec.overwrite {
        return Err("输出文件已存在（未勾选覆盖）".into());
    }

    let mut cmd = Command::new(ff);
    cmd.arg("-y").arg("-i").arg(input).stdin(Stdio::null());

    // ── 第一道：让 ffmpeg 把输入的元数据与章节整体搬到输出 ──
    if spec.keep_metadata {
        cmd.arg("-map_metadata")
            .arg("0")
            .arg("-map_metadata:s:v")
            .arg("0:s:v")
            .arg("-map_metadata:s:a")
            .arg("0:s:a")
            .arg("-map_chapters")
            .arg("0");
    }

    match spec.mode {
        ConvertMode::Copy => {
            // 仅封装：不重编码，最快；但要求源编码与目标容器兼容，
            // 否则会生成“容器换了、编码放不出”的文件。
            cmd.arg("-c").arg("copy");
        }
        ConvertMode::Reencode => {
            // 重编码为 H.264 + AAC：兼容性最好，标准播放器都能放。
            cmd.arg("-c:v")
                .arg("libx264")
                .arg("-preset")
                .arg("fast")
                // yuv420p 是标准播放器/Windows 媒体播放器能解码的像素格式，
                // 缺少它会出现“能生成但放不出来”的问题。
                .arg("-pix_fmt")
                .arg("yuv420p");
            // 码率：指定了目标码率就按它压（文件大小可控），否则走 CRF 质量模式
            match spec.video_bitrate_kbps {
                Some(kbps) => {
                    cmd.arg("-b:v").arg(format!("{kbps}k"));
                }
                None => {
                    cmd.arg("-crf").arg("20");
                }
            }
            cmd.arg("-c:a").arg("aac").arg("-b:a").arg("192k");
        }
        ConvertMode::MaxCompat => {
            // 目标：Windows「电影和电视」/ Media Player 双击就能播。
            // 下面几项缺一不可：
            //   · H.264 High —— 不用 HEVC（Win 没装扩展直接放不了）
            //   · yuv420p 8bit —— 4:2:2 / 10bit 播放器不认
            //   · 偶数宽高 —— yuv420p 的硬要求，内窥镜常见 961×721 这类奇数
            //   · AAC 48kHz 立体声 —— 怪异声道布局/采样率会导致有画无声或压根不出画
            cmd.arg("-c:v")
                .arg("libx264")
                .arg("-preset")
                .arg("medium")
                .arg("-profile:v")
                .arg("high")
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-vf")
                .arg("scale=trunc(iw/2)*2:trunc(ih/2)*2");
            match spec.video_bitrate_kbps {
                Some(kbps) => {
                    cmd.arg("-b:v").arg(format!("{kbps}k"));
                }
                None => {
                    cmd.arg("-crf").arg("20");
                }
            }
            cmd.arg("-c:a")
                .arg("aac")
                .arg("-b:a")
                .arg("128k")
                .arg("-ac")
                .arg("2")
                .arg("-ar")
                .arg("48000");
        }
    }

    // mp4/mov/m4v 索引前置；保留元数据时额外打开 use_metadata_tags，
    // 否则 mp4 muxer 会丢弃非标准 tag。
    let e = spec.target_ext.to_lowercase();
    if e == "mp4" || e == "mov" || e == "m4v" {
        if spec.keep_metadata {
            cmd.arg("-movflags").arg("+faststart+use_metadata_tags");
        } else {
            cmd.arg("-movflags").arg("+faststart");
        }
    }
    cmd.arg(output);

    let out = cmd.output().map_err(|e| format!("执行 ffmpeg 失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // 优先取含关键词的错误行，其次取最后一行非空内容
        let key = err
            .lines()
            .find(|l| {
                l.contains("Error")
                    || l.contains("not supported")
                    || l.contains("Could not find tag")
                    || l.contains("Invalid")
            })
            .or_else(|| err.lines().filter(|l| !l.trim().is_empty()).last())
            .unwrap_or("未知错误")
            .trim()
            .to_string();
        return if spec.mode == ConvertMode::Copy {
            Err(format!(
                "{key}（源编码可能与目标格式不兼容，建议改用『重编码』方式）"
            ))
        } else {
            Err(format!("ffmpeg 报错: {key}"))
        };
    }

    // ── 第二道：exiftool 深度复制全部元数据 ──
    let mut status = if !spec.keep_metadata {
        String::new()
    } else if crate::exiftool::exiftool_path().is_none() {
        "元数据：已由 ffmpeg 保留（未装 exiftool，未做深度复制）".to_string()
    } else {
        match copy_metadata_from_source(input, output) {
            Ok(()) => "元数据：已完整保留（ffmpeg + exiftool）".to_string(),
            Err(e) => format!("元数据：ffmpeg 已保留，exiftool 深度复制失败({e})"),
        }
    };

    // ── 第三道：还原文件系统时间戳（创建 / 修改 / 访问）──
    // ffmpeg 写新文件、exiftool 的 -overwrite_original 重写文件，都会把
    // 修改时间刷成“现在”。必须放在元数据复制**之后**，否则前脚设好后脚又被覆盖。
    if !status.is_empty() {
        status.push_str("；");
    }
    match copy_fs_times(input, output) {
        Ok(()) => status.push_str("文件时间：已还原为原文件时间"),
        Err(e) => status.push_str(&format!("文件时间还原失败({e})")),
    }
    Ok(status)
}

/// 把源文件的文件系统时间戳（创建 / 修改 / 访问）复制到目标文件。
/// 注意：这是 NTFS 层面的文件时间，与 EXIF 里的“拍摄时间”是两回事。
fn copy_fs_times(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("读取源文件时间失败: {e}"))?;
    let modified = meta
        .modified()
        .map_err(|e| format!("取修改时间失败: {e}"))?;
    // 创建/访问时间在新式文件系统上一般都有，取不到就退回修改时间
    let accessed = meta.accessed().unwrap_or(modified);
    let created = meta.created().unwrap_or(modified);
    crate::apply::set_fs_times(dst, created, accessed, modified)
        .map_err(|e| format!("设置文件时间失败: {e}"))
}

/// 用 exiftool 把 `src` 的全部元数据复制到 `dst`
/// （拍摄时间、设备型号、GPS、自定义标签等一并保留）。
fn copy_metadata_from_source(src: &Path, dst: &Path) -> Result<(), String> {
    let exep = crate::exiftool::exiftool_path().ok_or_else(|| "未找到 exiftool".to_string())?;
    // 走 UTF-8 参数文件，避免 Windows 命令行对 Unicode 路径的编码问题
    let lines = [
        "-m".to_string(), // 忽略次要错误（少数 tag 不可写）
        "-overwrite_original".to_string(),
        "-TagsFromFile".to_string(),
        src.to_string_lossy().to_string(),
        "-all:all".to_string(),
        "-unsafe".to_string(),
        dst.to_string_lossy().to_string(),
    ];
    let argfile = std::env::temp_dir().join(format!(
        "ptf_meta_{}_{}.args",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::write(&argfile, lines.join("\n")).map_err(|e| format!("写入参数文件失败: {e}"))?;
    let out = Command::new(&exep)
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
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .last()
            .unwrap_or("未知错误")
            .trim()
            .to_string())
    }
}
