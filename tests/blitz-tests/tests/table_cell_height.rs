//! A table cell's `height` is a minimum (CSS 2.1 §17.5.3): the row grows to fit the cell's
//! content, as Chromium and Firefox do.

use blitz_test_harness::Harness;

fn cell(id: &str, style: &str, content_height: u32) -> String {
    format!(
        "<tr><td id='{id}' style='{style}'><div style='height:{content_height}px'></div></td></tr>"
    )
}

#[test]
fn cell_height_is_a_minimum() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><table cellpadding=0 cellspacing=0 width=300>{}{}{}{}{}</table></body>",
        cell("a", "padding:17px 0 9px;height:1px", 32),
        cell("b", "padding:0 0 9px;height:1px", 16),
        cell("c", "padding:0 0 56px;height:1px", 16),
        cell("d", "padding:10px;height:50px", 16),
        cell("e", "padding:10px;height:50px;box-sizing:border-box", 16),
    ));
    let rects: Vec<_> = ["#a", "#b", "#c", "#d", "#e"]
        .map(|id| harness.layout_rect(id))
        .into_iter()
        .map(|r| (r.y, r.height))
        .collect();
    assert_eq!(
        rects,
        [
            (0.0, 58.0),
            (58.0, 25.0),
            (83.0, 72.0),
            (155.0, 70.0),
            (225.0, 50.0)
        ]
    );
}
