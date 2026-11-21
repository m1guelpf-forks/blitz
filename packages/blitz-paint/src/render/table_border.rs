//! A table's collapsed borders, painted as Blink paints them. Each border runs along one grid
//! track between two lines; at a corner it reaches across the crossing border when it wins the
//! corner (by width, then style, then which box it came from) and stops short of it otherwise;
//! and it is drawn as the top or left side of a box, snapped to whole CSS px, in its style.

use std::cmp::Ordering;

use anyrender::{AntiAliasing, PaintScene};
use blitz_dom::node::SpecialElementData;
use blitz_dom::{CollapsedBorder, CollapsedBorders};
use kurbo::{Affine, BezPath, Circle, Point, Rect, Shape as _, Size, Vec2};
use peniko::{Color, Fill};
use style::computed_values::border_collapse::T as BorderCollapse;
use style::values::computed::BorderStyle;

use super::ElementCx;
use super::border::inset_outset_color;
use crate::color::ToColorColor as _;

/// The borders on a table's grid lines, laid out as Blink lays them out so that neighbours are
/// fixed offsets apart: line `r` holds row `r`'s vertical edges at even slots (one per column
/// line) and the horizontal edges along line `r` at odd ones (one per column).
struct Grid {
    per_line: usize,
    edges: Vec<Option<CollapsedBorder>>,
    /// Where each column line is, and each row line (CSS px from the table's origin).
    columns: Vec<Option<f64>>,
    rows: Vec<Option<f64>>,
}

impl Grid {
    /// The edges around `cells` (with their layouts), on `rows` rows and `columns` columns. The
    /// edges inside a spanning cell have no border.
    fn new(
        cells: impl Iterator<Item = (CollapsedBorders, [u16; 4], Rect)>,
        rows: usize,
        columns: usize,
    ) -> Self {
        let per_line = 2 * columns + 1;
        let mut grid = Self {
            per_line,
            edges: vec![None; (rows + 1) * per_line],
            columns: vec![None; columns + 1],
            rows: vec![None; rows + 1],
        };
        for (borders, [row, column, rowspan, span], rect) in cells {
            let (top, left) = (usize::from(row), usize::from(column));
            let bottom = (top + usize::from(rowspan)).min(rows);
            let right = (left + usize::from(span)).min(columns);
            grid.columns[left].get_or_insert(rect.x0);
            grid.columns[right].get_or_insert(rect.x1);
            grid.rows[top].get_or_insert(rect.y0);
            grid.rows[bottom].get_or_insert(rect.y1);
            let mut set = |index: usize, border: CollapsedBorder| {
                let paints = border.width > 0.0
                    && !matches!(border.style, BorderStyle::None | BorderStyle::Hidden);
                if paints && grid.edges[index].is_none() {
                    grid.edges[index] = Some(border);
                }
            };
            for column in left..right {
                set(top * per_line + 2 * column + 1, borders.top);
                set(bottom * per_line + 2 * column + 1, borders.bottom);
            }
            for row in top..bottom {
                set(row * per_line + 2 * left, borders.left);
                set(row * per_line + 2 * right, borders.right);
            }
        }
        grid
    }

    fn at(&self, index: usize, offset: isize) -> Option<usize> {
        index
            .checked_add_signed(offset)
            .filter(|&index| index < self.edges.len())
    }

    fn border(&self, index: Option<usize>) -> Option<&CollapsedBorder> {
        index.and_then(|index| self.edges[index].as_ref())
    }

    fn width(&self, index: Option<usize>) -> f64 {
        self.border(index)
            .map_or(0.0, |border| f64::from(border.width))
    }

    /// Which of two edges paints the corner they meet at: `Greater` for `a`, `Less` for `b`,
    /// `Equal` for both.
    fn compare(&self, a: Option<usize>, b: Option<usize>) -> Ordering {
        if a.is_some() && a == b {
            return Ordering::Equal;
        }
        match (self.border(a), self.border(b)) {
            (Some(a), Some(b)) => a
                .width
                .partial_cmp(&b.width)
                .filter(|order| order.is_ne())
                .or_else(|| a.style.partial_cmp(&b.style).filter(|order| order.is_ne()))
                .unwrap_or_else(|| b.order.cmp(&a.order)),
            (a, b) => a.is_some().cmp(&b.is_some()),
        }
    }

    /// How far edge `index` reaches past the lines at its start and end: half across the corner
    /// when it paints the corner, otherwise half short of it.
    fn reach(&self, index: usize, horizontal: bool) -> (f64, f64) {
        let line = self.per_line as isize;
        let at = |offset| self.at(index, offset);
        // Around each corner: the horizontal edges before and after it, the vertical ones over
        // and under it.
        let (start, end) = if horizontal {
            (
                [at(-2), at(0), at(-(line + 1)), at(-1)],
                [at(0), at(2), at(-(line - 1)), at(1)],
            )
        } else {
            (
                [at(-1), at(1), at(-line), at(0)],
                [at(line - 1), at(line + 1), at(0), at(line)],
            )
        };
        let corner = |[before, after, over, under]: [Option<usize>; 4], lost: Ordering| {
            let along = self.compare(before, after);
            let across = self.compare(over, under);
            let horizontal_winner = if along.is_gt() { before } else { after };
            let vertical_winner = if across.is_gt() { over } else { under };
            let versus = self.compare(horizontal_winner, vertical_winner);
            let (wins, size) = if horizontal {
                (versus.is_ge() && along != lost, self.width(vertical_winner))
            } else {
                (
                    versus.is_le() && across != lost,
                    self.width(horizontal_winner),
                )
            };
            if wins { size / 2.0 } else { -size / 2.0 }
        };
        (
            corner(start, Ordering::Greater),
            corner(end, Ordering::Less),
        )
    }
}

impl ElementCx<'_, '_> {
    /// Paints a table's collapsed borders, after its cells.
    pub(crate) fn draw_table_borders(&self, scene: &mut impl PaintScene) {
        let SpecialElementData::TableRoot(table) = &self.element.special_data else {
            return;
        };
        if table.border_collapse != BorderCollapse::Collapse {
            return;
        }
        let cells = table.cells.iter().filter_map(|cell| {
            let borders = cell.collapsed?;
            let layout = self.context.dom.get_node(cell.node_id)?.final_layout();
            let origin = Point::new(layout.location.x.into(), layout.location.y.into());
            let size = Size::new(layout.size.width.into(), layout.size.height.into());
            Some((
                borders,
                [cell.row, cell.column, cell.rowspan, cell.span],
                Rect::from_origin_size(origin, size),
            ))
        });
        let grid = Grid::new(
            cells,
            table.rows.len(),
            table.style.grid_template_columns.len(),
        );
        // Snapped to whole CSS px of the page, so measured from its origin.
        let origin = self.transform.translation() / self.scale;
        let mut sides = Sides {
            scene,
            transform: self.transform,
            scale: self.scale,
            origin,
        };
        for (index, border) in grid.edges.iter().enumerate() {
            let Some(border) = border else {
                continue;
            };
            let (line, slot) = (index / grid.per_line, index % grid.per_line);
            let horizontal = slot % 2 == 1;
            let track = slot / 2;
            let half = f64::from(border.width) / 2.0;
            let line_at = |lines: &[Option<f64>], index: usize| lines.get(index).copied().flatten();
            let (start, end) = grid.reach(index, horizontal);
            let rect = if horizontal {
                let (Some(x0), Some(x1), Some(y)) = (
                    line_at(&grid.columns, track),
                    line_at(&grid.columns, track + 1),
                    line_at(&grid.rows, line),
                ) else {
                    continue;
                };
                Rect::new(x0 - start, y - half, x1 + end, y + half)
            } else {
                let (Some(y0), Some(y1), Some(x)) = (
                    line_at(&grid.rows, line),
                    line_at(&grid.rows, line + 1),
                    line_at(&grid.columns, track),
                ) else {
                    continue;
                };
                Rect::new(x - half, y0 - start, x + half, y1 + end)
            };
            sides.side(
                (rect + origin).round(),
                horizontal,
                border.color.as_srgb_color(),
                border.style,
            );
        }
    }
}

/// Draws box sides in whole CSS px of the page (Blink's `DrawLineForBoxSide` for a side that
/// meets no other).
struct Sides<'s, S> {
    scene: &'s mut S,
    transform: Affine,
    scale: f64,
    origin: Vec2,
}

/// How a dashed or dotted line is broken up.
enum Pattern {
    /// Butt-ended dashes this long, this far apart.
    Dashes(f64, f64),
    /// Round dots, as wide as the line, this far apart centre to centre.
    Dots(f64),
}

impl<S: PaintScene> Sides<'_, S> {
    fn fill(&mut self, rect: Rect, color: Color) {
        if color.components[3] == 0.0 || rect.is_zero_area() {
            return;
        }
        let rect = (rect - self.origin).scale_from_origin(self.scale);
        self.scene.fill(
            Fill::NonZero,
            self.transform,
            color,
            None,
            &rect,
            AntiAliasing::Enabled,
        );
    }

    /// Draws `rect` as a top side when `top`, otherwise as a left one.
    fn side(&mut self, rect: Rect, top: bool, color: Color, style: BorderStyle) {
        let (thickness, length) = if top {
            (rect.height(), rect.width())
        } else {
            (rect.width(), rect.height())
        };
        if length <= 0.0 || thickness <= 0.0 {
            return;
        }
        let style = match style {
            BorderStyle::Double if thickness < 3.0 => BorderStyle::Solid,
            BorderStyle::Groove | BorderStyle::Ridge if thickness <= 1.0 => BorderStyle::Solid,
            style => style,
        };
        match style {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => self.fill(rect, color),
            // A top or left side is the shadowed one of an `inset` border.
            BorderStyle::Inset | BorderStyle::Outset => {
                self.fill(rect, inset_outset_color(style == BorderStyle::Inset, color));
            }
            // Its outer thirds, rounded down from a third of a px more.
            BorderStyle::Double => {
                let third = ((thickness + 1.0) / 3.0).trunc();
                if top {
                    self.fill(
                        Rect {
                            y1: rect.y0 + third,
                            ..rect
                        },
                        color,
                    );
                    self.fill(
                        Rect {
                            y0: rect.y1 - third,
                            ..rect
                        },
                        color,
                    );
                } else {
                    self.fill(
                        Rect {
                            x1: rect.x0 + third,
                            ..rect
                        },
                        color,
                    );
                    self.fill(
                        Rect {
                            x0: rect.x1 - third,
                            ..rect
                        },
                        color,
                    );
                }
            }
            // An `inset` half and an `outset` one, the outer half the larger.
            BorderStyle::Groove | BorderStyle::Ridge => {
                let (outer, inner) = if style == BorderStyle::Groove {
                    (BorderStyle::Inset, BorderStyle::Outset)
                } else {
                    (BorderStyle::Outset, BorderStyle::Inset)
                };
                let (a, b) = if top {
                    let middle = ((rect.y0 + rect.y1 + 1.0) / 2.0).trunc();
                    (Rect { y1: middle, ..rect }, Rect { y0: middle, ..rect })
                } else {
                    let middle = ((rect.x0 + rect.x1 + 1.0) / 2.0).trunc();
                    (Rect { x1: middle, ..rect }, Rect { x0: middle, ..rect })
                };
                self.side(a, top, color, outer);
                self.side(b, top, color, inner);
            }
            BorderStyle::Dotted | BorderStyle::Dashed => {
                self.line(rect, top, color, thickness, style == BorderStyle::Dotted);
            }
        }
    }

    /// A dashed or dotted side: a line `width` thick through its middle.
    fn line(&mut self, rect: Rect, top: bool, color: Color, width: f64, dotted: bool) {
        let (mut start, mut end, mut across) = if top {
            (rect.x0, rect.x1, rect.y0 + (width / 2.0).trunc())
        } else {
            (rect.y0, rect.y1, rect.x0 + (width / 2.0).trunc())
        };
        let along = |start: f64, end: f64, across0: f64, across1: f64| {
            if top {
                Rect::new(start, across0, end, across1)
            } else {
                Rect::new(across0, start, across1, end)
            }
        };
        let length = end - start;
        let pattern = pattern(length, width, dotted);
        if dotted && width <= 3.0 {
            // Thin dots are square: whole ones at both ends, the line between them.
            let dots = end_dots(length, width);
            let (above, below) = ((width / 2.0).trunc(), width - (width / 2.0).trunc());
            if let Some((growth, offset)) = dots.start {
                let dot = along(
                    start,
                    start + width + growth,
                    across - above,
                    across + below,
                );
                self.fill(dot, color);
                start += 2.0 * width + offset;
            }
            if let Some(growth) = dots.end {
                let dot = along(end - width - growth, end, across - above, across + below);
                self.fill(dot, color);
                end -= width + growth + 1.0;
            }
        } else if dotted {
            // Round dots' caps reach past the line's ends.
            start += width / 2.0;
            end -= width / 2.0;
        }
        if width % 2.0 == 1.0 {
            across += 0.5;
        }
        let (across0, across1) = (across - width / 2.0, across + width / 2.0);
        let (run, direction) = ((end - start).abs(), (end - start).signum());
        let at = |distance: f64| start + direction * distance;
        match pattern {
            None => self.fill(
                along(start.min(end), start.max(end), across0, across1),
                color,
            ),
            Some(Pattern::Dashes(dash, gap)) => {
                let mut distance = 0.0;
                while distance < run {
                    let (a, b) = (at(distance), at((distance + dash).min(run)));
                    self.fill(along(a.min(b), a.max(b), across0, across1), color);
                    distance += dash + gap;
                }
            }
            Some(Pattern::Dots(period)) => {
                let mut dots = BezPath::new();
                let mut distance = 0.0;
                while distance < run {
                    let along = at(distance);
                    let centre = if top {
                        Point::new(along, across)
                    } else {
                        Point::new(across, along)
                    };
                    let centre = ((centre - self.origin).to_vec2() * self.scale).to_point();
                    dots.extend(Circle::new(centre, width * self.scale / 2.0).path_elements(0.1));
                    distance += period;
                }
                self.scene.fill(
                    Fill::NonZero,
                    self.transform,
                    color,
                    None,
                    &dots,
                    AntiAliasing::Enabled,
                );
            }
        }
    }
}

/// How a line `length` long and `width` thick breaks up (Blink's `DashEffectFromStrokeStyle`):
/// `None` when it's solid.
fn pattern(length: f64, width: f64, dotted: bool) -> Option<Pattern> {
    if !dotted || width <= 3.0 {
        let (dash, gap) = match (dotted, width >= 3.0) {
            (true, _) => (width, width),
            (false, true) => (2.0 * width, width),
            (false, false) => (3.0 * width, 2.0 * width),
        };
        if length <= 2.0 * dash {
            return None;
        }
        let two_dashes = 2.0 * dash + gap;
        if length <= two_dashes {
            let scale = length / two_dashes;
            return Some(Pattern::Dashes(dash * scale, gap * scale));
        }
        let gap = if dotted {
            gap
        } else {
            best_gap(length, dash, gap)
        };
        return Some(Pattern::Dashes(dash, gap));
    }
    if length < 2.0 * width {
        return Some(Pattern::Dots(2.0 * width));
    }
    // A hair less, so the last dot isn't lost to rounding.
    Some(Pattern::Dots(best_gap(length, width, width) + width - 0.01))
}

/// The gap between `dash`es along `length` that's nearest `gap` with whole dashes at both ends.
fn best_gap(length: f64, dash: f64, gap: f64) -> f64 {
    let fewer = ((length + gap) / (dash + gap)).floor();
    let more = fewer + 1.0;
    let fewer_gap = (length - fewer * dash) / (fewer - 1.0);
    let more_gap = (length - more * dash) / (more - 1.0);
    if more_gap <= 0.0 || (fewer_gap - gap).abs() < (more_gap - gap).abs() {
        fewer_gap
    } else {
        more_gap
    }
}

/// The square dots a thin dotted line draws at its ends itself, so both are whole (Blink's
/// `EnforceDotsAtEndpoints`).
struct EndDots {
    /// How much longer than wide the first dot is, and how much longer the gap after it.
    start: Option<(f64, f64)>,
    /// How much longer than wide the last dot is.
    end: Option<f64>,
}

fn end_dots(length: f64, width: f64) -> EndDots {
    let (length, width) = (length as i64, width as i64);
    let (by_4, by_6) = (length % 4, length % 6);
    let mut dots = EndDots {
        start: None,
        end: None,
    };
    if (width == 1 && length % 2 == 0) || (width == 3 && by_6 == 0) {
        dots.start = Some((1.0, 1.0));
    }
    if (width == 2 && (by_4 == 0 || by_4 == 1)) || (width == 3 && (by_6 == 1 || by_6 == 2)) {
        dots.start = Some((0.0, -1.0));
    }
    if (width == 2 && by_4 == 0) || (width == 3 && by_6 == 1) {
        dots.end = Some(0.0);
    }
    if (width == 2 && by_4 == 3) || (width == 3 && (by_6 == 4 || by_6 == 5)) {
        dots.start = Some((0.0, 1.0));
    }
    if width == 3 && by_6 == 5 {
        dots.end = Some(0.0);
    } else if width == 3 && by_6 == 0 {
        dots.end = Some(1.0);
    }
    dots
}
