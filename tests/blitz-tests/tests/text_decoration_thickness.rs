//! Text decoration lines are as thick as Blink resolves them (`auto` is a tenth of the font
//! size, lengths are rounded) and are drawn crisply, snapped to whole CSS pixels.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const SCALE: f64 = 2.0;
const WIDTH: u32 = 200;
const HEIGHT: u32 = 100;

/// The rows of a column in the middle of an underlined span (in device pixels), with how red
/// each red row is (1.0 being fully red).
fn red_rows(decoration: &str) -> Vec<(u32, f32)> {
    let html = format!(
        "<html style='background:#fff'><body style='margin:0;font-size:15px;line-height:40px'>\
         <span style='color:transparent;text-decoration:underline #f00;{decoration}'>xxxxxxxx</span>\
         </body></html>"
    );
    let mut doc = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            viewport: Some(Viewport::new(
                WIDTH,
                HEIGHT,
                SCALE as f32,
                ColorScheme::Light,
            )),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, &mut doc, SCALE, WIDTH, HEIGHT, 0, 0),
        WIDTH,
        HEIGHT,
    );
    (0..HEIGHT)
        .filter_map(|y| {
            let i = ((y * WIDTH + 20) * 4) as usize;
            let redness = 1.0 - buffer[i + 1] as f32 / 255.0;
            (redness > 0.0).then_some((y, redness))
        })
        .collect()
}

fn assert_crisp_rows(rows: &[(u32, f32)], count: usize) {
    assert_eq!(rows.len(), count, "{rows:?}");
    assert!(rows.iter().all(|&(_, redness)| redness == 1.0), "{rows:?}");
    // The line starts on a whole CSS pixel
    assert_eq!(rows[0].0 % SCALE as u32, 0, "{rows:?}");
}

#[test]
fn auto_thickness_is_rounded_down_to_whole_css_pixels() {
    // 15px / 10 = 1.5px, drawn 1 CSS px (2 device px) thick
    assert_crisp_rows(&red_rows(""), 2);
}

#[test]
fn length_thickness_is_rounded_to_whole_css_pixels() {
    assert_crisp_rows(&red_rows("text-decoration-thickness:2.6px"), 6);
    assert_crisp_rows(&red_rows("text-decoration-thickness:0.3px"), 2);
}
