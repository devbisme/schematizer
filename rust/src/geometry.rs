//! Points, transformation matrices, bounding boxes and segments.
//!
//! The placement and routing engine works in mils; symbols and the written
//! files are in millimeters.

use std::ops::{Add, Div, Mul, Neg, Sub};

pub const MILS_PER_MM: f64 = 39.37008;
pub const MMS_PER_MIL: f64 = 0.0254;

/// Round half to even, as Python's `round()` does.
#[inline]
pub fn pyround(x: f64) -> f64 {
    x.round_ties_even()
}

/// Python's `round(x, ndigits)`.
///
/// Like Python, this rounds the exact binary value of `x`, so 2.675 (really
/// 2.67499999...) rounds down to 2.67, where `(x * 100).round()` gives 2.68.
/// Rust's decimal formatting does exactly that, with ties to even.
pub fn round_to(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let r: f64 = format!("{:.*}", ndigits, x).parse().unwrap_or(x);
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// A 2D transformation:
/// ```text
/// x' = a*x + c*y + dx
/// y' = b*x + d*y + dy
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tx {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub dx: f64,
    pub dy: f64,
}

impl Default for Tx {
    fn default() -> Self {
        Tx::IDENT
    }
}

impl Tx {
    pub const IDENT: Tx = Tx { a: 1.0, b: 0.0, c: 0.0, d: 1.0, dx: 0.0, dy: 0.0 };
    pub const ROT_0: Tx = Tx { a: 1.0, b: 0.0, c: 0.0, d: 1.0, dx: 0.0, dy: 0.0 };
    pub const ROT_90: Tx = Tx { a: 0.0, b: 1.0, c: -1.0, d: 0.0, dx: 0.0, dy: 0.0 };
    pub const ROT_180: Tx = Tx { a: -1.0, b: 0.0, c: 0.0, d: -1.0, dx: 0.0, dy: 0.0 };
    pub const ROT_270: Tx = Tx { a: 0.0, b: -1.0, c: 1.0, d: 0.0, dx: 0.0, dy: 0.0 };
    pub const FLIP_X: Tx = Tx { a: -1.0, b: 0.0, c: 0.0, d: 1.0, dx: 0.0, dy: 0.0 };
    pub const FLIP_Y: Tx = Tx { a: 1.0, b: 0.0, c: 0.0, d: -1.0, dx: 0.0, dy: 0.0 };

    pub const fn new(a: f64, b: f64, c: f64, d: f64, dx: f64, dy: f64) -> Tx {
        Tx { a, b, c, d, dx, dy }
    }

    pub const fn lin(a: f64, b: f64, c: f64, d: f64) -> Tx {
        Tx { a, b, c, d, dx: 0.0, dy: 0.0 }
    }

    pub const fn translate(dx: f64, dy: f64) -> Tx {
        Tx { a: 1.0, b: 0.0, c: 0.0, d: 1.0, dx, dy }
    }

    pub const fn scale_by(s: f64) -> Tx {
        Tx { a: s, b: 0.0, c: 0.0, d: s, dx: 0.0, dy: 0.0 }
    }

    /// Build a transformation from a string of H, V, L, R operations applied left to right.
    pub fn from_symtx(symtx: &str) -> Tx {
        let mut tx = Tx::IDENT;
        for op in symtx.chars() {
            let m = match op.to_ascii_uppercase() {
                'H' => Tx::lin(-1.0, 0.0, 0.0, 1.0),
                'V' => Tx::lin(1.0, 0.0, 0.0, -1.0),
                'L' => Tx::lin(0.0, 1.0, -1.0, 0.0),
                'R' => Tx::lin(0.0, -1.0, 1.0, 0.0),
                _ => continue,
            };
            tx = tx * m;
        }
        tx
    }

    pub fn origin(&self) -> Point {
        Point::new(self.dx, self.dy)
    }

    pub fn scale(&self) -> f64 {
        (Point::new(1.0, 0.0) * *self - Point::new(0.0, 0.0) * *self).magnitude()
    }

    pub fn move_by(&self, vec: Point) -> Tx {
        *self * Tx::translate(vec.x, vec.y)
    }

    pub fn rot_90cw(&self) -> Tx {
        *self * Tx::lin(0.0, 1.0, -1.0, 0.0)
    }

    pub fn rot(&self, degs: f64) -> Tx {
        let r = degs.to_radians();
        *self * Tx::lin(r.cos(), r.sin(), -r.sin(), r.cos())
    }

    pub fn flip_x(&self) -> Tx {
        *self * Tx::lin(-1.0, 0.0, 0.0, 1.0)
    }

    pub fn flip_y(&self) -> Tx {
        *self * Tx::lin(1.0, 0.0, 0.0, -1.0)
    }

    pub fn no_translate(&self) -> Tx {
        Tx::lin(self.a, self.b, self.c, self.d)
    }

    /// Return (rotation in degrees, mirrored in X, mirrored in Y) of the linear part.
    pub fn analyze_transform(&self) -> (f64, bool, bool) {
        let rotation_degrees = self.c.atan2(self.a).to_degrees();
        let rotation_degrees =
            (pyround((360.0 - rotation_degrees).rem_euclid(360.0) / 90.0) * 90.0).rem_euclid(360.0);
        let det = self.a * self.d - self.b * self.c;
        let (mut mirror_x, mut mirror_y) = (false, false);
        if det < 0.0 {
            // Test X mirror: flip the sign of a, b and c.
            let det_x = (-self.a) * self.d - (-self.b) * (-self.c);
            let det_y = self.a * (-self.d) - self.b * self.c;
            if det_x > 0.0 {
                mirror_x = true;
            } else if det_y > 0.0 {
                mirror_y = true;
            } else {
                mirror_x = true;
            }
        }
        (rotation_degrees, mirror_x, mirror_y)
    }

    pub fn key(&self) -> [i64; 6] {
        let k = |v: f64| (v * 1000.0).round() as i64;
        [k(self.a), k(self.b), k(self.c), k(self.d), k(self.dx), k(self.dy)]
    }
}

impl Mul<Tx> for Tx {
    type Output = Tx;
    #[inline]
    fn mul(self, tx: Tx) -> Tx {
        Tx {
            a: self.a * tx.a + self.b * tx.c,
            b: self.a * tx.b + self.b * tx.d,
            c: self.c * tx.a + self.d * tx.c,
            d: self.c * tx.b + self.d * tx.d,
            dx: self.dx * tx.a + self.dy * tx.c + tx.dx,
            dy: self.dx * tx.b + self.dy * tx.d + tx.dy,
        }
    }
}

impl Mul<f64> for Tx {
    type Output = Tx;
    fn mul(self, m: f64) -> Tx {
        self * Tx::scale_by(m)
    }
}

/// A 2D point or vector.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

pub type Vector = Point;

/// A point with integer coordinates, for use as a map key.
pub type PKey = (i64, i64);

impl Point {
    #[inline]
    pub const fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// An integer key for the point (the point should already be on integer coordinates).
    #[inline]
    pub fn key(&self) -> PKey {
        (self.x.round() as i64, self.y.round() as i64)
    }

    pub fn from_key(k: PKey) -> Point {
        Point::new(k.0 as f64, k.1 as f64)
    }

    pub fn xprod(&self, pt: Point) -> f64 {
        self.x * pt.y - self.y * pt.x
    }

    pub fn mask(&self, msk: (f64, f64)) -> Point {
        Point::new(self.x * msk.0, self.y * msk.1)
    }

    pub fn round(&self) -> Point {
        Point::new(pyround(self.x), pyround(self.y))
    }

    pub fn snap(&self, grid: f64) -> Point {
        let f = |x: f64| (grid * pyround(x / grid)).trunc();
        Point::new(f(self.x), f(self.y))
    }

    pub fn min(&self, pt: Point) -> Point {
        Point::new(self.x.min(pt.x), self.y.min(pt.y))
    }

    pub fn max(&self, pt: Point) -> Point {
        Point::new(self.x.max(pt.x), self.y.max(pt.y))
    }

    pub fn magnitude(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn norm(&self) -> Point {
        let m = self.magnitude();
        if m == 0.0 {
            Point::new(0.0, 0.0)
        } else {
            *self / m
        }
    }

    pub fn flip_xy(&mut self) {
        std::mem::swap(&mut self.x, &mut self.y);
    }

    /// Component-wise product.
    pub fn mul_pt(&self, m: Point) -> Point {
        Point::new(self.x * m.x, self.y * m.y)
    }

    /// Lexicographic comparison (x, then y), as Python tuples compare.
    pub fn lt(&self, other: &Point) -> bool {
        (self.x, self.y) < (other.x, other.y)
    }
}

impl Add for Point {
    type Output = Point;
    #[inline]
    fn add(self, p: Point) -> Point {
        Point::new(self.x + p.x, self.y + p.y)
    }
}
impl Add<f64> for Point {
    type Output = Point;
    fn add(self, v: f64) -> Point {
        Point::new(self.x + v, self.y + v)
    }
}
impl Sub for Point {
    type Output = Point;
    #[inline]
    fn sub(self, p: Point) -> Point {
        Point::new(self.x - p.x, self.y - p.y)
    }
}
impl Sub<f64> for Point {
    type Output = Point;
    fn sub(self, v: f64) -> Point {
        Point::new(self.x - v, self.y - v)
    }
}
impl Mul<Tx> for Point {
    type Output = Point;
    #[inline]
    fn mul(self, m: Tx) -> Point {
        Point::new(self.x * m.a + self.y * m.c + m.dx, self.x * m.b + self.y * m.d + m.dy)
    }
}
impl Mul<f64> for Point {
    type Output = Point;
    #[inline]
    fn mul(self, m: f64) -> Point {
        Point::new(self.x * m, self.y * m)
    }
}
impl Div<f64> for Point {
    type Output = Point;
    #[inline]
    fn div(self, d: f64) -> Point {
        Point::new(self.x / d, self.y / d)
    }
}
impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

/// A bounding box. An empty box has min = +inf and max = -inf.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub min: Point,
    pub max: Point,
}

impl Default for BBox {
    fn default() -> Self {
        BBox::empty()
    }
}

impl BBox {
    pub const fn empty() -> BBox {
        BBox {
            min: Point::new(f64::INFINITY, f64::INFINITY),
            max: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
        }
    }

    pub fn new(p1: Point, p2: Point) -> BBox {
        BBox {
            min: Point::new(p1.x.min(p2.x), p1.y.min(p2.y)),
            max: Point::new(p1.x.max(p2.x), p1.y.max(p2.y)),
        }
    }

    pub fn from_pts(pts: &[Point]) -> BBox {
        let mut b = BBox::empty();
        for p in pts {
            b.add_pt(*p);
        }
        b
    }

    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y
    }

    pub fn add_pt(&mut self, p: Point) -> &mut BBox {
        self.min = Point::new(self.min.x.min(p.x), self.min.y.min(p.y));
        self.max = Point::new(self.max.x.max(p.x), self.max.y.max(p.y));
        self
    }

    pub fn add(&mut self, b: BBox) -> &mut BBox {
        self.min = Point::new(self.min.x.min(b.min.x), self.min.y.min(b.min.y));
        self.max = Point::new(self.max.x.max(b.max.x), self.max.y.max(b.max.y));
        self
    }

    pub fn plus(&self, b: BBox) -> BBox {
        let mut r = *self;
        r.add(b);
        r
    }

    pub fn round(&self) -> BBox {
        BBox::new(self.min.round(), self.max.round())
    }

    pub fn is_inside(&self, pt: Point) -> bool {
        self.min.x <= pt.x && pt.x <= self.max.x && self.min.y <= pt.y && pt.y <= self.max.y
    }

    pub fn intersects(&self, b: &BBox) -> bool {
        self.min.x < b.max.x && self.max.x > b.min.x && self.min.y < b.max.y && self.max.y > b.min.y
    }

    pub fn intersection(&self, b: &BBox) -> Option<BBox> {
        if !self.intersects(b) {
            return None;
        }
        Some(BBox::new(self.min.max(b.min), self.max.min(b.max)))
    }

    pub fn resize(&self, v: Point) -> BBox {
        BBox::new(self.min - v, self.max + v)
    }

    pub fn snap_resize(&self, grid: f64) -> BBox {
        let mut b = self.resize(Point::new(grid - 1.0, grid - 1.0));
        b.min = b.min.snap(grid);
        b.max = b.max.snap(grid);
        b
    }

    pub fn area(&self) -> f64 {
        self.w() * self.h()
    }

    pub fn w(&self) -> f64 {
        (self.max.x - self.min.x).abs()
    }

    pub fn h(&self) -> f64 {
        (self.max.y - self.min.y).abs()
    }

    pub fn ctr(&self) -> Point {
        (self.max + self.min) / 2.0
    }

    pub fn ll(&self) -> Point {
        Point::new(self.min.x, self.min.y)
    }
    pub fn lr(&self) -> Point {
        Point::new(self.max.x, self.min.y)
    }
    pub fn ul(&self) -> Point {
        Point::new(self.min.x, self.max.y)
    }
    pub fn ur(&self) -> Point {
        Point::new(self.max.x, self.max.y)
    }

    /// The box as (x1, y1, x2, y2).
    pub fn tuple(&self) -> (f64, f64, f64, f64) {
        (self.min.x, self.min.y, self.max.x, self.max.y)
    }
}

impl Mul<Tx> for BBox {
    type Output = BBox;
    fn mul(self, m: Tx) -> BBox {
        let (x1, y1, x2, y2) = (self.min.x, self.min.y, self.max.x, self.max.y);
        let tx1 = x1 * m.a + y1 * m.c + m.dx;
        let ty1 = x1 * m.b + y1 * m.d + m.dy;
        let tx2 = x2 * m.a + y2 * m.c + m.dx;
        let ty2 = x2 * m.b + y2 * m.d + m.dy;
        // Python's min(inf, a, b) keeps inf only when both are NaN.
        let mn = |a: f64, b: f64| f64::INFINITY.min(a).min(b);
        let mx = |a: f64, b: f64| f64::NEG_INFINITY.max(a).max(b);
        BBox { min: Point::new(mn(tx1, tx2), mn(ty1, ty2)), max: Point::new(mx(tx1, tx2), mx(ty1, ty2)) }
    }
}

/// A line segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub p1: Point,
    pub p2: Point,
}

impl Segment {
    pub const fn new(p1: Point, p2: Point) -> Segment {
        Segment { p1, p2 }
    }

    pub fn round(&self) -> Segment {
        Segment::new(self.p1.round(), self.p2.round())
    }

    pub fn flip_xy(&mut self) {
        self.p1.flip_xy();
        self.p2.flip_xy();
    }

    /// True if two parallel axis-aligned segments overlap when projected onto their axis.
    pub fn shadows(&self, other: &Segment) -> bool {
        let (smin, smax, omin, omax);
        if self.p1.x == self.p2.x && other.p1.x == other.p2.x {
            smin = self.p1.y.min(self.p2.y);
            smax = self.p1.y.max(self.p2.y);
            omin = other.p1.y.min(other.p2.y);
            omax = other.p1.y.max(other.p2.y);
        } else if self.p1.y == self.p2.y && other.p1.y == other.p2.y {
            smin = self.p1.x.min(self.p2.x);
            smax = self.p1.x.max(self.p2.x);
            omin = other.p1.x.min(other.p2.x);
            omax = other.p1.x.max(other.p2.x);
        } else {
            return false;
        }
        omin < smax && omax > smin
    }
}

impl Mul<Tx> for Segment {
    type Output = Segment;
    fn mul(self, m: Tx) -> Segment {
        Segment::new(self.p1 * m, self.p2 * m)
    }
}
