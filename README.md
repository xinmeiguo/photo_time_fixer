# photo_time_fixer

批量修改照片 / 录像的时间属性，并可批量转换、压缩视频格式。Windows 桌面小工具，单文件 exe，无需安装。

## 功能

### 1. 修改时间

- **拍摄时间（元数据）**：照片写 `DateTimeOriginal / CreateDate / ModifyDate`；视频写 `CreateDate / ModifyDate`（`-api QuickTimeUTC`）。走 exiftool。
- **文件时间**：创建时间、修改时间（NTFS 层），直接调 `SetFileTime`。
- **绝对起始时间**：填一个起始时刻，之后按「每张间隔 N 分钟」依次递增，可选附加 0~59 随机秒。
- **整体偏移**：对每张文件已有的拍摄时间/修改时间整体加减，用于批量校正时差。
- **时间格式不限**：`2026-10-19 16:30`、`2026/10/19 16点30分`、`20261019 163000`、`下午4:30`、全角字符、RFC3339 等都能识别，界面上实时显示"识别结果"。
- **预览 + 备份 + 恢复**：应用前生成 `undo_<时间戳>.json`，可从界面「恢复…」一键回滚。恢复完成会自动重新扫描，预览表里的原时间立即刷新。
- **批量处理可随时取消**：进度条旁的「取消」按钮生效于当前文件处理完之后，已改过的文件不回滚（要回滚请用备份）。

### 2. 视频格式转换

- 目标格式：mp4 / mkv / mov / avi / webm / flv / m4v。
- 三种方式：
  - **最大兼容**（默认）：H.264 High + yuv420p 8bit + 偶数宽高 + AAC 48kHz 立体声。专治 Windows 自带播放器放不了的内窥镜/监控录像（HEVC、4:2:2、10bit、奇数分辨率、怪异声道）。
  - **重编码**：H.264 + AAC，跨格式转换用这个。
  - **仅封装**：`-c copy`，最快且不损画质，但要求编码与目标容器兼容（如 mp4 ↔ mkv）。
- **指定视频码率**（kbps）压小文件；不指定则用 CRF 20 自动质量。
- **元数据不丢**：ffmpeg `-map_metadata` + exiftool `-TagsFromFile` 双重复制；整批文件只启动**一次** exiftool（不是每个文件一次）。
- **文件时间不变成"现在"**：转换完成后把源文件的创建/修改/访问时间原样写回输出文件。
- 默认输出到 `<所选文件夹>\out`，处理前会验证目录可写（网络共享无权限会提前提示）。
- 递归扫描时输出目录内的文件会被排除，避免重复转换。
- **可中途取消**：取消后正在跑的 ffmpeg 会被立即终止，未完成的输出文件自动删除，后面的文件不再开工。

## 依赖的第三方工具

两个工具都是**可选的**，界面顶部有对应状态灯：

| 工具 | 用途 | 缺失时 |
| --- | --- | --- |
| [exiftool](https://exiftool.org/) | 读写照片/视频元数据 | 点「下载安装 exiftool」，自动下载解压到 exe 同级 `tools/` |
| [ffmpeg](https://ffmpeg.org/)（gyan.dev 构建） | 视频格式转换 | 点「下载安装 ffmpeg」，同上 |

也可以自己装好放进 PATH，程序会优先用 exe 同级 `tools/`，其次 PATH，最后常见安装位置。

下载时会显示「已下载 / 总大小」进度条（分块流式读取，不是干等），失败自动重试 3 次（gyan.dev 偶发 502 基本一次重试就过）。收不够 Content-Length 会判为截断，直接报错而不是交给解压器报"文件末端损坏"。

## 构建

要求 Rust 1.88+ 与 **GNU 工具链**（`x86_64-pc-windows-gnu`，由 `rust-toolchain.toml` 锁定），另需 mingw 的 `windres`（编 Windows 资源节）。

```bash
# Windows（需把 mingw 的 bin 目录放进 PATH）
cargo build --release
```

产物：`target/release/photo_time_fixer.exe`（约 8 MB，静态链接 mingw 运行时，只依赖系统 DLL）。

发布配置按体积优先（`Cargo.toml` 的 `[profile.release]`）：`opt-level = "s"` + 全量 LTO + `codegen-units = 1` + `strip = true`。
代价是链接时间明显变长（约 10 分钟），但产物从 28 MB 降到 8 MB。

> 项目路径含中文时，建议把 target 放到纯 ASCII 目录：
> `CARGO_TARGET_DIR=D:/ptf-target cargo build --release`

## 下载

**[下载最新版 photo_time_fixer.exe](https://github.com/xinmeiguo/photo_time_fixer/releases/latest)**

编译好的 exe **不随仓库提交**（约 28 MB，会让仓库臃肿），请从 Releases 下载。
双击即可运行，无需安装任何运行库。

首次使用时，如果电脑没有 exiftool / ffmpeg，点界面顶部的对应按钮即可自动下载到程序同级的 `tools/` 目录。

## 目录结构

```
src/
├── main.rs        入口：窗口尺寸、窗口图标、设计系统初始化
├── app/           egui 界面（按职责拆分）
│   ├── mod.rs         状态定义、面板骨架、备份失败弹窗
│   ├── state.rs       界面状态类型、后台消息枚举、路径小工具
│   ├── poll.rs        后台线程消息回收（唯一一处 try_recv）
│   ├── actions.rs     用户动作：扫描 / 应用 / 转换 / 下载
│   ├── tab_time.rs    「修改时间」页渲染
│   └── tab_convert.rs 「视频格式转换」页渲染
├── apply.rs       时间计划计算、文件系统时间写入
├── datetime.rs    任意格式的时间解析
├── download.rs    HTTP 流式下载（进度回调 + 失败重试 + 截断校验）
├── exiftool.rs    exiftool 检测 / 下载 / 元数据读写（支持整批一次复制）
├── ffmpeg.rs      ffmpeg 检测 / 下载 / 转码与批量收尾
├── files.rs       目录扫描与照片/视频分类
├── backup.rs      修改前备份与恢复
└── ds.rs          egui_sauge 设计系统接入（字体表、主题、字号层级）
assets/
├── icon.ico           多尺寸应用图标
├── icon_128.rgba      窗口图标原始像素（128×128，避免引入图像解码库）
└── app.rc             Windows 资源脚本（图标 + 版本信息）
vendor/
└── egui_sauge/    内置的设计系统源码（Apache-2.0，见其 LICENSE）
```

## 说明

- 界面风格基于 [egui_sauge](https://github.com/macnjil4/egui_sauge) 设计系统。
- 所有时间写操作都会**先备份再执行**，失败项在日志区逐条列出。
- 视频转换**不覆盖原文件**（输出到 `out/`）；若目标格式与原格式相同，输出名自动加 `_converted` 后缀。
