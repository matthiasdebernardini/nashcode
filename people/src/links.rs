//! The links layer: where a link starts and ends, the curve between, and which one
//! the pointer is on.
//!
//! The endpoints are not stored anywhere. Each card writes its resolved bounds into a
//! shared map during prepaint, and this layer reads that map during paint of the same
//! frame, so a link follows the card through a scroll, a resize, or a rem change
//! without any of them telling it to.
//!
//! The arithmetic is pure and the hover test walks the same samples the painter
//! draws, so what lights up under the pointer is what is on screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{Bounds, Path, PathBuilder, Pixels, Point, point, px};

use crate::board::CardId;

/// Where every card was on the last frame that drew it. Shared between the cards,
/// which fill it in prepaint, and the links canvas, which reads it in paint.
pub type CardBounds = Rc<RefCell<HashMap<CardId, Bounds<Pixels>>>>;

/// The two ends of one link, on the facing edges of its two cards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Endpoints {
    pub from: Point<Pixels>,
    pub to: Point<Pixels>,
}

/// A link leaves the right edge of the left card and arrives at the left edge of the
/// right card, both at the card's vertical centre. Lanes are ordered left to right,
/// so the reading direction and the drawing direction are the same one.
pub fn endpoints(from: Bounds<Pixels>, to: Bounds<Pixels>) -> Endpoints {
    Endpoints {
        from: point(from.right(), from.center().y),
        to: point(to.left(), to.center().y),
    }
}

/// The control points of the S-curve: level with each end, half the horizontal gap
/// in. The curve therefore leaves and arrives horizontally, which is what makes a
/// row of them read as wires rather than as a scribble.
fn controls(ends: &Endpoints) -> (Point<Pixels>, Point<Pixels>) {
    let reach = (ends.to.x - ends.from.x) * 0.5;
    (point(ends.from.x + reach, ends.from.y), point(ends.to.x - reach, ends.to.y))
}

/// The curve, ready to paint. `None` when the two ends are the same point, which
/// lyon cannot stroke.
pub fn path(ends: &Endpoints, width: Pixels) -> Option<Path<Pixels>> {
    if ends.from == ends.to {
        return None;
    }
    let (first, second) = controls(ends);
    let mut builder = PathBuilder::stroke(width);
    builder.move_to(ends.from);
    builder.cubic_bezier_to(ends.to, first, second);
    builder.build().ok()
}

/// The same curve as a polyline. The hover test walks these, so the pointer agrees
/// with the picture; sixteen steps is under half a pixel of error at the widths a
/// desktop window has.
pub fn sample(ends: &Endpoints, steps: usize) -> Vec<Point<Pixels>> {
    let (first, second) = controls(ends);
    (0..=steps)
        .map(|step| {
            let t = step as f32 / steps as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            let blend = |from: Pixels, one: Pixels, two: Pixels, to: Pixels| {
                px(a * f32::from(from) + b * f32::from(one) + c * f32::from(two) + d * f32::from(to))
            };
            point(
                blend(ends.from.x, first.x, second.x, ends.to.x),
                blend(ends.from.y, first.y, second.y, ends.to.y),
            )
        })
        .collect()
}

/// Which link the pointer is on, if any: the nearest within `tolerance`.
///
/// `links` pairs each candidate with the index of the [`crate::board::Link`] it draws,
/// so the answer names a link rather than a position in this slice.
pub fn nearest(
    links: &[(usize, Endpoints)],
    pointer: Point<Pixels>,
    tolerance: Pixels,
) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (index, ends) in links {
        let curve = sample(ends, 16);
        let mut closest = f32::MAX;
        for pair in curve.windows(2) {
            closest = closest.min(distance_to_segment(pointer, pair[0], pair[1]));
        }
        if closest <= f32::from(tolerance) && best.is_none_or(|(so_far, _)| closest < so_far) {
            best = Some((closest, *index));
        }
    }
    best.map(|(_, index)| index)
}

/// How far `p` is from the segment `a`–`b`.
fn distance_to_segment(p: Point<Pixels>, a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    let (px_, py) = (f32::from(p.x), f32::from(p.y));
    let (ax, ay) = (f32::from(a.x), f32::from(a.y));
    let (bx, by) = (f32::from(b.x), f32::from(b.y));
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    // A zero-length segment is a point; the projection below would divide by zero.
    let t = if length <= f32::EPSILON {
        0.0
    } else {
        (((px_ - ax) * dx + (py - ay) * dy) / length).clamp(0.0, 1.0)
    };
    let (nx, ny) = (ax + t * dx, ay + t * dy);
    ((px_ - nx).powi(2) + (py - ny).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn card(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(100.), px(40.)))
    }

    #[test]
    fn a_link_leaves_the_right_edge_and_arrives_at_the_left_edge_of_its_two_cards() {
        let ends = endpoints(card(0., 0.), card(300., 100.));

        assert_eq!(ends.from, point(px(100.), px(20.)), "right edge, vertical centre");
        assert_eq!(ends.to, point(px(300.), px(120.)), "left edge, vertical centre");
    }

    #[test]
    fn an_endpoint_moves_exactly_as_far_as_its_card_scrolls() {
        // What makes the links follow a scroll: nothing is cached, so a card that
        // moved forty points up takes its end of every wire with it.
        let still = endpoints(card(0., 0.), card(300., 100.));
        let scrolled = endpoints(card(0., -40.), card(300., 60.));

        assert_eq!(scrolled.from.y, still.from.y - px(40.));
        assert_eq!(scrolled.to.y, still.to.y - px(40.));
        assert_eq!((scrolled.from.x, scrolled.to.x), (still.from.x, still.to.x));
    }

    #[test]
    fn the_curve_starts_and_ends_on_its_endpoints_and_leaves_them_level() {
        let ends = endpoints(card(0., 0.), card(300., 200.));
        let curve = sample(&ends, 16);

        assert_eq!(curve[0], ends.from);
        assert_eq!(*curve.last().expect("a sample"), ends.to);
        // It leaves horizontally: the first step drops far less than it travels.
        let step = curve[1];
        assert!(
            (step.y - ends.from.y).abs() < (step.x - ends.from.x).abs(),
            "{step:?} is not a level departure from {:?}",
            ends.from
        );
        // And it stays between its two ends rather than looping out.
        assert!(curve.iter().all(|p| p.x >= ends.from.x && p.x <= ends.to.x));
        assert!(curve.iter().all(|p| p.y >= ends.from.y && p.y <= ends.to.y));
    }

    #[test]
    fn the_pointer_finds_the_link_it_is_on_and_no_other() {
        let first = endpoints(card(0., 0.), card(300., 0.));
        let second = endpoints(card(0., 200.), card(300., 200.));
        let links = [(7usize, first), (9usize, second)];

        // Dead on the upper wire.
        assert_eq!(nearest(&links, point(px(200.), px(20.)), px(6.)), Some(7));
        // Dead on the lower one.
        assert_eq!(nearest(&links, point(px(200.), px(220.)), px(6.)), Some(9));
        // Between them, and out of reach of both.
        assert_eq!(nearest(&links, point(px(200.), px(120.)), px(6.)), None);
    }

    #[test]
    fn a_pointer_between_two_wires_takes_the_nearer_one() {
        // Two wires ten points apart, both well inside the tolerance, so only the
        // distance decides which one the pointer claims.
        let upper = endpoints(card(0., 0.), card(300., 0.)); // y = 20
        let lower = endpoints(card(0., 10.), card(300., 10.)); // y = 30
        let links = [(1usize, upper), (2usize, lower)];

        assert_eq!(nearest(&links, point(px(150.), px(22.)), px(20.)), Some(1));
        assert_eq!(nearest(&links, point(px(150.), px(28.)), px(20.)), Some(2));
    }

    #[test]
    fn two_cards_on_top_of_one_another_draw_nothing_rather_than_panicking() {
        let same = Endpoints { from: point(px(5.), px(5.)), to: point(px(5.), px(5.)) };
        assert!(path(&same, px(1.5)).is_none());
        assert!(path(&endpoints(card(0., 0.), card(300., 0.)), px(1.5)).is_some());
    }
}
