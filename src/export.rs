#![forbid(unsafe_code)]

use crate::{
    fonts::TextFonts,
    geometry,
    model::{Document, Tool},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use egui::{Color32, Pos2};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use std::{collections::HashMap, fmt::Write};
/// Group opacity keeps the highlighter from darkening its own joins. Pencil
/// triangles retain the mean of their vertex densities.
pub fn svg(doc: &Document, fonts: &TextFonts) -> Result<String, String> {
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><title>OverInk annotations</title>\n",
        doc.size[0], doc.size[1], doc.size[0], doc.size[1]
    );
    for s in &doc.strokes {
        let m = geometry::mesh(s, 0.);
        let _ = write!(
            out,
            "<g opacity=\"{:.4}\" fill=\"{}\">",
            s.opacity,
            s.color.css()
        );
        for t in m.indices.as_chunks::<3>().0 {
            let a = m.vertices[t[0] as usize];
            let b = m.vertices[t[1] as usize];
            let c = m.vertices[t[2] as usize];
            out.push_str("<path");
            if s.tool == Tool::Pencil {
                let density = (a.coverage + b.coverage + c.coverage) / 3.;
                let _ = write!(out, " fill-opacity=\"{density:.4}\"");
            }
            let _ = write!(
                out,
                " d=\"M{:.3},{:.3} L{:.3},{:.3} L{:.3},{:.3} Z\"/>",
                a.xy[0], a.xy[1], b.xy[0], b.xy[1], c.xy[0], c.xy[1]
            );
        }
        out.push_str("</g>\n");
    }
    if !doc.text_boxes.is_empty() {
        out.push_str(concat!(
            "<metadata>OpenMoji artwork, where used, is by HfG Schwäbisch Gmünd and contributors. ",
            "Source: https://openmoji.org/. License: https://creativecommons.org/licenses/by-sa/4.0/. ",
            "Glyphs are rasterized and monochrome glyphs use the annotation color.</metadata>\n",
        ));
        svg_text(&mut out, doc, fonts)?;
    }
    out.push_str("</svg>\n");
    Ok(out)
}

fn svg_text(out: &mut String, doc: &Document, fonts: &TextFonts) -> Result<(), String> {
    let ctx = egui::Context::default();
    ctx.set_fonts(fonts.definitions());
    let mut glyphs = HashMap::new();
    let mut next_glyph = 0;
    let mut definitions = String::new();
    let mut body = String::new();
    for (index, item) in doc.text_boxes.iter().enumerate() {
        let rect = item.text_rect();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, doc.size.into())),
            ..Default::default()
        };
        let mut layout = None;
        let frame = ctx.run_ui(input, |ui| {
            layout = Some(fonts.layout_preferred(
                ui,
                &item.text,
                item.font_size,
                Color32::WHITE,
                rect.width(),
                item.font.as_deref(),
            ));
        });
        // egui can replace the glyph atlas between passes and reuse its UVs.
        if frame.textures_delta.set.iter().any(|(id, deltas)| {
            *id == egui::TextureId::default() && deltas.iter().any(|delta| delta.pos.is_none())
        }) {
            glyphs.clear();
        }
        frame.drop_without_applying_deltas();
        let mut atlas = None;
        let _ = writeln!(
            definitions,
            "<clipPath id=\"text-box-{index}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>",
            rect.left(),
            rect.top(),
            rect.width(),
            rect.height()
        );
        let _ = writeln!(
            body,
            "<g clip-path=\"url(#text-box-{index})\" aria-label=\"{}\">",
            escape(&item.text)
        );
        let color = item.color.srgb8();
        let layout = layout.ok_or("Failed to lay out annotation text")?;
        let start = rect.min - egui::vec2(0., item.visible_scroll_offset(layout.size().y));
        for row in &layout.rows {
            let origin = start + row.pos.to_vec2();
            if origin.y > rect.bottom() {
                break;
            }
            if origin.y + row.size.y < rect.top() {
                continue;
            }
            for glyph in &row.glyphs {
                let uv = glyph.uv_rect;
                if uv.is_nothing() {
                    continue;
                }
                let tint = (!glyph.is_color).then_some(color);
                let key = (uv.min, uv.max, tint);
                let id = match glyphs.entry(key) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let atlas = atlas.get_or_insert_with(|| ctx.fonts(|fonts| fonts.image()));
                        let (width, height) = (uv.max[0] - uv.min[0], uv.max[1] - uv.min[1]);
                        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
                        for y in usize::from(uv.min[1])..usize::from(uv.max[1]) {
                            for x in usize::from(uv.min[0])..usize::from(uv.max[0]) {
                                let pixel = atlas.pixels[y * atlas.size[0] + x];
                                rgba.extend(match tint {
                                    Some([red, green, blue]) => [red, green, blue, pixel.a()],
                                    None => pixel.to_srgba_unmultiplied(),
                                });
                            }
                        }
                        let mut png = Vec::new();
                        PngEncoder::new(&mut png)
                            .write_image(
                                &rgba,
                                width.into(),
                                height.into(),
                                ExtendedColorType::Rgba8,
                            )
                            .map_err(|error| error.to_string())?;
                        let id = next_glyph;
                        next_glyph += 1;
                        let _ = writeln!(
                            definitions,
                            "<image id=\"glyph-{id}\" width=\"{}\" height=\"{}\" href=\"data:image/png;base64,{}\"/>",
                            uv.size.x,
                            uv.size.y,
                            STANDARD.encode(png)
                        );
                        *entry.insert(id)
                    }
                };
                let position = origin + glyph.pos.to_vec2() + uv.offset;
                let _ = writeln!(
                    body,
                    "<use href=\"#glyph-{id}\" x=\"{}\" y=\"{}\"/>",
                    position.x, position.y
                );
            }
        }
        body.push_str("</g>\n");
    }
    let _ = writeln!(out, "<defs>\n{definitions}</defs>\n{body}");
    Ok(())
}

fn escape(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\t' | '\n' | '\r' => escaped.push(ch),
            '\u{fffe}' | '\u{ffff}' => (),
            ch if !ch.is_control() => escaped.push(ch),
            _ => (),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        color::InkColor,
        model::{Point, Stroke, TextBox},
    };

    #[test]
    fn pencil_exports_every_core_triangle_and_its_density() {
        let stroke = Stroke {
            tool: Tool::Pencil,
            color: InkColor::default(),
            width: 8.,
            opacity: 0.68,
            pressure_curve: 1.,
            pressure_enabled: true,
            points: vec![
                Point::new(10., 12., 0.2, [0.; 2]),
                Point::new(95., 44., 0.9, [30., 0.]),
            ],
        };
        let core = geometry::mesh(&stroke, 0.);
        assert!(core.vertices.iter().all(|v| v.coverage > 0.));
        assert!(core.vertices.iter().any(|v| v.coverage < 0.5));
        let doc = Document {
            strokes: vec![stroke.into()],
            ..Document::default()
        };
        let svg = svg(&doc, &TextFonts::default()).unwrap();
        assert_eq!(svg.matches("<path").count(), core.indices.len() / 3);
        assert_eq!(svg.matches("fill-opacity").count(), core.indices.len() / 3);
    }

    #[test]
    fn identical_glyphs_share_images_without_reusing_the_wrong_tint() {
        let item = TextBox {
            origin: [10., 10.],
            size: [200., 72.],
            font_size: 28.,
            font: None,
            color: InkColor::from_rgb8([0, 0, 0]),
            text: "中😀".into(),
            scroll_offset: 0.,
        };
        let doc = Document {
            text_boxes: vec![
                item.clone().into(),
                item.clone().into(),
                TextBox {
                    color: InkColor::from_rgb8([255, 0, 0]),
                    ..item
                }
                .into(),
            ],
            ..Document::default()
        };
        let svg = svg(&doc, &TextFonts::default()).unwrap();
        assert_eq!(svg.matches("<image ").count(), 3);
        assert_eq!(svg.matches("<use ").count(), 6);
        assert_eq!(svg.matches("href=\"#glyph-1\"").count(), 3);
    }

    #[test]
    fn scrolled_annotations_export_the_visible_rows() {
        let item = TextBox {
            origin: [10., 10.],
            size: [200., 56.],
            font_size: 28.,
            font: None,
            color: InkColor::default(),
            text: "A\n\n\n\n\n😀".into(),
            scroll_offset: 0.,
        };
        let fonts = TextFonts::default();
        let render = |scroll_offset| {
            svg(
                &Document {
                    text_boxes: vec![
                        TextBox {
                            scroll_offset,
                            ..item.clone()
                        }
                        .into(),
                    ],
                    ..Document::default()
                },
                &fonts,
            )
            .unwrap()
        };
        let top = render(0.);
        let bottom = render(1000.);
        assert_eq!(top.matches("<use ").count(), 1);
        assert_eq!(bottom.matches("<use ").count(), 1);
        assert_ne!(top, bottom);
        assert_eq!(render(2000.), bottom);
    }
}
