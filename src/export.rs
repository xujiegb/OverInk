use crate::{
    geometry,
    model::{Document, Tool},
};
use std::fmt::Write;
/// Group opacity keeps the highlighter from darkening its own joins. Pencil
/// triangles retain the mean of their vertex densities.
pub fn svg(doc: &Document) -> String {
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
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        color::InkColor,
        model::{Point, Stroke},
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
            strokes: vec![stroke],
            ..Document::default()
        };
        let svg = svg(&doc);
        assert_eq!(svg.matches("<path").count(), core.indices.len() / 3);
        assert_eq!(svg.matches("fill-opacity").count(), core.indices.len() / 3);
    }
}
