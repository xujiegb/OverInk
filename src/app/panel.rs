use super::{App, Board, Commands, canvas::Canvas, screenshot};
use crate::{
    appearance::{KeyboardSource, Language, Text, ThemeMode},
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
        freeze_unavailable: impl FnOnce() -> Option<Text>,
        mark: egui::TextureId,
        dialog_open: bool,
    ) -> Commands {
        let mut commands = Commands::default();
        let tr = self.language;
        let available = ui.available_size();
        // Transparent SDL controller: paint the complete toolbar body in egui
        // rather than clearing the whole native window to an opaque rectangle.
        let rail_color = toolbar::colors(ui.visuals().dark_mode).rail;
        ui.painter()
            .rect_filled(ui.max_rect(), f32::from(toolbar::OUTER_RADIUS), rail_color);
        let opened = self.panel.filter(|_| !self.picking);
        let shown = opened.or(self.panel_retiring);
        // Only the on-screen panel can advance an animation. Seed a newly
        // selected panel at zero so every tab still fades in from a fresh state.
        let transition = ui.scope_id().with("panel-transition");
        if opened.is_some() && opened != self.panel_retiring {
            ui.ctx().animate_bool_with_time(transition, false, 0.);
        }
        let panel_alpha = ui
            .ctx()
            .animate_bool_with_time(transition, opened.is_some(), 0.18);
        let visible = shown.filter(|_| panel_alpha > 0.005).or(opened);
        if visible != self.panel_retiring {
            ui.ctx().request_repaint();
        }
        self.panel_retiring = visible;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.;
            ui.allocate_ui_with_layout(
                vec2(toolbar::WIDTH, available.y),
                Layout::top_down(Align::Center),
                |ui| self.rail(ui, mark, &mut commands),
            );
            if let Some(panel) = visible {
                let width = (available.x - toolbar::WIDTH).max(0.);
                if width > 48. {
                    ui.allocate_ui_with_layout(
                        vec2(width, available.y),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_opacity(panel_alpha.clamp(0., 1.));
                            egui::Frame::new()
                                .corner_radius(egui::CornerRadius::same(toolbar::OUTER_RADIUS))
                                .fill(ui.visuals().window_fill())
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
                                                    tr.text("Close panel"),
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
                                                freeze_unavailable(),
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
            tr.text("OverInk · drag to move to another display"),
        );
        ui.add_space(3.);
        if self.picking {
            if toolbar::rail_button(
                ui,
                Icon::Close,
                tr.text("Cancel screen color picker (Esc)"),
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
        if toolbar::rail_button(ui, Icon::Quit, tr.text("Quit OverInk"), false).clicked() {
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
                        tr.text("Show drawing layer (H)")
                    } else {
                        tr.text("Hide drawing layer (H)")
                    };
                    if toolbar::rail_button(ui, eye, label, self.hidden).clicked() {
                        self.finish_all_contacts();
                        self.hidden = !self.hidden;
                        commands.mode = true;
                    }
                    if toolbar::rail_button(
                        ui,
                        Icon::Pointer,
                        tr.text("Mouse · click desktop (Space)"),
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
                                tr.text("Region screenshot"),
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
                        tr.text("Lines, shapes and laser"),
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
                        tr.text("Brush width and opacity"),
                    );
                    let actions = [
                        (
                            Icon::Undo,
                            tr.text("Undo (Ctrl/Cmd+Z)"),
                            super::HistoryAction::Undo,
                        ),
                        (
                            Icon::Redo,
                            tr.text("Redo (Ctrl/Cmd+Shift+Z)"),
                            super::HistoryAction::Redo,
                        ),
                        (
                            Icon::Clear,
                            tr.text("Clear this page"),
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
                        tr.text("Desktop background"),
                    );
                    self.panel_button(
                        ui,
                        Icon::Settings,
                        Panel::Settings,
                        tr.text("Settings and files"),
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
                        tr.text("Palette and screen picker")
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
        ui.small(tr.text("Line, arrow, box, ellipse and laser"));
    }

    fn brush(&mut self, ui: &mut Ui) {
        let tr = self.language;
        ui.add(egui::Slider::new(&mut self.width, 1.0..=48.0).text(tr.text("Width")));
        ui.add(egui::Slider::new(&mut self.opacity, 0.1..=1.).text(tr.text("Opacity")));
        ui.checkbox(&mut self.pressure, tr.text("Pen pressure"));
        ui.add(egui::Slider::new(&mut self.curve, 0.2..=3.).text(tr.text("Pressure curve")));
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
            tr.text("Tilt"),
            pen.tilt[0],
            pen.tilt[1]
        ));
    }

    fn palette(&mut self, ui: &mut Ui, commands: &mut Commands) {
        let tr = self.language;
        ui.horizontal_wrapped(|ui| {
            ui.label(tr.text("Palette"));
            let mut rgb = self.color.channels8();
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                commands.redraw_canvas |=
                    self.select_color(InkColor::from_channels(rgb, self.color.space));
            }
            if toolbar::hint(
                ui.button(tr.text("Pick screen")),
                tr.text("Choose a desktop pixel. Esc cancels."),
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
            let accept = ui.button(tr.text("Apply")).clicked()
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
            ui.colored_label(ui.visuals().error_fg_color, tr.text("Use #RRGGBB or #RGB"));
        }
        let label = |space| match space {
            Space::Srgb => tr.text("sRGB color"),
            Space::DisplayP3 => tr.text("Display P3 color"),
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
                tr.text("Live desktop"),
            )
            .clicked()
        {
            commands.background = self.board.is_some() || self.frozen;
            self.frozen = false;
            self.board = None;
        }
        let freeze = ui
            .add_enabled_ui(freeze_unavailable.is_none(), |ui| {
                let response = ui.selectable_label(self.frozen, tr.text("Frozen desktop"));
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
        if self.frozen && ui.button(tr.text("Refresh snapshot")).clicked() {
            commands.background = true;
        }
        for (board, label) in [
            (Board::White, tr.text("Whiteboard")),
            (Board::Black, tr.text("Blackboard")),
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
            ui.label(tr.text("Language"));
            let system = tr.text("Follow system");
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
            ui.label(tr.text("Theme"));
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
        ui.label(RichText::new(tr.text("On-screen keyboard")).strong());
        ui.horizontal(|ui| {
            ui.label(tr.text("Keyboard source"));
            let source = &mut self.preferences.keyboard_source;
            let system_supported = crate::keyboard::system_supported(video);
            let display_source = if *source == KeyboardSource::System && system_supported {
                tr.text("System keyboard")
            } else {
                tr.text("OverInk keyboard")
            };
            egui::ComboBox::from_id_salt("osk-source")
                .selected_text(display_source)
                .show_ui(ui, |ui| {
                    commands.preferences |= ui
                        .selectable_value(
                            source,
                            KeyboardSource::OverInk,
                            tr.text("OverInk keyboard (default)"),
                        )
                        .changed();
                    commands.preferences |= ui
                        .add_enabled_ui(system_supported, |ui| {
                            ui.selectable_value(
                                source,
                                KeyboardSource::System,
                                tr.text("System keyboard"),
                            )
                        })
                        .inner
                        .changed();
                });
        });
        ui.small(tr.text("The globe key changes your input method; the OverInk keyboard provides no candidate suggestions."));
        ui.separator();
        ui.label(RichText::new(tr.text("Annotation fonts")).strong());
        ui.small(tr.text(
            "Drag ☰ to change fallback priority. The three included fonts cannot be removed.",
        ));
        for (index, (id, name, bundled)) in self.text_fonts.entries().enumerate() {
            let frame = egui::Frame::default().inner_margin(egui::Margin::symmetric(5, 3));
            let (_, dropped) = ui.dnd_drop_zone::<usize, ()>(frame, |ui| {
                ui.horizontal(|ui| {
                    let drag_id = ui.make_persistent_id(("annotation-font-priority", id));
                    ui.dnd_drag_source(drag_id, index, |ui| {
                        ui.label("☰");
                    });
                    ui.label(name);
                    if bundled {
                        ui.small("🔒").on_hover_text(tr.text("Included font"));
                    } else if ui
                        .small_button("×")
                        .on_hover_text(tr.text("Remove font"))
                        .clicked()
                    {
                        commands.font_remove = Some(id.to_owned());
                    }
                });
            });
            if let Some(from) = dropped {
                commands.font_reorder = Some((*from, index));
            }
        }
        if ui
            .add_enabled(
                !dialog_open,
                egui::Button::new(tr.text("+ Add system font")),
            )
            .clicked()
        {
            self.adding_font = !self.adding_font;
            if self.adding_font {
                self.font_catalog
                    .get_or_insert_with(crate::fonts::installed_system_fonts);
            }
        }
        if self.adding_font {
            ui.add(
                egui::TextEdit::singleline(&mut self.font_search)
                    .hint_text(tr.text("Search fonts")),
            );
            let filter = self.font_search.to_lowercase();
            if let Some(catalog) = &self.font_catalog {
                egui::ScrollArea::vertical()
                    .id_salt("installed-fonts")
                    .max_height(180.)
                    .show(ui, |ui| {
                        for (index, font) in catalog.iter().enumerate() {
                            if font.matches_search(&filter)
                                && ui.selectable_label(false, &font.name).clicked()
                            {
                                commands.font_selection = Some(index);
                            }
                        }
                    });
            }
            if commands.font_selection.is_some() {
                self.adding_font = false;
                self.font_search.clear();
            }
        }
        ui.separator();
        ui.label(RichText::new(tr.text("Screenshots")).strong());
        let hdr_available = screenshot::hdr_available(&canvas.window);
        let mut hdr = hdr_available && self.preferences.screenshot_hdr;
        let checkbox = egui::Checkbox::new(&mut hdr, tr.text("Save HDR PNG"));
        if ui.add_enabled(hdr_available, checkbox).changed() {
            self.preferences.screenshot_hdr = hdr;
            commands.preferences = true;
        }
        if !hdr_available {
            ui.small(screenshot::unsupported_hdr(tr));
        }
        ui.label(tr.text("Default save folder"));
        ui.label(self.preferences.screenshot_folder.as_ref().map_or_else(
            || tr.text("Downloads").into(),
            |path| path.to_string_lossy(),
        ));
        ui.add_enabled_ui(!dialog_open, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr.text("Choose folder…")).clicked() {
                    commands.screenshot_folder = true;
                }
                if self.preferences.screenshot_folder.is_some()
                    && ui.button(tr.text("Use Downloads")).clicked()
                {
                    commands.reset_screenshot_folder = true;
                    commands.preferences = true;
                }
            });
        });
        ui.separator();
        ui.label(RichText::new(tr.text("Display")).strong());
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
                        tr.text("Native Wayland (restart to apply)"),
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
                    tr.text("One canvas per display"),
                )
            })
            .inner;
        commands.preferences |= per_display.changed();
        if video == "wayland" {
            ui.small(tr.text("Native Wayland uses a single canvas."));
        }
        egui::ComboBox::from_id_salt("display")
            .selected_text(
                self.displays
                    .get(self.display)
                    .map(|d| d.name.as_str())
                    .unwrap_or(tr.text("No display")),
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
            ui.small(tr.text("Draw directly on each screen. Undo follows the selected canvas."));
        } else if video != "wayland" {
            ui.small(tr.text("Drag the OverInk icon to move the canvas between screens."));
        }
        ui.horizontal(|ui| {
            ui.label(tr.text("Frame rate"));
            let auto = tr.text("Auto");
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
            .checkbox(&mut self.touch_enabled, tr.text("Touch draw"))
            .changed()
        {
            self.for_each_page(|app| app.discard_touches());
            commands.redraw_canvas = true;
        }
        ui.separator();
        ui.label(RichText::new(tr.text("Session")).strong());
        commands.preferences |= ui
            .checkbox(
                &mut self.preferences.restore_session,
                tr.text("Keep ink for the next launch"),
            )
            .changed();
        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.button(tr.text("Save")).clicked() {
                self.save();
                commands.redraw_canvas = true;
            }
            if ui.button(tr.text("Open")).clicked() {
                self.load();
                commands.redraw_canvas = true;
            }
            if ui.button(tr.text("Export SVG")).clicked() {
                self.export();
                commands.redraw_canvas = true;
            }
        });
        let count = self.history.strokes().len();
        ui.small(match tr {
            Language::English => format!("{count} strokes on this screen"),
            Language::Simplified => format!("当前屏幕有 {count} 条笔迹"),
            Language::Traditional => format!("目前螢幕有 {count} 條筆跡"),
            Language::French => format!("{count} tracés sur cet écran"),
            Language::Spanish => format!("{count} trazos en esta pantalla"),
            Language::Japanese => format!("この画面の描画：{count} 本"),
            Language::German => format!("{count} Striche auf diesem Bildschirm"),
            Language::Russian => format!("Штрихов на экране: {count}"),
            Language::Dutch => format!("{count} streken op dit scherm"),
            Language::Turkish => format!("Bu ekrandaki çizgi: {count}"),
            Language::Korean => format!("이 화면의 획: {count}개"),
            Language::Portuguese => format!("{count} traços neste ecrã"),
            Language::Italian => format!("{count} tratti su questo schermo"),
        });
        ui.separator();
        let (hdr, headroom) = render.hdr();
        let encoding = if hdr {
            tr.text("HDR output · SDR ink")
        } else if render.extended {
            tr.text("Linear sRGB")
        } else {
            "SDR sRGB"
        };
        ui.small(format!(
            "{video} / {} / {encoding} / {headroom:.2}×",
            render.name
        ));
        ui.small(color.status.get(tr));
        if video == "wayland" {
            ui.small(tr.text("The Wayland compositor controls window order and placement."));
        }
        ui.add_space(6.);
        if ui.button(tr.text("Quit OverInk")).clicked() {
            commands.quit = true;
        }
    }
}

fn tool_label(tool: Tool, tr: Language) -> &'static str {
    match tool {
        Tool::Pen => tr.text("Pen"),
        Tool::Pencil => tr.text("Pencil"),
        Tool::Marker => tr.text("Highlighter"),
        Tool::Line => tr.text("Line"),
        Tool::Arrow => tr.text("Arrow"),
        Tool::Rectangle => tr.text("Box"),
        Tool::Ellipse => tr.text("Ellipse"),
        Tool::Eraser => tr.text("Eraser"),
        Tool::Laser => tr.text("Laser"),
        Tool::Text => tr.text("Text (T)"),
        Tool::Select => tr.text("Select and move text (V)"),
    }
}
