mod app;
mod apply;
mod backup;
mod datetime;
mod download;
mod ds;
mod exiftool;
mod ffmpeg;
mod files;

use eframe::egui;

/// 读取窗口图标（128×128 RGBA）。
///
/// 为什么要单独做这件事：exe 资源节里的图标只决定**文件**在资源管理器里长什么样；
/// 任务管理器「应用」列表显示的是**窗口图标**（`WM_SETICON`），而 eframe 默认
/// 会把自己的 egui 图标挂上去，所以任务栏/任务管理器里一直是 egui 的 logo。
/// 通过 `with_icon` 显式指定，才能整体统一成我们的图标。
fn load_icon() -> egui::IconData {
    // assets/icon_128.rgba：8 字节头（宽、高各 u32 小端）+ RGBA 像素
    const RAW: &[u8] = include_bytes!("../assets/icon_128.rgba");
    let w = u32::from_le_bytes([RAW[0], RAW[1], RAW[2], RAW[3]]);
    let h = u32::from_le_bytes([RAW[4], RAW[5], RAW[6], RAW[7]]);
    egui::IconData {
        rgba: RAW[8..].to_vec(),
        width: w,
        height: h,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // 参照 wrepl_GUI 的窗口规格（1180×820 / min 940×640），
            // 本工具内容略少，取稍收敛的一档，避免右侧留白过多。
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([900.0, 620.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "照片/视频时间修改工具",
        options,
        Box::new(|cc| {
            // 与 wrepl_GUI 同一套设计系统：字体表 + 九档字号 + Palette 主题
            ds::install(&cc.egui_ctx);
            ds::apply(&cc.egui_ctx, true);
            Ok(Box::new(app::PhotoTimeFixer::new()))
        }),
    )
}
