//! Non-atomic inline elements paint their backgrounds, borders and box shadows on each of their
//! line box fragments, as if the element were never broken and then sliced into its fragments
//! (`box-decoration-break: slice`).

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 120;

fn render(body: &str) -> Vec<u8> {
    let html = format!(
        "<html style='background:#fff'><body style='margin:0;font-size:10px;line-height:20px'>\
         <div style='width:100px;margin-top:40px'>{body}</div></body></html>"
    );
    let mut doc = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            viewport: Some(Viewport::new(WIDTH, HEIGHT, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, &mut doc, 1.0, WIDTH, HEIGHT, 0, 0),
        WIDTH,
        HEIGHT,
    )
}

fn pixel(buffer: &[u8], x: u32, y: u32) -> [u8; 3] {
    let i = ((y * WIDTH + x) * 4) as usize;
    [buffer[i], buffer[i + 1], buffer[i + 2]]
}

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];

/// An inline-block that gives a span content of a known width, without painting anything
fn spacer(width: u32) -> String {
    format!("<i style='display:inline-block;width:{width}px;height:1px'></i>")
}

#[test]
fn background_covers_padding_but_not_margin() {
    let buffer = render(&format!(
        "<span style='margin-left:10px;padding:10px 20px;background:#f00'>{}</span>",
        spacer(10)
    ));
    // The first line box is at y = 40..60, and the content area around its baseline
    assert_eq!(pixel(&buffer, 5, 50), WHITE, "margin");
    assert_eq!(pixel(&buffer, 15, 50), RED, "left padding");
    assert_eq!(pixel(&buffer, 35, 50), RED, "content");
    assert_eq!(pixel(&buffer, 55, 50), RED, "right padding");
    assert_eq!(pixel(&buffer, 65, 50), WHITE, "after the span");
    // Vertical padding extends the background past the line box without changing its height
    assert_eq!(pixel(&buffer, 35, 38), RED, "top padding");
    assert_eq!(pixel(&buffer, 35, 62), RED, "bottom padding");
}

#[test]
fn start_border_is_on_the_first_fragment_and_end_border_on_the_last() {
    let buffer = render(&format!(
        "<span style='border-left:10px solid #00f;border-right:10px solid #00f;background:#f00'>\
         {} {}</span>",
        spacer(80),
        spacer(80)
    ));
    // First line: border, then background up to the line's end
    assert_eq!(pixel(&buffer, 5, 50), BLUE);
    assert_eq!(pixel(&buffer, 85, 50), RED);
    assert_eq!(
        pixel(&buffer, 95, 50),
        WHITE,
        "no end border on the first line"
    );
    // Second line: background from the line's start, then border
    assert_eq!(
        pixel(&buffer, 5, 70),
        RED,
        "no start border on the second line"
    );
    assert_eq!(pixel(&buffer, 85, 70), BLUE);
}

#[test]
fn rounded_corners_are_only_at_the_ends_of_the_element() {
    let buffer = render(&format!(
        "<span style='padding:0 10px;border-radius:10px;background:#f00'>{} {}</span>",
        spacer(80),
        spacer(80)
    ));
    // The content area is at most 20px tall, so a 10px radius rounds off its corners entirely
    let top_of_content = (40..60).find(|&y| pixel(&buffer, 50, y) == RED).unwrap();
    assert_eq!(
        pixel(&buffer, 0, top_of_content),
        WHITE,
        "rounded start corner"
    );
    assert_eq!(
        pixel(&buffer, 89, top_of_content),
        RED,
        "sliced end of first line"
    );
    assert_eq!(
        pixel(&buffer, 0, top_of_content + 20),
        RED,
        "sliced start of last line"
    );
}

#[test]
fn hidden_inline_elements_paint_no_decorations() {
    let buffer = render(&format!(
        "<span style='visibility:hidden;padding:0 20px;background:#f00'>{}</span>",
        spacer(10)
    ));
    assert_eq!(pixel(&buffer, 30, 50), WHITE);
}

#[test]
fn collapsible_white_space_at_the_end_of_a_line_is_not_painted() {
    let html = |white_space: &str| {
        format!(
            "<span style='white-space:{white_space};background:#f00'>{} {}</span>",
            spacer(80),
            spacer(80)
        )
    };
    // The space after the first spacer ends the first line
    assert_eq!(pixel(&render(&html("normal")), 80, 50), WHITE, "removed");
    assert_eq!(pixel(&render(&html("pre-wrap")), 80, 50), RED, "hanging");
}

#[test]
fn opacity_composites_an_inline_element_as_a_group() {
    // The nested span's background covers its parent's, so the parent's shows only where
    // the nested one isn't. Composited as one group, both are equally translucent.
    let buffer = render(&format!(
        "<span style='opacity:0.5;padding-left:20px;background:#f00'>\
         <span style='background:#f00'>{}</span></span>",
        spacer(20)
    ));
    let translucent_red = [255, 128, 128];
    let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 2);
    assert!(close(pixel(&buffer, 10, 50), translucent_red), "padding");
    assert!(
        close(pixel(&buffer, 30, 50), translucent_red),
        "nested span"
    );
}

#[test]
fn opacity_applies_to_atomic_inlines_in_an_inline_element() {
    let buffer = render(
        "<span style='opacity:0.5'><i style='display:inline-block;width:20px;height:10px;\
         background:#00f'></i></span>",
    );
    let pixel = pixel(&buffer, 10, 50);
    assert!(
        pixel[0].abs_diff(128) <= 2 && pixel[2] == 255,
        "translucent blue: {pixel:?}"
    );
}
