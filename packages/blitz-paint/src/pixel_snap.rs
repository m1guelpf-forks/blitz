use kurbo::{Affine, Vec2};

/// Rounds device-pixel coordinates in the space of a transform to whole CSS pixels of the scene,
/// as browsers snap the edges of boxes and text decoration lines so that they are crisp.
///
/// Coordinates are left as they are under transforms other than translations.
#[derive(Clone, Copy)]
pub(crate) struct PixelSnap {
    /// The transform's translation, if it is only a translation
    offset: Option<Vec2>,
    scale: f64,
}

impl PixelSnap {
    pub(crate) fn new(transform: Affine, scale: f64) -> Self {
        let [a, b, c, d, x, y] = transform.as_coeffs();
        let offset = (a == 1.0 && b == 0.0 && c == 0.0 && d == 1.0).then_some(Vec2 { x, y });
        Self { offset, scale }
    }

    pub(crate) fn x(self, x: f64) -> f64 {
        self.snap(x, self.offset.map(|offset| offset.x))
    }

    pub(crate) fn y(self, y: f64) -> f64 {
        self.snap(y, self.offset.map(|offset| offset.y))
    }

    fn snap(self, value: f64, offset: Option<f64>) -> f64 {
        match offset {
            Some(offset) => ((value + offset) / self.scale + 0.5).floor() * self.scale - offset,
            None => value,
        }
    }
}
