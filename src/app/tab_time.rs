//! 「修改时间」页的渲染。只负责画控件，业务逻辑全在 `actions`。

use std::sync::atomic::Ordering;

use eframe::egui;

use crate::datetime;

use super::state::Mode;
use super::PhotoTimeFixer;

impl PhotoTimeFixer {
    pub(super) fn render_time(&mut self, ui: &mut egui::Ui) {
        // 文件夹
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

        // 模式
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.mode, Mode::Absolute, "绝对起始时间");
            ui.radio_value(&mut self.mode, Mode::Offset, "整体偏移 (校正时差)");
        });
        if self.mode == Mode::Absolute {
            ui.horizontal(|ui| {
                ui.label("起始日期时间(任意格式):");
                ui.text_edit_singleline(&mut self.time_input);
            });
            let parsed = datetime::parse_flexible(&self.time_input);
            ui.label(format!(
                "识别结果: {}",
                parsed
                    .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "无法识别".into())
            ));
            ui.horizontal(|ui| {
                ui.label("每张间隔(分钟):");
                if ui.button("−").clicked() {
                    self.interval_min = self.interval_min.saturating_sub(1);
                }
                ui.add(
                    egui::DragValue::new(&mut self.interval_min)
                        .speed(1.0)
                        .range(0u32..=59)
                        .fixed_decimals(0),
                );
                if ui.button("+").clicked() {
                    self.interval_min = (self.interval_min + 1).min(59);
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.random, "随机秒 (0~59)");
            });
        } else {
            ui.horizontal(|ui| {
                ui.label("整体偏移:");
                ui.add(egui::Slider::new(&mut self.offset_hours, -24..=24i32).step_by(1.0));
                ui.label("时");
                ui.add(egui::Slider::new(&mut self.offset_mins, -59..=59i32).step_by(1.0));
                ui.label("分 (负值为提前)");
            });
            ui.label("说明：对每张文件“原始拍摄时间/修改时间”整体加减该偏移，常用于整批时差校正。");
        }
        ui.separator();

        // 时区 + 写入范围
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("时区")
                .selected_text(self.tz_label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.tz_hours, 8, "UTC+8 北京");
                    ui.selectable_value(&mut self.tz_hours, 9, "UTC+9 东京");
                    ui.selectable_value(&mut self.tz_hours, 0, "UTC+0");
                    ui.selectable_value(&mut self.tz_hours, -5, "UTC-5 纽约");
                    ui.selectable_value(&mut self.tz_hours, -8, "UTC-8 洛杉矶");
                    ui.selectable_value(&mut self.tz_hours, i32::MIN, "跟随系统");
                });
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.set_taken, "拍摄时间(元数据)");
            ui.checkbox(&mut self.set_created, "文件创建时间");
            ui.checkbox(&mut self.set_modified, "修改时间");
        });
        ui.separator();

        self.render_time_buttons(ui);
        self.render_preview_table(ui);
        ui.separator();
        self.render_apply_progress(ui);

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

    fn render_time_buttons(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let btn_size = egui::vec2(140.0, 36.0);

            let preview_btn =
                egui::Button::new(egui::RichText::new("预览新时间").size(15.0).strong())
                    .min_size(btn_size);
            if ui.add(preview_btn).clicked() {
                if self.files.is_empty() {
                    self.log("请先选择文件夹并扫描".into());
                } else {
                    self.recompute();
                    if self.plan.is_none() && self.mode == Mode::Absolute {
                        self.log_time_parse_hint();
                    }
                }
            }

            let apply_btn = egui::Button::new(egui::RichText::new("应用修改").size(15.0).strong())
                .min_size(btn_size);
            if ui.add_enabled(!self.applying, apply_btn).clicked() {
                self.do_apply();
            }

            let restore_btn = egui::Button::new(egui::RichText::new("恢复…").size(15.0).strong())
                .min_size(btn_size);
            if ui.add(restore_btn).clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("备份", &["json"])
                    .pick_file()
                {
                    self.do_restore(&file);
                }
            }
        });
    }

    fn render_preview_table(&mut self, ui: &mut egui::Ui) {
        let Some(plan) = &self.plan else {
            ui.label("（点击“预览新时间”查看计划，或先选择文件夹）");
            return;
        };
        ui.label(format!("共 {} 个文件（已按文件名排序）", plan.len()));
        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                egui::Grid::new("preview")
                    .striped(true)
                    .min_col_width(120.0)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.strong("文件名");
                        ui.strong("原时间");
                        ui.strong("新时间(预览)");
                        ui.strong("类型");
                        ui.end_row();
                        for item in plan {
                            let new = item.new_time.format("%Y-%m-%d %H:%M:%S").to_string();
                            ui.label(&item.file.name);
                            ui.label(item.file.original_display());
                            ui.label(new);
                            ui.label(item.file.kind.label());
                            ui.end_row();
                        }
                    });
            });
    }

    fn render_apply_progress(&mut self, ui: &mut egui::Ui) {
        if !self.applying && self.progress_total == 0 {
            return;
        }
        let total = self.progress_total.max(1);
        let frac = self.progress_done as f32 / total as f32;
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            ui.label(format!("{}/{}", self.progress_done, self.progress_total));
            if self.applying && ui.button("取消").clicked() {
                // 只置标志，真正的停止点在线程的每轮循环开头
                self.apply_cancel.store(true, Ordering::Relaxed);
                self.log("已请求取消，当前文件处理完后停止…".into());
            }
        });
        if !self.progress_current.is_empty() {
            ui.label(format!("正在处理: {}", self.progress_current));
        }
    }
}
