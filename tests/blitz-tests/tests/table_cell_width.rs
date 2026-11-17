//! In the auto table layout algorithm a cell's `width` is its column's preferred width, not a
//! minimum (CSS Tables 3 §3.9.1): the column shrinks to fit the table's width, down to the
//! min-content width of its cells' content. Expected widths are Chromium's.

use blitz_test_harness::Harness;

const BOX: &str = "<div style='width:10px;height:10px'></div>";

fn widths(harness: &Harness, ids: &[&str]) -> Vec<f32> {
    ids.iter().map(|id| harness.layout_rect(id).width).collect()
}

#[test]
fn column_shrinks_below_the_cell_width_to_fit_the_table() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><table width=100 cellpadding=0 cellspacing=0>\
         <tr><td id=a width=300>{BOX}</td></tr></table>\
         <div style='width:300px'><table cellpadding=0 cellspacing=0>\
         <tr><td id=b width=1280>{BOX}</td></tr></table></div></body>"
    ));
    assert_eq!(widths(&harness, &["#a", "#b"]), [100.0, 300.0]);
}

#[test]
fn column_grows_past_the_cell_width_to_fit_its_content() {
    let harness = Harness::from_html(
        "<body style='margin:0'><table cellpadding=0 cellspacing=0>\
         <tr><td id=a width=100><div style='width:200px;height:10px'></div></td></tr></table></body>",
    );
    assert_eq!(widths(&harness, &["#a"]), [200.0]);
}

#[test]
fn cell_widths_stay_preferred_widths_when_they_fit() {
    let harness = Harness::from_html(&format!(
        "<body style='margin:0'><div style='width:500px'>\
         <table cellpadding=0 cellspacing=0><tr><td colspan=2></td></tr>\
         <tr><td id=a width=300>{BOX}</td><td id=b width=100>{BOX}</td></tr></table>\
         <table width='100%' cellpadding=0 cellspacing=0>\
         <tr><td id=c width=200>{BOX}</td><td id=d>{BOX}</td></tr></table></div></body>"
    ));
    assert_eq!(
        widths(&harness, &["#a", "#b", "#c", "#d"]),
        [300.0, 100.0, 200.0, 300.0]
    );
}

#[test]
fn inline_blocks_in_cells_with_a_width_keep_their_baseline() {
    // A button-style link in a nested email layout: measuring the cell's min-content width
    // must not cost the link its baseline, which would add the line's descent below it.
    let harness = Harness::from_html(
        "<body style='margin:0;font:14px/24px sans-serif'><table width='100%' cellpadding=0 cellspacing=0>\
         <tr><td style='padding:24px 12px'><table width=600 cellpadding=0 cellspacing=0 style='width:100%'>\
         <tr><td style='padding:20px 32px'>Header</td></tr>\
         <tr><td id=cell width=536 style='padding:0 32px 32px'>\
         <a id=button style='display:inline-block;padding:12px 22px'>Read today's edition</a></td></tr>\
         </table></td></tr></table></body>",
    );
    let (cell, button) = (harness.layout_rect("#cell"), harness.layout_rect("#button"));
    assert_eq!((button.y, cell.height), (cell.y, button.height + 32.0));
}
