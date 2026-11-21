//! The collapsing border model (CSS 2.1 §17.6.2), as Blink lays it out: every edge between two
//! cells, and between a cell and the table's edge, has one border, the winner of the borders that
//! meet there (§17.6.2.1). Half of it is in the box on either side: a cell is laid out with half
//! of each of its edges' borders, and the table with half of its outer ones. The table paints
//! them, centred on the edges, over its cells.

use blitz_traits::node_id::NodeId;
use style::color::AbsoluteColor;
use style::properties::ComputedValues;
use style::values::computed::BorderStyle;
use stylo_taffy::CopyStyleGetters as _;

use crate::BaseDocument;

use super::{TableCell, TableRow};

/// The border one edge of a cell ends up with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollapsedBorder {
    /// Its width in CSS px (0 when no border won, or a `hidden` one did).
    pub width: f32,
    /// Its style, `inset` drawn as `ridge` and `outset` as `groove`, as the collapsing model
    /// draws them (CSS Backgrounds 3 §4.2).
    pub style: BorderStyle,
    pub color: AbsoluteColor,
    /// Where the box it came from is in the order boxes give their borders (cells in document
    /// order, then rows, row groups and the table): of two equal borders meeting at a corner,
    /// the earlier one paints it.
    pub order: usize,
}

impl CollapsedBorder {
    const NONE: Self = Self {
        width: 0.0,
        style: BorderStyle::None,
        color: AbsoluteColor::TRANSPARENT_BLACK,
        order: usize::MAX,
    };
}

/// A cell's edges, in the order of [`taffy::Rect`]'s physical sides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollapsedBorders {
    pub top: CollapsedBorder,
    pub right: CollapsedBorder,
    pub bottom: CollapsedBorder,
    pub left: CollapsedBorder,
}

impl CollapsedBorders {
    /// Half of each edge's border: the cell's share of them.
    pub(crate) fn halves(&self) -> taffy::Rect<f32> {
        taffy::Rect {
            top: self.top.width / 2.0,
            right: self.right.width / 2.0,
            bottom: self.bottom.width / 2.0,
            left: self.left.width / 2.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Right => Self::Left,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
        }
    }
}

/// One box's border on one side, as it takes part in the conflict.
#[derive(Clone, Copy)]
struct Candidate {
    width: f32,
    style: BorderStyle,
    color: AbsoluteColor,
    order: usize,
}

fn candidate(style: &ComputedValues, side: Side, order: usize) -> Candidate {
    let border = style.get_border();
    let (width, border_style, color) = match side {
        Side::Top => (
            border.border_top_width.0,
            border.border_top_style,
            &border.border_top_color,
        ),
        Side::Right => (
            border.border_right_width.0,
            border.border_right_style,
            &border.border_right_color,
        ),
        Side::Bottom => (
            border.border_bottom_width.0,
            border.border_bottom_style,
            &border.border_bottom_color,
        ),
        Side::Left => (
            border.border_left_width.0,
            border.border_left_style,
            &border.border_left_color,
        ),
    };
    Candidate {
        width: width.to_f32_px(),
        style: match border_style {
            BorderStyle::Inset => BorderStyle::Ridge,
            BorderStyle::Outset => BorderStyle::Groove,
            style => style,
        },
        color: color.resolve_to_absolute(&style.clone_color()),
        order,
    }
}

/// How strongly a style wins over another of the same width (§17.6.2.1 rule 3).
fn rank(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::Double => 8,
        BorderStyle::Solid => 7,
        BorderStyle::Dashed => 6,
        BorderStyle::Dotted => 5,
        BorderStyle::Ridge => 4,
        BorderStyle::Outset => 3,
        BorderStyle::Groove => 2,
        BorderStyle::Inset => 1,
        BorderStyle::None | BorderStyle::Hidden => 0,
    }
}

/// The border that wins among `candidates`, which come in order of precedence for a tie (a cell
/// before a row, a row group and the table; the top or left one of two cells): `hidden` hides
/// it, then the widest wins, then the stronger style, then the first.
fn winner(candidates: &[Candidate]) -> CollapsedBorder {
    if candidates
        .iter()
        .any(|candidate| candidate.style == BorderStyle::Hidden)
    {
        return CollapsedBorder::NONE;
    }
    candidates
        .iter()
        .filter(|candidate| candidate.style != BorderStyle::None && candidate.width > 0.0)
        .fold(None::<&Candidate>, |best, candidate| match best {
            Some(best)
                if (best.width, rank(best.style)) >= (candidate.width, rank(candidate.style)) =>
            {
                Some(best)
            }
            _ => Some(candidate),
        })
        .map_or(CollapsedBorder::NONE, |winner| CollapsedBorder {
            width: winner.width,
            style: winner.style,
            color: winner.color,
            order: winner.order,
        })
}

/// Collapses the borders around `cells` (laid out on `rows` and `columns` grid tracks) of table
/// `table_id`, and returns them by cell.
pub(crate) fn collapse(
    doc: &BaseDocument,
    table_id: NodeId,
    cells: &[TableCell],
    rows: &[TableRow],
    columns: u16,
) -> Vec<CollapsedBorders> {
    let style_of = |node_id: NodeId| doc.nodes[node_id].primary_styles();
    let row_count = rows.len() as u16;
    // Which cell covers each slot of the grid.
    let mut slots = vec![None; usize::from(row_count) * usize::from(columns)];
    for (index, cell) in cells.iter().enumerate() {
        for row in cell.row..(cell.row + cell.rowspan).min(row_count) {
            for column in cell.column..(cell.column + cell.span).min(columns) {
                slots[usize::from(row) * usize::from(columns) + usize::from(column)] = Some(index);
            }
        }
    }
    let at = |row: u16, column: u16| {
        (row < row_count && column < columns)
            .then(|| slots[usize::from(row) * usize::from(columns) + usize::from(column)])
            .flatten()
    };
    let group_of = |row: u16| rows.get(usize::from(row)).and_then(|row| row.group);

    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let (top, bottom) = (cell.row, (cell.row + cell.rowspan).min(row_count));
            let (left, right) = (cell.column, (cell.column + cell.span).min(columns));
            let edge = |side: Side| {
                let mut candidates = Vec::new();
                let own = style_of(cell.node_id).map(|style| candidate(&style, side, index));
                // The cells across the edge, in grid order.
                let mut neighbors: Vec<usize> = match side {
                    Side::Top if top > 0 => (left..right).filter_map(|c| at(top - 1, c)).collect(),
                    Side::Bottom => (left..right).filter_map(|c| at(bottom, c)).collect(),
                    Side::Left if left > 0 => {
                        (top..bottom).filter_map(|r| at(r, left - 1)).collect()
                    }
                    Side::Right => (top..bottom).filter_map(|r| at(r, right)).collect(),
                    _ => Vec::new(),
                };
                neighbors.dedup();
                neighbors.retain(|neighbor| *neighbor != index);
                let neighbors = neighbors.into_iter().filter_map(|neighbor| {
                    style_of(cells[neighbor].node_id)
                        .map(|style| candidate(&style, side.opposite(), neighbor))
                });
                // Of two cells, the top or left one wins a tie.
                if matches!(side, Side::Top | Side::Left) {
                    candidates.extend(neighbors);
                    candidates.extend(own);
                } else {
                    candidates.extend(own);
                    candidates.extend(neighbors);
                }
                // Then the rows and row groups the edge runs along or between.
                let mut row_sides: Vec<(u16, Side)> = Vec::new();
                match side {
                    Side::Top => {
                        row_sides.push((top, Side::Top));
                        if top > 0 {
                            row_sides.push((top - 1, Side::Bottom));
                        }
                    }
                    Side::Bottom => {
                        row_sides.push((bottom - 1, Side::Bottom));
                        if bottom < row_count {
                            row_sides.push((bottom, Side::Top));
                        }
                    }
                    Side::Left if left == 0 => row_sides.extend((top..bottom).map(|r| (r, side))),
                    Side::Right if right == columns => {
                        row_sides.extend((top..bottom).map(|r| (r, side)))
                    }
                    _ => {}
                }
                for &(row, row_side) in &row_sides {
                    if let Some(style) = rows
                        .get(usize::from(row))
                        .and_then(|row| style_of(row.node_id))
                    {
                        candidates.push(candidate(
                            &style,
                            row_side,
                            cells.len() + usize::from(row),
                        ));
                    }
                }
                for &(row, row_side) in &row_sides {
                    let Some(group) = group_of(row) else {
                        continue;
                    };
                    // A group's top and bottom borders are at its first and last rows only.
                    let at_group_edge = match row_side {
                        Side::Top => row == 0 || group_of(row - 1) != Some(group),
                        Side::Bottom => group_of(row + 1) != Some(group),
                        Side::Left | Side::Right => true,
                    };
                    if at_group_edge && let Some(style) = style_of(group) {
                        // Groups come after the rows, in order of their first rows.
                        let first = (0..=row).rev().take_while(|&r| group_of(r) == Some(group));
                        let order =
                            cells.len() + rows.len() + usize::from(first.last().unwrap_or(row));
                        candidates.push(candidate(&style, row_side, order));
                    }
                }
                // And the table's own, at its edges.
                let outer = match side {
                    Side::Top => top == 0,
                    Side::Bottom => bottom == row_count,
                    Side::Left => left == 0,
                    Side::Right => right == columns,
                };
                if outer && let Some(style) = style_of(table_id) {
                    candidates.push(candidate(&style, side, cells.len() + 2 * rows.len()));
                }
                winner(&candidates)
            };
            CollapsedBorders {
                top: edge(Side::Top),
                right: edge(Side::Right),
                bottom: edge(Side::Bottom),
                left: edge(Side::Left),
            }
        })
        .collect()
}
