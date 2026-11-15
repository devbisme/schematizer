//! Autorouter for the wiring between symbols in a schematic.
//!
//! The edges of each part's box are extended to form tracks that divide the
//! routing area into four-sided switchboxes. Each side of a switchbox is a
//! face, shared with the switchbox next door.
//!
//! Each face can carry as many wires as it has grid points (faces on a part
//! carry none, which keeps wires out of parts). Faces on parts get terminals
//! for the part's pins.
//!
//! Global routing then finds, for each net, a sequence of faces that connects
//! its pins: a maze router with switchboxes as its squares. Each net is given
//! a terminal on every face it crosses.
//!
//! That leaves switchboxes with fixed terminals along their faces. A greedy
//! switchbox router (<https://doi.org/10.1016/0167-9260(85)90029-X>) does the
//! detailed routing inside each one, and the wiring of all the switchboxes
//! put together is the wiring of the node.

use indexmap::IndexMap;
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use crate::geometry::{BBox, Point, Segment};
use crate::ir::{NetId, NodeId, PartId, PinId};

use super::{progress, Design, Error, Res};

const GRID: i64 = 50;

/// Times a child sheet is placed and routed before giving up, and how much
/// more room its parts get each time.
pub const SHEET_ROUTE_TRIES: usize = 6;
pub const SHEET_EXPANSION: f64 = 1.25;

/// Times global routing is tried, each with the nets that failed before routed first.
const GLOBAL_ROUTE_TRIES: usize = 4;

const TOP: usize = 0;
const LEFT: usize = 1;
const BOTTOM: usize = 2;
const RIGHT: usize = 3;

type Net = Option<NetId>;
type P = (i64, i64);

fn fail<T>(msg: impl Into<String>) -> Res<T> {
    Err(Error::Routing(msg.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TrackRef {
    horz: bool,
    idx: usize,
}

/// A horizontal or vertical line that faces lie on.
#[derive(Clone, Debug, Default)]
struct Track {
    coord: i64,
    faces: Vec<usize>,
    /// Indices of the orthogonal tracks that cross this one.
    splits: BTreeSet<usize>,
}

/// What a face is an edge of.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Owner {
    Part(PartId),
    /// The outside of the routing area, which no wire can cross.
    Boundary,
}

/// A point on a face where a net crosses it.
#[derive(Clone, Copy, Debug)]
struct Terminal {
    net: Net,
    coord: i64,
}

/// A side of a switchbox.
#[derive(Clone, Debug)]
struct Face {
    part: Vec<Owner>,
    track: TrackRef,
    /// Indices of the orthogonal tracks the face begins and ends at.
    beg: usize,
    end: usize,
    terminals: Vec<Terminal>,
    /// Faces a wire can get to from this one, and how far away they are.
    adjacent: Vec<(usize, f64)>,
    switchboxes: Vec<usize>,
    capacity: i64,
}

/// A vertical stretch of wire in a switchbox column, from one row to another.
#[derive(Clone, Copy, Debug)]
struct NetInterval {
    net: NetId,
    beg: usize,
    end: usize,
}

impl NetInterval {
    fn new(net: NetId, a: usize, b: usize) -> NetInterval {
        NetInterval { net, beg: a.min(b), end: a.max(b) }
    }
    fn intersects(&self, o: &NetInterval) -> bool {
        !(self.beg > o.end || self.end < o.beg)
    }
    fn obstructs(&self, o: &NetInterval) -> bool {
        self.intersects(o) && self.net != o.net
    }
}

/// A point on a switchbox face that the switchbox router hasn't reached yet.
#[derive(Clone, Copy, Debug)]
struct Target {
    net: NetId,
    row: usize,
    col: usize,
}

/// A rectangular region that wires are routed through.
#[derive(Clone, Debug)]
struct SwitchBox {
    /// Top, left, bottom and right faces.
    faces: [usize; 4],
    track_coords: Vec<i64>,
    column_coords: Vec<i64>,
    left_nets: Vec<Net>,
    right_nets: Vec<Net>,
    top_nets: Vec<Net>,
    bottom_nets: Vec<Net>,
    segments: IndexMap<NetId, Vec<Segment>>,
}

impl SwitchBox {
    /// Move the nets at the ends of the left and right faces (the corners) to
    /// the ends of the top and bottom faces, so they're routed in the columns.
    fn move_corner_nets(&mut self) {
        let last = self.left_nets.len() - 1;
        let cols = self.top_nets.len() - 1;
        if self.left_nets[0].is_some() {
            self.bottom_nets[0] = self.left_nets[0].take();
        }
        if self.left_nets[last].is_some() {
            self.top_nets[0] = self.left_nets[last].take();
        }
        if self.right_nets[0].is_some() {
            self.bottom_nets[cols] = self.right_nets[0].take();
        }
        if self.right_nets[last].is_some() {
            self.top_nets[cols] = self.right_nets[last].take();
        }
    }

    /// Flip X and Y to route from top to bottom instead of left to right.
    fn flip_xy(&mut self) {
        std::mem::swap(&mut self.column_coords, &mut self.track_coords);
        std::mem::swap(&mut self.top_nets, &mut self.right_nets);
        std::mem::swap(&mut self.bottom_nets, &mut self.left_nets);
        self.faces.swap(TOP, RIGHT);
        self.faces.swap(BOTTOM, LEFT);
        self.move_corner_nets();
        for segments in self.segments.values_mut() {
            for seg in segments.iter_mut() {
                seg.flip_xy();
            }
        }
    }
}

/// Python's `list[start:].index(x)` and `list[start::-1].index(x)`: the offset
/// of the nearest `net` going up from `start`, or (negative) going down.
fn net_search(net: Net, start: usize, track_nets: &[Net]) -> i64 {
    let large = 2 * track_nets.len() as i64;
    let up = track_nets.iter().skip(start).position(|&n| n == net).map_or(large, |i| i as i64);
    let down = if track_nets.is_empty() {
        large
    } else {
        let from = start.min(track_nets.len() - 1);
        track_nets[..=from].iter().rev().position(|&n| n == net).map_or(large, |i| i as i64)
    };
    if up <= down {
        up
    } else {
        -down
    }
}

/// Index a list the way Python does, with negative indices counting from the end.
fn py_index(i: i64, len: usize) -> Option<usize> {
    let i = if i < 0 { i + len as i64 } else { i };
    (i >= 0 && (i as usize) < len).then_some(i as usize)
}

/// A wire segment on integer coordinates, with its ends in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct S {
    p1: P,
    p2: P,
}

impl S {
    fn new(a: P, b: P) -> S {
        if b < a {
            S { p1: b, p2: a }
        } else {
            S { p1: a, p2: b }
        }
    }
    fn horz(&self) -> bool {
        self.p1.1 == self.p2.1
    }
    fn vert(&self) -> bool {
        self.p1.0 == self.p2.0
    }
    fn contains(&self, pt: P) -> bool {
        self.p1.0 <= pt.0 && pt.0 <= self.p2.0 && self.p1.1 <= pt.1 && pt.1 <= self.p2.1
    }
    fn bbox(&self) -> BBox {
        BBox::new(Point::new(self.p1.0 as f64, self.p1.1 as f64), Point::new(self.p2.0 as f64, self.p2.1 as f64))
    }
    fn to_segment(self) -> Segment {
        Segment::new(Point::new(self.p1.0 as f64, self.p1.1 as f64), Point::new(self.p2.0 as f64, self.p2.1 as f64))
    }
}

fn segs_bbox(segs: &[S]) -> BBox {
    let mut b = BBox::empty();
    for s in segs {
        b.add(s.bbox());
    }
    b
}

/// Split segments wherever a perpendicular segment of the net meets them or a pin of the net is on them.
fn split_segments(segments: &[S], net_pin_pts: &[P]) -> Vec<S> {
    let mut horz: Vec<S> = segments.iter().copied().filter(|s| s.horz()).collect();
    let mut vert: Vec<S> = segments.iter().copied().filter(|s| s.vert()).collect();
    let mut i = 0;
    while i < horz.len() {
        let y = horz[i].p1.1;
        let mut j = 0;
        while j < vert.len() {
            let x = vert[j].p1.0;
            if horz[i].p1.0 <= x && x <= horz[i].p2.0 && vert[j].p1.1 <= y && y <= vert[j].p2.1 {
                let pt = (x, y);
                if pt != horz[i].p1 && pt != horz[i].p2 {
                    horz.push(S { p1: pt, p2: horz[i].p2 });
                    horz[i].p2 = pt;
                }
                if pt != vert[j].p1 && pt != vert[j].p2 {
                    vert.push(S { p1: pt, p2: vert[j].p2 });
                    vert[j].p2 = pt;
                }
            }
            j += 1;
        }
        i += 1;
    }
    let mut i = 0;
    while i < horz.len() {
        for &pt in net_pin_pts {
            if pt.1 == horz[i].p1.1 && horz[i].p1.0 < pt.0 && pt.0 < horz[i].p2.0 {
                horz.push(S { p1: pt, p2: horz[i].p2 });
                horz[i].p2 = pt;
            }
        }
        i += 1;
    }
    let mut j = 0;
    while j < vert.len() {
        for &pt in net_pin_pts {
            if pt.0 == vert[j].p1.0 && vert[j].p1.1 < pt.1 && pt.1 < vert[j].p2.1 {
                vert.push(S { p1: pt, p2: vert[j].p2 });
                vert[j].p2 = pt;
            }
        }
        j += 1;
    }
    horz.extend(vert);
    horz
}

/// Merge segments that run the same way and overlap. (Also removes duplicates.)
fn merge_segments(segments: &[S]) -> Vec<S> {
    let mut merged: Vec<S> = Vec::new();
    let mut by_y: IndexMap<i64, Vec<S>> = IndexMap::new();
    for s in segments.iter().filter(|s| s.horz()) {
        by_y.entry(s.p1.1).or_default().push(*s);
    }
    for (_, mut segs) in by_y {
        segs.sort_by_key(|s| s.p1.0);
        merged.push(segs[0]);
        for s in &segs[1..] {
            let last = merged.last_mut().unwrap();
            if s.p1.0 <= last.p2.0 {
                last.p2.0 = last.p2.0.max(s.p2.0);
            } else {
                merged.push(*s);
            }
        }
    }
    let mut by_x: IndexMap<i64, Vec<S>> = IndexMap::new();
    for s in segments.iter().filter(|s| s.vert() && !s.horz()) {
        by_x.entry(s.p1.0).or_default().push(*s);
    }
    for (_, mut segs) in by_x {
        segs.sort_by_key(|s| s.p1.1);
        merged.push(segs[0]);
        for s in &segs[1..] {
            let last = merged.last_mut().unwrap();
            if s.p1.1 <= last.p2.1 {
                last.p2.1 = last.p2.1.max(s.p2.1);
            } else {
                merged.push(*s);
            }
        }
    }
    merged
}

/// Remove segments to break any loops in a net's wiring.
fn break_cycles(segments: Vec<S>) -> Vec<S> {
    let mut alive = vec![true; segments.len()];
    let mut adj_segs: IndexMap<P, Vec<usize>> = IndexMap::new();
    for (i, s) in segments.iter().enumerate() {
        adj_segs.entry(s.p1).or_default().push(i);
        adj_segs.entry(s.p2).or_default().push(i);
    }
    // The points next to each point, in order: that order decides which segment of a loop is cut.
    let mut adj_pts: HashMap<P, Vec<P>> = HashMap::new();
    for (pt, segs) in &adj_segs {
        let mut pts: Vec<P> = segs.iter().flat_map(|&i| [segments[i].p1, segments[i].p2]).filter(|p| p != pt).collect();
        pts.sort();
        pts.dedup();
        adj_pts.insert(*pt, pts);
    }
    let mut visited: HashSet<P> = HashSet::new();
    let mut frontier: Vec<P> = adj_segs.keys().take(1).copied().collect();
    while let Some(pt) = frontier.pop() {
        visited.insert(pt);
        for adj in adj_pts[&pt].clone() {
            if visited.contains(&adj) || frontier.contains(&adj) {
                // This point was already reached another way, so there's a loop.
                // Break it by removing the segment between the two points.
                let loop_seg = adj_segs[&pt].iter().copied().find(|i| alive[*i] && adj_segs[&adj].contains(i));
                if let Some(i) = loop_seg {
                    alive[i] = false;
                }
                adj_pts.get_mut(&pt).unwrap().retain(|p| *p != adj);
                adj_pts.get_mut(&adj).unwrap().retain(|p| *p != pt);
            } else {
                frontier.push(adj);
                // Keep the new point from coming back to this one later.
                adj_pts.get_mut(&adj).unwrap().retain(|p| *p != pt);
            }
        }
    }
    segments.into_iter().zip(alive).filter(|(_, a)| *a).map(|(s, _)| s).collect()
}

/// Remove stubs: segments with an end that touches nothing and isn't on a pin.
fn trim_stubs(segments: &[S], pin_pts: &HashSet<P>) -> Vec<S> {
    let mut segs: Vec<S> = segments.to_vec();
    segs.sort();
    segs.dedup();
    loop {
        let mut count: HashMap<P, usize> = HashMap::new();
        for s in &segs {
            *count.entry(s.p1).or_insert(0) += 1;
            *count.entry(s.p2).or_insert(0) += 1;
        }
        let is_stub = |s: &S| [s.p1, s.p2].iter().any(|p| count[p] == 1 && !pin_pts.contains(p));
        let before = segs.len();
        let kept: Vec<S> = segs.iter().copied().filter(|s| !is_stub(s)).collect();
        segs = kept;
        if segs.len() == before {
            break;
        }
    }
    segs
}

/// What the wire cleanup has to work around.
struct Cleanup<'a> {
    part_bboxes: Vec<BBox>,
    /// The net (or none) of the pin at each pin location.
    pin_nets: HashMap<P, Net>,
    pin_pts: &'a HashSet<P>,
}

impl Cleanup<'_> {
    /// True if a segment enters a part's box other than along an allowed segment.
    ///
    /// A part's box can reach past its pin tips, so the router's wire to such
    /// a pin runs a little way inside the box. A new segment can follow it
    /// there, but go no further in.
    fn inside_part(&self, seg: &S, part_bbox: &BBox, allowed: &[S]) -> bool {
        if !part_bbox.intersects(&seg.bbox()) {
            return false;
        }
        if seg.horz() {
            let lo = (seg.p1.0 as f64).max(part_bbox.min.x);
            let hi = (seg.p2.0 as f64).min(part_bbox.max.x);
            return !allowed.iter().any(|a| a.horz() && a.p1.1 == seg.p1.1 && a.p1.0 as f64 <= lo && hi <= a.p2.0 as f64);
        }
        let lo = (seg.p1.1 as f64).max(part_bbox.min.y);
        let hi = (seg.p2.1 as f64).min(part_bbox.max.y);
        !allowed.iter().any(|a| a.vert() && a.p1.0 == seg.p1.0 && a.p1.1 as f64 <= lo && hi <= a.p2.1 as f64)
    }

    /// True if a segment is obstructed by parts, pins or the wires of other nets.
    fn obstructed(
        &self,
        net: NetId,
        seg: &S,
        wires: &IndexMap<NetId, Vec<S>>,
        net_bboxes: &IndexMap<NetId, BBox>,
        allowed: &[S],
    ) -> bool {
        if self.part_bboxes.iter().any(|b| self.inside_part(seg, b, allowed)) {
            return true;
        }
        // A segment can run along the edge of a part, but not over the end of
        // a pin that isn't on its net: it would look connected to it.
        if self.pin_nets.iter().any(|(pt, pn)| *pn != Some(net) && seg.contains(*pt)) {
            return true;
        }

        // Boxes that line up edge to edge don't intersect, so the segment's box is grown a little.
        let seg_bbox = seg.bbox().resize(Point::new(2.0, 2.0));
        for (nt, nt_bbox) in net_bboxes {
            if *nt == net || !seg_bbox.intersects(nt_bbox) {
                continue;
            }
            for o in &wires[nt] {
                // Wires of two nets can't run along each other.
                if seg.vert() && o.vert() && seg.p1.0 == o.p1.0 {
                    if seg.p1.1 <= o.p2.1 && seg.p2.1 >= o.p1.1 {
                        return true;
                    }
                } else if seg.horz() && o.horz() && seg.p1.1 == o.p1.1 && seg.p1.0 <= o.p2.0 && seg.p2.0 >= o.p1.0 {
                    return true;
                }
                // KiCad joins a wire end to any wire it touches, so neither segment may end on the other.
                if o.contains(seg.p1) || o.contains(seg.p2) || seg.contains(o.p1) || seg.contains(o.p2) {
                    return true;
                }
            }
        }
        false
    }
}

/// The routing state of one node.
struct Router {
    h: Vec<Track>,
    v: Vec<Track>,
    faces: Vec<Face>,
    /// Top, bottom, left and right tracks of each part.
    part_tracks: HashMap<PartId, [TrackRef; 4]>,
    pin_face: HashMap<PinId, usize>,
}

impl Router {
    fn track(&self, t: TrackRef) -> &Track {
        if t.horz {
            &self.h[t.idx]
        } else {
            &self.v[t.idx]
        }
    }
    fn track_mut(&mut self, t: TrackRef) -> &mut Track {
        if t.horz {
            &mut self.h[t.idx]
        } else {
            &mut self.v[t.idx]
        }
    }
    /// The track orthogonal to a face that has the given index.
    fn ortho(&self, f: usize, idx: usize) -> &Track {
        if self.faces[f].track.horz {
            &self.v[idx]
        } else {
            &self.h[idx]
        }
    }
    fn beg_coord(&self, f: usize) -> i64 {
        self.ortho(f, self.faces[f].beg).coord
    }
    fn end_coord(&self, f: usize) -> i64 {
        self.ortho(f, self.faces[f].end).coord
    }
    fn track_coord(&self, f: usize) -> i64 {
        self.track(self.faces[f].track).coord
    }
    fn length(&self, f: usize) -> i64 {
        self.end_coord(f) - self.beg_coord(f)
    }
    /// A key that orders faces by where they are.
    fn sort_key(&self, f: usize) -> (u8, i64, i64, i64) {
        (if self.faces[f].track.horz { 1 } else { 2 }, self.track_coord(f), self.beg_coord(f), self.end_coord(f))
    }
    fn face_bbox(&self, f: usize) -> BBox {
        let (t, b, e) = (self.track_coord(f) as f64, self.beg_coord(f) as f64, self.end_coord(f) as f64);
        if self.faces[f].track.horz {
            BBox::new(Point::new(b, t), Point::new(e, t))
        } else {
            BBox::new(Point::new(t, b), Point::new(t, e))
        }
    }
    fn has_nets(&self, f: usize) -> bool {
        self.faces[f].terminals.iter().any(|t| t.net.is_some())
    }

    fn new_face(&mut self, part: Vec<Owner>, track: TrackRef, a: usize, b: usize) -> usize {
        let (beg, end) = (a.min(b), a.max(b));
        self.faces.push(Face { part, track, beg, end, terminals: Vec::new(), adjacent: Vec::new(), switchboxes: Vec::new(), capacity: 0 });
        let id = self.faces.len() - 1;
        let t = self.track_mut(track);
        t.faces.push(id);
        // The orthogonal tracks that bound the face split this track.
        t.splits.insert(beg);
        t.splits.insert(end);
        id
    }

    /// Extend a part's face along its track until it's blocked by a part's face that crosses it.
    fn extend_face(&mut self, f: usize) {
        if self.faces[f].part.is_empty() {
            return;
        }
        let track = self.faces[f].track;
        let n_ortho = if track.horz { self.v.len() } else { self.h.len() };
        for (start, dir) in [(self.faces[f].beg, -1i64), (self.faces[f].end, 1)] {
            let mut k = start as i64;
            'tracks: while k >= 0 && (k as usize) < n_ortho {
                let ortho = TrackRef { horz: !track.horz, idx: k as usize };
                for of in self.track(ortho).faces.clone() {
                    // The extension has to hit the inside of the face, not just touch its end.
                    if self.faces[of].beg < track.idx && track.idx < self.faces[of].end {
                        self.track_mut(ortho).splits.insert(track.idx);
                        self.track_mut(track).splits.insert(ortho.idx);
                        if !self.faces[of].part.is_empty() {
                            self.new_face(Vec::new(), track, start, ortho.idx);
                            break 'tracks;
                        }
                        break;
                    }
                }
                k += dir;
            }
        }
    }

    /// Unassigned terminals along a face, one per grid point.
    ///
    /// A terminal can be right at the start of a short face, but never at the
    /// end, where it would coincide with the first one of the next face.
    fn create_nonpin_terminals(&mut self, f: usize) -> Res<()> {
        let (b, e) = (self.beg_coord(f), self.end_coord(f));
        let beg = if e - b <= GRID { (b + GRID - 1).div_euclid(GRID) * GRID } else { (b + GRID).div_euclid(GRID) * GRID };
        let mut coord = beg;
        while coord < e {
            self.add_terminal(f, None, coord)?;
            coord += GRID;
        }
        Ok(())
    }

    fn add_terminal(&mut self, f: usize, net: Net, coord: i64) -> Res<()> {
        let face = &mut self.faces[f];
        if !face.part.is_empty() && net.is_none() {
            return Ok(());
        }
        if let Some(i) = face.terminals.iter().position(|t| t.coord == coord) {
            if net.is_none() {
                return Ok(());
            }
            if face.terminals[i].net.is_some() && face.terminals[i].net != net {
                return fail("Two nets need the same point on the edge of a routing area");
            }
            face.terminals.remove(i);
        }
        face.terminals.push(Terminal { net, coord });
        Ok(())
    }

    /// The left, bottom and right faces of the switchbox with the given top face.
    fn box_faces(&self, top: usize) -> Option<[usize; 4]> {
        let t = &self.faces[top];
        let left = *self.v[t.beg].faces.iter().find(|&&f| self.faces[f].end == t.track.idx)?;
        let right = *self.v[t.end].faces.iter().find(|&&f| self.faces[f].end == t.track.idx)?;
        self.check_box([top, left, self.find_bottom(top, left)?, right])
    }

    fn find_bottom(&self, top: usize, left: usize) -> Option<usize> {
        let t = &self.faces[top];
        self.h[self.faces[left].beg].faces.iter().copied().find(|&f| self.faces[f].beg == t.beg && self.faces[f].end == t.end)
    }

    fn check_box(&self, faces: [usize; 4]) -> Option<[usize; 4]> {
        let [top, left, bottom, right] = faces;
        // The left and right faces don't begin level where two parts are
        // butted against each other, which isn't a routable switchbox.
        if self.beg_coord(left) != self.beg_coord(right) {
            return None;
        }
        // If all four sides are on one part, the switchbox is inside it.
        let common = self.faces[top]
            .part
            .iter()
            .any(|o| [bottom, left, right].iter().all(|&f| self.faces[f].part.contains(o)));
        (!common).then_some(faces)
    }

    fn new_switchbox(&self, faces: [usize; 4]) -> SwitchBox {
        let [top, left, bottom, right] = faces;
        let net_at = |f: usize, coord: i64| self.faces[f].terminals.iter().find(|t| t.coord == coord).and_then(|t| t.net);
        let coords = |f: usize| self.faces[f].terminals.iter().map(|t| t.coord);

        // The horizontal routing tracks.
        let mut track_coords: Vec<i64> = coords(left).chain(coords(right)).collect();
        track_coords.push(self.track_coord(top));
        track_coords.push(self.track_coord(bottom));
        track_coords.sort();
        track_coords.dedup();
        if track_coords.len() == 2 {
            // The switchbox is too narrow to hold a track in the middle, so
            // two pseudo-tracks along the top and bottom let routing go on.
            track_coords = vec![track_coords[0], track_coords[0], track_coords[1], track_coords[1]];
        }
        let left_nets: Vec<Net> = track_coords.iter().map(|&c| net_at(left, c)).collect();
        let right_nets: Vec<Net> = track_coords.iter().map(|&c| net_at(right, c)).collect();

        // The vertical columns.
        let mut column_coords: Vec<i64> = coords(top).chain(coords(bottom)).collect();
        column_coords.push(self.track_coord(left));
        column_coords.push(self.track_coord(right));
        column_coords.sort();
        column_coords.dedup();
        let top_nets: Vec<Net> = column_coords.iter().map(|&c| net_at(top, c)).collect();
        let bottom_nets: Vec<Net> = column_coords.iter().map(|&c| net_at(bottom, c)).collect();

        let mut sb = SwitchBox {
            faces,
            track_coords,
            column_coords,
            left_nets,
            right_nets,
            top_nets,
            bottom_nets,
            segments: IndexMap::new(),
        };

        // Nets with a single terminal in the switchbox have nothing to connect to.
        let mut counts: HashMap<NetId, usize> = HashMap::new();
        for net in sb.left_nets.iter().chain(&sb.right_nets).chain(&sb.top_nets).chain(&sb.bottom_nets).flatten() {
            *counts.entry(*net).or_insert(0) += 1;
        }
        for side in [&mut sb.left_nets, &mut sb.right_nets, &mut sb.top_nets, &mut sb.bottom_nets] {
            for net in side.iter_mut() {
                if net.is_some_and(|n| counts[&n] <= 1) {
                    *net = None;
                }
            }
        }
        sb.move_corner_nets();
        sb
    }

    fn box_bbox(&self, sb: &SwitchBox) -> BBox {
        self.face_bbox(sb.faces[TOP]).plus(self.face_bbox(sb.faces[LEFT]))
    }

    fn box_has_nets(&self, sb: &SwitchBox) -> bool {
        sb.faces.iter().any(|&f| self.has_nets(f))
    }
}

impl Design {
    fn net_key(&self, net: NetId) -> (String, i64) {
        (self.c.nets[net].name.clone(), self.c.nets[net].code)
    }

    fn pin_pt(&self, pin: PinId) -> P {
        (self.c.pins[pin].pt * self.c.pin_part(pin).tx).round().key()
    }

    /// Extend wires from the pins out to the edges of their parts' boxes.
    /// Returns where all the wired pins are.
    fn add_routing_points(&mut self, node: NodeId, nets: &[NetId]) -> HashSet<P> {
        let mut pin_pts = HashSet::new();
        for &net in nets {
            for pin in self.get_internal_pins(node, net) {
                pin_pts.insert(self.pin_pt(pin));
                let part = self.c.pin_part(pin);
                let (bbox, tx) = (part.lbl_bbox, part.tx);
                let p = &mut self.c.pins[pin];
                let mut route_pt = p.pt;
                match p.orientation {
                    'U' => route_pt.y = bbox.min.y,
                    'D' => route_pt.y = bbox.max.y,
                    'L' => route_pt.x = bbox.max.x,
                    _ => route_pt.x = bbox.min.x,
                }
                p.route_pt = route_pt;
                if route_pt != p.pt {
                    let seg = Segment::new(p.pt, route_pt) * tx;
                    self.nodes[node].wires.entry(net).or_default().push(seg);
                }
            }
        }
        pin_pts
    }

    /// Create the horizontal and vertical tracks and the faces on them.
    fn create_routing_tracks(&self, node: NodeId, routing_bbox: BBox) -> Router {
        let key = |v: f64| v.round() as i64;
        let boxes: Vec<(Owner, BBox)> = self.nodes[node]
            .parts
            .iter()
            .map(|&p| (Owner::Part(p), (self.c.parts[p].lbl_bbox * self.c.parts[p].tx).round()))
            .chain([(Owner::Boundary, routing_bbox)])
            .collect();

        // The edges of each part's box, and of the routing area, define the tracks.
        let mut v_coords: Vec<i64> = boxes.iter().flat_map(|(_, b)| [key(b.min.x), key(b.max.x)]).collect();
        let mut h_coords: Vec<i64> = boxes.iter().flat_map(|(_, b)| [key(b.min.y), key(b.max.y)]).collect();
        v_coords.sort();
        v_coords.dedup();
        h_coords.sort();
        h_coords.dedup();

        let mut r = Router {
            h: h_coords.iter().map(|&coord| Track { coord, ..Default::default() }).collect(),
            v: v_coords.iter().map(|&coord| Track { coord, ..Default::default() }).collect(),
            faces: Vec::new(),
            part_tracks: HashMap::new(),
            pin_face: HashMap::new(),
        };

        // Faces for each side of each box.
        for (owner, b) in &boxes {
            let left = TrackRef { horz: false, idx: v_coords.binary_search(&key(b.min.x)).unwrap() };
            let right = TrackRef { horz: false, idx: v_coords.binary_search(&key(b.max.x)).unwrap() };
            let bottom = TrackRef { horz: true, idx: h_coords.binary_search(&key(b.min.y)).unwrap() };
            let top = TrackRef { horz: true, idx: h_coords.binary_search(&key(b.max.y)).unwrap() };
            r.new_face(vec![*owner], left, bottom.idx, top.idx);
            r.new_face(vec![*owner], right, bottom.idx, top.idx);
            r.new_face(vec![*owner], bottom, left.idx, right.idx);
            r.new_face(vec![*owner], top, left.idx, right.idx);
            if let Owner::Part(p) = owner {
                r.part_tracks.insert(*p, [top, bottom, left, right]);
            }
        }

        // Extend the part faces in each horizontal track, then each vertical track.
        for horz in [true, false] {
            let n = if horz { r.h.len() } else { r.v.len() };
            for idx in 0..n {
                for f in r.track(TrackRef { horz, idx }).faces.clone() {
                    r.extend_face(f);
                }
            }
        }

        // Split the faces where tracks cross them, and combine faces that coincide.
        for horz in [true, false] {
            let n = if horz { r.h.len() } else { r.v.len() };
            for idx in 0..n {
                let t = TrackRef { horz, idx };
                for split in r.track(t).splits.clone() {
                    for f in r.track(t).faces.clone() {
                        if r.faces[f].beg < split && split < r.faces[f].end {
                            let (part, beg) = (r.faces[f].part.clone(), r.faces[f].beg);
                            r.new_face(part, t, beg, split);
                            r.faces[f].beg = split;
                        }
                    }
                }
                let mut kept: Vec<usize> = Vec::new();
                for f in r.track(t).faces.clone() {
                    match kept.iter().copied().find(|&k| r.faces[k].beg == r.faces[f].beg && r.faces[k].end == r.faces[f].end) {
                        Some(k) => {
                            for o in r.faces[f].part.clone() {
                                if !r.faces[k].part.contains(&o) {
                                    r.faces[k].part.push(o);
                                }
                            }
                        }
                        None => kept.push(f),
                    }
                }
                r.track_mut(t).faces = kept;
            }
        }

        // Faces of a switchbox are adjacent: a wire can go from one to another.
        for idx in 1..r.h.len() {
            for top in r.h[idx].faces.clone() {
                let Some([top, left, bottom, right]) = r.box_faces(top) else { continue };
                for (a, b) in [(top, bottom), (left, right), (left, top), (left, bottom), (right, top), (right, bottom)] {
                    // Faces on the boundary never take wires. Faces on the
                    // same part aren't adjacent, or wires would cross the part.
                    let (pa, pb) = (&r.faces[a].part, &r.faces[b].part);
                    if pa.contains(&Owner::Boundary) || pb.contains(&Owner::Boundary) || pa.iter().any(|o| pb.contains(o)) {
                        continue;
                    }
                    let dist = if r.faces[a].track.horz == r.faces[b].track.horz {
                        // Straight across, plus the average distance along the faces.
                        (r.track_coord(a) - r.track_coord(b)).abs() as f64 + (r.length(a) + r.length(b)) as f64 / 4.0
                    } else {
                        (r.length(a) + r.length(b)) as f64 / 2.0
                    };
                    r.faces[a].adjacent.push((b, dist));
                    r.faces[b].adjacent.push((a, dist));
                }
            }
        }
        for f in 0..r.faces.len() {
            let mut adj = std::mem::take(&mut r.faces[f].adjacent);
            adj.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| r.sort_key(a.0).cmp(&r.sort_key(b.0))));
            r.faces[f].adjacent = adj;
        }
        r
    }

    /// Create terminals on the faces: one per grid point, and one for each wired pin.
    fn create_terminals(&self, node: NodeId, nets: &[NetId], r: &mut Router) -> Res<()> {
        let live: Vec<usize> = r.h.iter().chain(r.v.iter()).flat_map(|t| t.faces.clone()).collect();
        for &f in &live {
            r.create_nonpin_terminals(f)?;
        }
        for &net in nets {
            for pin in self.get_internal_pins(node, net) {
                // Find which side of its part the pin is on.
                let part = self.c.pins[pin].part;
                let pt = self.c.pins[pin].route_pt * self.c.parts[part].tx;
                let [top, bottom, left, right] = r.part_tracks[&part];
                let mut closest = (pt.y - r.track(top).coord as f64).abs();
                let (mut pin_track, mut coord) = (top, pt.x);
                for (track, dist, c) in [
                    (bottom, (pt.y - r.track(bottom).coord as f64).abs(), pt.x),
                    (left, (pt.x - r.track(left).coord as f64).abs(), pt.y),
                    (right, (pt.x - r.track(right).coord as f64).abs(), pt.y),
                ] {
                    if dist < closest {
                        closest = dist;
                        pin_track = track;
                        coord = c;
                    }
                }
                let coord = coord.round() as i64;
                // Then the face in that track the pin is on.
                let face = r
                    .track(pin_track)
                    .faces
                    .iter()
                    .copied()
                    .find(|&f| r.faces[f].part.contains(&Owner::Part(part)) && r.beg_coord(f) <= coord && coord <= r.end_coord(f));
                if let Some(f) = face {
                    if !r.pin_face.contains_key(&pin) {
                        r.pin_face.insert(pin, f);
                        r.faces[f].terminals.push(Terminal { net: Some(net), coord });
                    }
                }
            }
        }
        // A face can carry as many wires as it has terminals; a part's face, none.
        for &f in &live {
            r.faces[f].capacity = if r.faces[f].part.is_empty() { r.faces[f].terminals.len() as i64 } else { 0 };
        }
        Ok(())
    }

    /// Find, for each net, the faces its wires cross to connect its pins.
    ///
    /// Each net's route is a list of wires, each a list of faces from one of
    /// the net's pins to a face already on the route.
    fn global_router(&mut self, node: NodeId, nets: &mut Vec<NetId>, r: &mut Router) -> Res<Vec<(NetId, Vec<Vec<usize>>)>> {
        // Nets whose pins are close together have the fewest ways to go, so they're routed first.
        let rank = |d: &Design, net: NetId| {
            let mut bbox = BBox::empty();
            for pin in d.get_internal_pins(node, net) {
                bbox.add_pt(d.c.pins[pin].route_pt);
            }
            (bbox.w() + bbox.h(), d.c.nets[net].pins.len())
        };
        let mut ranked: Vec<((f64, usize), NetId)> = nets.iter().map(|&n| (rank(self, n), n)).collect();
        ranked.sort_by(|a, b| a.0 .0.total_cmp(&b.0 .0).then(a.0 .1.cmp(&b.0 .1)));
        *nets = ranked.into_iter().map(|(_, n)| n).collect();

        // A net that can't get through was blocked by nets routed before it.
        // So the routes are cleared and all are routed again with it first.
        let capacities: Vec<i64> = r.faces.iter().map(|f| f.capacity).collect();
        let mut first: Vec<NetId> = Vec::new();
        for attempt in 0..GLOBAL_ROUTE_TRIES {
            let mut routes = Vec::new();
            let order: Vec<NetId> = first.iter().copied().chain(nets.iter().copied().filter(|n| !first.contains(n))).collect();
            let mut failed = None;
            for net in order {
                match self.route_net(node, net, r) {
                    Some(route) => routes.push((net, route)),
                    None => {
                        failed = Some(net);
                        break;
                    }
                }
            }
            let Some(net) = failed else { return Ok(routes) };
            if attempt == GLOBAL_ROUTE_TRIES - 1 {
                return fail(format!("Global routing failure: {}", self.c.nets[net].name));
            }
            first.retain(|n| *n != net);
            first.insert(0, net);
            for (f, c) in r.faces.iter_mut().zip(&capacities) {
                f.capacity = *c;
            }
        }
        Ok(Vec::new())
    }

    /// The wires that connect the faces holding a net's pins.
    fn route_net(&mut self, node: NodeId, net: NetId, r: &mut Router) -> Option<Vec<Vec<usize>>> {
        let mut pin_faces: Vec<usize> = Vec::new();
        for pin in self.get_internal_pins(node, net) {
            let f = *r.pin_face.get(&pin)?;
            if !pin_faces.contains(&f) {
                pin_faces.push(f);
            }
        }
        pin_faces.sort_by_key(|&f| r.sort_key(f));
        let net_pin_faces: HashSet<usize> = pin_faces.iter().copied().collect();
        if pin_faces.is_empty() {
            return Some(Vec::new());
        }

        // From a random face, look for a route to any of the others.
        let mut start_faces = pin_faces;
        let start = start_faces.remove(self.rng.below(start_faces.len()));
        let stop: HashSet<usize> = start_faces.iter().copied().collect();
        let initial = rt_srch(r, &net_pin_faces, start, &stop)?;
        // The faces on that route are where further routes can stop.
        let mut stop: HashSet<usize> = initial.iter().copied().collect();
        let mut route = vec![initial];
        for start in start_faces {
            let next = rt_srch(r, &net_pin_faces, start, &stop)?;
            stop.extend(next.iter().copied());
            route.push(next);
        }
        Some(route)
    }

    /// Create switchboxes from the faces and combine small ones into larger ones.
    fn create_switchboxes(&mut self, r: &mut Router) -> Res<Vec<SwitchBox>> {
        for f in r.faces.iter_mut() {
            f.switchboxes.clear();
        }
        // A switchbox for each horizontal face that can be the top of one.
        let mut boxes: Vec<SwitchBox> = Vec::new();
        for idx in 1..r.h.len() {
            for top in r.h[idx].faces.clone() {
                if let Some(faces) = r.box_faces(top) {
                    for f in faces {
                        r.faces[f].switchboxes.push(boxes.len());
                    }
                    boxes.push(r.new_switchbox(faces));
                }
            }
        }

        // Small switchboxes are the most likely to fail routing, so the ones
        // with nets are used as seeds to grow larger ones, smallest first.
        let mut seeds: Vec<usize> = (0..boxes.len()).filter(|&i| r.box_has_nets(&boxes[i])).collect();
        seeds.sort_by(|&a, &b| {
            let (ba, bb) = (r.box_bbox(&boxes[a]), r.box_bbox(&boxes[b]));
            (ba.w() + ba.h()).total_cmp(&(bb.w() + bb.h()))
        });
        let mut available = vec![true; boxes.len()];
        let mut coalesced = Vec::new();
        for seed in seeds {
            if !available[seed] {
                continue;
            }
            available[seed] = false;
            // The boxes along the top, left, bottom and right of the growing switchbox.
            let mut lists: [Vec<usize>; 4] = [vec![seed], vec![seed], vec![seed], vec![seed]];
            let mut active = [true; 4];
            loop {
                // Grow in the shorter dimension so the switchbox stays squarish.
                let mut bbox = BBox::empty();
                for list in &lists {
                    bbox.add(r.box_bbox(&boxes[list[0]]));
                }
                let growth: Vec<usize> = if bbox.w() == bbox.h() {
                    vec![TOP, LEFT, BOTTOM, RIGHT]
                } else if bbox.w() < bbox.h() {
                    vec![LEFT, RIGHT]
                } else {
                    vec![TOP, BOTTOM]
                };
                let growth: Vec<usize> = growth.into_iter().filter(|&d| active[d]).collect();
                if growth.is_empty() {
                    break;
                }
                let direction = growth[self.rng.below(growth.len())];

                // The boxes beyond the growth side all have to be free to take.
                let mut beyond = Vec::new();
                for &b in &lists[direction] {
                    let face = boxes[b].faces[direction];
                    if !r.faces[face].part.is_empty() {
                        break;
                    }
                    match r.faces[face].switchboxes.iter().copied().find(|&o| o != b) {
                        Some(adj) if available[adj] => beyond.push(adj),
                        _ => break,
                    }
                }
                if beyond.len() != lists[direction].len() {
                    active[direction] = false;
                    continue;
                }
                for &adj in &beyond {
                    available[adj] = false;
                }
                // The first and last boxes on the growth side are also on the sides next to it.
                let (first, last) = (beyond[0], *beyond.last().unwrap());
                lists[direction] = beyond;
                lists[(direction + 3) % 4].push(first);
                lists[(direction + 1) % 4].insert(0, last);
            }

            // New faces around the group of switchboxes.
            let mut total = [0usize; 4];
            for direction in [TOP, LEFT, BOTTOM, RIGHT] {
                let face_list: Vec<usize> = lists[direction].iter().map(|&b| boxes[b].faces[direction]).collect();
                let beg = face_list.iter().map(|&f| r.faces[f].beg).min().unwrap();
                let end = face_list.iter().map(|&f| r.faces[f].end).max().unwrap();
                let track = r.faces[face_list[0]].track;
                let total_face = r.new_face(Vec::new(), track, beg, end);
                r.create_nonpin_terminals(total_face)?;
                for &f in &face_list {
                    for t in r.faces[f].terminals.clone() {
                        if t.net.is_some() {
                            r.add_terminal(total_face, t.net, t.coord)?;
                        }
                    }
                }
                r.faces[total_face].capacity = r.faces[total_face].terminals.len() as i64;
                total[direction] = total_face;
            }
            if r.check_box(total).is_none() {
                return fail("Unroutable switchbox");
            }
            coalesced.push(r.new_switchbox(total));
        }
        Ok(coalesced)
    }

    /// Route the wires between the terminals on the faces of a switchbox.
    fn route_switchbox(&self, sb: &mut SwitchBox, r: &Router) -> Res<()> {
        if !r.box_has_nets(sb) {
            return Ok(());
        }
        let sbf = || fail::<()>("Switchbox routing failure");
        let rows = sb.left_nets.len();
        let last_track = rows - 1;

        // The nets the router has yet to reach along the top, bottom and right
        // faces, nearest column first.
        let (min_row, max_row, max_col) = (1usize, rows.saturating_sub(2), sb.top_nets.len());
        let mut targets: Vec<Target> = Vec::new();
        for (col, (t, b)) in sb.top_nets.iter().zip(&sb.bottom_nets).enumerate() {
            if let Some(net) = t {
                targets.push(Target { net: *net, row: max_row, col });
            }
            if let Some(net) = b {
                targets.push(Target { net: *net, row: min_row, col });
            }
        }
        for (row, net) in sb.right_nets.iter().enumerate() {
            if let Some(net) = net {
                targets.push(Target { net: *net, row, col: max_col });
            }
        }
        targets.sort_by_cached_key(|t| (t.col, t.row, self.net_key(t.net)));

        let mut nets_in_column: Vec<Vec<Net>> = vec![sb.left_nets.clone()];
        let mut all_column_intvls: Vec<Vec<NetInterval>> = Vec::new();

        for col in 0..sb.top_nets.len() {
            let (t_net, b_net) = (sb.top_nets[col], sb.bottom_nets[col]);
            let mut active: Vec<Net> = nets_in_column.last().unwrap().clone();
            if col == 0 && t_net.is_none() && b_net.is_none() {
                // Nothing happens in the first column if it has no top or bottom net.
                all_column_intvls.push(Vec::new());
                nets_in_column.push(active);
                continue;
            }
            active[0] = b_net;
            active[last_track] = t_net;

            // Connect the top and bottom nets of this column to horizontal tracks.
            let mut column: Vec<NetInterval> = Vec::new();
            if t_net.is_some() && t_net == b_net {
                column.push(NetInterval::new(t_net.unwrap(), 0, last_track));
            } else {
                // The closest track with the net, then the closest empty track,
                // searching from the net's side of the column.
                let find = |net: Net, down: bool| -> Vec<Option<usize>> {
                    if net.is_none() {
                        return vec![None];
                    }
                    let inner: Vec<usize> = if down { (1..last_track).rev().collect() } else { (1..last_track).collect() };
                    let mut found = Vec::new();
                    if let Some(&i) = inner.iter().find(|&&i| active[i] == net) {
                        found.push(Some(i));
                    }
                    if let Some(&i) = inner.iter().find(|&&i| active[i].is_none()) {
                        found.push(Some(i));
                    }
                    found
                };
                let t_cncts = find(t_net, true);
                let b_cncts = find(b_net, false);
                let mut chosen = None;
                'pairs: for &t in &t_cncts {
                    for &b in &b_cncts {
                        // The connections don't interfere if either is missing,
                        // or the top one stays above the bottom one.
                        let ok = match (t, b) {
                            (Some(t), Some(b)) => t > b || (t == b && t_net == b_net),
                            _ => true,
                        };
                        if ok {
                            chosen = Some((t, b));
                            break 'pairs;
                        }
                    }
                }
                let Some((t_cnct, b_cnct)) = chosen else { return sbf() };
                if let Some(t) = t_cnct {
                    column.push(NetInterval::new(t_net.unwrap(), t, last_track));
                }
                if let Some(b) = b_cnct {
                    column.push(NetInterval::new(b_net.unwrap(), 0, b));
                }
            }

            // The active nets, with the nets of the new vertical connections.
            let mut augmented = active.clone();
            for intvl in &column {
                augmented[intvl.beg] = Some(intvl.net);
                augmented[intvl.end] = Some(intvl.net);
            }

            // Targets in this column and to its left have been dealt with.
            targets.retain(|t| t.col > col);

            // Put target nets from columns to the right on tracks near them,
            // to steer the vertical connections toward them.
            {
                let mut placed: Vec<Net> = vec![None; rows];
                let active_right: Vec<Net> = sb.right_nets.iter().map(|n| if n.is_some() && augmented.contains(n) { *n } else { None }).collect();
                let mut search: Vec<Net> = augmented[1..last_track].to_vec();
                for target in &targets {
                    let net = Some(target.net);
                    if !augmented.contains(&net) || placed.contains(&net) || active_right.contains(&net) {
                        continue;
                    }
                    let net_off = net_search(net, target.row, &search);
                    let empty_off = net_search(None, target.row, &search);
                    let off = if net_off.abs() <= empty_off.abs() { net_off } else { empty_off };
                    let Some(i) = py_index(target.row as i64 + off + 1, placed.len()) else { continue };
                    placed[i] = net;
                    if let Some(j) = py_index(target.row as i64 + off, search.len()) {
                        search[j] = net;
                    }
                }
                for i in 0..rows {
                    augmented[i] = augmented[i].or(placed[i]).or(active_right[i]);
                }
            }

            // Connect tracks in the column that carry the same net.
            {
                let mut multi: Vec<NetId> = augmented.iter().flatten().copied().collect::<HashSet<_>>().into_iter().collect();
                multi.retain(|n| augmented.iter().filter(|a| **a == Some(*n)).count() > 1);
                multi.sort_by_cached_key(|n| self.net_key(*n));
                let mut candidates: Vec<NetInterval> = Vec::new();
                for net in multi {
                    let idxs: Vec<usize> = (0..rows).filter(|&i| augmented[i] == Some(net)).collect();
                    for (k, &a) in idxs.iter().enumerate() {
                        for &b in &idxs[k + 1..] {
                            candidates.push(NetInterval::new(net, a, b));
                        }
                    }
                }
                // Shortest first, each added if it doesn't run into a connection that's already there.
                candidates.sort_by_key(|i| i.end - i.beg);
                for cand in candidates {
                    if !column.iter().any(|c| cand.obstructs(c)) {
                        column.push(cand);
                    }
                }
                // Merge the intervals of each net where they touch.
                let mut column_nets: Vec<NetId> = column.iter().map(|i| i.net).collect::<HashSet<_>>().into_iter().collect();
                column_nets.sort_by_cached_key(|n| self.net_key(*n));
                for net in column_nets {
                    let mut intervals: Vec<NetInterval> = column.iter().copied().filter(|i| i.net == net).collect();
                    if intervals.len() < 2 {
                        continue;
                    }
                    column.retain(|i| i.net != net);
                    intervals.sort_by_key(|i| i.beg);
                    let mut merged: Vec<NetInterval> = vec![intervals[0]];
                    for intvl in &intervals[1..] {
                        let last = merged.last_mut().unwrap();
                        if last.intersects(intvl) {
                            *last = NetInterval::new(net, last.beg.min(intvl.beg), last.end.max(intvl.end));
                        } else {
                            merged.push(*intvl);
                        }
                    }
                    column.extend(merged);
                }
            }

            // Work out which nets carry on into the next column.
            let rightward: HashSet<NetId> = targets.iter().map(|t| t.net).collect();
            let mut flow_thru = active.clone();
            for intvl in &column {
                for trk in intvl.beg..=intvl.end {
                    if flow_thru[trk] == Some(intvl.net) {
                        // The net meets its own interval here. It may be
                        // extended from the interval below, or it may end.
                        flow_thru[trk] = None;
                    }
                }
            }
            let mut next = flow_thru.clone();
            let mut column_nets: Vec<NetId> = column.iter().map(|i| i.net).collect::<HashSet<_>>().into_iter().collect();
            column_nets.sort_by_cached_key(|n| self.net_key(*n));
            for net in column_nets {
                let mut intervals: Vec<NetInterval> = column.iter().copied().filter(|i| i.net == net).collect();
                // A net with one interval and nothing more to reach on the right ends here.
                if !rightward.contains(&net) && intervals.len() < 2 {
                    continue;
                }
                intervals.sort_by_key(|i| i.beg);
                let mut target_row: Option<usize> = targets.iter().find(|t| t.net == net).map(|t| t.row);
                let n = intervals.len();
                for (i, intvl) in intervals.iter().enumerate() {
                    if intvl.beg == 0 && intvl.end < last_track {
                        // The interval starts at the bottom, so the net carries on from where it ends.
                        next[intvl.end] = Some(net);
                        continue;
                    }
                    if intvl.end == last_track && intvl.beg > 0 {
                        // The interval ends at the top, so the net carries on from where it begins.
                        next[intvl.beg] = Some(net);
                        continue;
                    }
                    match target_row {
                        None => {
                            // Nothing to reach on the right, so this is joining up the net's intervals.
                            if i == 0 {
                                next[intvl.end] = Some(net);
                            } else if i == n - 1 {
                                next[intvl.beg] = Some(net);
                            } else {
                                match (flow_thru[intvl.beg].is_some(), flow_thru[intvl.end].is_some()) {
                                    // The net comes in at one end, so it goes out the other.
                                    (true, false) => next[intvl.end] = Some(net),
                                    (false, true) => next[intvl.beg] = Some(net),
                                    _ => return sbf(),
                                }
                            }
                        }
                        Some(row) => {
                            // Aim for the target on the right: the track in the
                            // interval nearest its row that's open or has the net.
                            let row = row.clamp(intvl.beg, intvl.end);
                            target_row = Some(row);
                            let intvl_nets = &active[intvl.beg..=intvl.end];
                            let net_row = net_search(Some(net), row - intvl.beg, intvl_nets) + row as i64;
                            let open_row = net_search(None, row - intvl.beg, intvl_nets) + row as i64;
                            let exit =
                                if (net_row - row as i64).abs() <= (open_row - row as i64).abs() { net_row } else { open_row };
                            if exit < intvl.beg as i64 || exit > intvl.end as i64 {
                                return sbf();
                            }
                            next[exit as usize] = Some(net);
                        }
                    }
                }
            }

            // Trim the ends of intervals that reach past where their nets enter or leave the column.
            for intvl in column.iter_mut() {
                let net = Some(intvl.net);
                let trks: Vec<usize> = (intvl.beg..=intvl.end).filter(|&i| active[i] == net || next[i] == net).collect();
                let (Some(&lo), Some(&hi)) = (trks.first(), trks.last()) else { return sbf() };
                intvl.beg = lo;
                intvl.end = hi;
            }

            nets_in_column.push(next);
            all_column_intvls.push(column);
        }

        // The nets on the tracks have to end up where they are on the right face.
        if nets_in_column.last().unwrap() != &sb.right_nets {
            return sbf();
        }

        // Horizontal wires along the tracks, column by column.
        let mut column_coords = vec![r.track_coord(sb.faces[LEFT])];
        column_coords.extend(sb.column_coords.iter().copied());
        column_coords.push(r.track_coord(sb.faces[RIGHT]));
        let pt = |x: i64, y: i64| Point::new(x as f64, y as f64);
        for (col_idx, nets) in nets_in_column.iter().enumerate() {
            let (beg, end) = (column_coords[col_idx], column_coords[col_idx + 1]);
            for trk in 1..last_track {
                if let Some(net) = nets[trk] {
                    let y = sb.track_coords[trk];
                    sb.segments.entry(net).or_default().push(Segment::new(pt(beg, y), pt(end, y)));
                }
            }
        }
        // Vertical wires in the columns.
        for (idx, column) in all_column_intvls.iter().enumerate() {
            let x = sb.column_coords[idx];
            for intvl in column {
                let seg = Segment::new(pt(x, sb.track_coords[intvl.beg]), pt(x, sb.track_coords[intvl.end]));
                sb.segments.entry(intvl.net).or_default().push(seg);
            }
        }
        Ok(())
    }

    /// Tidy the wires: merge, split and trim them, then remove jogs and put them on the grid.
    fn cleanup_wires(&mut self, node: NodeId, pin_pts: &HashSet<P>) {
        let parts = self.nodes[node].parts.clone();
        let mut pin_nets: HashMap<P, Net> = HashMap::new();
        for &part in &parts {
            for &pin in &self.c.parts[part].pins {
                pin_nets.insert(self.pin_pt(pin), self.c.pins[pin].net);
            }
        }
        let cl = Cleanup {
            part_bboxes: parts.iter().map(|&p| self.c.parts[p].bbox * self.c.parts[p].tx).collect(),
            pin_nets,
            pin_pts,
        };
        let key = |p: Point| p.round().key();
        let nets: Vec<NetId> = self.nodes[node].wires.keys().copied().collect();
        let net_pin_pts: HashMap<NetId, Vec<P>> =
            nets.iter().map(|&n| (n, self.get_internal_pins(node, n).iter().map(|&p| self.pin_pt(p)).collect())).collect();

        // A general cleanup of each net's segments.
        let mut wires: IndexMap<NetId, Vec<S>> = IndexMap::new();
        let mut net_bboxes: IndexMap<NetId, BBox> = IndexMap::new();
        for &net in &nets {
            let raw = &self.nodes[node].wires[&net];
            let mut bbox = BBox::empty();
            for s in raw {
                bbox.add_pt(s.p1);
                bbox.add_pt(s.p2);
            }
            net_bboxes.insert(net, bbox);
            let segs: Vec<S> = raw.iter().map(|s| S::new(key(s.p1), key(s.p2))).filter(|s| s.p1 != s.p2).collect();
            let segs = merge_segments(&segs);
            let segs = split_segments(&segs, &net_pin_pts[&net]);
            let segs: Vec<S> = break_cycles(segs).into_iter().filter(|s| s.p1 != s.p2).collect();
            wires.insert(net, trim_stubs(&segs, pin_pts));
        }

        // Remove jogs from the segments of each net until nothing more improves.
        let mut keep_cleaning = true;
        while keep_cleaning {
            keep_cleaning = false;
            for &net in &nets {
                let pins = &net_pin_pts[&net];
                let mut segs = wires[&net].clone();
                loop {
                    segs = split_segments(&segs, pins);
                    let stop = self.remove_jog(net, &mut segs, &wires, &net_bboxes, &cl);
                    segs.retain(|s| s.p1 != s.p2);
                    segs = merge_segments(&segs);
                    segs = split_segments(&segs, pins);
                    segs.retain(|s| s.p1 != s.p2);
                    segs = trim_stubs(&segs, pin_pts);
                    if stop {
                        break;
                    }
                    net_bboxes.insert(net, segs_bbox(&segs));
                    keep_cleaning = true;
                }

                // Put wires the router left off the grid back on it.
                segs = snap_to_grid(net, segs, &wires, &net_bboxes, &cl);
                segs = merge_segments(&segs);
                segs = split_segments(&segs, pins);
                segs.retain(|s| s.p1 != s.p2);
                segs = trim_stubs(&break_cycles(segs), pin_pts);
                net_bboxes.insert(net, segs_bbox(&segs));
                // Merging rejoins segments that were split where they cross a
                // pin of the net. KiCad only connects a pin to a wire that
                // ends on it, so they're split there again.
                segs = split_segments(&merge_segments(&segs), pins);
                wires.insert(net, segs);
            }
        }

        for (net, segs) in wires {
            self.nodes[node].wires.insert(net, segs.into_iter().map(S::to_segment).collect());
        }
    }

    /// Replace one staircase or tophat jog of a net with a single corner, if
    /// there's one that can be. Returns true if nothing was changed.
    fn remove_jog(
        &mut self,
        net: NetId,
        segments: &mut Vec<S>,
        wires: &IndexMap<NetId, Vec<S>>,
        net_bboxes: &IndexMap<NetId, BBox>,
        cl: &Cleanup,
    ) -> bool {
        // The segments are shuffled to vary the order jogs are found in.
        self.rng.shuffle(segments);

        // The corners: points, not on pins, where one horizontal and one vertical segment end.
        let mut at: HashMap<P, Vec<S>> = HashMap::new();
        for s in segments.iter().filter(|s| s.horz()).chain(segments.iter().filter(|s| s.vert() && !s.horz())) {
            at.entry(s.p1).or_default().push(*s);
            at.entry(s.p2).or_default().push(*s);
        }
        at.retain(|pt, segs| segs.len() == 2 && !cl.pin_pts.contains(pt) && segs[0].horz() && !segs[1].horz());

        // A segment with a corner at each end is the middle of a jog.
        for segment in segments.clone() {
            let (Some(c1), Some(c2)) = (at.get(&segment.p1), at.get(&segment.p2)) else { continue };
            let mut jog_segs: Vec<S> = c1.iter().chain(c2.iter()).copied().collect();
            jog_segs.sort();
            jog_segs.dedup();
            let mut ends: Vec<P> =
                jog_segs.iter().flat_map(|s| [s.p1, s.p2]).filter(|p| *p != segment.p1 && *p != segment.p2).collect();
            ends.sort();
            ends.dedup();
            if ends.len() != 2 {
                continue;
            }
            // Go from one end of the jog to the other with a single corner, either way round.
            let (p1, p3) = (ends[0], ends[1]);
            let mut p2s = [(p1.0, p3.1), (p3.0, p1.1)];
            self.rng.shuffle(&mut p2s);
            for p2 in p2s {
                let new_segs: Vec<S> = [(p1, p2), (p2, p3)].iter().filter(|(a, b)| a != b).map(|&(a, b)| S::new(a, b)).collect();
                if !new_segs.iter().any(|s| cl.obstructed(net, s, wires, net_bboxes, segments)) {
                    segments.retain(|s| !jog_segs.contains(s));
                    segments.extend(new_segs);
                    return false;
                }
            }
        }
        true
    }

    /// Make sure each wired pin is connected and no other net's wire runs over it.
    ///
    /// KiCad connects a pin only to a wire that ends on it, so a wire that
    /// stops short of a pin, or another net's wire across its end, leaves the
    /// drawing with connections missing even though routing seemed to succeed.
    pub fn check_pin_wires(&self, node: NodeId, nets: &[NetId]) -> Res<()> {
        let n = &self.nodes[node];
        let key = |p: Point| p.round().key();
        let empty = Vec::new();
        for &net in nets {
            let segs = n.wires.get(&net).unwrap_or(&empty);
            let ends: HashSet<P> = segs.iter().flat_map(|s| [key(s.p1), key(s.p2)]).collect();
            let pins = self.get_internal_pins(node, net);
            for &pin in &pins {
                let pt = self.pin_pt(pin);
                let name = || format!("{}.{}", self.c.pin_part(pin).ref_(), self.c.pins[pin].num);
                if !ends.contains(&pt) {
                    return fail(format!("No wire of {} ends on pin {}", self.c.nets[net].name, name()));
                }
                for (other, osegs) in &n.wires {
                    if *other != net && osegs.iter().any(|s| S::new(key(s.p1), key(s.p2)).contains(pt)) {
                        return fail(format!(
                            "{} wire runs over pin {} of {}",
                            self.c.nets[*other].name,
                            name(),
                            self.c.nets[net].name
                        ));
                    }
                }
            }
            if !self.wires_join(segs, &pins) {
                return fail(format!("Wires of {} are in separate pieces", self.c.nets[net].name));
            }
        }
        Ok(())
    }

    /// True if wire segments join all the pins they're for.
    ///
    /// Segments are joined where they share an end or the end of one lands on
    /// another, which is how KiCad connects them. Net labels for the same net
    /// join each other, so pieces with a label on them count as joined.
    fn wires_join(&self, segments: &[Segment], pins: &[PinId]) -> bool {
        let segs: Vec<S> = segments.iter().map(|s| S::new(s.p1.round().key(), s.p2.round().key())).collect();
        let labels = segs.len(); // The last one stands for the labels.
        let mut parent: Vec<usize> = (0..=labels).collect();
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        // (The ends of a segment are in order, but a segment needn't be horizontal or vertical.)
        let on = |pt: P, s: &S| {
            s.p1.0.min(s.p2.0) <= pt.0 && pt.0 <= s.p1.0.max(s.p2.0) && s.p1.1.min(s.p2.1) <= pt.1 && pt.1 <= s.p1.1.max(s.p2.1)
        };
        for i in 0..segs.len() {
            for j in 0..i {
                if on(segs[i].p1, &segs[j]) || on(segs[i].p2, &segs[j]) || on(segs[j].p1, &segs[i]) || on(segs[j].p2, &segs[i]) {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
            }
        }
        let mut pieces: Vec<usize> = Vec::new();
        for &pin in pins {
            let pt = self.pin_pt(pin);
            let mut touching: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].p1 == pt || segs[i].p2 == pt).collect();
            if self.c.pin_part(pin).is_net_terminal {
                touching.push(labels);
            }
            if touching.is_empty() {
                return false;
            }
            for &i in &touching[1..] {
                let (a, b) = (find(&mut parent, i), find(&mut parent, touching[0]));
                parent[a] = b;
            }
            pieces.push(touching[0]);
        }
        let roots: HashSet<usize> = pieces.into_iter().map(|p| find(&mut parent, p)).collect();
        roots.len() <= 1
    }

    /// Add junctions where a wire of a net ends on, or crosses, another of the same net.
    fn add_junctions(&mut self, node: NodeId) {
        let n = &mut self.nodes[node];
        for (net, segs) in &n.wires {
            let horz: Vec<&Segment> = segs.iter().filter(|s| s.p1.y == s.p2.y).collect();
            let vert: Vec<&Segment> = segs.iter().filter(|s| s.p1.x == s.p2.x).collect();
            let mut junctions = Vec::new();
            for h in &horz {
                let y = h.p1.y;
                for v in &vert {
                    let x = v.p1.x;
                    if (h.p1.x < x && x < h.p2.x && v.p1.y <= y && y <= v.p2.y)
                        || (v.p1.y < y && y < v.p2.y && h.p1.x <= x && x <= h.p2.x)
                    {
                        junctions.push(Point::new(x, y));
                    }
                }
            }
            n.junctions.entry(*net).or_default().extend(junctions);
        }
    }

    /// Route a child node, placing it again with more room if that fails.
    ///
    /// A child on a sheet of its own appears in its parent as a sheet symbol
    /// whose size doesn't depend on how its insides are placed, so it can be
    /// placed and routed again by itself. That beats starting the whole
    /// design over, where every sheet has to route on the same try.
    fn route_child(&mut self, child: NodeId) -> Res<()> {
        let saved = self.opts.expansion_factor;
        let mut expansion_factor = saved;
        let mut result = Ok(());
        for attempt in 0..SHEET_ROUTE_TRIES {
            let outcome = if attempt > 0 {
                let _note = progress::prefixed(&format!("try {}/{},", attempt + 1, SHEET_ROUTE_TRIES));
                self.opts.expansion_factor = expansion_factor;
                self.place(child).and_then(|_| self.route_node(child, true))
            } else {
                self.route_node(child, true)
            };
            match outcome {
                Ok(()) => {
                    result = Ok(());
                    break;
                }
                // (A placement can fail too: see MAX_SHEET_SIZE.)
                Err(e) if e.is_layout_failure() => {
                    if self.nodes[child].flattened || attempt == SHEET_ROUTE_TRIES - 1 {
                        result = Err(e);
                        break;
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
            expansion_factor *= SHEET_EXPANSION;
        }
        if result.is_ok() {
            // Lay the sheet out some more times, with the room it took to
            // route it, and keep the best.
            self.opts.expansion_factor = expansion_factor;
            self.best_layout(child, self.opts.sheet_tries);
        }
        self.opts.expansion_factor = saved;
        result
    }

    /// Route the wires between part pins in a node and its children.
    pub fn route(&mut self, node: NodeId) -> Res<()> {
        self.route_node(node, true)
    }

    /// Route a node. With `route_sheets` off, children on sheets of their own are left as they are.
    pub fn route_node(&mut self, node: NodeId, route_sheets: bool) -> Res<()> {
        // A child that holds the same circuit as one already routed is drawn the same way.
        let mut routed: Vec<NodeId> = Vec::new();
        for child in self.children(node) {
            if self.nodes[child].flattened {
                self.route_child(child)?;
            } else if route_sheets {
                if !(self.opts.reuse_layouts && self.reuse_layout(child, &routed)) {
                    self.route_child(child)?;
                }
                routed.push(child);
            }
        }

        let rng_state = self.rng.clone();
        self.route_parts(node)?;

        // Use the routing to improve the placement: move parts to straighten
        // wires that step sideways, and route again.
        if self.opts.nudge && !self.nodes[node].wires.is_empty() {
            self.straighten(node, &rng_state);
        }
        Ok(())
    }

    /// Route the wires between the parts of a node, leaving its children as they are.
    ///
    /// Any wiring the node already has is replaced.
    pub fn route_parts(&mut self, node: NodeId) -> Res<()> {
        self.nodes[node].wires = IndexMap::new();
        self.nodes[node].junctions = IndexMap::new();
        if self.nodes[node].parts.is_empty() {
            return Ok(());
        }
        let mut internal_nets = self.get_internal_nets(node);
        if internal_nets.is_empty() {
            return Ok(());
        }
        let name = if self.nodes[node].name.is_empty() { "top sheet".to_string() } else { self.nodes[node].name.clone() };
        progress::report(&format!("routing {}", name));

        // Extend the pins' wires to the edges of their parts' boxes.
        let pin_pts = self.add_routing_points(node, &internal_nets);

        // The routing area: the parts, with a channel around them wide enough for every net.
        let channel = (internal_nets.len() + 1) as f64 * GRID as f64;
        let routing_bbox = self.internal_bbox(node).resize(Point::new(channel, channel)).round();

        let mut r = self.create_routing_tracks(node, routing_bbox);
        self.create_terminals(node, &internal_nets, &mut r)?;

        // Global routing, then a terminal for each net on each face it crosses.
        let routes = self.global_router(node, &mut internal_nets, &mut r)?;
        for (net, route) in &routes {
            for wire in route {
                cvt_faces_to_terminals(&mut r, *net, wire)?;
            }
        }

        // Detailed routing in each switchbox: left to right, or failing that, top to bottom.
        let mut switchboxes = self.create_switchboxes(&mut r)?;
        for sb in switchboxes.iter_mut() {
            if self.route_switchbox(sb, &r).is_err() {
                sb.segments.clear();
                sb.flip_xy();
                self.route_switchbox(sb, &r)?;
                sb.flip_xy();
            }
            for (net, segs) in &sb.segments {
                self.nodes[node].wires.entry(*net).or_default().extend(segs.iter().copied());
            }
        }

        self.cleanup_wires(node, &pin_pts);
        self.add_junctions(node);
        self.check_pin_wires(node, &internal_nets)
    }
}

/// Find the shortest path from a face to one of the stop faces (Dijkstra).
///
/// The path may touch a face on a part only if it holds a pin of the net or
/// is a stop face. Returns the faces from start to stop, or None if there's
/// no way through.
fn rt_srch(r: &mut Router, net_pin_faces: &HashSet<usize>, start: usize, stop: &HashSet<usize>) -> Option<Vec<usize>> {
    if stop.is_empty() || stop.contains(&start) {
        return Some(Vec::new());
    }
    #[derive(PartialEq)]
    struct Dist(f64);
    impl Eq for Dist {}
    impl PartialOrd for Dist {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Dist {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            self.0.total_cmp(&o.0)
        }
    }

    let mut prev: HashMap<usize, usize> = HashMap::new();
    let mut visited: HashSet<usize> = HashSet::from([start]);
    // Candidates are ordered by distance, then by when the face they were
    // reached from was visited, then by that face's order of adjacent faces.
    let mut candidates: BinaryHeap<Reverse<(Dist, usize, usize, usize, usize)>> = BinaryHeap::new();
    let (mut face, mut face_dist, mut visit_cnt) = (start, 0.0, 0usize);
    loop {
        for (rank, &(adj, dist)) in r.faces[face].adjacent.iter().enumerate() {
            if visited.contains(&adj) {
                continue;
            }
            // Faces with no room left are skipped.
            if !stop.contains(&adj) && !net_pin_faces.contains(&adj) && r.faces[adj].capacity <= 0 {
                continue;
            }
            candidates.push(Reverse((Dist(face_dist + dist), visit_cnt, rank, adj, face)));
        }
        visit_cnt += 1;

        let prev_face;
        loop {
            let Reverse((d, _, _, f, p)) = candidates.pop()?;
            if !visited.contains(&f) {
                (face, face_dist, prev_face) = (f, d.0, p);
                break;
            }
        }
        prev.insert(face, prev_face);
        visited.insert(face);

        if stop.contains(&face) {
            let mut path = vec![face];
            while *path.last().unwrap() != start {
                path.push(prev[path.last().unwrap()]);
            }
            // The faces on the path each have room for one less wire.
            for &f in &path[..path.len() - 1] {
                if r.faces[f].capacity > 0 {
                    r.faces[f].capacity -= 1;
                }
            }
            path.reverse();
            return Some(path);
        }
    }
}

/// Give a net a terminal on each face of one of its global wires.
fn cvt_faces_to_terminals(r: &mut Router, net: NetId, wire: &[usize]) -> Res<()> {
    if wire.is_empty() {
        return Ok(());
    }
    // (face, index of its terminal) once a face has been converted.
    let mut terms: Vec<Option<usize>> = vec![None; wire.len()];

    // Faces on parts already have the terminals of the part's pins.
    for (i, &f) in wire.iter().enumerate() {
        if !r.faces[f].part.is_empty() {
            match r.faces[f].terminals.iter().position(|t| t.net == Some(net)) {
                Some(t) => terms[i] = Some(t),
                None => return fail("Global routing failure"),
            }
        }
    }

    for i in 0..wire.len() - 1 {
        let Some(t) = terms[i] else { return fail("Global routing failure") };
        if terms[i + 1].is_some() {
            continue;
        }
        let (from, next) = (wire[i], wire[i + 1]);
        let coord = r.faces[from].terminals[t].coord;
        let n = r.faces[next].terminals.len();
        let search: Vec<usize> = if r.faces[next].track.horz != r.faces[from].track.horz {
            // The faces meet at a corner. A terminal near the corner needs the least wire.
            if r.faces[next].beg == r.faces[from].track.idx {
                (0..n).collect()
            } else if r.faces[next].end == r.faces[from].track.idx {
                (0..n).rev().collect()
            } else {
                return fail("Global routing failure");
            }
        } else {
            // The faces are on opposite sides. Look for a terminal straight
            // across, then one step either side, and so on.
            let mut lower: Vec<usize> = (0..n).filter(|&k| r.faces[next].terminals[k].coord <= coord).collect();
            lower.sort_by_key(|&k| Reverse(r.faces[next].terminals[k].coord));
            let mut upper: Vec<usize> = (0..n).filter(|&k| r.faces[next].terminals[k].coord > coord).collect();
            upper.sort_by_key(|&k| r.faces[next].terminals[k].coord);
            let mut both = Vec::with_capacity(n);
            for k in 0..lower.len().max(upper.len()) {
                both.extend(lower.get(k));
                both.extend(upper.get(k));
            }
            both
        };
        // A terminal already on the net, else one that's free.
        let found = search
            .iter()
            .copied()
            .find(|&k| r.faces[next].terminals[k].net == Some(net))
            .or_else(|| search.iter().copied().find(|&k| r.faces[next].terminals[k].net.is_none()));
        match found {
            Some(k) => {
                r.faces[next].terminals[k].net = Some(net);
                terms[i + 1] = Some(k);
            }
            None => return fail("Global routing failure"),
        }
    }
    Ok(())
}

/// Move the wires of a net that run off the grid onto it.
///
/// The router follows the edges of parts, which needn't be on the grid. Each
/// off-grid X (or Y) is moved to the nearest grid line on either side that
/// leaves the moved segments unobstructed. A coordinate shared with a pin
/// isn't moved, since the wire would come off the pin.
fn snap_to_grid(
    net: NetId,
    mut segments: Vec<S>,
    wires: &IndexMap<NetId, Vec<S>>,
    net_bboxes: &IndexMap<NetId, BBox>,
    cl: &Cleanup,
) -> Vec<S> {
    for axis in 0..2 {
        let get = |p: P| if axis == 0 { p.0 } else { p.1 };
        let pts: HashSet<P> = segments.iter().flat_map(|s| [s.p1, s.p2]).collect();
        let pinned: HashSet<i64> = pts.iter().filter(|p| cl.pin_pts.contains(p)).map(|&p| get(p)).collect();
        let mut coords: Vec<i64> =
            pts.iter().map(|&p| get(p)).filter(|v| v.rem_euclid(GRID) != 0 && !pinned.contains(v)).collect();
        coords.sort();
        coords.dedup();
        for v in coords {
            let lo = v.div_euclid(GRID) * GRID;
            let mut choices = [lo, lo + GRID];
            choices.sort_by_key(|c| ((c - v).abs(), *c));
            for c in choices {
                let mv = |p: P| -> P {
                    if get(p) != v {
                        p
                    } else if axis == 0 {
                        (c, p.1)
                    } else {
                        (p.0, c)
                    }
                };
                let moved: Vec<(P, P)> = segments.iter().map(|s| (mv(s.p1), mv(s.p2))).collect();
                let changed: Vec<S> = moved
                    .iter()
                    .zip(&segments)
                    .filter(|(m, s)| **m != (s.p1, s.p2) && m.0 != m.1)
                    .map(|(m, _)| S::new(m.0, m.1))
                    .collect();
                if !changed.iter().any(|m| cl.obstructed(net, m, wires, net_bboxes, &segments)) {
                    segments = moved.into_iter().filter(|m| m.0 != m.1).map(|m| S::new(m.0, m.1)).collect();
                    break;
                }
            }
        }
    }
    segments
}
