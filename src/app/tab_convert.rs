//! 「视频格式转换」页的渲染。

use std::sync::atomic::Ordering;

use eframe::egui;

use crate::ffmpeg::ConvertMode;
use crate::files::MediaKind;

use super::PhotoTimeFixer;

impl PhotoTimeFixer {
    pub(super) fn render_convert(&mut self, ui: &mut egui::Ui) {
        // 文件夹选择（与时间页共享 folder/recursive，可独立触发扫描）
        ui.horizontal(|ui| {
            ui.label("文件夹:");
            ui.text_edit_singleline(&mut self.folder);
            if ui.button("选择文件夹…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.folder = dir.to_string_lossy().to_string();
                    self.rescan();
                }
            }
            ui.checkbox(&mut self.recursive, "包含子文件夹");
            if ui
                .add_enabled(!self.scanning, egui::Button::new("刷新扫描"))
                .clicked()
            {
                self.rescan();
            }
            if self.scanning {
                ui.label("扫描中…");
            }
        });
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("目标格式:");
            egui::ComboBox::from_id_salt("convert_ext_combo")
                .selected_text(self.convert_ext.clone())
                .width(160.0)
                .show_ui(ui, |ui| {
                    for ext in ["mp4", "mkv", "mov", "avi", "webm", "flv", "m4v"] {
                        ui.selectable_value(&mut self.convert_ext, ext.to_string(), ext);
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("转换方式:");
            egui::ComboBox::from_id_salt("convert_mode_combo")
                .selected_text(self.convert_mode.label().to_string())
                .width(240.0)
                .show_ui(ui, |ui| {
                    for m in [
                        ConvertMode::MaxCompat,
                        ConvertMode::Reencode,
                        ConvertMode::Copy,
                    ] {
                        ui.selectable_value(&mut self.convert_mode, m, m.label());
                    }
                });
        });
        // 视频码率：指定码率可大幅压小文件（内窥镜录像常见码率虚高）
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.convert_bitrate_enabled, "指定视频码率");
            ui.add_enabled_ui(self.convert_bitrate_enabled, |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.convert_bitrate_kbps)
                        .speed(50.0)
                        .range(100u32..=50000)
                        .suffix(" kbps"),
                );
            });
        });
        // 元数据：默认开启，转换后不丢 EXIF / 拍摄时间 / 设备信息
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.convert_keep_metadata,
                "保留元数据（EXIF / 拍摄时间 / 设备信息，不丢失）",
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.convert_overwrite, "覆盖已存在的输出文件");
        });
        ui.horizontal(|ui| {
            ui.label("输出目录(留空=视频文件夹\\out):");
            ui.text_edit_singleline(&mut self.convert_out_dir);
            if ui.button("选择目录…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.convert_out_dir = dir.to_string_lossy().to_string();
                }
            }
        });

        let video_count = self
            .files
            .iter()
            .filter(|f| f.kind == MediaKind::Video)
            .count();
        ui.label(format!("当前文件夹视频数量: {video_count}"));

        ui.horizontal(|ui| {
            let btn_size = egui::vec2(140.0, 36.0);
            let convert_btn =
                egui::Button::new(egui::RichText::new("开始转换").size(15.0).strong())
                    .min_size(btn_size);
            if ui.add_enabled(!self.converting, convert_btn).clicked() {
                self.start_convert();
            }
            if !self.ffmpeg_installed {
                ui.colored_label(egui::Color32::from_rgb(200, 80, 60), "未安装 ffmpeg");
            }
        });

        self.render_convert_progress(ui);

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("日志:");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    self.log.clear();
                }
            });
        });
        egui::ScrollArea::vertical()
            .max_height(40.0)
            .show(ui, |ui| {
                for l in &self.log {
                    ui.label(l);
                }
            });
    }

    fn render_convert_progress(&mut self, ui: &mut egui::Ui) {
        if !self.converting && self.progress_total == 0 {
            return;
        }
        let total = self.progress_total.max(1);
        let frac = self.progress_done as f32 / total as f32;
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            ui.label(format!("{}/{}", self.progress_done, self.progress_total));
            if self.converting && ui.button("取消").clicked() {
                // 置位后：正在跑的 ffmpeg 会被杀掉（未完成的产物自动删除），
                // 后面的文件不再开工。
                self.convert_cancel.store(true, Ordering::Relaxed);
                self.log("已请求取消，正在中止当前视频…".into());
            }
        });
        if !self.progress_current.is_empty() {
            ui.label(format!("正在转换: {}", self.progress_current));
        }
    }
}
