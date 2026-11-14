use super::{App, Commands};
use crate::{
    appearance::{Language, Text, ThemeMode},
    color::{InkColor, PRESETS, Space},
    frame_rate::FrameRate,
    icc::ColorManager,
    model::Tool,
    sdl::Renderer,
    toolbar::{self, Icon, Panel},
};
use egui::{Align, Layout, RichText, Ui, vec2};

const DIRECT_TOOLS: [Tool; 4] = [Tool::Pen, Tool::Pencil, Tool::Marker, Tool::Eraser];

impl App {
    pub(super) fn ui(
        &mut self,
        ui: &mut Ui,
        render: &Renderer,
        color: &ColorManager,
        video: &str,
        freeze_unavailable: Option<Text>,
        mark: egui::TextureId,
    ) -> Commands {
        let mut commands = Commands::default();
        let tr = self.language;
        let available = ui.available_size();
        let opened = self.panel.filter(|_| !self.picking);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.;
            ui.allocate_ui_with_layout(
                vec2(toolbar::WIDTH, available.y),
                Layout::top_down(Align::Center),
                |ui| self.rail(ui, mark, &mut commands),
            );
            if let Some(panel) = opened {
                let width = (available.x - toolbar::WIDTH).max(0.);
                if width > 48. {
                    ui.allocate_ui_with_layout(
                        vec2(width, available.y),
                        Layout::top_down(Align::Min),
                        |ui| {
                            egui::Frame::new()
                                .fill(ui.visuals().window_fill())
                                .inner_margin(egui::Margin::same(13))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = vec2(8., 7.);
                                    ui.set_width((width - 26.).max(0.));
                                    ui.set_min_height((available.y - 26.).max(0.));
                                    ui.horizontal(|ui| {
                                        ui.heading(panel.title(tr));
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if toolbar::button(
                                                    ui,
                                                    Icon::Close,
                                                    tr.text("Close panel", "关闭面板", "關閉面板"),
                                                    false,
                                                )
                                                .clicked()
                                                {
                                                    self.panel = None;
                                                }
                                            },
                                        );
                                    });
                                    ui.separator();
                                    ui.label(RichText::new(self.status.get(tr)).small().weak());
                                    egui::ScrollArea::vertical()
                                        .id_salt(panel)
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| match panel {
                                            Panel::Shapes => self.shapes(ui, &mut commands),
                                            Panel::Brush => self.brush(ui),
                                            Panel::Colors => self.palette(ui, &mut commands),
                                            Panel::Background => self.background_controls(
                                                ui,
                                                freeze_unavailable,
                                                &mut commands,
                                            ),
                                            Panel::Settings => self.settings(
                                                ui,
                                                render,
                                                color,
                                                video,
                                                &mut commands,
                                            ),
                                        });
                                });
                        },
                    );
                }
            }
        });
        commands
    }

    fn rail(&mut self, ui: &mut Ui, mark: egui::TextureId, commands: &mut Commands) {
        let tr = self.language;
        ui.spacing_mut().item_spacing = vec2(4., 2.);
        ui.add_space(4.);
        toolbar::hint(
            ui.add(egui::Image::new((mark, vec2(29., 29.)))),
            tr.text(
                "OverInk · drag to move to another display",
                "OverInk · 拖动切换显示器",
                "OverInk · 拖曳切換顯示器",
            ),
        );
        ui.add_space(4.);
        if self.picking {
            if toolbar::button(
                ui,
                Icon::Close,
                tr.text(
                    "Cancel screen color picker (Esc)",
                    "取消屏幕取色 (Esc)",
                    "取消螢幕取色 (Esc)",
                ),
                true,
            )
            .clicked()
            {
                commands.pick = true;
            }
        } else {
            self.rail_tools(ui, commands);
        }
        ui.add_space(2.);
        if toolbar::button(
            ui,
            Icon::Quit,
            tr.text("Quit OverInk", "退出 OverInk", "結束 OverInk"),
            false,
        )
        .clicked()
        {
            commands.quit = true;
        }
    }

    fn rail_tools(&mut self, ui: &mut Ui, commands: &mut Commands) {
        let tr = self.language;
        egui::ScrollArea::vertical()
            .id_salt("tool-rail")
            .max_height((ui.available_height() - 34.).max(0.))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let eye = if self.hidden { Icon::EyeOff } else { Icon::Eye };
                    let label = if self.hidden {
                        tr.text(
                            "Show drawing layer (H)",
                            "显示书写图层 (H)",
                            "顯示書寫圖層 (H)",
                        )
                    } else {
                        tr.text(
                            "Hide drawing layer (H)",
                            "隐藏书写图层 (H)",
                            "隱藏書寫圖層 (H)",
                        )
                    };
                    if toolbar::button(ui, eye, label, self.hidden).clicked() {
                        self.finish_all_contacts();
                        self.hidden = !self.hidden;
                        commands.mode = true;
                    }
                    if toolbar::button(
                        ui,
                        Icon::Pointer,
                        tr.text(
                            "Mouse · click desktop (Space)",
                            "鼠标 · 点击桌面（空格键）",
                            "滑鼠 · 點按桌面（空白鍵）",
                        ),
                        self.pass,
                    )
                    .clicked()
                    {
                        self.use_mouse();
                        commands.mode = true;
                    }
                    for tool in DIRECT_TOOLS {
                        if toolbar::button(
                            ui,
                            Icon::Tool(tool),
                            tool_label(tool, tr),
                            !self.pass && self.tool == tool,
                        )
                        .clicked()
                        {
                            self.use_tool(tool);
                            self.panel = None;
                            commands.mode = true;
                        }
                    }
                    let shape_selected = !self.pass && !DIRECT_TOOLS.contains(&self.tool);
                    let shapes = if shape_selected {
                        Icon::Tool(self.tool)
                    } else {
                        Icon::Shapes
                    };
                    if toolbar::button(
                        ui,
                        shapes,
                        tr.text(
                            "Lines, shapes and laser",
                            "线条、形状与激光笔",
                            "線條、圖形與雷射筆",
                        ),
                        shape_selected || self.panel == Some(Panel::Shapes),
                    )
                    .clicked()
                    {
                        self.toggle_panel(Panel::Shapes);
                    }
                    ui.separator();
                    self.panel_button(
                        ui,
                        Icon::Brush,
                        Panel::Brush,
                        tr.text(
                            "Brush width and opacity",
                            "笔刷粗细与不透明度",
                            "筆刷粗細與不透明度",
                        ),
                    );
                    let actions = [
                        (
                            Icon::Undo,
                            tr.text(
                                "Undo (Ctrl/Cmd+Z)",
                                "撤销 (Ctrl/Cmd+Z)",
                                "復原 (Ctrl/Cmd+Z)",
                            ),
                            crate::model::History::undo as fn(&mut crate::model::History),
                        ),
                        (
                            Icon::Redo,
                            tr.text(
                                "Redo (Ctrl/Cmd+Shift+Z)",
                                "重做 (Ctrl/Cmd+Shift+Z)",
                                "重做 (Ctrl/Cmd+Shift+Z)",
                            ),
                            crate::model::History::redo,
                        ),
                        (
                            Icon::Clear,
                            tr.text("Clear this page", "清除当前页面", "清除目前頁面"),
                            crate::model::History::clear,
                        ),
                    ];
                    for (index, (icon, label, action)) in actions.into_iter().enumerate() {
                        let enabled = self.history_buttons()[index];
                        if ui
                            .add_enabled_ui(enabled, |ui| toolbar::button(ui, icon, label, false))
                            .inner
                            .clicked()
                        {
                            self.reset_contacts();
                            action(&mut self.history);
                            commands.redraw_canvas = true;
                        }
                    }
                    self.panel_button(
                        ui,
                        Icon::Display,
                        Panel::Background,
                        tr.text("Desktop background", "桌面背景", "桌面背景"),
                    );
                    self.panel_button(
                        ui,
                        Icon::Settings,
                        Panel::Settings,
                        tr.text("Settings and files", "设置与文件", "設定與檔案"),
                    );
                    ui.separator();
                    for row in PRESETS.as_chunks::<2>().0 {
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - 40.) * 0.5).max(0.));
                            for rgb in row {
                                let color = InkColor::from_rgb8(*rgb);
                                if toolbar::swatch(
                                    ui,
                                    vec2(18., 18.),
                                    *rgb,
                                    &color.css(),
                                    self.color.srgb8() == *rgb,
                                )
                                .clicked()
                                {
                                    self.select_preset(*rgb);
                                }
                            }
                        });
                    }
                    let label = format!(
                        "{} · {}",
                        self.color.hex(),
                        tr.text(
                            "Palette and screen picker",
                            "色盘与屏幕取色",
                            "調色盤與螢幕取色",
                        )
                    );
                    if toolbar::swatch(
                        ui,
                        vec2(40., 22.),
                        self.color.srgb8(),
                        &label,
                        self.panel == Some(Panel::Colors),
                    )
                    .clicked()
                    {
                        self.toggle_panel(Panel::Colors);
                    }
                    ui.add_space(6.);
                });
            });
    }

    fn toggle_panel(&mut self, panel: Panel) {
        self.panel = (self.panel != Some(panel)).then_some(panel);
    }

    fn panel_button(&mut self, ui: &mut Ui, icon: Icon, panel: Panel, label: &str) {
        if toolbar::button(ui, icon, label, self.panel == Some(panel)).clicked() {
            self.toggle_panel(panel);
        }
    }

    fn shapes(&mut self, ui: &mut Ui, commands: &mut Commands) {
        let tr = self.language;
        ui.horizontal_wrapped(|ui| {
            for tool in Tool::ALL
                .into_iter()
                .filter(|tool| !DIRECT_TOOLS.contains(tool))
            {
                if toolbar::button(
                    ui,
                    Icon::Tool(tool),
                    tool_label(tool, tr),
                    !self.pass && self.tool == tool,
                )
                .clicked()
                {
                    self.use_tool(tool);
                    self.panel = None;
                    commands.mode = true;
                }
            }
        });
        ui.small(tr.text(
            "Line, arrow, box, ellipse and laser",
            "直线、箭头、矩形、椭圆与激光笔",
            "直線、箭頭、矩形、橢圓與雷射筆",
        ));
    }

    fn brush(&mut self, ui: &mut Ui) {
        let tr = self.language;
        ui.add(
            egui::Slider::new(&mut self.width, 1.0..=48.0).text(tr.text("Width", "粗细", "粗細")),
        );
        ui.add(egui::Slider::new(&mut self.opacity, 0.1..=1.).text(tr.text(
            "Opacity",
            "不透明度",
            "不透明度",
        )));
        ui.checkbox(
            &mut self.pressure,
            tr.text("Pen pressure", "笔尖压感", "筆尖感壓"),
        );
        ui.add(egui::Slider::new(&mut self.curve, 0.2..=3.).text(tr.text(
            "Pressure curve",
            "压感曲线",
            "感壓曲線",
        )));
        ui.separator();
        let pen = self
            .input
            .pens
            .values()
            .find(|p| p.down)
            .or_else(|| self.input.pens.values().next())
            .copied()
            .unwrap_or_default();
        ui.add(egui::ProgressBar::new(pen.pressure).text(format!("{:.1}%", pen.pressure * 100.)));
        ui.small(format!(
            "{} {:.0}° / {:.0}°",
            tr.text("Tilt", "倾角", "傾角"),
            pen.tilt[0],
            pen.tilt[1]
        ));
    }

    fn palette(&mut self, ui: &mut Ui, commands: &mut Commands) {
        let tr = self.language;
        ui.horizontal_wrapped(|ui| {
            ui.label(tr.text("Palette", "色盘", "調色盤"));
            let mut rgb = self.color.channels8();
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                self.select_color(InkColor::from_channels(rgb, self.color.space));
            }
            if toolbar::hint(
                ui.button(tr.text("Pick screen", "屏幕取色", "螢幕取色")),
                tr.text(
                    "Choose a desktop pixel. Esc cancels.",
                    "点击桌面任一点取色，按 Esc 取消。",
                    "點按桌面任一點取色，按 Esc 取消。",
                ),
            )
            .clicked()
            {
                commands.pick = true;
            }
        });
        ui.horizontal(|ui| {
            self.hex_entry.sync(self.color);
            ui.label("HEX");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.hex_entry.text)
                    .desired_width(104.)
                    .char_limit(16)
                    .hint_text("#FFFFFF"),
            );
            let accept = ui.button(tr.text("Apply", "应用", "套用")).clicked()
                || (response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if (response.has_focus() || response.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Escape))
            {
                self.hex_entry.revert();
            } else {
                if (response.changed() || accept)
                    && let Some(color) = self.hex_entry.apply(self.color.space)
                {
                    self.color = color;
                }
                if response.lost_focus() || accept {
                    self.hex_entry.normalize();
                }
            }
        });
        if self.hex_entry.invalid {
            ui.colored_label(
                ui.visuals().error_fg_color,
                tr.text(
                    "Use #RRGGBB or #RGB",
                    "请输入 #RRGGBB 或 #RGB",
                    "請輸入 #RRGGBB 或 #RGB",
                ),
            );
        }
        let label = |space| match space {
            Space::Srgb => tr.text("sRGB color", "sRGB 颜色", "sRGB 色彩"),
            Space::DisplayP3 => tr.text("Display P3 color", "Display P3 颜色", "Display P3 色彩"),
        };
        egui::ComboBox::from_id_salt("source-gamut")
            .selected_text(label(self.color.space))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.color.space, Space::Srgb, label(Space::Srgb));
                ui.selectable_value(
                    &mut self.color.space,
                    Space::DisplayP3,
                    label(Space::DisplayP3),
                );
            });
    }

    fn background_controls(
        &mut self,
        ui: &mut Ui,
        freeze_unavailable: Option<Text>,
        commands: &mut Commands,
    ) {
        let tr = self.language;
        let freeze_unavailable = freeze_unavailable.map(|reason| reason.get(tr));
        if ui
            .selectable_label(
                !self.board && !self.frozen,
                tr.text("Live desktop", "实时桌面", "即時桌面"),
            )
            .clicked()
        {
            commands.background = self.board || self.frozen;
            self.frozen = false;
            self.board = false;
        }
        let freeze = ui
            .add_enabled_ui(freeze_unavailable.is_none(), |ui| {
                let response = ui.selectable_label(
                    self.frozen,
                    tr.text("Frozen desktop", "冻结桌面", "凍結桌面"),
                );
                if let Some(reason) = freeze_unavailable {
                    toolbar::hint(response, reason)
                } else {
                    response
                }
            })
            .inner;
        if freeze.clicked() && !self.frozen {
            commands.background = true;
            self.frozen = true;
            self.board = false;
        }
        if let Some(reason) = freeze_unavailable {
            ui.small(reason);
        }
        if self.frozen
            && ui
                .button(tr.text("Refresh snapshot", "刷新快照", "更新快照"))
                .clicked()
        {
            commands.background = true;
        }
        if ui
            .checkbox(&mut self.board, tr.text("Whiteboard", "白板", "白板"))
            .changed()
        {
            commands.mode = true;
            if self.board {
                self.frozen = false;
                commands.background = true;
            }
        }
    }

    fn settings(
        &mut self,
        ui: &mut Ui,
        render: &Renderer,
        color: &ColorManager,
        video: &str,
        commands: &mut Commands,
    ) {
        let tr = self.language;
        ui.horizontal(|ui| {
            ui.label(tr.text("Language", "语言", "語言"));
            let system = tr.text("Follow system", "跟随系统", "跟隨系統");
            let choice = &mut self.preferences.language;
            egui::ComboBox::from_id_salt("ui-language")
                .selected_text(choice.map_or(system, Language::name))
                .show_ui(ui, |ui| {
                    commands.preferences |= ui.selectable_value(choice, None, system).changed();
                    for language in Language::ALL {
                        commands.preferences |= ui
                            .selectable_value(choice, Some(language), language.name())
                            .changed();
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label(tr.text("Theme", "主题", "主題"));
            let choice = &mut self.preferences.theme;
            egui::ComboBox::from_id_salt("ui-theme")
                .selected_text(choice.label(tr))
                .show_ui(ui, |ui| {
                    for theme in ThemeMode::ALL {
                        commands.preferences |= ui
                            .selectable_value(choice, theme, theme.label(tr))
                            .changed();
                    }
                });
        });
        ui.separator();
        ui.label(RichText::new(tr.text("Display", "显示器", "顯示器")).strong());
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            #[cfg(target_os = "linux")]
            let can_toggle = self.gnome_guide.is_none();
            #[cfg(target_os = "freebsd")]
            let can_toggle = true;
            commands.preferences |= ui
                .add_enabled(
                    can_toggle,
                    egui::Checkbox::new(
                        &mut self.preferences.wayland,
                        tr.text(
                            "Native Wayland (restart to apply)",
                            "原生 Wayland（重启后生效）",
                            "原生 Wayland（重新啟動後生效）",
                        ),
                    ),
                )
                .changed();
            #[cfg(target_os = "linux")]
            if let Some(guide) = &self.gnome_guide {
                commands.gnome_action = ui.group(|ui| guide.ui(ui, tr, false)).inner;
            }
        }
        let per_display = ui
            .add_enabled_ui(video != "wayland", |ui| {
                ui.checkbox(
                    &mut self.preferences.per_display_windows,
                    tr.text(
                        "One canvas per display",
                        "每块屏幕独立画布",
                        "每個螢幕獨立畫布",
                    ),
                )
            })
            .inner;
        commands.preferences |= per_display.changed();
        if video == "wayland" {
            ui.small(tr.text(
                "Native Wayland uses a single canvas.",
                "原生 Wayland 使用单窗口。",
                "原生 Wayland 使用單視窗。",
            ));
        }
        egui::ComboBox::from_id_salt("display")
            .selected_text(
                self.displays
                    .get(self.display)
                    .map(|d| d.name.as_str())
                    .unwrap_or(tr.text("No display", "无显示器", "無顯示器")),
            )
            .show_ui(ui, |ui| {
                for display in &self.displays {
                    if ui
                        .selectable_label(self.pages.active == Some(display.id), &display.name)
                        .clicked()
                    {
                        commands.display = Some(display.id);
                        ui.close();
                    }
                }
            });
        if self.multi {
            ui.small(tr.text(
                "Draw directly on each screen. Undo follows the selected canvas.",
                "在各屏幕直接书写。撤销作用于当前画布。",
                "在各螢幕直接書寫。復原作用於目前畫布。",
            ));
        } else if video != "wayland" {
            ui.small(tr.text(
                "Drag the OverInk icon to move the canvas between screens.",
                "拖动 OverInk 图标可把画布移到另一块屏幕。",
                "拖曳 OverInk 圖示可將畫布移到另一個螢幕。",
            ));
        }
        ui.horizontal(|ui| {
            ui.label(tr.text("Frame rate", "帧率", "影格率"));
            let auto = tr.text("Auto", "自动", "自動");
            egui::ComboBox::from_id_salt("frame-rate")
                .selected_text(if self.frame_rate == FrameRate::Auto {
                    format!("{auto} ({:.2} FPS)", self.frame_rate.hz(self.refresh_hz()))
                } else {
                    self.frame_rate.label().into()
                })
                .show_ui(ui, |ui| {
                    for rate in FrameRate::ALL {
                        let label = if rate == FrameRate::Auto {
                            auto
                        } else {
                            rate.label()
                        };
                        if ui
                            .selectable_value(&mut self.frame_rate, rate, label)
                            .changed()
                        {
                            commands.frame_rate = true;
                        }
                    }
                });
        });
        if ui
            .checkbox(
                &mut self.touch_enabled,
                tr.text("Touch draw", "触屏书写", "觸控書寫"),
            )
            .changed()
        {
            self.for_each_page(|app| app.discard_touches());
            commands.redraw_canvas = true;
        }
        ui.separator();
        ui.label(RichText::new(tr.text("Session", "会话", "工作階段")).strong());
        commands.preferences |= ui
            .checkbox(
                &mut self.preferences.restore_session,
                tr.text(
                    "Keep ink for the next launch",
                    "退出后保留笔迹",
                    "結束後保留筆跡",
                ),
            )
            .changed();
        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.button(tr.text("Save", "保存", "儲存")).clicked() {
                self.save();
                commands.redraw_canvas = true;
            }
            if ui.button(tr.text("Open", "打开", "開啟")).clicked() {
                self.load();
                commands.redraw_canvas = true;
            }
            if ui
                .button(tr.text("Export SVG", "导出 SVG", "匯出 SVG"))
                .clicked()
            {
                self.export();
                commands.redraw_canvas = true;
            }
        });
        let count = self.history.strokes().len();
        ui.small(match tr {
            Language::English => format!("{count} strokes on this screen"),
            Language::Simplified => format!("当前屏幕有 {count} 条笔迹"),
            Language::Traditional => format!("目前螢幕有 {count} 條筆跡"),
        });
        ui.separator();
        let (hdr, headroom) = render.hdr();
        let encoding = if hdr {
            tr.text(
                "HDR output · SDR ink",
                "HDR 输出 · SDR 笔迹",
                "HDR 輸出 · SDR 筆跡",
            )
        } else if render.extended {
            tr.text("Linear sRGB", "线性 sRGB", "線性 sRGB")
        } else {
            "SDR sRGB"
        };
        ui.small(format!(
            "{video} / {} / {encoding} / {headroom:.2}×",
            render.name
        ));
        ui.small(color.status.get(tr));
        if video == "wayland" {
            ui.small(tr.text(
                "The Wayland compositor controls window order and placement.",
                "Wayland 窗口层级和位置由合成器控制。",
                "Wayland 視窗層級與位置由合成器控制。",
            ));
        }
        ui.add_space(6.);
        if ui
            .button(tr.text("Quit OverInk", "退出 OverInk", "結束 OverInk"))
            .clicked()
        {
            commands.quit = true;
        }
    }
}

fn tool_label(tool: Tool, tr: Language) -> &'static str {
    match tool {
        Tool::Pen => tr.text("Pen", "笔", "筆"),
        Tool::Pencil => tr.text("Pencil", "铅笔", "鉛筆"),
        Tool::Marker => tr.text("Highlighter", "荧光笔", "螢光筆"),
        Tool::Line => tr.text("Line", "直线", "直線"),
        Tool::Arrow => tr.text("Arrow", "箭头", "箭頭"),
        Tool::Rectangle => tr.text("Box", "矩形", "矩形"),
        Tool::Ellipse => tr.text("Ellipse", "椭圆", "橢圓"),
        Tool::Eraser => tr.text("Eraser", "橡皮擦", "橡皮擦"),
        Tool::Laser => tr.text("Laser", "激光笔", "雷射筆"),
    }
}
