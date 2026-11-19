use super::{App, Board, Commands, canvas::Canvas, screenshot};
use crate::{
    appearance::{Language, Text, ThemeMode},
    color::{InkColor, PRESETS, Space},
    frame_rate::FrameRate,
    model::Tool,
    toolbar::{self, Icon, Panel},
};
use egui::{Align, Layout, RichText, Ui, vec2};

const DIRECT_TOOLS: [Tool; 4] = [Tool::Pen, Tool::Pencil, Tool::Marker, Tool::Eraser];

impl App {
    pub(super) fn ui(
        &mut self,
        ui: &mut Ui,
        canvas: &Canvas,
        video: &str,
        freeze_unavailable: Option<Text>,
        mark: egui::TextureId,
        dialog_open: bool,
        panel_progress: f32,
    ) -> Commands {
        let mut commands = Commands::default();
        let tr = self.language;
        let available = ui.available_size();
        let active = self.panel.filter(|_| !self.picking);
        let opened = active.or(self.closing_panel);
        let mut content_opacity = 0.0;
        for panel in [
            Panel::Shapes,
            Panel::Brush,
            Panel::Colors,
            Panel::Background,
            Panel::Settings,
        ] {
            let opacity = ui.ctx().animate_bool_with_time(
                egui::Id::new(("toolbar-panel-content", panel)),
                active == Some(panel),
                0.18,
            );
            if opened == Some(panel) {
                content_opacity = opacity;
            }
        }
        if content_opacity > 0.001 && content_opacity < 0.999 {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.;
            ui.allocate_ui_with_layout(
                vec2(toolbar::WIDTH, available.y),
                Layout::top_down(Align::Center),
                |ui| {
                    egui::Frame::new()
                        .fill(ui.visuals().window_fill())
                        .corner_radius(12)
                        .show(ui, |ui| self.rail(ui, mark, &mut commands));
                },
            );
            if let Some(panel) = opened.filter(|_| panel_progress > 0.001) {
                let width = (available.x - toolbar::WIDTH).max(0.);
                if width > 48. {
                    ui.allocate_ui_with_layout(
                        vec2(width, available.y),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_opacity((content_opacity * panel_progress).clamp(0., 1.));
                            if active.is_none() {
                                ui.disable();
                            }
                            egui::Frame::new()
                                .fill(ui.visuals().window_fill())
                                .corner_radius(14)
                                .inner_margin(egui::Margin::same(13))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = vec2(6., 4.);
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
                                                canvas,
                                                video,
                                                &mut commands,
                                                dialog_open,
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
        ui.spacing_mut().item_spacing = vec2(4., 1.);
        ui.add_space(3.);
        toolbar::hint(
            ui.add(egui::Image::new((mark, vec2(29., 29.)))),
            tr.text(
                "OverInk · drag to move to another display",
                "OverInk · 拖动切换显示器",
                "OverInk · 拖曳切換顯示器",
            ),
        );
        ui.add_space(3.);
        if self.picking {
            if toolbar::rail_button(
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
        ui.add_space(1.);
        if toolbar::rail_button(
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
            .auto_shrink([false, true])
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
                    if toolbar::rail_button(ui, eye, label, self.hidden).clicked() {
                        self.finish_all_contacts();
                        self.hidden = !self.hidden;
                        commands.mode = true;
                    }
                    if toolbar::rail_button(
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
                    self.rail_tool(ui, Tool::Select, commands);
                    if ui
                        .add_enabled_ui(!self.suspended && !self.screenshot.saving(), |ui| {
                            toolbar::rail_button(
                                ui,
                                Icon::Screenshot,
                                tr.text("Region screenshot", "区域截图", "區域螢幕擷取"),
                                false,
                            )
                        })
                        .inner
                        .clicked()
                    {
                        commands.screenshot = true;
                    }
                    for tool in DIRECT_TOOLS.into_iter().chain([Tool::Text]) {
                        self.rail_tool(ui, tool, commands);
                    }
                    let shape_selected =
                        !self.pass && (self.tool.is_shape() || self.tool == Tool::Laser);
                    let shapes = if shape_selected {
                        Icon::Tool(self.tool)
                    } else {
                        Icon::Shapes
                    };
                    if toolbar::rail_button(
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
                    ui.add(egui::Separator::default().spacing(3.));
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
                            super::HistoryAction::Undo,
                        ),
                        (
                            Icon::Redo,
                            tr.text(
                                "Redo (Ctrl/Cmd+Shift+Z)",
                                "重做 (Ctrl/Cmd+Shift+Z)",
                                "重做 (Ctrl/Cmd+Shift+Z)",
                            ),
                            super::HistoryAction::Redo,
                        ),
                        (
                            Icon::Clear,
                            tr.text("Clear this page", "清除当前页面", "清除目前頁面"),
                            super::HistoryAction::Clear,
                        ),
                    ];
                    for (index, (icon, label, action)) in actions.into_iter().enumerate() {
                        let enabled = self.history_buttons()[index];
                        if ui
                            .add_enabled_ui(enabled, |ui| {
                                toolbar::rail_button(ui, icon, label, false)
                            })
                            .inner
                            .clicked()
                        {
                            self.change_history(action);
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
                    ui.add(egui::Separator::default().spacing(3.));
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
                                    commands.redraw_canvas |= self.select_preset(*rgb);
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
                    ui.add_space(3.);
                });
            });
    }

    fn toggle_panel(&mut self, panel: Panel) {
        self.panel = (self.panel != Some(panel)).then_some(panel);
    }

    fn rail_tool(&mut self, ui: &mut Ui, tool: Tool, commands: &mut Commands) {
        if toolbar::rail_button(
            ui,
            Icon::Tool(tool),
            tool_label(tool, self.language),
            !self.pass && self.tool == tool,
        )
        .clicked()
        {
            self.use_tool(tool);
            self.panel = None;
            commands.mode = true;
        }
    }

    fn panel_button(&mut self, ui: &mut Ui, icon: Icon, panel: Panel, label: &str) {
        if toolbar::rail_button(ui, icon, label, self.panel == Some(panel)).clicked() {
            self.toggle_panel(panel);
        }
    }

    fn shapes(&mut self, ui: &mut Ui, commands: &mut Commands) {
        let tr = self.language;
        ui.horizontal_wrapped(|ui| {
            for tool in Tool::ALL
                .into_iter()
                .filter(|tool| tool.is_shape() || *tool == Tool::Laser)
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
                commands.redraw_canvas |=
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
                    commands.redraw_canvas |= self.apply_color(color);
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
        let mut space = self.color.space;
        egui::ComboBox::from_id_salt("source-gamut")
            .selected_text(label(space))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut space, Space::Srgb, label(Space::Srgb));
                ui.selectable_value(&mut space, Space::DisplayP3, label(Space::DisplayP3));
            });
        if space != self.color.space {
            commands.redraw_canvas |= self.apply_color(InkColor {
                space,
                ..self.color
            });
        }
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
                self.board.is_none() && !self.frozen,
                tr.text("Live desktop", "实时桌面", "即時桌面"),
            )
            .clicked()
        {
            commands.background = self.board.is_some() || self.frozen;
            self.frozen = false;
            self.board = None;
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
            self.board = None;
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
        for (board, label) in [
            (Board::White, tr.text("Whiteboard", "白板", "白板")),
            (Board::Black, tr.text("Blackboard", "黑板", "黑板")),
        ] {
            if ui
                .selectable_label(self.board == Some(board), label)
                .clicked()
                && self.board != Some(board)
            {
                self.board = Some(board);
                self.frozen = false;
                commands.background = true;
            }
        }
    }

    fn settings(
        &mut self,
        ui: &mut Ui,
        canvas: &Canvas,
        video: &str,
        commands: &mut Commands,
        dialog_open: bool,
    ) {
        let render = &canvas.render;
        let color = &canvas.color;
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
        ui.label(RichText::new(tr.text("Annotation fonts", "文本框字体", "文字框字型")).strong());
        ui.small(tr.text(
            "Drag to reorder font fallback priority. Included fonts cannot be deleted.",
            "拖动字体调整回退优先级。三个内置字体不可删除。",
            "拖曳字型以調整備援優先順序。三種內建字型不可刪除。",
        ));
        if let Some(item) = self.text_menu_item() {
            let current = item.font.as_deref();
            let selected_name = current
                .and_then(|key| self.text_fonts.display_name(key))
                .unwrap_or(tr.text(
                    "Automatic (font priority)",
                    "自动（按优先级）",
                    "自動（依優先順序）",
                ));
            ui.horizontal(|ui| {
                ui.label(tr.text("Selected text box", "选中文本框", "選取文字框"));
                egui::ComboBox::from_id_salt("selected-annotation-font")
                    .selected_text(selected_name)
                    .width(190.)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                current.is_none(),
                                tr.text("Automatic", "自动", "自動"),
                            )
                            .clicked()
                        {
                            commands.box_font = Some(None);
                        }
                        for entry in self.text_fonts.entries() {
                            if ui
                                .selectable_label(current == Some(entry.key), entry.name)
                                .clicked()
                            {
                                commands.box_font = Some(Some(entry.key.to_owned()));
                            }
                        }
                    });
            });
        }
        let mut hovered = None;
        let pointer = ui.input(|input| input.pointer.latest_pos());
        for (index, font) in self.text_fonts.entries().iter().enumerate() {
            ui.horizontal(|ui| {
                let label = format!("⋮⋮  {}", font.name);
                let drag = ui.add(egui::Label::new(label).sense(egui::Sense::click_and_drag()));
                if drag.drag_started() {
                    self.font_drag = Some(index);
                }
                if pointer.is_some_and(|pos| drag.rect.contains(pos)) {
                    hovered = Some(index);
                }
                if font.bundled {
                    ui.weak(tr.text("Included", "内置", "內建"));
                } else if ui
                    .small_button("×")
                    .on_hover_text(tr.text("Remove font", "删除字体", "刪除字型"))
                    .clicked()
                {
                    commands.remove_font = Some(font.key.to_owned());
                }
            });
        }
        if ui.input(|input| input.pointer.any_released()) {
            if let (Some(from), Some(to)) = (self.font_drag.take(), hovered)
                && from != to
            {
                commands.move_font = Some((from, to));
            }
        }
        ui.add_enabled_ui(!dialog_open, |ui| {
            if ui
                .button(tr.text("Add font…", "添加字体…", "新增字型…"))
                .clicked()
            {
                commands.choose_font = true;
            }
        });
        if let Some(path) = &mut self.font_path_input {
            let entry = ui.add(
                egui::TextEdit::singleline(path)
                    .id(egui::Id::unique(super::FONT_PATH_FIELD))
                    .hint_text(tr.text("Font file path", "字体文件路径", "字型檔案路徑")),
            );
            let submitted =
                entry.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            let usable = !path.trim().is_empty();
            if (submitted
                || ui
                    .add_enabled(
                        usable,
                        egui::Button::new(tr.text("Apply font", "应用字体", "套用字型")),
                    )
                    .clicked())
                && usable
            {
                commands.font_path = Some(std::path::PathBuf::from(path.trim()));
            }
        }
        ui.separator();
        ui.label(RichText::new(tr.text("Screenshots", "截图", "螢幕擷取")).strong());
        let hdr_available = screenshot::hdr_available(&canvas.window);
        let mut hdr = hdr_available && self.preferences.screenshot_hdr;
        let checkbox = egui::Checkbox::new(
            &mut hdr,
            tr.text("Save HDR PNG", "保存 HDR PNG", "儲存 HDR PNG"),
        );
        if ui.add_enabled(hdr_available, checkbox).changed() {
            self.preferences.screenshot_hdr = hdr;
            commands.preferences = true;
        }
        if !hdr_available {
            ui.small(screenshot::unsupported_hdr(tr));
        }
        ui.label(tr.text("Default save folder", "默认保存文件夹", "預設儲存資料夾"));
        ui.label(self.preferences.screenshot_folder.as_ref().map_or_else(
            || tr.text("Downloads", "下载", "下載").into(),
            |path| path.to_string_lossy(),
        ));
        ui.add_enabled_ui(!dialog_open, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(tr.text("Choose folder…", "选择文件夹…", "選擇資料夾…"))
                    .clicked()
                {
                    commands.screenshot_folder = true;
                }
                if self.preferences.screenshot_folder.is_some()
                    && ui
                        .button(tr.text("Use Downloads", "使用下载文件夹", "使用下載資料夾"))
                        .clicked()
                {
                    self.preferences.screenshot_folder = None;
                    commands.preferences = true;
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
        Tool::Text => tr.text("Text (T)", "文字（T）", "文字（T）"),
        Tool::Select => tr.text(
            "Select and move text (V)",
            "选择和移动文字（V）",
            "選取和移動文字（V）",
        ),
    }
}
