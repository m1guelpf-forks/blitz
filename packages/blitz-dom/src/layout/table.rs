use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;
use stylo_taffy::CopyStyleGetters;

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::typed_om::NumericBaseType;
use style::values::computed::calc::CalcPercentageLeaf;
use style::values::computed::length_percentage::{
    CalcLengthPercentage, CalcNode, ComputedLeaf, Unpacked as UnpackedLengthPercentage,
};
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::Optional;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout, values::computed::BorderStyle,
};
use style_traits::values::specified::AllowedNumericType;
use taffy::{
    DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, style_helpers,
};

use thin_vec::ThinVec;

use crate::BaseDocument;
use crate::layout::LayoutPassState;
use crate::node::NodeKind;

use super::construct::{AnonymousBox, create_anonymous_box};
use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::replaced::is_replaced_element;
use super::resolve_calc_value;

pub struct TableTreeWrapper<'a, 'doc> {
    pub(crate) doc: &'a mut LayoutPassState<'doc>,
    pub(crate) ctx: Arc<TableContext>,
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
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo<Atom>>>,
    pub border_style: Option<ServoArc<Border>>,
    pub border_collapse: BorderCollapse,
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
    /// For each column, the number of further rows it is occupied by a rowspan cell
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
            *remaining = rowspan - 1;
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

fn side_width(width: app_units::Au, style: BorderStyle) -> f32 {
    if style.none_or_hidden() {
        0.0
    } else {
        width.to_f32_px()
    }
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
        rows: Vec::new(),
        columns: column_sizes,
        first_cell_border: None,
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
        cells,
        oof_children,
        rows,
        columns: mut column_sizes,
        first_cell_border,
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
    if is_fixed {
        // In the fixed table layout algorithm, percentage column widths resolve against
        // the table's content width minus the horizontal border-spacing between columns,
        // whereas Taffy resolves percentage tracks against the container's inner width
        // (from which only the outer border-spacing has been removed via padding). Fold
        // the inner border-spacing into the percentage using `calc(N% - N * spacing)`.
        let spacing_x = match border_collapse {
            BorderCollapse::Separate => border_spacing.width.px(),
            BorderCollapse::Collapse => 0.0,
        };
        let inner_spacing = spacing_x * num_columns.saturating_sub(1) as f32;
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
            let spacing_x = border_spacing.width.px();
            let spacing_y = border_spacing.height.px();
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
        BorderCollapse::Collapse => first_cell_border
            .as_ref()
            .map(|border| {
                let x = side_width(border.border_left_width.0, border.border_left_style).max(
                    side_width(border.border_right_width.0, border.border_right_style),
                );
                let y = side_width(border.border_top_width.0, border.border_top_style).max(
                    side_width(border.border_bottom_width.0, border.border_bottom_style),
                );
                taffy::Size {
                    width: style_helpers::length(x),
                    height: style_helpers::length(y),
                }
            })
            .unwrap_or(taffy::Size::ZERO.map(style_helpers::length)),
    };

    if border_collapse == BorderCollapse::Collapse {
        style.border = taffy::Rect {
            left: style.gap.width,
            right: style.gap.width,
            top: style.gap.height,
            bottom: style.gap.height,
        };
    }

    let layout_children = cells
        .iter()
        .map(|cell| cell.node_id)
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
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            border_style: first_cell_border,
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
    /// Columns, which `collect_columns` handles, and captions, which aren't laid out yet.
    Ignored,
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
                TablePart::RowGroup(_) | TablePart::Row | TablePart::Ignored | TablePart::Contents
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
        _ if display.outside() == DisplayOutside::TableCaption => TablePart::Ignored,
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
    rows: Vec<TableRow>,
    columns: Vec<TrackSizingFunction>,
    first_cell_border: Option<ServoArc<Border>>,
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

        let height = doc.nodes[row_id].primary_styles().and_then(|style| {
            let height = stylo_taffy::convert::dimension(style.get_height());
            (height.tag() == taffy::CompactLength::LENGTH_TAG).then(|| height.value())
        });
        self.rows.push(TableRow {
            node_id: row_id,
            group: None,
            height,
            is_constrained: height.is_some(),
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

        // In the collapsed borders model the borders are laid out as gutters between
        // the cells (see the table's `gap`) rather than as part of the cells
        if self.border_collapse == BorderCollapse::Collapse {
            style.border = taffy::Rect::ZERO.map(style_helpers::length);
        }

        if self.first_cell_border.is_none() {
            self.first_cell_border = Some(stylo_style.clone_border());
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
        style.size.width = style_helpers::auto();
        self.cells.push(TableCell {
            node_id,
            row: self.row.saturating_sub(1),
            column: col,
            has_length_width,
            has_length_height,
            style,
        });

        self.cursor.place(colspan, rowspan);
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
        &self.ctx.style
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
