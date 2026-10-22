//! Where a picture or video frame lands on a display for each fit mode.

use crate::model::Fit;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(
            self.left.max(o.left),
            self.top.max(o.top),
            self.right.min(o.right),
            self.bottom.min(o.bottom),
        )
    }

    pub fn union(&self, o: &Rect) -> Rect {
        Rect::new(
            self.left.min(o.left),
            self.top.min(o.top),
            self.right.max(o.right),
            self.bottom.max(o.bottom),
        )
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.left + dx,
            self.top + dy,
            self.right + dx,
            self.bottom + dy,
        )
    }
}

/// The rectangle the whole (scaled) source occupies on a `dst_w` x `dst_h` canvas. It can
/// reach past the canvas edges (Fill and Center crop).
pub fn place(fit: Fit, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
    let (sw, sh, dw, dh) = (
        src_w.max(1) as f64,
        src_h.max(1) as f64,
        dst_w as f64,
        dst_h as f64,
    );
    let (w, h) = match fit {
        Fit::Stretch => (dw, dh),
        Fit::Center => (sw, sh),
        Fit::Fill => {
            let s = (dw / sw).max(dh / sh);
            (sw * s, sh * s)
        }
        Fit::Fit => {
            let s = (dw / sw).min(dh / sh);
            (sw * s, sh * s)
        }
    };
    let (w, h) = (w.round() as i32, h.round() as i32);
    let left = ((dst_w as i32) - w) / 2;
    let top = ((dst_h as i32) - h) / 2;
    Rect::new(left, top, left + w, top + h)
}

/// For video: the visible part of the source in normalised coordinates (0..1) and where it
/// goes on the canvas, in pixels.
pub fn video_rects(fit: Fit, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> ([f32; 4], Rect) {
    let placed = place(fit, src_w, src_h, dst_w, dst_h);
    let canvas = Rect::new(0, 0, dst_w as i32, dst_h as i32);
    let dst = placed.intersect(&canvas);
    let (pw, ph) = (placed.width().max(1) as f32, placed.height().max(1) as f32);
    let src = [
        (dst.left - placed.left) as f32 / pw,
        (dst.top - placed.top) as f32 / ph,
        (dst.right - placed.left) as f32 / pw,
        (dst.bottom - placed.top) as f32 / ph,
    ];
    (src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_crops_the_long_side() {
        // A 4:3 picture on a 16:9 display fills the width and crops top and bottom.
        let r = place(Fit::Fill, 1600, 1200, 1920, 1080);
        assert_eq!((r.width(), r.height()), (1920, 1440));
        assert_eq!((r.left, r.top), (0, -180));
        let (src, dst) = video_rects(Fit::Fill, 1600, 1200, 1920, 1080);
        assert_eq!(dst, Rect::new(0, 0, 1920, 1080));
        assert!(
            (src[1] - 0.125).abs() < 1e-3 && (src[3] - 0.875).abs() < 1e-3,
            "{src:?}"
        );
    }

    #[test]
    fn fit_letterboxes() {
        let r = place(Fit::Fit, 1600, 1200, 1920, 1080);
        assert_eq!(r, Rect::new(240, 0, 1680, 1080));
        let (src, dst) = video_rects(Fit::Fit, 1600, 1200, 1920, 1080);
        assert_eq!(src, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(dst, r);
    }

    #[test]
    fn stretch_and_center() {
        assert_eq!(
            place(Fit::Stretch, 640, 480, 1920, 1080),
            Rect::new(0, 0, 1920, 1080)
        );
        assert_eq!(
            place(Fit::Center, 640, 480, 1920, 1080),
            Rect::new(640, 300, 1280, 780)
        );
        // A larger-than-display picture is centred and cropped on every side.
        let (src, dst) = video_rects(Fit::Center, 3840, 2160, 1920, 1080);
        assert_eq!(dst, Rect::new(0, 0, 1920, 1080));
        assert_eq!(src, [0.25, 0.25, 0.75, 0.75]);
    }

    #[test]
    fn rect_ops() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, -20, 150, 60);
        assert_eq!(a.intersect(&b), Rect::new(50, 0, 100, 60));
        assert_eq!(a.union(&b), Rect::new(0, -20, 150, 100));
        assert!(Rect::new(10, 10, 10, 20).is_empty());
    }
}
