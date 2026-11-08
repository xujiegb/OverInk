use crate::{geometry, model::Document};
use std::fmt::Write;
/// SVG stores an opaque triangle union in a group, with group opacity applied
/// once: pressure is preserved and a highlighter cannot darken itself at joins.
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
            if a.coverage < 0.5 || b.coverage < 0.5 || c.coverage < 0.5 {
                continue;
            }
            let _ = write!(
                out,
                "<path d=\"M{:.3},{:.3} L{:.3},{:.3} L{:.3},{:.3} Z\"/>",
                a.xy[0], a.xy[1], b.xy[0], b.xy[1], c.xy[0], c.xy[1]
            );
        }
        out.push_str("</g>\n");
    }
    out.push_str("</svg>\n");
    out
}
