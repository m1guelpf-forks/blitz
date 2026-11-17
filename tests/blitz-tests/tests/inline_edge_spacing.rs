//! The inline-start and inline-end margin, border and padding of non-atomic inline elements take
//! up space in the line (CSS 2.1 §10.3.1): at the start of the element's first fragment and the
//! end of its last (`box-decoration-break: slice`), while vertical spacing doesn't change the
//! line's height (CSS 2.1 §10.8.1).

use blitz_dom::{BaseDocument, QualName, local_name, node::TextLayout, ns};
use blitz_test_harness::Harness;

fn inline<'a>(doc: &'a BaseDocument, selector: &str) -> &'a TextLayout {
    let id = doc.query_selector(selector).unwrap().unwrap();
    doc.get_node(id)
        .unwrap()
        .element_data()
        .unwrap()
        .inline_layout_data
        .as_ref()
        .unwrap()
}

/// The text of an inline layout without the characters that stand in for inline boxes.
fn content_text(layout: &TextLayout) -> String {
    layout
        .text
        .char_indices()
        .filter(|&(i, _)| !layout.is_inline_box_char(i))
        .map(|(_, c)| c)
        .collect()
}

const BOX: &str = "display:inline-block;height:10px";

#[test]
fn horizontal_margin_border_and_padding_take_space() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div><span id='s' style='margin:0 3px;border:2px solid;padding:0 5px'>\
         <i id='a' style='{BOX};width:10px'></i></span><i id='b' style='{BOX};width:10px'></i></div></body>"
    ));
    assert_eq!(harness.layout_rect("#a").x, 10.0);
    assert_eq!(harness.layout_rect("#b").x, 30.0);

    // The span's border box excludes its margins
    let doc = harness.base();
    let rects = doc.node_client_rects(harness.node("#s"));
    assert_eq!(rects.len(), 1);
    assert_eq!((rects[0].x, rects[0].width), (3.0, 24.0));
}

#[test]
fn percentages_resolve_against_the_containing_block() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div style='width:200px'><span style='padding-left:10%'>\
         <i id='a' style='{BOX};width:10px'></i></span></div></body>"
    ));
    assert_eq!(harness.layout_rect("#a").x, 20.0);
}

#[test]
fn start_edge_is_on_first_fragment_and_end_edge_on_last() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div style='width:50px;font-size:10px'><span id='s' style='padding:0 10px'>\
         <i id='a' style='{BOX};width:40px'></i> <i id='b' style='{BOX};width:30px'></i></span>\
         <i id='c' style='{BOX};width:1px'></i></div></body>"
    ));
    let (a, b, c) = (
        harness.layout_rect("#a"),
        harness.layout_rect("#b"),
        harness.layout_rect("#c"),
    );
    assert_eq!(a.x, 10.0);
    assert!(b.y > a.y, "the span should wrap");
    assert_eq!(b.x, 0.0);
    assert_eq!((c.x, c.y), (40.0, b.y));
}

#[test]
fn edges_never_wrap_apart_from_the_content_next_to_them() {
    // The start edge fits on the first line but the content after it doesn't, so both wrap
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div style='width:50px'><i id='a' style='{BOX};width:25px'></i>\
         <span style='padding-left:20px'><i id='b' style='{BOX};width:10px'></i></span></div></body>"
    ));
    let (a, b) = (harness.layout_rect("#a"), harness.layout_rect("#b"));
    assert!(b.y > a.y);
    assert_eq!(b.x, 20.0);

    // The end edge overflows the line rather than wrapping on its own
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div style='width:50px'><span style='padding-right:20px'>\
         <i id='a' style='{BOX};width:40px'></i></span><i id='b' style='{BOX};width:10px'></i></div></body>"
    ));
    let (a, b) = (harness.layout_rect("#a"), harness.layout_rect("#b"));
    assert!(b.y > a.y);
    assert_eq!(b.x, 0.0);
}

#[test]
fn vertical_spacing_does_not_change_line_height() {
    let harness = Harness::from_html(
        "<div id='d' style='font-size:10px;line-height:20px'>\
         <span style='padding:30px 4px;border:5px solid;margin:40px 0'>x</span></div>",
    );
    assert_eq!(harness.layout_rect("#d").height, 20.0);
}

#[test]
fn inlines_without_spacing_have_no_edge_boxes() {
    let harness = Harness::from_html(
        "<div id='d'>a <span style='padding:4px 0;border:3px none;margin:auto'>b</span> c</div>",
    );
    let doc = harness.base();
    let layout = inline(&doc, "#d");
    assert_eq!(layout.text, "a b c");
    assert_eq!(layout.layout.inline_boxes().count(), 0);
}

#[test]
fn white_space_collapses_across_edges() {
    let pad = "style='padding:0 4px'";
    for (html, text) in [
        (format!("x <a {pad}> y </a> z"), "x y z"),
        (format!("x<a {pad}> y </a>z"), "x y z"),
        (format!("x <a {pad}></a> y"), "x y"),
        (format!("x <a {pad}> </a> y"), "x y"),
        (format!(" <a {pad}>\n y\n</a>\n"), "y"),
        (format!("<a {pad}>x <b>y</b> </a> z"), "x y z"),
    ] {
        let harness = Harness::from_html(&format!("<div id='d'>{html}</div>"));
        let doc = harness.base();
        assert_eq!(content_text(inline(&doc, "#d")), text, "{html:?}");
    }
}

#[test]
fn trailing_space_in_an_element_goes_after_its_end_edge() {
    // The space is held back until content follows, so that it isn't kept inside the element's
    // end edge, where it couldn't be removed at the end of the line.
    let harness = Harness::from_html("<div id='d'>a <b style='padding:1px'>x </b>y</div>");
    let doc = harness.base();
    assert_eq!(
        inline(&doc, "#d").text,
        "a \u{FFFC}\u{2060}x\u{2060}\u{FFFC} y"
    );
}

#[test]
fn adding_and_removing_spacing_relays_out_the_line() {
    let mut harness = Harness::from_html(&format!(
        "<style>.a {{ padding: 0 7px }} .b {{ padding: 0 9px }}</style><body style='margin:0'><div>\
         <span id='s'><i id='a' style='{BOX};width:10px'></i></span></div></body>"
    ));
    let span = harness.node("#s");
    harness.pump();
    assert_eq!(harness.layout_rect("#a").x, 0.0);
    for (class, x) in [("a", 7.0), ("b", 9.0), ("", 0.0), ("a", 7.0)] {
        let class_attr = QualName::new(None, ns!(), local_name!("class"));
        harness
            .base_mut()
            .mutate()
            .set_attribute(span, class_attr, class);
        harness.pump();
        assert_eq!(harness.layout_rect("#a").x, x, "class: {class}");
    }
}
