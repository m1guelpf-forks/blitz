//! `text-decoration-skip-ink: auto`, as Blink paints it: an underline or overline is cut where
//! the glyphs above it cross it (`TextPainter::ClipDecorationsStripe`).

use std::ops::Range;

use blitz_dom::node::TextBrush;
use kurbo::{CubicBez, ParamCurve as _, Point, QuadBez};
use parley::Line;
use skrifa::instance::{LocationRef, NormalizedCoord, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider as _};

use crate::text::glyph_runs;

/// Where a decoration line is cut, in the line's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct InkGap {
    pub(crate) x0: f64,
    pub(crate) x1: f64,
    pub(crate) y0: f64,
    pub(crate) y1: f64,
}

/// The gaps a decoration line spanning `top..bottom` skips in the ink of the line's glyph runs
/// `runs`, sorted and merged. As Blink: the band the glyphs are measured across is the line's
/// less half a CSS px above and below; a glyph whose outline crosses it (Skia's
/// `SkFont::getIntercepts`, from the outline's leftmost crossing to its rightmost) cuts the line
/// from `dilation` before to `dilation` after, and a CSS px above and below the band.
pub(crate) fn ink_gaps(
    line: &Line<'_, TextBrush>,
    runs: Range<usize>,
    top: f64,
    bottom: f64,
    dilation: f64,
    scale: f64,
) -> Vec<InkGap> {
    let band = (
        top + scale / 2.0,
        (bottom - scale / 2.0).max(top + scale / 2.0),
    );
    let mut spans: Vec<(f64, f64)> = Vec::new();
    for glyph_run in glyph_runs(line).skip(runs.start).take(runs.len()) {
        let run = glyph_run.run();
        let font = run.font();
        let Ok(font_ref) = FontRef::from_index(font.data.as_ref(), font.index) else {
            continue;
        };
        let outlines = font_ref.outline_glyphs();
        let coords: &[NormalizedCoord] = bytemuck::cast_slice(run.normalized_coords());
        for glyph in glyph_run.positioned_glyphs() {
            let Some(outline) = outlines.get(GlyphId::new(glyph.id)) else {
                continue;
            };
            let baseline = glyph.y as f64;
            let mut pen = BandPen::new(band.0 - baseline, band.1 - baseline);
            let settings =
                DrawSettings::unhinted(Size::new(run.font_size()), LocationRef::new(coords));
            if outline.draw(settings, &mut pen).is_err() {
                continue;
            }
            if let Some((left, right)) = pen.extent() {
                let x = glyph.x as f64;
                spans.push((x + left - dilation, x + right + dilation));
            }
        }
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut gaps: Vec<InkGap> = Vec::with_capacity(spans.len());
    for (x0, x1) in spans {
        match gaps.last_mut() {
            Some(last) if x0 <= last.x1 => last.x1 = last.x1.max(x1),
            _ if x1 > x0 => gaps.push(InkGap {
                x0,
                x1,
                y0: band.0 - scale,
                y1: band.1 + scale,
            }),
            _ => {}
        }
    }
    gaps
}

/// Measures where an outline (y up, as fonts draw) crosses the band `top..bottom` (y down from
/// the baseline), as Skia's `calculate_path_gap`: where each segment crosses the band's edges,
/// and the segment's points inside it.
struct BandPen {
    top: f64,
    bottom: f64,
    start: Point,
    current: Point,
    left: f64,
    right: f64,
}

impl BandPen {
    fn new(top: f64, bottom: f64) -> Self {
        Self {
            top,
            bottom,
            start: Point::ZERO,
            current: Point::ZERO,
            left: f64::INFINITY,
            right: f64::NEG_INFINITY,
        }
    }

    fn extent(&self) -> Option<(f64, f64)> {
        (self.left < self.right).then_some((self.left, self.right))
    }

    fn expand(&mut self, x: f64) {
        self.left = self.left.min(x);
        self.right = self.right.max(x);
    }

    /// The points of a segment strictly inside the band, and whether any of them reach it.
    fn add_points(&mut self, points: &[Point]) -> bool {
        let (min, max) = points
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), p| {
                (min.min(p.y), max.max(p.y))
            });
        for p in points {
            if self.top < p.y && p.y < self.bottom {
                self.expand(p.x);
            }
        }
        min <= self.bottom && max >= self.top
    }

    fn line(&mut self, to: Point) {
        let from = self.current;
        for offset in [self.top, self.bottom] {
            let t = (offset - from.y) / (to.y - from.y);
            if (0.0..1.0).contains(&t) {
                self.expand(from.x + t * (to.x - from.x));
            }
        }
        self.add_points(&[from, to]);
        self.current = to;
    }

    fn crossings(&mut self, at: impl Fn(f64) -> Point, roots: impl Fn(f64) -> Vec<f64>) {
        for offset in [self.top, self.bottom] {
            for t in roots(offset) {
                if (0.0..=1.0).contains(&t) {
                    self.expand(at(t).x);
                }
            }
        }
    }
}

impl OutlinePen for BandPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = Point::new(x as f64, -y as f64);
        self.current = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.line(Point::new(x as f64, -y as f64));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let quad = QuadBez::new(
            self.current,
            Point::new(cx0 as f64, -cy0 as f64),
            Point::new(x as f64, -y as f64),
        );
        if self.add_points(&[quad.p0, quad.p1, quad.p2]) {
            let [y0, y1, y2] = [quad.p0.y, quad.p1.y, quad.p2.y];
            self.crossings(
                |t| quad.eval(t),
                |offset| {
                    kurbo::common::solve_quadratic(y0 - offset, 2.0 * (y1 - y0), y0 - 2.0 * y1 + y2)
                        .to_vec()
                },
            );
        }
        self.current = quad.p2;
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let cubic = CubicBez::new(
            self.current,
            Point::new(cx0 as f64, -cy0 as f64),
            Point::new(cx1 as f64, -cy1 as f64),
            Point::new(x as f64, -y as f64),
        );
        if self.add_points(&[cubic.p0, cubic.p1, cubic.p2, cubic.p3]) {
            let [y0, y1, y2, y3] = [cubic.p0.y, cubic.p1.y, cubic.p2.y, cubic.p3.y];
            self.crossings(
                |t| cubic.eval(t),
                |offset| {
                    kurbo::common::solve_cubic(
                        y0 - offset,
                        3.0 * (y1 - y0),
                        3.0 * (y0 - 2.0 * y1 + y2),
                        y3 - y0 + 3.0 * (y1 - y2),
                    )
                    .to_vec()
                },
            );
        }
        self.current = cubic.p3;
    }

    fn close(&mut self) {
        if self.current != self.start {
            self.line(self.start);
        }
    }
}
