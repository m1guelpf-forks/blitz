use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;
use stylo_taffy::CopyStyleGetters;

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::typed_om::NumericBaseType;
use style::values::computed::calc::CalcPercentageLeaf;
use style::values::computed::length_percentage::{
    CalcLengthPercentage, CalcNode, ComputedLeaf, Unpacked as UnpackedLengthPercentage,
};
use style::values::computed::table::CaptionSide;
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::Optional;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout,
};
use style_traits::values::specified::AllowedNumericType;
use taffy::{
    CoreStyle as _, DetailedGridInfo, LayoutPartialTree as _, MaybeResolve, ResolveOrZero,
    TrackSizingFunction, style_helpers,
};

use thin_vec::ThinVec;

use crate::BaseDocument;
use crate::layout::LayoutPassState;
use crate::node::NodeKind;

use super::construct::{AnonymousBox, create_anonymous_box};

mod collapse;

use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::replaced::is_replaced_element;
use super::resolve_calc_value;
pub use collapse::{CollapsedBorder, CollapsedBorders};

pub struct TableTreeWrapper<'a, 'doc> {
    pub(crate) doc: &'a mut LayoutPassState<'doc>,
    pub(crate) ctx: Arc<TableContext>,
    /// The table's style with its columns' used widths, when `size_columns` set them.
    pub(crate) style: Option<taffy::Style<Atom>>,
}

// Deliberately not `Clone`: `style` may hold raw pointers into `calc_values`.
#[derive(Debug)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    pub cells: Vec<TableCell>,
    /// Absolutely positioned descendants of the table's row groups and rows (and of the
    /// table itself). They take no part in the table grid.
    pub oof_children: Vec<(NodeId, taffy::Position)>,
    pub rows: Vec<TableRow>,
    pub columns: Vec<TableColumn>,
    /// The table's captions, in document order.
    pub captions: Vec<TableCaption>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo<Atom>>>,
    pub border_collapse: BorderCollapse,
    /// Whether the table uses the fixed table layout algorithm.
    pub is_fixed: bool,
    /// The widths of a fixed-layout table's columns, as its first row and `<col>`s set them.
    fixed_columns: Vec<FixedColumn>,
    /// Backing storage for `calc()` track sizes synthesised by the table layout code.
    /// Taffy stores calc values as raw pointers, so these must outlive the `style`.
    #[allow(dead_code)]
    calc_values: Vec<LengthPercentage>,
}

// #[derive(Debug, Clone, Eq, PartialEq)]
// pub enum TableItemKind {
//     Row,
//     Cell,
// }

#[derive(Debug, Clone)]
pub struct TableCell {
    // kind: TableItemKind,
    pub node_id: NodeId,
    /// Index (into `TableContext::rows`) of the row the cell originates in
    pub row: u16,
    /// Index of the column the cell originates in
    pub column: u16,
    /// How many columns the cell spans
    pub span: u16,
    /// How many rows the cell spans
    pub rowspan: u16,
    /// The borders collapsed around the cell, when its table collapses borders
    pub collapsed: Option<CollapsedBorders>,
    /// The cell's percentage `width`, as a fraction, which the auto table layout algorithm
    /// gives its column
    percent_width: Option<f32>,
    /// Whether the cell has a length `width`, which only sets its column's preferred width (see
    /// `compute_child_layout`)
    has_length_width: bool,
    /// Whether the cell has a length `height`, which is only a minimum (see
    /// `compute_child_layout`)
    has_length_height: bool,
    style: taffy::Style<Atom>,
}

/// Tracks the current column position while walking the table's cells, so that
/// cells are assigned the same columns Taffy's dense auto-placement will give them
/// (skipping columns still occupied by rowspan cells from earlier rows).
#[derive(Debug, Default)]
struct ColumnCursor {
    /// The column the next cell in the current row will be placed in
    col: u16,
    /// The total number of columns seen so far
    num_columns: u16,
    /// For each column, the number of rows (from the last one started) a rowspan cell occupies
    /// it for
    rowspans: Vec<u16>,
}

impl ColumnCursor {
    fn start_row(&mut self) {
        self.col = 0;
        for remaining in self.rowspans.iter_mut() {
            *remaining = remaining.saturating_sub(1);
        }
    }

    /// Advance past occupied columns, returning the column of the next cell
    fn next_free(&mut self) -> u16 {
        while self.rowspans.get(self.col as usize).is_some_and(|r| *r > 0) {
            self.col += 1;
        }
        self.col
    }

    fn place(&mut self, colspan: u16, rowspan: u16) {
        let end = self.col + colspan;
        if self.rowspans.len() < end as usize {
            self.rowspans.resize(end as usize, 0);
        }
        for remaining in &mut self.rowspans[self.col as usize..end as usize] {
            *remaining = rowspan;
        }
        self.col = end;
        self.num_columns = self.num_columns.max(end);
    }
}

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub node_id: NodeId,
    /// The column group (if any) that contains the column
    pub group: Option<NodeId>,
}

/// A `display: table-caption` child of the table. Captions are laid out above the table's grid
/// (or below it with `caption-side: bottom`) as wide as the table, which they make at least as
/// wide as their min-content width. The table's own background and borders surround the grid
/// alone. This is Chromium's table wrapper box, which is the table box itself there.
#[derive(Debug, Clone)]
pub struct TableCaption {
    pub node_id: NodeId,
    /// The caption sits below the grid (`caption-side: bottom`).
    pub below: bool,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: NodeId,
    /// The row group (if any) that contains the row
    pub group: Option<NodeId>,
    /// The row's specified `height`, if it is a length
    pub height: Option<f32>,
    /// Whether the row or any cell originating in it (and spanning only this row) has a
    /// specified length `height`. Such rows do not receive excess table height when there
    /// are unconstrained rows to receive it instead.
    pub is_constrained: bool,
    /// Whether the row (or its group) is `visibility: collapse`: it is laid out with no height,
    /// its cells still sizing the columns.
    pub is_collapsed: bool,
}

/// The used width of one border side: border widths are not adjusted for
/// border-style in computed styles, so a border with `none`/`hidden` style
/// must be treated as zero-width.
/// The writing mode the table's grid runs in (the table's own)
#[cfg_attr(not(feature = "writing-mode"), allow(unused_variables))]
fn table_wm(doc: &BaseDocument, table_root_node_id: NodeId) -> WritingMode {
    #[cfg(feature = "writing-mode")]
    {
        if doc.try_root_element().map(|root| root.id) == Some(table_root_node_id) {
            doc.root_layout_wm(table_root_node_id)
        } else {
            doc.nodes[table_root_node_id].writing_mode()
        }
    }
    #[cfg(not(feature = "writing-mode"))]
    {
        WritingMode::empty()
    }
}

/// Convert a table part's style into the table's writing mode
fn table_taffy_style(style: &ComputedValues, table_wm: WritingMode) -> taffy::Style<Atom> {
    stylo_taffy::to_taffy_style_in(style, table_wm)
}

/// Build a `calc(<percent> + <length>)` track sizing function. The calc value is
/// boxed and stored in `calc_values` so that the raw pointer Taffy holds stays valid.
fn percent_plus_length(
    calc_values: &mut Vec<LengthPercentage>,
    percent: f32,
    length: f32,
) -> TrackSizingFunction {
    let node = CalcNode::Sum(
        vec![
            CalcNode::Leaf(ComputedLeaf::Percentage(CalcPercentageLeaf::new(
                percent,
                Optional::Some(NumericBaseType::Length),
            ))),
            CalcNode::Leaf(ComputedLeaf::Length(Length::new(length))),
        ]
        .into(),
    );
    let value = LengthPercentage::new_calc(node, AllowedNumericType::NonNegative);
    let dim = match value.unpack() {
        UnpackedLengthPercentage::Calc(calc) => {
            let ptr = calc as *const CalcLengthPercentage as *const ();
            // SAFETY: the pointer targets the heap allocation owned by `value`, which
            // is kept alive by `calc_values` for as long as the TableContext exists.
            unsafe { taffy::Dimension::from_raw(taffy::CompactLength::calc(ptr)) }
        }
        UnpackedLengthPercentage::Length(len) => style_helpers::length(len.px()),
        UnpackedLengthPercentage::Percentage(p) => style_helpers::percent(p.0),
    };
    calc_values.push(value);
    dim.into()
}

/// Builds the table's grid from its rows and cells. Anonymous rows generated
/// for misparented content directly in the table are added to `anonymous_boxes`
/// (the table's to own); row groups and rows own the ones generated in them.
pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: NodeId,
    anonymous_boxes: &mut ThinVec<NodeId>,
) -> (TableContext, Vec<NodeId>) {
    let wm = table_wm(doc, table_root_node_id);
    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = table_taffy_style(&stylo_styles, wm);
    style.item_is_table = true;
    // Use `dense` row-flow so that each cell scans the row from its
    // leftmost column for the first free track. Without `dense`,
    // `place_definite_secondary_axis_item` keeps a per-item secondary
    // cursor across rows, which means cells in later rows do not
    // backfill columns freed up by rowspan cells from earlier rows.
    style.grid_auto_flow = taffy::GridAutoFlow::RowDense;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    // The fixed table layout algorithm only applies when the table has a non-auto width
    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => !style.size.width.is_auto(),
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;
    let mut spacing = (border_spacing.width.px(), border_spacing.height.px());

    drop(stylo_styles);

    let mut columns: Vec<TableColumn> = Vec::new();
    let mut column_sizes: Vec<taffy::TrackSizingFunction> = Vec::new();
    for child_id in children.iter().copied() {
        collect_columns(doc, child_id, wm, &mut columns, &mut column_sizes);
    }
    // Percentage column widths only take effect in the fixed table layout algorithm
    if !is_fixed {
        for column in column_sizes.iter_mut() {
            if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = style_helpers::auto();
            }
        }
    }
    let mut calc_values: Vec<LengthPercentage> = Vec::new();
    let mut builder = TableBuilder {
        wm,
        is_fixed,
        border_collapse,
        row: 0,
        cursor: ColumnCursor::default(),
        cells: Vec::new(),
        oof_children: Vec::new(),
        captions: Vec::new(),
        rows: Vec::new(),
        columns: column_sizes,
        percent_columns: Vec::new(),
    };
    // Header row groups are laid out before other rows, and footer row groups after
    for order in [
        RowGroupOrder::Header,
        RowGroupOrder::Body,
        RowGroupOrder::Footer,
    ] {
        if order == RowGroupOrder::Body {
            builder.collect_children(
                doc,
                table_root_node_id,
                &children,
                TableLevel::Table,
                anonymous_boxes,
            );
            continue;
        }
        for child_id in children.iter().copied() {
            if table_part(doc, child_id) == TablePart::RowGroup(order) {
                builder.collect_row_group(doc, child_id);
            }
        }
    }
    let TableBuilder {
        cursor,
        mut cells,
        oof_children,
        captions,
        rows,
        columns: mut column_sizes,
        percent_columns,
        ..
    } = builder;
    let remaining_column = if is_fixed {
        style_helpers::minmax(style_helpers::length(0.0), style_helpers::fr(1.0))
    } else {
        style_helpers::auto()
    };
    // Trailing `<col>` columns which contain no cells and have no definite width
    // (i.e. would be zero-width) are dropped, so they don't add border-spacing.
    while column_sizes.len() > cursor.num_columns as usize
        && column_sizes
            .last()
            .is_some_and(|c| c.min.into_raw().tag() == taffy::CompactLength::AUTO_TAG)
    {
        column_sizes.pop();
        columns.pop();
    }
    let num_columns = cursor.num_columns.max(column_sizes.len() as u16);
    column_sizes.resize(num_columns as usize, remaining_column);
    // A table without columns has no border spacing, as in Blink: it is as big as its borders
    // and padding.
    if num_columns == 0 {
        spacing = (0.0, 0.0);
    }
    let mut fixed_columns = Vec::new();
    if is_fixed {
        // In the fixed table layout algorithm, percentage column widths resolve against
        // the table's content width minus the horizontal border-spacing between columns,
        // whereas Taffy resolves percentage tracks against the container's inner width
        // (from which only the outer border-spacing has been removed via padding). Fold
        // the inner border-spacing into the percentage using `calc(N% - N * spacing)`.
        let spacing_x = match border_collapse {
            BorderCollapse::Separate => spacing.0,
            BorderCollapse::Collapse => 0.0,
        };
        let inner_spacing = spacing_x * num_columns.saturating_sub(1) as f32;
        fixed_columns = column_sizes
            .iter()
            .map(|column| {
                let max = column.max.into_raw();
                match max.tag() {
                    taffy::CompactLength::LENGTH_TAG => FixedColumn::Length(max.value()),
                    taffy::CompactLength::PERCENT_TAG => FixedColumn::Percent {
                        percent: max.value(),
                        extra: 0.0,
                    },
                    taffy::CompactLength::AUTO_TAG | taffy::CompactLength::FR_TAG => {
                        FixedColumn::Auto
                    }
                    _ => FixedColumn::Calc,
                }
            })
            .collect();
        for (col_idx, percent, extra) in &percent_columns {
            if let Some(column) = fixed_columns.get_mut(*col_idx as usize) {
                *column = FixedColumn::Percent {
                    percent: *percent,
                    extra: *extra,
                };
            }
        }
        let mut percent_track = |percent: f32, extra: f32| -> TrackSizingFunction {
            let extra = extra - percent * inner_spacing;
            if extra == 0.0 {
                style_helpers::percent(percent)
            } else {
                percent_plus_length(&mut calc_values, percent, extra)
            }
        };

        for column in column_sizes.iter_mut() {
            if column.max.is_auto() {
                *column = remaining_column;
            } else if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = percent_track(column.max.into_raw().value(), 0.0);
            }
        }

        // Percentage widths on cells apply to the cell's content box, so the
        // cell's horizontal padding and border must be added to the column width.
        for (col_idx, percent, extra) in percent_columns {
            if let Some(column) = column_sizes.get_mut(col_idx as usize) {
                *column = percent_track(percent, extra);
            }
        }
    }

    style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
    // A row's `height` is a minimum: the row grows to fit the content of its cells.
    // Excess table height is distributed to unconstrained rows (via the stretching of
    // `auto` tracks) if there are any, and to all rows otherwise. So constrained rows
    // use a `max-content` (rather than `auto`) maximum when there are unconstrained rows.
    let has_unconstrained_rows = rows.iter().any(|row| !row.is_constrained);
    style.grid_template_rows = rows
        .iter()
        .map(|row| {
            if row.is_collapsed {
                return style_helpers::minmax(
                    style_helpers::length(0.0),
                    style_helpers::length(0.0),
                );
            }
            let min = match row.height {
                Some(height) => style_helpers::length(height),
                None => style_helpers::auto(),
            };
            let max = if row.is_constrained && has_unconstrained_rows {
                style_helpers::max_content()
            } else {
                style_helpers::auto()
            };
            style_helpers::minmax(min, max)
        })
        .collect();

    style.gap = match border_collapse {
        BorderCollapse::Separate => {
            // In the separated borders model, `border-spacing` also applies between
            // the table border and the outermost cells, in addition to between cells.
            let (spacing_x, spacing_y) = spacing;
            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
            style.padding = taffy::Rect {
                left: style_helpers::length(padding.left + spacing_x),
                right: style_helpers::length(padding.right + spacing_x),
                top: style_helpers::length(padding.top + spacing_y),
                bottom: style_helpers::length(padding.bottom + spacing_y),
            };
            taffy::Size {
                width: style_helpers::length(spacing_x),
                height: style_helpers::length(spacing_y),
            }
        }
        // In the collapsed borders model the cells touch, each with half of the borders
        // between them, and the table has half of the outer ones and no padding.
        BorderCollapse::Collapse => {
            let collapsed = collapse::collapse(doc, table_root_node_id, &cells, &rows, num_columns);
            let mut outer = taffy::Rect {
                top: 0.0f32,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            };
            for (cell, borders) in cells.iter_mut().zip(collapsed) {
                let halves = borders.halves();
                if cell.row == 0 {
                    outer.top = outer.top.max(halves.top);
                }
                if usize::from(cell.row + cell.rowspan) >= rows.len() {
                    outer.bottom = outer.bottom.max(halves.bottom);
                }
                if cell.column == 0 {
                    outer.left = outer.left.max(halves.left);
                }
                if cell.column + cell.span >= num_columns {
                    outer.right = outer.right.max(halves.right);
                }
                cell.style.border = halves.map(style_helpers::length);
                cell.collapsed = Some(borders);
                if let Some(element) = doc.nodes[cell.node_id].element_data_mut() {
                    element.collapsed_border = Some(halves);
                }
            }
            style.border = outer.map(style_helpers::length);
            style.padding = taffy::Rect::ZERO.map(style_helpers::length);
            taffy::Size::ZERO.map(style_helpers::length)
        }
    };
    if border_collapse == BorderCollapse::Separate {
        for cell in &cells {
            if let Some(element) = doc.nodes[cell.node_id].element_data_mut() {
                element.collapsed_border = None;
            }
        }
    }

    let layout_children = captions
        .iter()
        .map(|caption| caption.node_id)
        .chain(cells.iter().map(|cell| cell.node_id))
        .chain(oof_children.iter().map(|(node_id, _)| *node_id))
        .collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            cells,
            oof_children,
            rows,
            columns,
            captions,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            is_fixed,
            fixed_columns,
            calc_values,
        },
        layout_children,
    )
}

/// Collect `<col>` elements (and `display: table-column` elements) along with their
/// widths, which take precedence over cell widths in the fixed table layout algorithm.
/// Columns without a specified width are recorded as `auto`.
fn collect_columns(
    doc: &mut BaseDocument,
    node_id: NodeId,
    wm: WritingMode,
    columns: &mut Vec<TableColumn>,
    column_sizes: &mut Vec<TrackSizingFunction>,
) {
    let node = &doc.nodes[node_id];
    if !node.is_element() {
        return;
    }
    let Some(display) = node.display_style() else {
        return;
    };

    match display.inside() {
        DisplayInside::TableColumnGroup => {
            let first_column = columns.len();
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_columns(doc, child_id, wm, columns, column_sizes);
            }
            doc.nodes[node_id].children = children;
            for column in &mut columns[first_column..] {
                column.group.get_or_insert(node_id);
            }
        }
        DisplayInside::TableColumn => {
            let style = table_taffy_style(&node.primary_styles().unwrap(), wm);
            let span: u16 = node
                .attr(local_name!("span"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.max(1))
                .unwrap_or(1);
            let column: TrackSizingFunction = match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    // A definite `max-width` clamps the column width; a zero width is treated as auto
                    let mut width = style.size.width.value();
                    if style.max_size.width.into_raw().tag() == taffy::CompactLength::LENGTH_TAG {
                        width = width.min(style.max_size.width.into_raw().value());
                    }
                    if width > 0.0 {
                        style_helpers::length(width)
                    } else {
                        style_helpers::auto()
                    }
                }
                taffy::CompactLength::PERCENT_TAG => {
                    style_helpers::percent(style.size.width.value())
                }
                // Browsers treat calc() widths on columns as auto
                _ => style_helpers::auto(),
            };
            // A definite `min-width` on a column acts as a floor on its width
            let column = match style.min_size.width.into_raw().tag() {
                taffy::CompactLength::LENGTH_TAG if column.max.is_auto() => style_helpers::minmax(
                    style_helpers::length(style.min_size.width.into_raw().value()),
                    style_helpers::auto(),
                ),
                _ => column,
            };
            for _ in 0..span {
                columns.push(TableColumn {
                    node_id,
                    group: None,
                });
                column_sizes.push(column);
            }
        }
        _ => {}
    }
}

/// What a box is in the table model, as the child of a table, row group or row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TablePart {
    /// It generates no box: `display: none`, a comment.
    Nothing,
    /// White space only, which is dropped unless it neighbors content that's wrapped in an
    /// anonymous box (CSS 2.1 §17.2.1 1.2–1.3), whatever its `white-space`.
    Space,
    /// Absolutely positioned: laid out by its containing block, not the table.
    OutOfFlow,
    /// `display: contents`: its children take its place.
    Contents,
    RowGroup(RowGroupOrder),
    Row,
    Cell,
    /// Columns, which `collect_columns` handles.
    Ignored,
    Caption,
    /// Anything else (a block, inline content, a nested table...), which belongs in a cell.
    Other,
}

/// Where a row group's rows go: header groups before the others, footer groups after.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowGroupOrder {
    Header,
    Body,
    Footer,
}

/// The parent whose children are being collected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableLevel {
    Table,
    RowGroup,
    Row,
}

impl TableLevel {
    /// Whether `part` is a proper child at this level, rather than one an anonymous row or
    /// cell wraps.
    fn contains(self, part: TablePart) -> bool {
        match self {
            Self::Table => matches!(
                part,
                TablePart::RowGroup(_)
                    | TablePart::Row
                    | TablePart::Ignored
                    | TablePart::Caption
                    | TablePart::Contents
            ),
            Self::RowGroup => matches!(part, TablePart::Row | TablePart::Contents),
            Self::Row => matches!(part, TablePart::Cell | TablePart::Contents),
        }
    }
}

fn table_part(doc: &BaseDocument, node_id: NodeId) -> TablePart {
    let node = &doc.nodes[node_id];
    match node.data.kind() {
        NodeKind::Element | NodeKind::AnonymousBlock => {}
        NodeKind::Text if node.is_whitespace_node() => return TablePart::Space,
        NodeKind::Text => return TablePart::Other,
        _ => return TablePart::Nothing,
    }
    let Some(display) = node.display_style() else {
        #[cfg(feature = "tracing")]
        tracing::info!("Ignoring table descendent because it has no styles");
        return TablePart::Nothing;
    };
    if display.inside() == DisplayInside::Contents {
        return TablePart::Contents;
    }
    if display.outside() == DisplayOutside::None {
        return TablePart::Nothing;
    }
    if node
        .primary_styles()
        .is_some_and(|style| style.clone_position().is_absolutely_positioned())
    {
        return TablePart::OutOfFlow;
    }
    // A replaced element is never a table box (CSS Display 3 §2.4)
    if node
        .element_data()
        .is_some_and(|element| is_replaced_element(&element.name.local))
    {
        return TablePart::Other;
    }
    match display.inside() {
        DisplayInside::TableHeaderGroup => TablePart::RowGroup(RowGroupOrder::Header),
        DisplayInside::TableRowGroup => TablePart::RowGroup(RowGroupOrder::Body),
        DisplayInside::TableFooterGroup => TablePart::RowGroup(RowGroupOrder::Footer),
        DisplayInside::TableRow => TablePart::Row,
        DisplayInside::TableCell => TablePart::Cell,
        DisplayInside::TableColumn | DisplayInside::TableColumnGroup => TablePart::Ignored,
        _ if display.outside() == DisplayOutside::TableCaption => TablePart::Caption,
        _ => TablePart::Other,
    }
}

/// Boxes that aren't layout children are never visited by box construction, so their
/// construction damage is cleared here.
fn remove_construct_damage(doc: &mut BaseDocument, node_id: NodeId) {
    doc.nodes[node_id].remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
}

/// Collects a table's rows and cells, and the column widths its first row's cells set.
struct TableBuilder {
    wm: WritingMode,
    is_fixed: bool,
    border_collapse: BorderCollapse,
    /// The number of rows so far (the 1-based grid line of the current row)
    row: u16,
    cursor: ColumnCursor,
    cells: Vec<TableCell>,
    oof_children: Vec<(NodeId, taffy::Position)>,
    captions: Vec<TableCaption>,
    rows: Vec<TableRow>,
    columns: Vec<TrackSizingFunction>,
    /// Percentage widths set on first-row cells: (column index, percentage, padding + border)
    percent_columns: Vec<(u16, f32, f32)>,
}

impl TableBuilder {
    /// Collects `children`, the children of `parent_id` at `level` of the table model.
    ///
    /// Content that is misparented there is fixed up as CSS 2.1 §17.2.1 describes: each run
    /// of consecutive children that don't belong in a row is wrapped in an anonymous cell,
    /// and each run that doesn't belong in a table or row group in an anonymous row (where
    /// the same applies). `owned` records the anonymous boxes. Header and footer row groups
    /// directly in the table are left to their own passes (see `build_table_context`).
    fn collect_children(
        &mut self,
        doc: &mut BaseDocument,
        parent_id: NodeId,
        children: &[NodeId],
        level: TableLevel,
        owned: &mut ThinVec<NodeId>,
    ) {
        let parts: Vec<TablePart> = children.iter().map(|id| table_part(doc, *id)).collect();
        // Whether the nearest sibling in flow on that side is wrapped in an anonymous box
        let wrapped = |siblings: &[TablePart], reverse: bool| {
            let in_flow = |part: &&TablePart| {
                !matches!(
                    part,
                    TablePart::Nothing | TablePart::OutOfFlow | TablePart::Space
                )
            };
            let nearest = if reverse {
                siblings.iter().rev().find(in_flow)
            } else {
                siblings.iter().find(in_flow)
            };
            nearest.is_some_and(|part| !level.contains(*part))
        };
        let mut run: ThinVec<NodeId> = ThinVec::new();
        for (at, child_id) in children.iter().copied().enumerate() {
            match (parts[at], level) {
                (TablePart::Nothing, _) => remove_construct_damage(doc, child_id),
                (TablePart::Space, _) => {
                    if wrapped(&parts[..at], true) || wrapped(&parts[at + 1..], false) {
                        run.push(child_id);
                    }
                }
                (TablePart::OutOfFlow, _) => {
                    let position = doc.nodes[child_id]
                        .primary_styles()
                        .unwrap()
                        .clone_position();
                    self.oof_children
                        .push((child_id, stylo_taffy::convert::position(position)));
                }
                (TablePart::Contents, _) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    remove_construct_damage(doc, child_id);
                    self.collect_owned(doc, child_id, level);
                }
                (TablePart::RowGroup(order), TableLevel::Table) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    if order == RowGroupOrder::Body {
                        self.collect_row_group(doc, child_id);
                    }
                }
                (TablePart::Ignored, TableLevel::Table) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    remove_construct_damage(doc, child_id);
                }
                (TablePart::Caption, TableLevel::Table) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    let below = doc.nodes[child_id].primary_styles().is_some_and(|style| {
                        style.get_inherited_table().caption_side == CaptionSide::Bottom
                    });
                    self.captions.push(TableCaption {
                        node_id: child_id,
                        below,
                    });
                }
                (TablePart::Row, TableLevel::Table | TableLevel::RowGroup) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    self.collect_row(doc, child_id);
                }
                (TablePart::Cell, TableLevel::Row) => {
                    self.wrap(doc, parent_id, level, &mut run, owned);
                    self.push_cell(doc, child_id);
                }
                _ => run.push(child_id),
            }
        }
        self.wrap(doc, parent_id, level, &mut run, owned);
    }

    /// Wraps `run` (children of `parent_id` that don't belong at `level`) in an anonymous row
    /// or cell, and collects that.
    fn wrap(
        &mut self,
        doc: &mut BaseDocument,
        parent_id: NodeId,
        level: TableLevel,
        run: &mut ThinVec<NodeId>,
        owned: &mut ThinVec<NodeId>,
    ) {
        if run.is_empty() {
            return;
        }
        let kind = match level {
            TableLevel::Row => AnonymousBox::TableCell,
            TableLevel::Table | TableLevel::RowGroup => AnonymousBox::TableRow,
        };
        let anonymous_id = create_anonymous_box(doc, parent_id, kind);
        doc.nodes[anonymous_id].children = std::mem::take(run);
        owned.push(anonymous_id);
        match level {
            TableLevel::Row => self.push_cell(doc, anonymous_id),
            TableLevel::Table | TableLevel::RowGroup => self.collect_row(doc, anonymous_id),
        }
    }

    /// Collects the children of `node_id` (a row group, row or `display: contents` box at
    /// `level`), replacing the anonymous boxes it owned with the ones generated now.
    fn collect_owned(&mut self, doc: &mut BaseDocument, node_id: NodeId, level: TableLevel) {
        for anonymous_id in std::mem::take(&mut doc.nodes[node_id].anonymous_blocks) {
            doc.deallocate_anonymous_block(anonymous_id);
        }
        let children = std::mem::take(&mut doc.nodes[node_id].children);
        let mut owned = ThinVec::new();
        self.collect_children(doc, node_id, &children, level, &mut owned);
        doc.nodes[node_id].children = children;
        doc.nodes[node_id].anonymous_blocks = owned;
    }

    fn collect_row_group(&mut self, doc: &mut BaseDocument, group_id: NodeId) {
        remove_construct_damage(doc, group_id);
        let first_row = self.rows.len();
        self.collect_owned(doc, group_id, TableLevel::RowGroup);
        for row in &mut self.rows[first_row..] {
            row.group.get_or_insert(group_id);
        }
    }

    fn collect_row(&mut self, doc: &mut BaseDocument, row_id: NodeId) {
        remove_construct_damage(doc, row_id);
        self.row += 1;
        self.cursor.start_row();

        let style = doc.nodes[row_id].primary_styles();
        let height = style.as_ref().and_then(|style| {
            let height = stylo_taffy::convert::dimension(style.get_height());
            (height.tag() == taffy::CompactLength::LENGTH_TAG).then(|| height.value())
        });
        // `visibility` inherits, so a collapsed row group's rows are collapsed too.
        let is_collapsed = style.is_some_and(|style| {
            style.get_inherited_box().visibility
                == style::properties::longhands::visibility::computed_value::T::Collapse
        });
        self.rows.push(TableRow {
            node_id: row_id,
            group: None,
            height,
            is_constrained: height.is_some(),
            is_collapsed,
        });

        self.collect_owned(doc, row_id, TableLevel::Row);
    }

    fn push_cell(&mut self, doc: &BaseDocument, node_id: NodeId) {
        let node = &doc.nodes[node_id];
        let stylo_style = &node.primary_styles().unwrap();
        let colspan: u16 = node
            .attr(local_name!("colspan"))
            .and_then(|val| val.parse().ok())
            .unwrap_or(1);
        let rowspan: u16 = node
            .attr(local_name!("rowspan"))
            .and_then(|val| val.parse::<u16>().ok())
            .map(|v| v.clamp(1, 65534))
            .unwrap_or(1);
        let mut style = table_taffy_style(stylo_style, self.wm);
        let col = self.cursor.next_free();

        // In the collapsed borders model the cell's borders are its share of the borders
        // collapsed around it, which `build_table_context` sets once every cell is known
        if self.border_collapse == BorderCollapse::Collapse {
            style.border = taffy::Rect::ZERO.map(style_helpers::length);
        }

        // In the fixed table layout algorithm the widths of columns are not
        // affected by cell contents, so cells must not impose a min-content
        // floor on their tracks.
        if self.is_fixed {
            style.min_size.width = style_helpers::length(0.0);
        }

        // Column widths from `<col>` elements take precedence over cell widths
        let col_needs_width =
            (col as usize) >= self.columns.len() || self.columns[col as usize].max.is_auto();
        if self.row == 1 && col_needs_width {
            let column: TrackSizingFunction = match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    let len = style.size.width.value();
                    let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                    let border = style.border.resolve_or_zero(None, resolve_calc_value);
                    let width = match style.box_sizing {
                        taffy::BoxSizing::ContentBox => {
                            len + padding.left + padding.right + border.left + border.right
                        }
                        taffy::BoxSizing::BorderBox => len,
                    };
                    if self.is_fixed {
                        style_helpers::length(width)
                    } else {
                        // In the auto table layout algorithm a cell's width is the
                        // column's preferred width: the column shrinks below it (down to
                        // its content's min-content width) to fit the table's width.
                        style_helpers::minmax(
                            style_helpers::min_content(),
                            style_helpers::length(width),
                        )
                    }
                }
                taffy::CompactLength::PERCENT_TAG => {
                    if self.is_fixed {
                        let extra = match style.box_sizing {
                            taffy::BoxSizing::ContentBox => {
                                let padding =
                                    style.padding.resolve_or_zero(None, resolve_calc_value);
                                let border = style.border.resolve_or_zero(None, resolve_calc_value);
                                padding.left + padding.right + border.left + border.right
                            }
                            taffy::BoxSizing::BorderBox => 0.0,
                        };
                        // Resolved in `build_table_context` once the column count is known
                        self.percent_columns
                            .push((col, style.size.width.value(), extra));
                        style_helpers::percent(style.size.width.value())
                    } else {
                        style_helpers::auto()
                    }
                }
                taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                // Dimension values are always length, percentage, auto or calc(),
                // so any other tag is a calc() value. Pass it through so that
                // Taffy resolves it against the table's inner width.
                _ => style.size.width.into(),
            };
            if (col as usize) < self.columns.len() {
                if !column.max.is_auto() {
                    self.columns[col as usize] = column;
                }
            } else {
                self.columns.resize(col as usize, style_helpers::auto());
                self.columns.push(column);
            }
        }

        // The margin properties do not apply to table-internal elements
        style.margin = taffy::Rect::ZERO.map(style_helpers::length);

        // Let Taffy auto-place the column. Combined with
        // `grid_auto_flow: RowDense` set on the table root, each cell
        // scans from the first track in its row for a free position,
        // which makes cells automatically skip columns occupied by
        // rowspan cells from earlier rows.
        style.grid_column = taffy::Line {
            start: style_helpers::auto(),
            end: style_helpers::span(colspan),
        };
        style.grid_row = taffy::Line {
            start: style_helpers::line(self.row as i16),
            end: style_helpers::span(rowspan),
        };
        if rowspan == 1 && style.size.height.tag() == taffy::CompactLength::LENGTH_TAG {
            if let Some(row) = self.rows.last_mut() {
                row.is_constrained = true;
            }
        }
        // A cell's height is a minimum: the cell (and its rows with it) grows to fit its
        // content rather than letting it overflow into the next row.
        let has_length_height = style.size.height.tag() == taffy::CompactLength::LENGTH_TAG;
        if has_length_height {
            let height = style.size.height.value();
            let min = style.min_size.height.into_raw();
            if min.tag() != taffy::CompactLength::LENGTH_TAG || min.value() < height {
                style.min_size.height = style_helpers::length(height);
            }
            style.size.height = style_helpers::auto();
        }

        let has_length_width = style.size.width.tag() == taffy::CompactLength::LENGTH_TAG;
        let percent_width = (style.size.width.tag() == taffy::CompactLength::PERCENT_TAG)
            .then(|| style.size.width.value());
        style.size.width = style_helpers::auto();
        self.cells.push(TableCell {
            node_id,
            row: self.row.saturating_sub(1),
            column: col,
            span: colspan.max(1),
            rowspan,
            collapsed: None,
            percent_width,
            has_length_width,
            has_length_height,
            style,
        });

        self.cursor.place(colspan, rowspan);
    }
}

/// Chromium's `kTableMaxInlineSize`: the max-content width of a fixed-layout table with a
/// percentage width, so that such a table fills whatever its percentage resolves against.
const TABLE_MAX_WIDTH: f32 = 1_000_000.0;

/// What the auto table layout algorithm knows about a column (CSS Tables 3 §3.9.1, as Chromium
/// keeps it in `TableTypes::Column`).
#[derive(Debug, Clone, Copy, Default)]
struct ColumnWidths {
    min: f32,
    max: f32,
    /// A percentage width, as a fraction.
    percent: Option<f32>,
    /// Whether a length width constrains the column.
    constrained: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ColumnKind {
    Percent,
    Constrained,
    Auto,
}

impl ColumnWidths {
    fn kind(&self) -> ColumnKind {
        match (self.percent, self.constrained) {
            (Some(_), _) => ColumnKind::Percent,
            (None, true) => ColumnKind::Constrained,
            (None, false) => ColumnKind::Auto,
        }
    }

    /// Takes in a cell that spans only this column: the widest min-content width, and the max
    /// width of the constrained cells when there are any.
    fn encompass(&mut self, cell: ColumnWidths) {
        self.min = self.min.max(cell.min);
        self.max = match (self.constrained, cell.constrained) {
            (false, true) => cell.max,
            (true, false) => self.max,
            _ => self.max.max(cell.max),
        }
        .max(self.min);
        self.constrained |= cell.constrained;
        self.percent = match (self.percent, cell.percent) {
            (Some(own), Some(cell)) => Some(own.max(cell)),
            (own, cell) => own.or(cell),
        };
    }

    /// The column's width at its percentage of `target`, never below its min-content width.
    fn percent_width(&self, target: f32) -> f32 {
        self.percent
            .map_or(self.min, |percent| (percent * target).max(self.min))
    }
}

/// The widths `columns` get from `target`, as Chromium shares it out
/// (`DistributeInlineSizeToComputedInlineSizeAuto`, CSS Tables 3 §3.9.3). There are four
/// guesses: every column at its min-content width; percentage columns at their percentage too;
/// constrained columns at their width too; auto columns at their max-content width too.
/// Between two guesses each column goes as far from the one towards the other as the table
/// does. Beyond the last, the extra goes to the auto columns in proportion to their
/// max-content widths, or (when `grow_constrained`) to the constrained columns in proportion to
/// their widths, or else to the percentage columns in proportion to their percentages, and
/// equally among them when those are all zero.
fn distribute(target: f32, columns: &[ColumnWidths], grow_constrained: bool) -> Vec<f32> {
    use ColumnKind::*;
    let guesses: [Vec<f32>; 4] = std::array::from_fn(|guess| {
        columns
            .iter()
            .map(|column| match (column.kind(), guess) {
                (_, 0) => column.min,
                (Percent, _) => column.percent_width(target),
                (Constrained, 1) | (Auto, 1 | 2) => column.min,
                (Constrained, _) | (Auto, _) => column.max,
            })
            .collect()
    });
    let sums = guesses.each_ref().map(|guess| guess.iter().sum::<f32>());
    if target <= sums[0] {
        return guesses[0].clone();
    }
    if let Some(guess) = (1..4).find(|guess| target <= sums[*guess]) {
        let (from, to) = (&guesses[guess - 1], &guesses[guess]);
        let share = (target - sums[guess - 1]) / (sums[guess] - sums[guess - 1]);
        return from
            .iter()
            .zip(to)
            .map(|(from, to)| from + (to - from) * share)
            .collect();
    }
    let mut widths = guesses[3].clone();
    let extra = target - sums[3];
    let growing = [Auto, Constrained, Percent]
        .into_iter()
        .filter(|kind| *kind != Constrained || grow_constrained)
        .map(|kind| {
            (0..columns.len())
                .filter(|c| columns[*c].kind() == kind)
                .collect::<Vec<_>>()
        })
        .find(|members| !members.is_empty());
    if let Some(members) = growing {
        let weight = |c: usize| match columns[c].kind() {
            Percent => columns[c].percent.unwrap_or(0.0),
            Constrained | Auto => columns[c].max,
        };
        let total: f32 = members.iter().map(|c| weight(*c)).sum();
        for c in &members {
            widths[*c] += if total > 0.0 {
                extra * weight(*c) / total
            } else {
                extra / members.len() as f32
            };
        }
    }
    widths
}

/// A table's max-content width from its columns', as Chromium works it out: wide enough for
/// every column's max-content width, for each percentage column's max-content width to be its
/// percentage of the table, and for the other columns to fit in what the percentages leave.
fn max_content_width(columns: &[ColumnWidths]) -> f32 {
    let sum: f32 = columns.iter().map(|column| column.max).sum();
    let percent: f32 = columns.iter().filter_map(|column| column.percent).sum();
    if percent <= 0.0 {
        return sum;
    }
    let for_percentages = columns
        .iter()
        .filter_map(|column| {
            column
                .percent
                .filter(|percent| *percent > 0.0)
                .map(|percent| column.max / percent)
        })
        .fold(0.0, f32::max);
    let rest: f32 = columns
        .iter()
        .filter(|column| column.percent.is_none())
        .map(|column| column.max)
        .sum();
    let for_rest = if percent < 1.0 {
        rest / (1.0 - percent)
    } else if rest > 0.0 {
        TABLE_MAX_WIDTH
    } else {
        0.0
    };
    sum.max(for_percentages).max(for_rest)
}

/// A fixed-layout table's column, as its first row and `<col>`s set it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FixedColumn {
    /// A length (border-box).
    Length(f32),
    /// A percentage (as a fraction) of the room for the columns, plus the cell's horizontal
    /// padding and border.
    Percent {
        percent: f32,
        extra: f32,
    },
    Auto,
    /// A `calc()` width, which Taffy resolves.
    Calc,
}

impl FixedColumn {
    /// Which columns take what's left of the table's width first: auto ones, then lengths,
    /// then percentages.
    fn rank(&self) -> u8 {
        match self {
            Self::Auto | Self::Calc => 0,
            Self::Length(_) => 1,
            Self::Percent { .. } => 2,
        }
    }
}

/// The widths of a fixed-layout table's `columns` sharing `target`, as Chromium shares it out
/// (`DistributeInlineSizeToComputedInlineSizeFixed`): length columns get their length and
/// percentage columns their percentage, the latter shrinking in proportion to their widths to
/// fit what the lengths leave. What's left over goes to the auto columns equally, or without
/// any to the length columns in proportion to their widths, or else to the percentage ones.
fn distribute_fixed(target: f32, columns: &[FixedColumn]) -> Vec<f32> {
    let mut widths: Vec<f32> = columns
        .iter()
        .map(|column| match column {
            FixedColumn::Length(width) => *width,
            FixedColumn::Percent { percent, extra } => percent * target + extra,
            FixedColumn::Auto | FixedColumn::Calc => 0.0,
        })
        .collect();
    let total = |rank: u8, widths: &[f32]| -> f32 {
        columns
            .iter()
            .zip(widths)
            .filter(|(column, _)| column.rank() == rank)
            .map(|(_, width)| width)
            .sum()
    };
    let (lengths, percentages) = (total(1, &widths), total(2, &widths));
    if lengths + percentages > target {
        let scale = (target - lengths).max(0.0) / percentages.max(f32::EPSILON);
        for (column, width) in columns.iter().zip(&mut widths) {
            if column.rank() == 2 {
                *width *= scale;
            }
        }
        return widths;
    }
    let Some(rank) = (0..3).find(|rank| columns.iter().any(|column| column.rank() == *rank)) else {
        return widths;
    };
    let extra = target - lengths - percentages;
    let members = columns
        .iter()
        .filter(|column| column.rank() == rank)
        .count() as f32;
    let weights = total(rank, &widths);
    for (column, width) in columns.iter().zip(&mut widths) {
        if column.rank() == rank {
            *width += if rank == 0 || weights <= 0.0 {
                extra / members
            } else {
                extra * *width / weights
            };
        }
    }
    widths
}

/// A table's horizontal sizes for a layout at some inputs.
struct TableSizes {
    /// Padding and border, which include the outer border spacing.
    edges: f32,
    /// The spacing between columns.
    gap: f32,
    /// All of it between `count` columns.
    gaps: f32,
    /// What turns a content-box width into a border-box one.
    box_sizing: f32,
    /// The table's width when it's known or resolves (border-box).
    specified: Option<f32>,
    min: Option<f32>,
    max: Option<f32>,
}

impl TableSizes {
    fn new(style: &taffy::Style<Atom>, inputs: taffy::tree::LayoutInput, count: usize) -> Self {
        let parent_width = inputs.parent_size.width;
        let edges = (style
            .padding
            .resolve_or_zero(parent_width, resolve_calc_value)
            + style
                .border
                .resolve_or_zero(parent_width, resolve_calc_value))
        .horizontal_axis_sum();
        let gap = style
            .gap
            .width
            .resolve_or_zero(parent_width, resolve_calc_value);
        let box_sizing = match style.box_sizing {
            taffy::BoxSizing::ContentBox => edges,
            taffy::BoxSizing::BorderBox => 0.0,
        };
        fn resolve(
            size: impl MaybeResolve<Option<f32>, Option<f32>>,
            parent_width: Option<f32>,
            box_sizing: f32,
        ) -> Option<f32> {
            size.maybe_resolve(parent_width, resolve_calc_value)
                .map(|width| width + box_sizing)
        }
        let specified = inputs.known_dimensions.width.or_else(|| {
            (inputs.sizing_mode == taffy::SizingMode::InherentSize)
                .then(|| resolve(style.size.width, parent_width, box_sizing))
                .flatten()
        });
        Self {
            edges,
            gap,
            gaps: gap * count.saturating_sub(1) as f32,
            box_sizing,
            specified,
            min: resolve(style.min_size.width, parent_width, box_sizing),
            max: resolve(style.max_size.width, parent_width, box_sizing),
        }
    }

    /// `width` within the table's min and max widths, and at least `floor`.
    fn clamp(&self, width: f32, floor: f32) -> f32 {
        let width = self.max.map_or(width, |max| width.min(max));
        let width = self.min.map_or(width, |min| width.max(min));
        width.max(floor)
    }
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_, '_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_, '_> {}

impl TableTreeWrapper<'_, '_> {
    /// Sizes the table's columns for a layout at `inputs` as browsers do, where Taffy's grid
    /// sizing would share the room beyond the columns' min-content widths equally: sets the
    /// columns' widths and the table's width in the style the grid lays out with, or, when the
    /// table is only being measured for its width, returns that.
    ///
    /// An auto-layout table is as wide as its width says, or else as the space available to it
    /// up to its max-content width ([`max_content_width`]), within its min and max widths and
    /// never narrower than its columns' min-content widths; [`distribute`] shares that out.
    /// Measured in min-content or max-content space it's as wide as its columns' min-content
    /// or max-content widths, not wider for its percentage columns, which would make it as wide
    /// as anything in a flex container or another table's cell (Chromium leaves them out of a
    /// table's intrinsic sizes too).
    ///
    /// A fixed-layout table is as wide as its width says and never narrower than its columns'
    /// lengths; [`distribute_fixed`] shares that out. Measured while its percentage width
    /// doesn't resolve, browsers size it from its columns' lengths alone and give it a huge
    /// max-content width ([`TABLE_MAX_WIDTH`]), so that it fills whatever the percentage then
    /// resolves against.
    ///
    /// Either way the table is never narrower than `floor`, its captions' widest min-content
    /// width ([`Self::caption_min_width`]).
    pub(crate) fn size_columns(
        &mut self,
        inputs: taffy::tree::LayoutInput,
        floor: f32,
    ) -> Option<f32> {
        self.style = None;
        let ctx = Arc::clone(&self.ctx);
        let style = &ctx.style;
        let count = style.grid_template_columns.len();
        if count == 0 {
            return None;
        }
        let sizes = TableSizes::new(style, inputs, count);
        let space = inputs.available_space.width;
        let measuring = inputs.run_mode == taffy::RunMode::ComputeSize
            && inputs.axis == taffy::RequestedAxis::Horizontal
            && sizes.specified.is_none()
            && !matches!(space, taffy::AvailableSpace::Definite(_));
        let (width, widths) = if ctx.is_fixed {
            if ctx.fixed_columns.contains(&FixedColumn::Calc) {
                return None;
            }
            let lengths = ctx
                .fixed_columns
                .iter()
                .map(|column| match column {
                    FixedColumn::Length(width) => *width,
                    _ => 0.0,
                })
                .sum::<f32>()
                + sizes.edges
                + sizes.gaps;
            let lengths = lengths.max(floor);
            // Without a width, the table's percentage width doesn't resolve: it's being measured
            let width = match (sizes.specified, space) {
                (Some(width), _) => width,
                (None, taffy::AvailableSpace::Definite(available)) => {
                    available.min(TABLE_MAX_WIDTH)
                }
                (None, taffy::AvailableSpace::MaxContent) if measuring => {
                    return Some(TABLE_MAX_WIDTH);
                }
                (None, taffy::AvailableSpace::MinContent) if measuring => return Some(lengths),
                (None, _) => return None,
            };
            let width = sizes.clamp(width, lengths);
            let target = width - sizes.edges - sizes.gaps;
            (width, distribute_fixed(target, &ctx.fixed_columns))
        } else {
            let columns = self.column_widths(inputs, count, sizes.gap);
            let sum = |of: fn(&ColumnWidths) -> f32| {
                columns.iter().map(of).sum::<f32>() + sizes.edges + sizes.gaps
            };
            let min = sum(|column| column.min).max(floor);
            let width = match (sizes.specified, space) {
                (Some(width), _) => width,
                (None, taffy::AvailableSpace::Definite(available)) => {
                    available.min(max_content_width(&columns) + sizes.edges + sizes.gaps)
                }
                (None, taffy::AvailableSpace::MinContent) => min,
                (None, taffy::AvailableSpace::MaxContent) => sum(|column| column.max),
            };
            let width = sizes.clamp(width, min);
            if measuring {
                return Some(width);
            }
            let target = width - sizes.edges - sizes.gaps;
            (width, distribute(target, &columns, true))
        };

        let mut style = style.clone();
        style.grid_template_columns = widths
            .into_iter()
            .map(|width| {
                let track: TrackSizingFunction = style_helpers::length(width);
                track.into()
            })
            .collect();
        style.size.width = style_helpers::length(width - sizes.box_sizing);
        style.min_size.width = style_helpers::auto();
        style.max_size.width = style_helpers::auto();
        self.style = Some(style);
        None
    }

    /// Narrows the cells that are wider than their columns to those columns, after a full layout
    /// of the grid. A grid item is never narrower than its padding and borders, but a table cell
    /// is as wide as its columns: in a fixed-layout table whose percentage columns take all its
    /// width, an `auto` column and its cells are 0 wide, and the cell's content overflows it from
    /// where its padding puts it (as in Chromium), with no background or borders showing.
    pub(crate) fn fit_cells_to_columns(&mut self, inputs: taffy::tree::LayoutInput) {
        if inputs.run_mode != taffy::RunMode::PerformLayout {
            return;
        }
        let ctx = Arc::clone(&self.ctx);
        let grid = ctx.computed_grid_info.borrow();
        let Some(columns) = grid.as_ref().map(|grid| &grid.columns.positions) else {
            return;
        };
        for cell in &ctx.cells {
            let (first, last) = (
                usize::from(cell.column),
                usize::from(cell.column + cell.span) - 1,
            );
            let (Some(first), Some(last)) = (columns.get(first), columns.get(last)) else {
                continue;
            };
            let width = (last.end - first.start).abs();
            let layout = self.doc.nodes[cell.node_id].unrounded_layout_mut();
            if layout.size.width > width {
                layout.size.width = width;
                // Borders it has no room for don't paint.
                if layout.border.left + layout.border.right > width {
                    (layout.border.left, layout.border.right) = (0.0, 0.0);
                }
            }
        }
    }

    /// The widest of the captions' min-content widths, margins included: the table is never
    /// narrower, so a caption's longest word never overflows it.
    pub(crate) fn caption_min_width(&mut self) -> f32 {
        let ctx = Arc::clone(&self.ctx);
        let mut widest: f32 = 0.0;
        for caption in &ctx.captions {
            let width = self
                .doc
                .compute_child_layout(
                    crate::taffy_node_id(caption.node_id),
                    taffy::tree::LayoutInput {
                        run_mode: taffy::RunMode::ComputeSize,
                        sizing_mode: taffy::SizingMode::InherentSize,
                        axis: taffy::RequestedAxis::Horizontal,
                        known_dimensions: taffy::Size::NONE,
                        known_dimensions_are_definite: taffy::Size {
                            width: true,
                            height: true,
                        },
                        parent_size: taffy::Size::NONE,
                        available_space: taffy::Size {
                            width: taffy::AvailableSpace::MinContent,
                            height: taffy::AvailableSpace::MinContent,
                        },
                        vertical_margins_are_collapsible: taffy::Line::FALSE,
                    },
                )
                .size
                .width;
            let style = self
                .doc
                .child_layout_style(&self.doc.nodes[caption.node_id]);
            let margin = style
                .margin()
                .resolve_or_zero(None, resolve_calc_value)
                .horizontal_axis_sum();
            widest = widest.max(width + margin);
        }
        widest
    }

    /// Lays the captions out as wide as the table, whose grid's layout is `output`: the
    /// `caption-side: top` ones above the grid and the others below it, each below the last,
    /// and makes `output` the whole table's. In a full layout it places them and moves the grid
    /// (its cells and its rows' tracks) down below the captions above it. Returns how far down
    /// the grid starts.
    pub(crate) fn lay_out_captions(
        &mut self,
        inputs: taffy::tree::LayoutInput,
        output: &mut taffy::LayoutOutput,
    ) -> f32 {
        let ctx = Arc::clone(&self.ctx);
        let measuring_width = inputs.run_mode == taffy::RunMode::ComputeSize
            && inputs.axis == taffy::RequestedAxis::Horizontal;
        if ctx.captions.is_empty()
            || measuring_width
            || inputs.run_mode == taffy::RunMode::PerformHiddenLayout
        {
            return 0.0;
        }
        let width = output.size.width;
        let parent = Some(width);
        // The heights of the captions above the grid and below it, margins included.
        let mut heights = [0.0f32; 2];
        let mut placed = Vec::new();
        for (order, caption) in ctx.captions.iter().enumerate() {
            let (margin, padding, border, caption_width) = {
                let style = self
                    .doc
                    .child_layout_style(&self.doc.nodes[caption.node_id]);
                let margin = style.margin().resolve_or_zero(parent, resolve_calc_value);
                let padding = style.padding().resolve_or_zero(parent, resolve_calc_value);
                let border = style.border().resolve_or_zero(parent, resolve_calc_value);
                let edges = (padding + border).horizontal_axis_sum();
                let box_sizing = match style.box_sizing() {
                    taffy::BoxSizing::ContentBox => edges,
                    taffy::BoxSizing::BorderBox => 0.0,
                };
                fn resolve(
                    size: impl MaybeResolve<Option<f32>, Option<f32>>,
                    parent: Option<f32>,
                    box_sizing: f32,
                ) -> Option<f32> {
                    size.maybe_resolve(parent, resolve_calc_value)
                        .map(|size| size + box_sizing)
                }
                // A block box, which fills the width it's given unless it has one of its own.
                let mut caption_width = resolve(style.size().width, parent, box_sizing)
                    .unwrap_or(width - margin.horizontal_axis_sum());
                if let Some(max) = resolve(style.max_size().width, parent, box_sizing) {
                    caption_width = caption_width.min(max);
                }
                if let Some(min) = resolve(style.min_size().width, parent, box_sizing) {
                    caption_width = caption_width.max(min);
                }
                (margin, padding, border, caption_width.max(edges))
            };
            let node_id = crate::taffy_node_id(caption.node_id);
            let laid = self.doc.compute_child_layout(
                node_id,
                taffy::tree::LayoutInput {
                    run_mode: inputs.run_mode,
                    sizing_mode: taffy::SizingMode::InherentSize,
                    axis: taffy::RequestedAxis::Both,
                    known_dimensions: taffy::Size {
                        width: Some(caption_width),
                        height: None,
                    },
                    known_dimensions_are_definite: taffy::Size {
                        width: true,
                        height: true,
                    },
                    parent_size: taffy::Size {
                        width: parent,
                        height: None,
                    },
                    available_space: taffy::Size {
                        width: taffy::AvailableSpace::Definite(caption_width),
                        height: taffy::AvailableSpace::MaxContent,
                    },
                    vertical_margins_are_collapsible: taffy::Line::FALSE,
                },
            );
            let side = usize::from(caption.below);
            let top = heights[side] + margin.top;
            heights[side] = top + laid.size.height + margin.bottom;
            placed.push((
                node_id,
                caption.below,
                taffy::Layout {
                    order: order as u32,
                    location: taffy::Point {
                        x: margin.left,
                        y: top,
                    },
                    size: laid.size,
                    scrollable_overflow_rect: laid.scrollable_overflow_rect,
                    scrollbar_size: taffy::Size::ZERO,
                    border,
                    padding,
                    margin,
                },
            ));
        }
        let [above, below] = heights;
        let grid_height = output.size.height;
        if inputs.run_mode == taffy::RunMode::PerformLayout {
            for (node_id, caption_below, mut layout) in placed {
                if caption_below {
                    layout.location.y += above + grid_height;
                }
                self.doc.set_unrounded_layout(node_id, &layout);
            }
            if above > 0.0 {
                for cell in &ctx.cells {
                    self.doc.nodes[cell.node_id]
                        .unrounded_layout_mut()
                        .location
                        .y += above;
                }
                if let Some(grid) = ctx.computed_grid_info.borrow_mut().as_mut() {
                    for row in &mut grid.rows.positions {
                        row.start += above;
                        row.end += above;
                    }
                }
            }
        }
        output.size.height += above + below;
        output.baselines.first = output.baselines.first.map(|baseline| baseline + above);
        output.baselines.last = output.baselines.last.map(|baseline| baseline + above);
        let overflow = output.scrollable_overflow_rect.rect();
        output.scrollable_overflow_rect = taffy::ScrollableOverflow::new(
            taffy::Rect {
                bottom: overflow.bottom + above + below,
                ..overflow
            },
            output.scrollable_overflow_rect.is_reversed(),
        );
        above
    }

    /// Each column's widths: from the `<col>` widths, then from the cells that span only it,
    /// then widened by the cells that span it with others (narrower spans first), as Chromium
    /// does (`DistributeColspanCellToColumnsAuto`).
    fn column_widths(
        &mut self,
        inputs: taffy::tree::LayoutInput,
        count: usize,
        gap: f32,
    ) -> Vec<ColumnWidths> {
        let ctx = Arc::clone(&self.ctx);
        let mut columns: Vec<ColumnWidths> = ctx
            .style
            .grid_template_columns
            .iter()
            .map(|track| {
                let taffy::GridTemplateComponent::Single(track) = track else {
                    return ColumnWidths::default();
                };
                let (min, max) = (track.min.into_raw(), track.max.into_raw());
                // A `<col>`'s `min-width` (`minmax(length, auto)`) is a floor
                let floor = (min.tag() == taffy::CompactLength::LENGTH_TAG
                    && max.tag() == taffy::CompactLength::AUTO_TAG)
                    .then(|| min.value());
                let mut column = match max.tag() {
                    taffy::CompactLength::LENGTH_TAG => ColumnWidths {
                        max: max.value(),
                        constrained: true,
                        ..ColumnWidths::default()
                    },
                    taffy::CompactLength::PERCENT_TAG => ColumnWidths {
                        percent: Some(max.value()),
                        ..ColumnWidths::default()
                    },
                    _ => ColumnWidths::default(),
                };
                column.min = floor.unwrap_or(0.0);
                column.max = column.max.max(column.min);
                column
            })
            .collect();
        let mut spanning = Vec::new();
        for (index, cell) in ctx.cells.iter().enumerate() {
            let column = cell.column as usize;
            if column >= count {
                continue;
            }
            let min = self.measure_cell(index, inputs, taffy::AvailableSpace::MinContent);
            let max = self
                .measure_cell(index, inputs, taffy::AvailableSpace::MaxContent)
                .max(min);
            let widths = ColumnWidths {
                min,
                max,
                percent: cell.percent_width,
                constrained: cell.has_length_width,
            };
            match (cell.span as usize).min(count - column) {
                1 => columns[column].encompass(widths),
                span => spanning.push((column, span, widths)),
            }
        }
        spanning.sort_by_key(|(column, span, _)| (*span, *column));
        for (start, span, cell) in spanning {
            let spanned = &mut columns[start..start + span];
            let inner_gaps = gap * (span - 1) as f32;
            // A spanning cell's percentage beyond its columns' own goes to its columns without
            // one, in proportion to their max-content widths
            if let Some(percent) = cell.percent {
                let surplus = percent - spanned.iter().filter_map(|c| c.percent).sum::<f32>();
                let others = spanned.iter().filter(|c| c.percent.is_none()).count();
                if surplus > 0.0 && others > 0 {
                    let max: f32 = spanned
                        .iter()
                        .filter(|c| c.percent.is_none())
                        .map(|c| c.max)
                        .sum();
                    for column in spanned.iter_mut().filter(|c| c.percent.is_none()) {
                        column.percent = Some(if max > 0.0 {
                            surplus * column.max / max
                        } else {
                            surplus / others as f32
                        });
                    }
                }
            }
            let mins = distribute((cell.min - inner_gaps).max(0.0), spanned, true);
            for (column, min) in spanned.iter_mut().zip(mins) {
                column.min = column.min.max(min);
            }
            let maxes = distribute((cell.max - inner_gaps).max(0.0), spanned, cell.constrained);
            for (column, max) in spanned.iter_mut().zip(maxes) {
                column.max = column.max.max(column.min).max(max);
            }
        }
        columns
    }

    /// Cell `index`'s min-content or max-content width.
    fn measure_cell(
        &mut self,
        index: usize,
        inputs: taffy::tree::LayoutInput,
        space: taffy::AvailableSpace,
    ) -> f32 {
        let inputs = taffy::tree::LayoutInput {
            run_mode: taffy::RunMode::ComputeSize,
            sizing_mode: taffy::SizingMode::InherentSize,
            axis: taffy::RequestedAxis::Horizontal,
            known_dimensions: taffy::Size::NONE,
            available_space: taffy::Size {
                width: space,
                height: taffy::AvailableSpace::MaxContent,
            },
            parent_size: taffy::Size::NONE,
            ..inputs
        };
        self.compute_child_layout(taffy::NodeId::from(index), inputs)
            .size
            .width
    }

    /// Measures a cell without its own `width` and `height` (`SizingMode::ContentSize`). The
    /// result isn't cached: a node's layout cache holds results for one sizing mode, so
    /// storing it would evict the cell's other measurements, and measuring the cell again
    /// re-measures its inline boxes from their own caches, which don't keep their baselines.
    fn measure_content(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let inputs = taffy::tree::LayoutInput {
            sizing_mode: taffy::SizingMode::ContentSize,
            ..inputs
        };
        self.doc
            .compute_child_layout_internal(node_id, inputs, None)
    }
}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_, '_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        self.style.as_ref().unwrap_or(&self.ctx.style)
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = crate::taffy_node_id(self.ctx.cells[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.cells[usize::from(node_id)];
        let node_id = crate::taffy_node_id(cell.node_id);
        // A cell's specified width is its column's preferred width, not a floor: the cell's
        // min-content width is that of its content (CSS Tables 3 §3.9.1), so a column can
        // shrink below the widths of its cells to fit the table's width.
        if cell.has_length_width
            && inputs.known_dimensions.width.is_none()
            && inputs.available_space.width == taffy::AvailableSpace::MinContent
        {
            return self.measure_content(node_id, inputs);
        }
        if cell.has_length_height {
            if inputs.known_dimensions.height.is_some() {
                // Laid out at its row's height, the cell's content still resolves percentage
                // heights against the cell's own height, as in browsers
                let mut inputs = inputs;
                inputs.known_dimensions_are_definite.height = false;
                return self.doc.compute_child_layout(node_id, inputs);
            }
            if inputs.axis != taffy::RequestedAxis::Horizontal {
                // The cell's height is a minimum: it's as tall as its content when that's taller
                let specified = self.doc.compute_child_layout(node_id, inputs);
                let content = self.measure_content(node_id, inputs);
                return if content.size.height > specified.size.height {
                    content
                } else {
                    specified
                };
            }
        }
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_, '_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }

    fn set_detailed_grid_info(
        &mut self,
        _node_id: taffy::NodeId,
        detailed_grid_info: DetailedGridInfo<Atom>,
    ) {
        *self.ctx.computed_grid_info.borrow_mut() = Some(detailed_grid_info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected widths are what Chromium lays out for the same tables.

    fn assert_widths(actual: Vec<f32>, expected: &[f32]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?}");
        for (actual_width, expected_width) in actual.iter().zip(expected) {
            assert!(
                (actual_width - expected_width).abs() < 0.05,
                "{actual:?} != {expected:?}"
            );
        }
    }

    fn auto(min: f32, max: f32) -> ColumnWidths {
        ColumnWidths {
            min,
            max,
            ..ColumnWidths::default()
        }
    }

    #[test]
    fn auto_columns_share_room_in_proportion_to_their_growth() {
        // `<td style="width:100%">` (30px) and a cell of two 50px inline blocks, 130px of room
        let percent = ColumnWidths {
            percent: Some(1.0),
            ..auto(30.0, 30.0)
        };
        assert_widths(
            distribute(130.0, &[percent, auto(50.0, 100.0)], true),
            &[80.0, 50.0],
        );
        // Between min-content and max-content widths
        assert_widths(
            distribute(300.0, &[auto(50.0, 250.0), auto(50.0, 50.0)], true),
            &[250.0, 50.0],
        );
        assert_widths(
            distribute(200.0, &[auto(50.0, 250.0), auto(50.0, 100.0)], true),
            &[130.0, 70.0],
        );
        // Beyond them, in proportion to the max-content widths
        assert_widths(
            distribute(600.0, &[auto(10.0, 100.0), auto(10.0, 200.0)], true),
            &[200.0, 400.0],
        );
        // A length column gets its length before auto columns grow
        let constrained = ColumnWidths {
            constrained: true,
            ..auto(10.0, 100.0)
        };
        assert_widths(
            distribute(150.0, &[constrained, auto(10.0, 300.0)], true),
            &[100.0, 50.0],
        );
    }

    #[test]
    fn percentages_widen_a_table_with_room() {
        let percent = ColumnWidths {
            percent: Some(1.0),
            ..auto(30.0, 30.0)
        };
        assert_eq!(
            max_content_width(&[percent, auto(50.0, 100.0)]),
            TABLE_MAX_WIDTH
        );
        let half = ColumnWidths {
            percent: Some(0.5),
            ..auto(0.0, 40.0)
        };
        assert_eq!(max_content_width(&[half, auto(0.0, 100.0)]), 200.0);
    }

    #[test]
    fn fixed_columns_share_a_table_as_chromium_does() {
        use FixedColumn::{Auto, Length};
        let percent = |percent: f32| FixedColumn::Percent {
            percent,
            extra: 0.0,
        };
        // Percentages under 100% grow in proportion
        assert_widths(
            distribute_fixed(600.0, &[percent(0.1), percent(0.6), percent(0.2)]),
            &[66.67, 400.0, 133.33],
        );
        // Lengths take what percentages leave
        assert_widths(
            distribute_fixed(600.0, &[Length(100.0), percent(0.3), percent(0.2)]),
            &[300.0, 180.0, 120.0],
        );
        assert_widths(
            distribute_fixed(600.0, &[Length(100.0), Length(200.0)]),
            &[200.0, 400.0],
        );
        // Auto columns take it first, equally
        assert_widths(
            distribute_fixed(600.0, &[Length(100.0), Auto, Auto]),
            &[100.0, 250.0, 250.0],
        );
        // Percentages shrink to fit what the lengths leave
        assert_widths(
            distribute_fixed(600.0, &[percent(0.6), percent(0.7)]),
            &[276.92, 323.08],
        );
        assert_widths(
            distribute_fixed(600.0, &[Length(400.0), percent(0.4), Auto]),
            &[400.0, 200.0, 0.0],
        );
        // A percentage cell's padding comes on top (`border-spacing: 10px`, 1px padding)
        let padded = |percent: f32| FixedColumn::Percent {
            percent,
            extra: 2.0,
        };
        assert_widths(
            distribute_fixed(560.0, &[Length(102.0), padded(0.3), padded(0.2)]),
            &[276.0, 170.0, 114.0],
        );
    }
}
