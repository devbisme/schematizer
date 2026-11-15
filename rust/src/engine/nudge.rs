//! Use the routed wires to improve the placement: nudge parts to straighten staircases.
//!
//! A wire that steps sideways between two pins (a staircase) means the pins
//! are almost, but not quite, in line. Wire cleanup can't remove it, because
//! any path between the two pins needs the step. Moving one of the parts by
//! the size of the step can: the pins line up and the wire runs straight.
//!
//! After a sheet is routed, `straighten()` looks for such staircases, moves
//! the lighter part of each, and routes the sheet again. The new routing is
//! kept only if it has fewer bends and crossings; otherwise the parts go back
//! where they were.

use std::collections::{HashMap, HashSet};

use crate::geometry::{BBox, Point, Segment, Tx};
use crate::ir::{NetId, NodeId, PartId, PinId};
use crate::quality::count_crossings;

use super::rng::Rng;
use super::{progress, Design};

/// The longest step (mils) a part is moved to remove. 3 grid steps, the same
/// near-miss distance placement uses when it lines pins up.
pub const MAX_NUDGE: f64 = 150.0;
/// How many times a sheet is nudged and routed again while that keeps helping.
pub const MAX_ROUNDS: usize = 3;
/// The most times a sheet is routed again in all, counting the tries that are undone.
pub const MAX_TRIES: usize = 8;
/// Parts with more wired pins than this aren't moved: straightening one wire
/// of a part like that bends its others.
pub const MAX_WIRED_PINS: usize = 4;

type P = (i64, i64);

/// A staircase: the net, its near end, the step's start and end, and its far end.
type Stair = (NetId, P, P, P, P);

fn ends(segments: &[Segment]) -> HashMap<P, Vec<usize>> {
    let mut map: HashMap<P, Vec<usize>> = HashMap::new();
    for (i, s) in segments.iter().enumerate() {
        map.entry(s.p1.key()).or_default().push(i);
        map.entry(s.p2.key()).or_default().push(i);
    }
    map
}

fn horizontal(s: &Segment) -> bool {
    s.p1.y == s.p2.y
}

/// True if exactly two segments meet at a point, at a right angle.
fn is_corner(segs: &[usize], segments: &[Segment]) -> bool {
    segs.len() == 2 && horizontal(&segments[segs[0]]) != horizontal(&segments[segs[1]])
}

impl Design {
    /// Where the pins of a node's parts are.
    fn pin_points(&self, node: NodeId) -> HashMap<P, PinId> {
        let mut pts = HashMap::new();
        for &part in &self.nodes[node].parts {
            let tx = self.c.parts[part].tx;
            for &pin in &self.c.parts[part].pins {
                pts.insert((self.c.pins[pin].pt * tx).round().key(), pin);
            }
        }
        pts
    }

    /// (bends plus crossings, wire length) of a node's wiring, for comparing two routings of it.
    ///
    /// A wire crossing another counts the same as a bend: either makes a wire harder to follow.
    pub fn wiring_cost(&self, node: NodeId) -> (usize, f64) {
        let pin_pts = self.pin_points(node);
        let (mut bends, mut length) = (0, 0.0);
        let mut all = Vec::new();
        for segments in self.nodes[node].wires.values() {
            for (pt, segs) in ends(segments) {
                if is_corner(&segs, segments) && !pin_pts.contains_key(&pt) {
                    bends += 1;
                }
            }
            for s in segments {
                length += (s.p2.x - s.p1.x).abs() + (s.p2.y - s.p1.y).abs();
                all.push(((s.p1.x, s.p1.y), (s.p2.x, s.p2.y)));
            }
        }
        (bends + count_crossings(&all, &HashSet::new()), length)
    }

    /// Find the wires that step sideways on their way between two points.
    ///
    /// For each, the wire runs from the near end to the step's start, steps
    /// sideways to the step's end, and runs on to the far end in the
    /// direction it came.
    fn find_staircases(&self, node: NodeId) -> Vec<Stair> {
        let pin_pts = self.pin_points(node);
        let mut stairs = Vec::new();
        for (&net, segments) in &self.nodes[node].wires {
            let ends = ends(segments);
            let corner = |pt: P| is_corner(&ends[&pt], segments) && !pin_pts.contains_key(&pt);
            for (i, mid) in segments.iter().enumerate() {
                let (m1, m2) = (mid.p1.key(), mid.p2.key());
                if !(corner(m1) && corner(m2)) {
                    continue;
                }
                let step = (mid.p2.x - mid.p1.x).abs() + (mid.p2.y - mid.p1.y).abs();
                if step > MAX_NUDGE {
                    continue;
                }
                let other = |pt: P| -> P {
                    let s = &segments[*ends[&pt].iter().find(|&&j| j != i).unwrap()];
                    if s.p1.key() == pt {
                        s.p2.key()
                    } else {
                        s.p1.key()
                    }
                };
                let (pa, pb) = (other(m1), other(m2));
                // The outer segments of a staircase leave the step in opposite
                // directions. (If they leave the same way it's a U, which
                // moving a part sideways doesn't fix.)
                let da = (pa.0 - m1.0) + (pa.1 - m1.1);
                let db = (pb.0 - m2.0) + (pb.1 - m2.1);
                if da * db < 0 {
                    stairs.push((net, pa, m1, m2, pb));
                }
            }
        }
        stairs
    }

    /// Choose a part to move for each staircase. Returns the moves and each part's number of wired pins.
    fn plan_moves(&self, node: NodeId, stairs: &[Stair]) -> (Vec<(PartId, P)>, HashMap<PartId, usize>) {
        let pin_pts = self.pin_points(node);
        let wire_ends: HashSet<P> =
            self.nodes[node].wires.values().flatten().flat_map(|s| [s.p1.key(), s.p2.key()]).collect();
        let mut wired: HashMap<PartId, usize> = HashMap::new();
        for (pt, &pin) in &pin_pts {
            if wire_ends.contains(pt) {
                *wired.entry(self.c.pins[pin].part).or_insert(0) += 1;
            }
        }

        let mut wanted: Vec<(PartId, Vec<P>)> = Vec::new();
        for &(net, pa, ma, mb, pb) in stairs {
            // Moving the part at one end by the step, or the part at the other
            // end by the step reversed, puts both ends in line.
            let mut best: Option<((usize, usize, String), PartId, P)> = None;
            for (pt, mv) in [(pa, (mb.0 - ma.0, mb.1 - ma.1)), (pb, (ma.0 - mb.0, ma.1 - mb.1))] {
                let Some(&pin) = pin_pts.get(&pt) else { continue };
                if self.c.pins[pin].net != Some(net) {
                    continue;
                }
                let part = self.c.pins[pin].part;
                // Move the part that has the least else to disturb.
                let key = (wired.get(&part).copied().unwrap_or(0), self.c.parts[part].pins.len(), self.c.parts[part].ref_());
                if best.as_ref().is_none_or(|b| key < b.0) {
                    best = Some((key, part, mv));
                }
            }
            if let Some((_, part, mv)) = best {
                match wanted.iter_mut().find(|(p, _)| *p == part) {
                    Some((_, moves)) => {
                        if !moves.contains(&mv) {
                            moves.push(mv);
                        }
                    }
                    None => wanted.push((part, vec![mv])),
                }
            }
        }
        // A part asked to go two ways stays put.
        let moves = wanted.into_iter().filter(|(_, m)| m.len() == 1).map(|(p, m)| (p, m[0])).collect();
        (moves, wired)
    }

    fn nudge_bbox(&self, part: PartId, tx: Tx) -> BBox {
        self.c.parts[part].lbl_bbox * tx
    }

    /// Move the parts that have room to move. Returns true if any moved.
    fn apply_moves(&mut self, node: NodeId, moves: &[(PartId, P)]) -> bool {
        let parts = self.nodes[node].parts.clone();
        // A flattened node is drawn on its parent's sheet in the space it was
        // given at placement, so its parts must stay inside that.
        let limit = self.nodes[node].flattened.then(|| {
            let mut b = BBox::empty();
            for &p in &parts {
                b.add(self.nudge_bbox(p, self.c.parts[p].tx));
            }
            b
        });
        let mut sorted = moves.to_vec();
        sorted.sort_by_cached_key(|(p, _)| self.c.parts[*p].ref_());
        let mut moved = false;
        for (part, mv) in sorted {
            let tx = self.c.parts[part].tx.move_by(Point::new(mv.0 as f64, mv.1 as f64));
            let bbox = self.nudge_bbox(part, tx);
            if let Some(l) = &limit {
                if bbox.min.x < l.min.x || bbox.min.y < l.min.y || bbox.max.x > l.max.x || bbox.max.y > l.max.y {
                    continue;
                }
            }
            let hits_part = parts.iter().any(|&p| p != part && bbox.intersects(&self.nudge_bbox(p, self.c.parts[p].tx)));
            let hits_child =
                self.nodes[node].children.values().any(|&ch| bbox.intersects(&(self.nodes[ch].bbox * self.nodes[ch].tx)));
            if hits_part || hits_child {
                continue;
            }
            self.c.parts[part].tx = tx;
            moved = true;
        }
        moved
    }

    /// Move parts and route again. Keep the result if it has fewer bends and crossings, else undo it.
    fn try_moves(&mut self, node: NodeId, moves: &[(PartId, P)], rng_state: &Rng) -> bool {
        let saved_tx: Vec<(PartId, Tx)> = self.nodes[node].parts.iter().map(|&p| (p, self.c.parts[p].tx)).collect();
        let saved_wires = self.nodes[node].wires.clone();
        let saved_junctions = self.nodes[node].junctions.clone();
        let before = self.wiring_cost(node);

        if !self.apply_moves(node, moves) {
            return false;
        }
        // The router breaks ties randomly. Starting it where the first routing
        // started keeps the wires away from the moved parts as they were, so
        // the comparison is about the moves and not the luck of the draw.
        self.rng = rng_state.clone();
        let routed = {
            let _note = progress::prefixed("straightening wires,");
            self.route_parts(node).is_ok()
        };
        if routed {
            let after = self.wiring_cost(node);
            if after.0 < before.0 || (after.0 == before.0 && after.1 < before.1) {
                return true;
            }
        }
        for (part, tx) in saved_tx {
            self.c.parts[part].tx = tx;
        }
        self.nodes[node].wires = saved_wires;
        self.nodes[node].junctions = saved_junctions;
        false
    }

    /// Nudge parts of a routed node to straighten its staircase wires, and route it again.
    ///
    /// `rng_state` is the random generator's state when the node's routing
    /// began. Returns the number of times the node was nudged and its new
    /// routing kept.
    pub fn straighten(&mut self, node: NodeId, rng_state: &Rng) -> usize {
        let mut kept = 0;
        let mut tries = 0;
        let mut rejected: HashSet<(PartId, P)> = HashSet::new();

        for _ in 0..MAX_ROUNDS {
            let stairs = self.find_staircases(node);
            if stairs.is_empty() {
                break;
            }
            let (moves, wired) = self.plan_moves(node, &stairs);
            let wired_of = |p: PartId| wired.get(&p).copied().unwrap_or(0);
            let moves: Vec<(PartId, P)> =
                moves.into_iter().filter(|m| !rejected.contains(m) && wired_of(m.0) <= MAX_WIRED_PINS).collect();
            if moves.is_empty() || tries >= MAX_TRIES {
                break;
            }
            tries += 1;
            if self.try_moves(node, &moves, rng_state) {
                kept += 1;
                continue;
            }
            if moves.len() == 1 {
                rejected.insert(moves[0]);
                break;
            }
            // One bad move spoils the lot, so try them one at a time, starting
            // with the parts that have the least else to disturb.
            let mut singly = moves.clone();
            singly.sort_by_cached_key(|m| (wired_of(m.0), self.c.parts[m.0].ref_()));
            let mut improved = false;
            for m in singly {
                if tries >= MAX_TRIES {
                    break;
                }
                tries += 1;
                if self.try_moves(node, &[m], rng_state) {
                    kept += 1;
                    improved = true;
                } else {
                    rejected.insert(m);
                }
            }
            if !improved {
                break;
            }
        }
        kept
    }
}
