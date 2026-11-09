use super::{App, Commands};
use crate::{
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
        freeze_unavailable: Option<&'static str>,
        mark: egui::TextureId,
    ) -> Commands {
        let mut commands = Commands::default();
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
                                .fill(egui::Color32::WHITE)
                                .inner_margin(egui::Margin::same(13))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = vec2(8., 7.);
                                    ui.set_width((width - 26.).max(0.));
                                    ui.set_min_height((available.y - 26.).max(0.));
                                    ui.horizontal(|ui| {
                                        ui.heading(panel.title());
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if toolbar::button(
                                                    ui,
                                                    Icon::Close,
                                                    "Close panel",
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
                                    egui::ScrollArea::vertical()
                                        .id_salt(panel.title())
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| {
                                            match panel {
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
                                            }
                                            ui.add_space(12.);
                                            ui.label(
                                                RichText::new(&self.status)
                                                    .small()
                                                    .color(toolbar::MUTED),
                                            );
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
        ui.spacing_mut().item_spacing = vec2(4., 2.);
        ui.add_space(5.);
        ui.add(egui::Image::new((mark, vec2(33., 33.))))
            .on_hover_text("OverInk · drag to move to another display");
        ui.add_space(5.);
        if self.picking {
            if toolbar::button(ui, Icon::Close, "Cancel screen color picker (Esc)", true).clicked()
            {
                commands.pick = true;
            }
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("tool-rail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let eye = if self.hidden { Icon::EyeOff } else { Icon::Eye };
                    let label = if self.hidden {
                        "Show drawing layer (H)"
                    } else {
                        "Hide drawing layer (H)"
                    };
                    if toolbar::button(ui, eye, label, self.hidden).clicked() {
                        self.stop();
                        self.hidden = !self.hidden;
                        commands.mode = true;
                    }
                    if toolbar::button(
                        ui,
                        Icon::Pointer,
                        "Mouse · click desktop (Space)",
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
                            tool.label(),
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
                        "Lines, shapes and laser",
                        shape_selected || self.panel == Some(Panel::Shapes),
                    )
                    .clicked()
                    {
                        self.toggle_panel(Panel::Shapes);
                    }
                    ui.separator();
                    self.panel_button(ui, Icon::Brush, Panel::Brush, "Brush width and opacity");
                    if toolbar::button(ui, Icon::Undo, "Undo (Ctrl/Cmd+Z)", false).clicked() {
                        self.reset_contacts();
                        self.history.undo();
                    }
                    if toolbar::button(ui, Icon::Redo, "Redo (Ctrl/Cmd+Shift+Z)", false).clicked() {
                        self.reset_contacts();
                        self.history.redo();
                    }
                    if toolbar::button(ui, Icon::Clear, "Clear this page", false).clicked() {
                        self.reset_contacts();
                        self.history.clear();
                    }
                    self.panel_button(ui, Icon::Display, Panel::Background, "Desktop background");
                    self.panel_button(ui, Icon::Settings, Panel::Settings, "Settings and files");
                    ui.separator();
                    for row in PRESETS.as_chunks::<2>().0 {
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - 44.) * 0.5).max(0.));
                            for rgb in row {
                                let color = InkColor::from_rgb8(*rgb);
                                if toolbar::swatch(
                                    ui,
                                    vec2(20., 20.),
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
                    let label = format!("{} · palette and screen picker", self.color.hex());
                    if toolbar::swatch(
                        ui,
                        vec2(44., 24.),
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
        ui.horizontal_wrapped(|ui| {
            for tool in Tool::ALL
                .into_iter()
                .filter(|tool| !DIRECT_TOOLS.contains(tool))
            {
                if toolbar::button(
                    ui,
                    Icon::Tool(tool),
                    tool.label(),
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
        ui.small("Line, arrow, box, ellipse and laser");
    }

    fn brush(&mut self, ui: &mut Ui) {
        ui.add(egui::Slider::new(&mut self.width, 1.0..=48.0).text("Width"));
        ui.add(egui::Slider::new(&mut self.opacity, 0.1..=1.).text("Opacity"));
        ui.checkbox(&mut self.pressure, "Pen pressure");
        ui.add(egui::Slider::new(&mut self.curve, 0.2..=3.).text("Pressure curve"));
        ui.separator();
        let pen = self
            .pens
            .values()
            .find(|p| p.down)
            .or_else(|| self.pens.values().next())
            .copied()
            .unwrap_or_default();
        ui.add(egui::ProgressBar::new(pen.pressure).text(format!("{:.1}%", pen.pressure * 100.)));
        ui.small(format!("Tilt {:.0}° / {:.0}°", pen.tilt[0], pen.tilt[1]));
    }

    fn palette(&mut self, ui: &mut Ui, commands: &mut Commands) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Palette");
            let mut rgb = self.color.channels8();
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                self.select_color(InkColor::from_channels(rgb, self.color.space));
            }
            if ui
                .button("Pick screen")
                .on_hover_text("Choose a desktop pixel. Esc cancels.")
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
            let accept = ui.button("Apply").clicked()
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
            ui.colored_label(egui::Color32::from_rgb(168, 45, 66), "Use #RRGGBB or #RGB");
        }
        egui::ComboBox::from_id_salt("source-gamut")
            .selected_text(match self.color.space {
                Space::Srgb => "sRGB color",
                Space::DisplayP3 => "Display P3 color",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.color.space, Space::Srgb, "sRGB color");
                ui.selectable_value(&mut self.color.space, Space::DisplayP3, "Display P3 color");
            });
    }

    fn background_controls(
        &mut self,
        ui: &mut Ui,
        freeze_unavailable: Option<&'static str>,
        commands: &mut Commands,
    ) {
        if ui
            .selectable_label(!self.board && !self.frozen, "Live desktop")
            .clicked()
        {
            commands.background = self.board || self.frozen;
            self.frozen = false;
            self.board = false;
        }
        let freeze = ui
            .add_enabled_ui(freeze_unavailable.is_none(), |ui| {
                ui.selectable_label(self.frozen, "Frozen desktop")
                    .on_disabled_hover_text(freeze_unavailable.unwrap_or_default())
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
        if self.frozen && ui.button("Refresh snapshot").clicked() {
            commands.background = true;
        }
        if ui.checkbox(&mut self.board, "Whiteboard").changed() {
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
        ui.label(RichText::new("Display").strong());
        egui::ComboBox::from_id_salt("display")
            .selected_text(
                self.displays
                    .get(self.display)
                    .map(|d| d.name.as_str())
                    .unwrap_or("No display"),
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
        ui.small("Drag the OverInk icon to move between screens.");
        ui.horizontal(|ui| {
            ui.label("Frame rate");
            egui::ComboBox::from_id_salt("frame-rate")
                .selected_text(if self.frame_rate == FrameRate::Auto {
                    format!("Auto ({:.2} FPS)", self.frame_rate.hz(self.refresh_hz()))
                } else {
                    self.frame_rate.label().into()
                })
                .show_ui(ui, |ui| {
                    for rate in FrameRate::ALL {
                        if ui
                            .selectable_value(&mut self.frame_rate, rate, rate.label())
                            .changed()
                        {
                            commands.frame_rate = true;
                        }
                    }
                });
        });
        if ui.checkbox(&mut self.touch_enabled, "Touch draw").changed() {
            self.discard_touches();
        }
        ui.separator();
        ui.label(RichText::new("Session").strong());
        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                self.save();
            }
            if ui.button("Open").clicked() {
                self.load();
            }
            if ui.button("Export SVG").clicked() {
                self.export();
            }
        });
        ui.small(format!(
            "{} strokes on this screen",
            self.history.doc.strokes.len()
        ));
        ui.separator();
        let (hdr, headroom) = render.hdr();
        let encoding = if hdr {
            "HDR output · SDR ink"
        } else if render.extended {
            "Linear sRGB"
        } else {
            "SDR sRGB"
        };
        ui.small(format!(
            "{video} / {} / {encoding} / {headroom:.2}×",
            render.name
        ));
        ui.small(&color.status);
        if video == "wayland" {
            ui.small("Wayland stacking and placement are controlled by the compositor.");
        }
        ui.add_space(6.);
        if ui.button("Quit OverInk").clicked() {
            commands.quit = true;
        }
    }
}
