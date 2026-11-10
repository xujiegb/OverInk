use crate::{appearance::Language, model::Tool};
use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, vec2};

mod tooltip;
pub use tooltip::{Tooltips, hint};

pub const WIDTH: f32 = 56.;
pub const HEIGHT: f32 = 580.;
pub const PANEL_WIDTH: f32 = 312.;
pub const GRIP_HEIGHT: f32 = 39.;
const ACTIVE: Color32 = Color32::from_rgb(0, 122, 157);

pub struct Colors {
    pub rail: Color32,
    pub panel: Color32,
    ink: Color32,
    hover: Color32,
}

pub fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            rail: Color32::from_rgb(27, 36, 46),
            panel: Color32::from_rgb(34, 45, 58),
            ink: Color32::from_rgb(220, 234, 246),
            hover: Color32::from_rgb(52, 69, 88),
        }
    } else {
        Colors {
            rail: Color32::from_rgb(246, 250, 253),
            panel: Color32::WHITE,
            ink: Color32::from_rgb(26, 52, 77),
            hover: Color32::from_rgb(224, 239, 249),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Panel {
    Shapes,
    Brush,
    Colors,
    Background,
    Settings,
}

impl Panel {
    pub fn title(self, language: Language) -> &'static str {
        match self {
            Self::Shapes => language.text("Shapes", "形状", "圖形"),
            Self::Brush => language.text("Brush", "笔刷", "筆刷"),
            Self::Colors => language.text("Color", "颜色", "色彩"),
            Self::Background => language.text("Background", "背景", "背景"),
            Self::Settings => language.text("Settings", "设置", "設定"),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Icon {
    Eye,
    EyeOff,
    Pointer,
    Tool(Tool),
    Shapes,
    Brush,
    Undo,
    Redo,
    Clear,
    Display,
    Settings,
    Close,
    Quit,
}

pub fn image() -> image::RgbaImage {
    image::load_from_memory(include_bytes!("../assets/overink.png"))
        .expect("valid bundled OverInk icon")
        .into_rgba8()
}

pub fn brand(ctx: &egui::Context) -> egui::TextureHandle {
    let image = image();
    ctx.load_texture(
        "overink-brand",
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        ),
        egui::TextureOptions::LINEAR,
    )
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(38., 30.), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, ui.is_enabled(), selected, label)
    });
    let painter = ui.painter();
    let colors = colors(ui.visuals().dark_mode);
    let bg = if selected {
        ACTIVE
    } else if response.hovered() || response.has_focus() {
        colors.hover
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, 8., bg);
    let color = if !ui.is_enabled() {
        colors.ink.gamma_multiply(0.4)
    } else if selected {
        Color32::WHITE
    } else {
        colors.ink
    };
    paint(icon, painter, rect.center(), color);
    hint(response, label)
}

pub fn swatch(ui: &mut Ui, size: Vec2, rgb: [u8; 3], label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, ui.is_enabled(), selected, label)
    });
    let painter = ui.painter();
    painter.rect_filled(
        rect.shrink(1.),
        5.,
        Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
    );
    if selected || response.has_focus() || response.hovered() {
        painter.rect_stroke(rect, 6., Stroke::new(1.5, ACTIVE), egui::StrokeKind::Inside);
        painter.rect_stroke(
            rect.shrink(2.),
            4.,
            Stroke::new(1.0, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
    }
    hint(response, label)
}

fn paint(icon: Icon, p: &egui::Painter, center: Pos2, color: Color32) {
    let at = |x: f32, y: f32| center + vec2(x - 12., y - 12.);
    let stroke = Stroke::new(1.65, color);
    let line = |points: &[(f32, f32)]| {
        p.add(egui::Shape::line(
            points.iter().map(|&(x, y)| at(x, y)).collect(),
            stroke,
        ));
    };
    let square = |x, y, w, h| {
        p.rect_stroke(
            Rect::from_min_size(at(x, y), vec2(w, h)),
            2.,
            stroke,
            egui::StrokeKind::Middle,
        );
    };
    match icon {
        Icon::Eye | Icon::EyeOff => {
            line(&[
                (2., 12.),
                (6., 8.),
                (10., 6.),
                (14., 6.),
                (18., 8.),
                (22., 12.),
                (18., 16.),
                (14., 18.),
                (10., 18.),
                (6., 16.),
                (2., 12.),
            ]);
            p.circle_stroke(at(12., 12.), 3.1, stroke);
            if matches!(icon, Icon::EyeOff) {
                line(&[(4., 20.), (20., 4.)]);
            }
        }
        Icon::Pointer => line(&[(5., 3.), (19., 12.), (12., 13.), (8., 20.), (5., 3.)]),
        Icon::Tool(Tool::Pen) => {
            line(&[
                (5., 19.),
                (8., 10.),
                (16., 4.),
                (20., 8.),
                (14., 16.),
                (5., 19.),
            ]);
            line(&[(5., 19.), (12., 12.)]);
            p.circle_filled(at(12., 12.), 1.65, color);
        }
        Icon::Tool(Tool::Pencil) => {
            line(&[
                (4., 20.),
                (6., 14.),
                (16., 4.),
                (20., 8.),
                (10., 18.),
                (4., 20.),
            ]);
            line(&[(13., 7.), (17., 11.)]);
            line(&[(6., 14.), (10., 18.)]);
        }
        Icon::Tool(Tool::Marker) => {
            line(&[
                (7., 16.),
                (5., 19.),
                (8., 20.),
                (11., 17.),
                (7., 13.),
                (5., 15.),
                (11., 21.),
                (20., 12.),
                (14., 6.),
                (7., 13.),
            ]);
            line(&[(11., 15.), (17., 9.)]);
        }
        Icon::Tool(Tool::Eraser) => {
            line(&[
                (3., 14.),
                (12., 5.),
                (21., 13.),
                (14., 20.),
                (9., 20.),
                (3., 14.),
            ]);
            line(&[(7., 10.), (16., 18.)]);
        }
        Icon::Tool(Tool::Line) => line(&[(4., 19.), (20., 5.)]),
        Icon::Tool(Tool::Arrow) => {
            line(&[(4., 19.), (20., 5.), (20., 12.)]);
            line(&[(20., 5.), (13., 5.)]);
        }
        Icon::Tool(Tool::Rectangle) => square(4., 6., 16., 12.),
        Icon::Tool(Tool::Ellipse) => {
            p.add(egui::Shape::ellipse_stroke(
                at(12., 12.),
                vec2(8., 6.),
                stroke,
            ));
        }
        Icon::Tool(Tool::Laser) => {
            line(&[(4., 20.), (13., 11.)]);
            p.circle_filled(at(17., 7.), 2.1, color);
            line(&[(17., 2.), (17., 3.5)]);
            line(&[(20.5, 7.), (22., 7.)]);
        }
        Icon::Shapes => {
            square(4., 4., 11., 11.);
            p.circle_stroke(at(16.5, 16.5), 4.5, stroke);
        }
        Icon::Brush => {
            p.circle_filled(at(12., 12.), 5.4, color);
            p.circle_stroke(at(12., 12.), 8.6, Stroke::new(0.9, color));
        }
        Icon::Undo | Icon::Redo => {
            let x = |x| {
                if matches!(icon, Icon::Redo) {
                    24. - x
                } else {
                    x
                }
            };
            line(&[(x(8.), 5.), (x(3.), 10.), (x(8.), 15.)]);
            line(&[
                (x(3.), 10.),
                (x(14.), 10.),
                (x(18.), 11.),
                (x(20.), 14.),
                (x(19.), 18.),
                (x(15.), 20.),
                (x(10.), 20.),
            ]);
        }
        Icon::Clear => {
            square(6., 8., 12., 12.);
            line(&[(4., 6.), (20., 6.)]);
            line(&[(9., 6.), (9., 3.5), (15., 3.5), (15., 6.)]);
            line(&[(10., 11.), (10., 17.)]);
            line(&[(14., 11.), (14., 17.)]);
        }
        Icon::Display => {
            square(3., 4., 18., 13.);
            line(&[(12., 17.), (12., 20.)]);
            line(&[(8., 20.), (16., 20.)]);
        }
        Icon::Settings => {
            for (y, x) in [(6., 9.), (12., 15.), (18., 7.)] {
                line(&[(4., y), (x - 2., y)]);
                line(&[(x + 2., y), (20., y)]);
                p.circle_stroke(at(x, y), 2., stroke);
            }
        }
        Icon::Close => {
            line(&[(6., 6.), (18., 18.)]);
            line(&[(18., 6.), (6., 18.)]);
        }
        Icon::Quit => {
            line(&[(12., 3.), (12., 12.)]);
            line(&[
                (8., 5.),
                (5., 8.),
                (4., 12.),
                (5., 16.),
                (8., 19.),
                (12., 20.),
                (16., 19.),
                (19., 16.),
                (20., 12.),
                (19., 8.),
                (16., 5.),
            ]);
        }
    }
}
