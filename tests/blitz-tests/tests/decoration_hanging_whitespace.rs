//! Text decorations stop before the whitespace that hangs past the end of a wrapped line, as
//! in browsers (which don't paint it), rather than underlining the trailing space.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

#[test]
fn underline_stops_before_a_wrapped_lines_trailing_space() {
    let (width, height) = (100, 60);
    let mut doc = HtmlDocument::from_html(
        "<body style='margin:0;background:white'><div id=line style='width:60px;font:20px/30px monospace'>\
         <span style='text-decoration:underline'>aaaa bbbb</span></div></body>",
        DocumentConfig {
            viewport: Some(Viewport::new(width, height, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, doc.as_mut(), 1.0, width, height, 0, 0),
        width,
        height,
    );
    // The first line is "aaaa " (four 12px glyphs, then the space that hangs); the column
    // through the space has no ink, underline included.
    let x = 54;
    let ink: Vec<_> = (0..30)
        .filter(|&y| buffer[(y * width as usize + x) * 4] < 200)
        .collect();
    assert_eq!(ink, [] as [usize; 0]);
}
