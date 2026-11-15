//! Autoplacer for arranging symbols in a schematic.
//!
//! The parts of each child node are placed first, then the blocks for the
//! children are placed along with the parts of this node.
//!
//! The parts of a node are separated into groups: groups of parts that are
//! connected to each other by nets, and one group of parts that aren't wired
//! to anything (floating parts).
//!
//! Connected parts are either put where a person would put them (see
//! `preplace`), or placed with force-directed placement: each net pulls its
//! parts together and any overlap of parts pushes them apart. The pull
//! dominates at first, then gives way to the push. After that the parts are
//! put on the routing grid.
//!
//! The blocks of connected parts, floating parts and child nodes are then
//! arranged with force-directed placement too.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::geometry::{pyround, BBox, Point, Tx};
use crate::ir::{NetId, NodeId, PartId, Pin, PinId};

use super::constants::{BLK_EXT_PAD, BLK_INT_PAD, GRID};
use super::conventions::{best_orientations, pin_exit_vec};
use super::rng::Rng;
use super::{progress, Design, Error, Res};

/// The largest a placed sheet can be (mils). The largest paper, A0, is 47 inches wide.
pub const MAX_SHEET_SIZE: f64 = 1_000_000.0;

/// Key of the anchor and pull pins of parts that aren't wired to anything.
pub const SIMILARITY: usize = usize::MAX;

/// Groups with more connected parts than this are put in rows when
/// pre-placement is off, since force-directed placement is too slow for them.
const ROW_PLACE_THRESHOLD: usize = 20;

/// Pins offset by no more than this from sharing a row or column are near
/// misses that `align_parts()` tries to straighten.
pub const NEAR_MISS_DIST: f64 = 150.0;

// Sizes for estimating where a stubbed pin's net label is drawn, for scoring
// orientations by how badly labels pile up.
const LABEL_REACH: f64 = 250.0;
const LABEL_HALF_H: f64 = 30.0;
const LABEL_BODY_MARGIN: f64 = 40.0;
const LABEL_TEXT_BAND_HALF_W: f64 = 300.0;
const LABEL_TEXT_BAND_HALF_H: f64 = 70.0;
const LABEL_OVERLAP_WEIGHT: f64 = 75.0;

type PinMap = IndexMap<usize, Vec<PinId>>;

/// Placement scratch state of a part.
#[derive(Clone, Debug, Default)]
pub struct PartPlace {
    /// Satellite parts fixed beside this one, which turn with it.
    pub riders: Vec<PartId>,
    /// A rider's transformation relative to the part it rides on.
    pub rel_tx: Tx,
    /// A host's placement box before its satellites were added to it.
    pub preplace_bbox: Option<BBox>,
    /// Pins of this part on each net, which other parts pull on.
    pub anchor_pins: PinMap,
    /// Pins of other parts on each net, which pull on this part.
    pub pull_pins: PinMap,
    /// Centroid of the part's anchor pins on each net.
    pub pin_ctrs: IndexMap<NetId, Point>,
    saved: Option<(PinMap, PinMap)>,
}

/// Something that force-directed placement moves: a part or a block of parts.
#[derive(Clone, Copy, Debug)]
struct Body {
    tx: Tx,
    /// Placement box in the body's own coordinates.
    bbox: BBox,
}

/// A point forces act on. It moves with a body, or it's fixed in place.
#[derive(Clone, Copy, Debug)]
struct FPin {
    body: Option<usize>,
    pt: Point,
}

/// The pins of one net that pull on a body.
struct NetGroup {
    anchors: Vec<usize>,
    pulls: Vec<usize>,
    mult: f64,
}

enum Attraction {
    /// For each body, its nets.
    Nets(Vec<Vec<NetGroup>>),
    /// Each body's anchor pin, the pins that pull on every body, and how
    /// strongly each pair of bodies attract.
    Similarity { anchors: Vec<usize>, pulls: Vec<usize>, sim: Vec<Vec<f64>> },
}

/// A force-directed placement problem. The anchored bodies come first.
struct Forces {
    bodies: Vec<Body>,
    n_anchored: usize,
    pins: Vec<FPin>,
    attraction: Attraction,
    pin_normalize: bool,
    use_push_pull: bool,
}

impl Forces {
    fn pin_pt(&self, pin: usize) -> Point {
        let p = &self.pins[pin];
        match p.body {
            Some(b) => p.pt * self.bodies[b].tx,
            None => p.pt,
        }
    }

    /// Attractive force on a body from the bodies connected to it.
    fn attractive(&self, body: usize, pts: &[Point]) -> Point {
        match &self.attraction {
            Attraction::Nets(groups) => {
                let mut total = Point::new(0.0, 0.0);
                for g in &groups[body] {
                    let (mut fx, mut fy) = (0.0, 0.0);
                    for &a in &g.anchors {
                        let ap = pts[a];
                        for &p in &g.pulls {
                            fx += pts[p].x - ap.x;
                            fy += pts[p].y - ap.y;
                        }
                    }
                    let mut force = Point::new(fx, fy);
                    if self.pin_normalize {
                        // Normalize the net's force across its anchor and pull pins.
                        let n = (g.anchors.len() * g.pulls.len()).max(1);
                        force = force / n as f64;
                    }
                    total = total + force * g.mult;
                }
                total
            }
            Attraction::Similarity { anchors, pulls, sim } => {
                let ap = pts[anchors[body]];
                let (mut fx, mut fy) = (0.0, 0.0);
                for &p in pulls {
                    let other = self.pins[p].body.unwrap();
                    let s = sim[body][other];
                    fx += s * (pts[p].x - ap.x);
                    fy += s * (pts[p].y - ap.y);
                }
                Point::new(fx, fy)
            }
        }
    }

    /// Repulsive force on a body from the bodies overlapping it.
    fn repulsive(&self, body: usize, boxes: &[BBox], rng: &mut Rng) -> Point {
        let b = boxes[body];
        let mut total = Point::new(0.0, 0.0);
        for (other, o) in boxes.iter().enumerate() {
            if other == body || !b.intersects(o) {
                continue;
            }
            // The moves that separate the boxes to the left, right, top and
            // bottom, with a small random offset to break the symmetry of
            // bodies exactly on top of each other. The smallest one is used.
            let (rnd_x, rnd_y) = (rng.random() - 0.5, rng.random() - 0.5);
            let moves = [
                (o.min.x - b.max.x - rnd_x, 0.0),
                (o.max.x - b.min.x - rnd_x, 0.0),
                (0.0, o.max.y - b.min.y - rnd_y),
                (0.0, o.min.y - b.max.y - rnd_y),
            ];
            let mut best = moves[0];
            for m in &moves[1..] {
                if m.0.abs() + m.1.abs() < best.0.abs() + best.1.abs() {
                    best = *m;
                }
            }
            total = total + Point::new(best.0, best.1);
        }
        total
    }

    fn snapshot(&self) -> (Vec<Point>, Vec<BBox>) {
        let pts = (0..self.pins.len()).map(|i| self.pin_pt(i)).collect();
        let boxes = self.bodies.iter().map(|b| b.bbox * b.tx).collect();
        (pts, boxes)
    }

    fn total_force(&self, body: usize, scale: f64, alpha: f64, pts: &[Point], boxes: &[BBox], rng: &mut Rng) -> Point {
        self.attractive(body, pts) * (scale * (1.0 - alpha)) + self.repulsive(body, boxes, rng) * alpha
    }

    fn enclosing_bbox(&self) -> BBox {
        let mut bbox = BBox::empty();
        for b in &self.bodies {
            bbox.add(b.bbox * b.tx);
        }
        bbox
    }

    /// Put the bodies at random places within an area sized to hold them.
    fn random_placement(&mut self, rng: &mut Rng) {
        let area: f64 = self.bodies.iter().map(|b| b.bbox.area()).sum();
        let side = 3.0 * area.sqrt(); // The multiplier is ad hoc.
        for b in self.bodies.iter_mut() {
            let pt = Point::new(rng.random() * side, rng.random() * side);
            b.tx = b.tx.move_by(pt);
        }
    }

    /// Put the centers of all the bodies on one point.
    fn central_placement(&mut self) {
        if self.bodies.len() <= 1 {
            return;
        }
        let ctr = self.enclosing_bbox().ctr();
        for b in self.bodies.iter_mut() {
            let mv = ctr - b.bbox.ctr() * b.tx;
            b.tx = b.tx * Tx::translate(mv.x, mv.y);
        }
    }

    /// The scale that makes the attractive forces about as large as the repulsive ones.
    fn scale_forces(&mut self, rng: &mut Rng) -> f64 {
        let original: Vec<Tx> = self.bodies.iter().map(|b| b.tx).collect();
        let n = self.bodies.len();

        // Attractive forces are largest when the bodies are scattered.
        self.random_placement(rng);
        let (pts, boxes) = self.snapshot();
        let attractive: f64 = (0..n).map(|i| self.total_force(i, 1.0, 0.0, &pts, &boxes, rng).magnitude()).sum();

        // Repulsive forces are largest when the bodies are piled up.
        self.central_placement();
        let (pts, boxes) = self.snapshot();
        let repulsive: f64 = (0..n).map(|i| self.total_force(i, 1.0, 1.0, &pts, &boxes, rng).magnitude()).sum();

        for (b, tx) in self.bodies.iter_mut().zip(original) {
            b.tx = tx;
        }
        if attractive == 0.0 {
            1.0
        } else {
            repulsive / attractive
        }
    }

    /// Move the mobile bodies under the pull of their nets and the push of overlapping bodies.
    fn push_and_pull(&mut self, rng: &mut Rng) {
        let n = self.bodies.len();
        let mobile: Vec<usize> = (self.n_anchored..n).collect();
        if !self.use_push_pull || mobile.is_empty() {
            return;
        }

        // With no anchored bodies, the average force is removed so the whole
        // lot doesn't drift off in one direction.
        let rmv_drift = self.n_anchored == 0;
        let scale = self.scale_forces(rng);

        // The schedule takes alpha from 0 (all attractive) to 1 (all
        // repulsive): (speed, alpha, stability coefficient).
        let schedule = [(0.50, 0.0, 0.1), (0.25, 0.0, 0.01), (0.25, 0.4, 0.1), (0.25, 0.8, 0.1), (0.25, 1.0, 0.01)];
        for (speed, alpha, stability_coef) in schedule {
            let mut speed = speed;
            // Below this the bodies are taken to have settled. Negative means not set yet.
            let mut stable_threshold = -1.0;
            let mut initial_sum = 0.0;
            for _ in 0..1000 {
                let (pts, boxes) = self.snapshot();
                let mut forces: Vec<Point> = Vec::with_capacity(mobile.len());
                let mut sum_of_forces = 0.0;
                for &i in &mobile {
                    let force = self.total_force(i, scale, alpha, &pts, &boxes, rng);
                    sum_of_forces += force.magnitude();
                    forces.push(force);
                }
                if rmv_drift {
                    let mut drift = Point::new(0.0, 0.0);
                    for f in &forces {
                        drift = drift + *f;
                    }
                    drift = drift / mobile.len() as f64;
                    for f in forces.iter_mut() {
                        *f = *f - drift;
                    }
                }
                for (&i, f) in mobile.iter().zip(&forces) {
                    let mv = *f * speed;
                    self.bodies[i].tx = self.bodies[i].tx * Tx::translate(mv.x, mv.y);
                }

                if stable_threshold < 0.0 {
                    initial_sum = sum_of_forces;
                    stable_threshold = sum_of_forces * stability_coef;
                } else if sum_of_forces <= stable_threshold {
                    break;
                } else if sum_of_forces > 10.0 * initial_sum {
                    // Rising forces usually mean the bodies are flying apart
                    // because the speed is too high.
                    speed *= 0.5;
                }
            }
        }
    }
}

/// Move a transformation so a point lands on the grid.
fn snap_tx(tx: Tx, snap_pt: Point) -> Tx {
    let pt = snap_pt * tx;
    let mv = pt.snap(GRID) - pt;
    tx * Tx::translate(mv.x, mv.y)
}

fn tx_key(tx: Tx) -> (i64, i64, i64, i64) {
    (pyround(tx.a) as i64, pyround(tx.b) as i64, pyround(tx.c) as i64, pyround(tx.d) as i64)
}

/// How far two pins are from being joined by a straight wire.
///
/// Returns the sideways offset between the pins if each one's wire leaves
/// toward the other, else None (no sideways shift lets a straight wire join them).
fn facing_offset(pt1: Point, exit1: Point, pt2: Point, exit2: Point) -> Option<f64> {
    let d = pt2 - pt1;
    if d.x.abs() < 1.0 && d.y.abs() < 1.0 {
        return Some(0.0); // The pins touch.
    }
    if exit1.x * exit2.x + exit1.y * exit2.y > -0.5 {
        return None; // The exits aren't opposed.
    }
    let (along, sideways) = if exit1.x.abs() > exit1.y.abs() { (d.x * exit1.x, d.y) } else { (d.y * exit1.y, d.x) };
    if along <= 0.0 {
        return None; // The pins face away from each other.
    }
    Some(sideways.abs())
}

/// A block of parts, or a child node, that's placed as one thing.
struct PartBlock {
    src: BlockSrc,
    bbox: BBox,
    snap_pt: Point,
    tag: usize,
}

enum BlockSrc {
    Parts(Vec<PartId>),
    Node(NodeId),
}

impl Design {
    /// The point of a part that's put on the grid.
    fn part_snap_pt(&self, part: PartId) -> Option<Point> {
        self.c.parts[part].pins.first().map(|&p| self.c.pins[p].pt)
    }

    pub fn snap_part_to_grid(&mut self, part: PartId) {
        if let Some(pt) = self.part_snap_pt(part) {
            self.c.parts[part].tx = snap_tx(self.c.parts[part].tx, pt);
        }
    }

    /// Give each part a placement box: its own box plus room around it for routing.
    pub fn add_placement_bboxes(&mut self, parts: &[PartId], expansion_factor: f64) {
        for &part in parts {
            let mut bbox = BBox::empty();
            bbox.add(self.c.parts[part].lbl_bbox);

            // The room on each side goes with the number of wired pins there,
            // with at least one routing channel.
            let (mut u, mut d, mut l, mut r) = (1.0, 1.0, 1.0, 1.0);
            for &pin in &self.c.parts[part].pins {
                if !self.c.pins[pin].stub && self.c.is_connected(pin) {
                    match self.c.pins[pin].orientation {
                        'U' => u += 1.0,
                        'D' => d += 1.0,
                        'L' => l += 1.0,
                        _ => r += 1.0,
                    }
                }
            }
            // The room is expanded after routing fails, except around net
            // terminals, which only label wires.
            let factor = if self.c.parts[part].is_net_terminal { 1.0 } else { expansion_factor };
            let (max, min) = (bbox.max, bbox.min);
            bbox.add_pt(max + Point::new(l, d) * GRID * factor);
            bbox.add_pt(min - Point::new(r, u) * GRID * factor);
            self.c.parts[part].place_bbox = Some(bbox);
        }
    }

    pub fn place_bbox(&self, part: PartId) -> BBox {
        self.c.parts[part].place_bbox.unwrap_or(self.c.parts[part].lbl_bbox)
    }

    /// The box around the placement boxes of some parts.
    pub fn enclosing_bbox(&self, parts: &[PartId]) -> BBox {
        let mut bbox = BBox::empty();
        for &part in parts {
            bbox.add(self.place_bbox(part) * self.c.parts[part].tx);
        }
        bbox
    }

    /// Set the point where a pin's wire crosses its part's placement box.
    fn add_place_pt(&mut self, pin: PinId) {
        let bbox = self.place_bbox(self.c.pins[pin].part);
        let p = &mut self.c.pins[pin];
        p.route_pt = p.pt;
        let mut pt = p.pt;
        match p.orientation {
            'U' => pt.y = bbox.min.y,
            'D' => pt.y = bbox.max.y,
            'L' => pt.x = bbox.max.x,
            _ => pt.x = bbox.min.x,
        }
        p.place_pt = Some(pt);
    }

    /// Set the anchor and pull pins that the attractive forces between parts act on.
    ///
    /// With no nets, the parts are floating and all pull on each other.
    pub fn add_anchor_pull_pins(&mut self, parts: &[PartId], nets: &[NetId]) {
        for &part in parts {
            let pl = &mut self.c.parts[part].pl;
            pl.anchor_pins = IndexMap::new();
            pl.pull_pins = IndexMap::new();
            pl.pin_ctrs = IndexMap::new();
        }
        let part_set: HashSet<PartId> = parts.iter().copied().collect();

        if !nets.is_empty() {
            let mut net_parts_of: Vec<Vec<PartId>> = Vec::with_capacity(nets.len());
            for &net in nets {
                // The pins and parts are kept in order: the forces they
                // generate are summed in that order.
                let pins: Vec<PinId> =
                    self.c.nets[net].pins.iter().copied().filter(|&p| part_set.contains(&self.c.pins[p].part)).collect();
                let mut net_parts: Vec<PartId> = Vec::new();
                for &pin in &pins {
                    let part = self.c.pins[pin].part;
                    if !net_parts.contains(&part) {
                        net_parts.push(part);
                    }
                }
                // Each pin is an anchor on its own part and pulls on the other
                // parts. Net terminals are pulled toward the parts they're
                // connected to, but don't pull on them.
                for &pin in &pins {
                    let own = self.c.pins[pin].part;
                    self.c.parts[own].pl.anchor_pins.entry(net).or_default().push(pin);
                    self.add_place_pt(pin);
                    if !self.c.parts[own].is_net_terminal {
                        for &part in &net_parts {
                            if part != own {
                                self.c.parts[part].pl.pull_pins.entry(net).or_default().push(pin);
                            }
                        }
                    }
                }
                net_parts_of.push(net_parts);
            }
            for (&net, net_parts) in nets.iter().zip(&net_parts_of) {
                for &part in net_parts {
                    let anchors = self.c.parts[part].pl.anchor_pins.get(&net).cloned().unwrap_or_default();
                    if !anchors.is_empty() {
                        let mut sum = Point::new(0.0, 0.0);
                        for &pin in &anchors {
                            sum = sum + self.c.pins[pin].place_pt.unwrap();
                        }
                        self.c.parts[part].pl.pin_ctrs.insert(net, sum / anchors.len() as f64);
                    }
                }
            }
        } else {
            let mut all_pull_pins = Vec::new();
            for &part in parts {
                // The anchor is the top-most pin, so the tops of floating parts line up.
                let pins = self.c.parts[part].pins.clone();
                let anchor = match pins.iter().copied().reduce(|best, p| {
                    if self.c.pins[p].pt.y > self.c.pins[best].pt.y {
                        p
                    } else {
                        best
                    }
                }) {
                    Some(pin) => {
                        self.add_place_pt(pin);
                        pin
                    }
                    None => {
                        // A part with no pins at all gets a pin made up for it.
                        let pin =
                            Pin { part, owner: part, place_pt: Some(self.place_bbox(part).max), ..Default::default() };
                        self.c.add_pin(pin)
                    }
                };
                self.c.parts[part].pl.anchor_pins.insert(SIMILARITY, vec![anchor]);
                all_pull_pins.push(anchor);
            }
            for &part in parts {
                self.c.parts[part].pl.pull_pins.insert(SIMILARITY, all_pull_pins.clone());
            }
        }
    }

    /// Set up force-directed placement of some parts, with the anchored ones first.
    fn part_forces(&self, anchored: &[PartId], mobile: &[PartId], similarity: Option<Vec<Vec<f64>>>) -> Forces {
        let parts: Vec<PartId> = anchored.iter().chain(mobile.iter()).copied().collect();
        let index: HashMap<PartId, usize> = parts.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let bodies: Vec<Body> =
            parts.iter().map(|&p| Body { tx: self.c.parts[p].tx, bbox: self.place_bbox(p) }).collect();

        let mut pins: Vec<FPin> = Vec::new();
        let mut pin_index: HashMap<PinId, usize> = HashMap::new();
        let mut pin_of = |pin: PinId, pins: &mut Vec<FPin>| -> usize {
            *pin_index.entry(pin).or_insert_with(|| {
                let p = &self.c.pins[pin];
                let pt = p.place_pt.unwrap_or(p.pt);
                match index.get(&p.part) {
                    Some(&b) => pins.push(FPin { body: Some(b), pt }),
                    // A pin on a part that isn't being placed stays where it is.
                    None => pins.push(FPin { body: None, pt: pt * self.c.parts[p.part].tx }),
                }
                pins.len() - 1
            })
        };

        let attraction = match similarity {
            Some(sim) => {
                let anchors: Vec<usize> = parts
                    .iter()
                    .map(|&p| pin_of(self.c.parts[p].pl.anchor_pins[&SIMILARITY][0], &mut pins))
                    .collect();
                let pulls: Vec<usize> = parts
                    .first()
                    .map(|&p| self.c.parts[p].pl.pull_pins[&SIMILARITY].clone())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|&pin| index.contains_key(&self.c.pins[pin].part))
                    .map(|pin| pin_of(pin, &mut pins))
                    .collect();
                Attraction::Similarity { anchors, pulls, sim }
            }
            None => {
                let mut groups = Vec::with_capacity(parts.len());
                for &part in &parts {
                    let pl = &self.c.parts[part].pl;
                    let mut part_groups = Vec::new();
                    for (net, anchor_pins) in &pl.anchor_pins {
                        let Some(pull_pins) = pl.pull_pins.get(net) else { continue };
                        if anchor_pins.is_empty() || pull_pins.is_empty() {
                            continue;
                        }
                        part_groups.push(NetGroup {
                            anchors: anchor_pins.iter().map(|&p| pin_of(p, &mut pins)).collect(),
                            pulls: pull_pins.iter().map(|&p| pin_of(p, &mut pins)).collect(),
                            // Point-to-point nets pull harder.
                            mult: if pull_pins.len() <= 1 { self.opts.pt_to_pt_mult } else { 1.0 },
                        });
                    }
                    groups.push(part_groups);
                }
                Attraction::Nets(groups)
            }
        };
        Forces {
            bodies,
            n_anchored: anchored.len(),
            pins,
            attraction,
            pin_normalize: self.opts.pin_normalize,
            use_push_pull: self.opts.use_push_pull,
        }
    }

    /// Evolve the placement of the mobile parts: force-directed placement,
    /// then onto the grid, then nudged so connected pins line up.
    fn evolve_placement(&mut self, anchored: &[PartId], mobile: &[PartId], nets: &[NetId], similarity: Option<Vec<Vec<f64>>>) {
        let mut forces = self.part_forces(anchored, mobile, similarity);
        forces.push_and_pull(&mut self.rng);
        for (i, &part) in anchored.iter().chain(mobile.iter()).enumerate() {
            self.c.parts[part].tx = forces.bodies[i].tx;
        }
        for &part in anchored.iter().chain(mobile.iter()) {
            self.snap_part_to_grid(part);
        }
        self.align_parts(anchored, mobile, nets);
    }

    /// A pin's position and the direction a wire leaves it, after its part's transformation.
    fn pin_pt_exit(&self, pin: PinId) -> (Point, Point) {
        let p = &self.c.pins[pin];
        let tx = self.c.parts[p.part].tx;
        let pt = p.pt * tx;
        (pt, (p.pt + pin_exit_vec(p.orientation)) * tx - pt)
    }

    /// The routed (unstubbed) pins of each net that are on the given parts,
    /// for the nets with pins on two or more of them.
    fn alignment_net_pins(&self, parts: &[PartId], nets: &[NetId]) -> IndexMap<NetId, Vec<PinId>> {
        let part_set: HashSet<PartId> = parts.iter().copied().collect();
        let mut net_pins = IndexMap::new();
        for &net in nets {
            let pins: Vec<PinId> = self.c.nets[net]
                .pins
                .iter()
                .copied()
                .filter(|&p| part_set.contains(&self.c.pins[p].part) && !self.c.pins[p].stub)
                .collect();
            let on_parts: HashSet<PartId> = pins.iter().map(|&p| self.c.pins[p].part).collect();
            if on_parts.len() >= 2 {
                net_pins.insert(net, pins);
            }
        }
        net_pins
    }

    /// Score how well the pins of a net line up: (pins that face a pin of
    /// another part along a straight line, pins that face one but are offset
    /// sideways by no more than NEAR_MISS_DIST, half-perimeter wire length).
    fn net_alignment_score(&self, pins: &[PinId]) -> (usize, usize, f64) {
        let pt_exits: Vec<(Point, Point)> = pins.iter().map(|&p| self.pin_pt_exit(p)).collect();
        let (mut aligned, mut near_miss) = (0, 0);
        for (i, &pin) in pins.iter().enumerate() {
            let (pt, exit) = pt_exits[i];
            let offsets: Vec<f64> = pins
                .iter()
                .enumerate()
                .filter(|(_, &o)| self.c.pins[o].part != self.c.pins[pin].part)
                .filter_map(|(j, _)| facing_offset(pt, exit, pt_exits[j].0, pt_exits[j].1))
                .collect();
            if offsets.iter().any(|&o| o < 1.0) {
                aligned += 1;
            } else if offsets.iter().any(|&o| o <= NEAR_MISS_DIST) {
                near_miss += 1;
            }
        }
        let xs = pt_exits.iter().map(|p| p.0.x);
        let ys = pt_exits.iter().map(|p| p.0.y);
        let hpwl = (xs.clone().fold(f64::NEG_INFINITY, f64::max) - xs.fold(f64::INFINITY, f64::min))
            + (ys.clone().fold(f64::NEG_INFINITY, f64::max) - ys.fold(f64::INFINITY, f64::min));
        (aligned, near_miss, hpwl)
    }

    /// Shift parts slightly so connected pins face each other along a straight line.
    ///
    /// Each mobile part is moved by the offset that lines up the most pins of
    /// its nets, as long as the move doesn't create a new overlap.
    pub fn align_parts(&mut self, anchored: &[PartId], mobile: &[PartId], nets: &[NetId]) {
        if nets.is_empty() || mobile.is_empty() {
            return;
        }
        let max_offset = 3.0 * GRID;
        let parts: Vec<PartId> = anchored.iter().chain(mobile.iter()).copied().collect();
        let net_pins = self.alignment_net_pins(&parts, nets);
        if net_pins.is_empty() {
            return;
        }

        // The nets touching each part.
        let mut part_nets: HashMap<PartId, Vec<NetId>> = HashMap::new();
        for (&net, pins) in &net_pins {
            let mut seen: Vec<PartId> = Vec::new();
            for &pin in pins {
                let part = self.c.pins[pin].part;
                if !seen.contains(&part) {
                    seen.push(part);
                    part_nets.entry(part).or_default().push(net);
                }
            }
        }
        let mut bboxes: HashMap<PartId, BBox> =
            parts.iter().map(|&p| (p, self.place_bbox(p) * self.c.parts[p].tx)).collect();

        // Move the smallest parts first: they disturb the least when they move.
        let mut order = mobile.to_vec();
        order.sort_by_key(|&p| self.c.parts[p].pins.len());

        for _ in 0..10 {
            let mut moved = false;
            for &part in &order {
                let Some(my_nets) = part_nets.get(&part).cloned() else { continue };
                let score = |d: &Design| -> (usize, f64) {
                    let (mut aligned, mut hpwl) = (0, 0.0);
                    for net in &my_nets {
                        let (a, _, h) = d.net_alignment_score(&net_pins[net]);
                        aligned += a;
                        hpwl += h;
                    }
                    (aligned, hpwl)
                };
                let overlapping = |bbox: &BBox, bboxes: &HashMap<PartId, BBox>| -> HashSet<PartId> {
                    parts.iter().copied().filter(|&o| o != part && bbox.intersects(&bboxes[&o])).collect()
                };

                // Moves that would line up one of the part's pins with a connected pin facing it.
                let (mut dxs, mut dys): (Vec<i64>, Vec<i64>) = (Vec::new(), Vec::new());
                for net in &my_nets {
                    let pins = &net_pins[net];
                    for &m in pins.iter().filter(|&&p| self.c.pins[p].part == part) {
                        let (m_pt, m_exit) = self.pin_pt_exit(m);
                        for &o in pins.iter().filter(|&&p| self.c.pins[p].part != part) {
                            let (o_pt, o_exit) = self.pin_pt_exit(o);
                            if facing_offset(m_pt, m_exit, o_pt, o_exit).is_none() {
                                continue;
                            }
                            // Shift sideways to the pins' common axis, staying on the grid.
                            let (d, list) = if m_exit.x.abs() > m_exit.y.abs() {
                                (pyround(o_pt.y - m_pt.y), &mut dys)
                            } else {
                                (pyround(o_pt.x - m_pt.x), &mut dxs)
                            };
                            if d != 0.0 && d.abs() <= max_offset && d.rem_euclid(GRID) == 0.0 && !list.contains(&(d as i64)) {
                                list.push(d as i64);
                            }
                        }
                    }
                }
                dxs.sort();
                dys.sort();
                let mut moves: Vec<(i64, i64)> = dxs.iter().map(|&dx| (dx, 0)).collect();
                moves.extend(dys.iter().map(|&dy| (0, dy)));
                // Moving in x and y at once lets a part line up on two sides.
                if dxs.len() * dys.len() <= 64 {
                    for &dx in &dxs {
                        for &dy in &dys {
                            moves.push((dx, dy));
                        }
                    }
                }

                let base_tx = self.c.parts[part].tx;
                let base_bbox = bboxes[&part];
                let base_overlaps = overlapping(&base_bbox, &bboxes);
                let base_aligned = score(self).0;
                let mut best: Option<((usize, f64, i64), Tx)> = None;
                for (dx, dy) in moves {
                    let move_tx = Tx::translate(dx as f64, dy as f64);
                    // Don't create overlaps that weren't already there.
                    if !overlapping(&(base_bbox * move_tx), &bboxes).is_subset(&base_overlaps) {
                        continue;
                    }
                    self.c.parts[part].tx = base_tx * move_tx;
                    let (aligned, hpwl) = score(self);
                    self.c.parts[part].tx = base_tx;
                    // The move must line up more pins. Ties go to the shorter
                    // wiring, then the smaller move.
                    let key = (aligned, -hpwl, -(dx.abs() + dy.abs()));
                    if aligned > base_aligned && best.as_ref().is_none_or(|(b, _)| key > *b) {
                        best = Some((key, move_tx));
                    }
                }
                if let Some((_, move_tx)) = best {
                    self.c.parts[part].tx = base_tx * move_tx;
                    bboxes.insert(part, base_bbox * move_tx);
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }

    /// The tension of the nets trying to turn or flip a part: for each of its
    /// pins, the distance to the nearest pin pulling on it.
    fn net_tension(&self, part: PartId) -> f64 {
        let pl = &self.c.parts[part].pl;
        let place_pt = |pin: PinId| {
            let p = &self.c.pins[pin];
            p.place_pt.unwrap_or(p.pt) * self.c.parts[p.part].tx
        };
        let mut tension = 0.0;
        for (net, anchor_pins) in &pl.anchor_pins {
            let Some(pull_pins) = pl.pull_pins.get(net) else { continue };
            if anchor_pins.is_empty() || pull_pins.is_empty() {
                continue;
            }
            let pull_pts: Vec<Point> = pull_pins.iter().map(|&p| place_pt(p)).collect();
            for &anchor in anchor_pins {
                let a = place_pt(anchor);
                tension += pull_pts.iter().map(|p| (a - *p).magnitude()).fold(f64::INFINITY, f64::min);
            }
        }
        tension
    }

    /// Estimate how badly a part's net labels pile onto things in its current orientation.
    ///
    /// Each stubbed, connected pin gets a label drawn as a flag extending out
    /// from the pin. This counts where those flags land on another part, on
    /// the part's own body or text, or on each other.
    fn label_overlap_cost(&self, part: PartId, neighbors: &[PartId]) -> f64 {
        let p = &self.c.parts[part];
        let lbl_pins: Vec<PinId> =
            p.pins.iter().copied().filter(|&pin| self.c.pins[pin].stub && self.c.is_connected(pin)).collect();
        if lbl_pins.is_empty() || p.pins.is_empty() {
            return 0.0;
        }
        let pin_span = BBox::from_pts(&p.pins.iter().map(|&pin| self.c.pins[pin].pt).collect::<Vec<_>>());

        let mut boxes = Vec::new();
        let mut out_dirs = Vec::new();
        for &pin in &lbl_pins {
            let pn = &self.c.pins[pin];
            let anchor = pn.pt * p.tx;
            let (x, y) = match pn.orientation {
                'U' => (0.0, 1.0),
                'D' => (0.0, -1.0),
                'L' => (-1.0, 0.0),
                _ => (1.0, 0.0),
            };
            let (vx, vy) = (x * p.tx.a + y * p.tx.c, x * p.tx.b + y * p.tx.d);
            let unit = |v: f64| if v > 0.5 { 1.0 } else if v < -0.5 { -1.0 } else { 0.0 };
            let out = Point::new(unit(vx), unit(vy));
            let far = Point::new(anchor.x + out.x * LABEL_REACH, anchor.y + out.y * LABEL_REACH);
            let pad_x = if out.y == 0.0 { 0.0 } else { LABEL_HALF_H };
            let pad_y = if out.y == 0.0 || out.x != 0.0 { LABEL_HALF_H } else { 0.0 };
            boxes.push(BBox {
                min: Point::new(anchor.x.min(far.x) - pad_x, anchor.y.min(far.y) - pad_y),
                max: Point::new(anchor.x.max(far.x) + pad_x, anchor.y.max(far.y) + pad_y),
            });
            out_dirs.push(out);
        }

        let mut cost = 0.0;
        // A label over the part's own body.
        let own_body = pin_span.resize(Point::new(LABEL_BODY_MARGIN, LABEL_BODY_MARGIN)) * p.tx;
        cost += boxes.iter().filter(|b| b.intersects(&own_body)).count() as f64;
        // A label pointing up or down across the part's reference and value,
        // which are always drawn level.
        let ctr = (pin_span * p.tx).ctr();
        let band = BBox::new(
            Point::new(ctr.x - LABEL_TEXT_BAND_HALF_W, ctr.y - LABEL_TEXT_BAND_HALF_H),
            Point::new(ctr.x + LABEL_TEXT_BAND_HALF_W, ctr.y + LABEL_TEXT_BAND_HALF_H),
        );
        cost += out_dirs.iter().zip(&boxes).filter(|(o, b)| o.x == 0.0 && b.intersects(&band)).count() as f64;
        // A label over a neighboring part.
        for &other in neighbors {
            if other == part {
                continue;
            }
            let Some(pb) = self.c.parts[other].place_bbox else { continue };
            let other_bbox = pb * self.c.parts[other].tx;
            cost += boxes.iter().filter(|b| b.intersects(&other_bbox)).count() as f64;
        }
        // Two of the part's own labels over each other.
        for i in 0..boxes.len() {
            for j in i + 1..boxes.len() {
                if boxes[i].intersects(&boxes[j]) {
                    cost += 1.0;
                }
            }
        }
        cost
    }

    /// Turn and flip parts to shorten their nets (and, with `label_aware`, to
    /// keep their labels from piling up). Returns true if any part was turned.
    fn adjust_orientations(&mut self, parts: &[PartId], label_aware: bool) -> bool {
        // The parts that aren't locked and have more than one orientation
        // that follows the drafting conventions.
        let mut allowed: HashMap<PartId, HashSet<(i64, i64, i64, i64)>> = HashMap::new();
        let mut movable = Vec::new();
        for &part in parts {
            if self.c.parts[part].orientation_locked {
                continue;
            }
            let txs: HashSet<_> = best_orientations(&self.c, part).into_iter().map(tx_key).collect();
            if txs.iter().any(|k| *k != tx_key(self.c.parts[part].tx)) {
                movable.push(part);
            }
            allowed.insert(part, txs);
        }
        if movable.is_empty() {
            return false;
        }
        let neighbors: Vec<PartId> = if label_aware { parts.to_vec() } else { Vec::new() };
        let cost = |d: &Design, part: PartId| -> f64 {
            let mut c = d.net_tension(part);
            if label_aware {
                c += LABEL_OVERLAP_WEIGHT * d.label_overlap_cost(part, &neighbors);
            }
            c
        };

        // Kernighan-Lin search for good orientations. The tension depends on
        // the nearest part, so this can go on forever; hence the limit.
        let mut iter_cnt = 0;
        for iteration in 0..10 {
            iter_cnt = iteration;
            let mut prev_tx: HashMap<PartId, Tx> = HashMap::new();
            let mut moved: Vec<(PartId, f64)> = Vec::new();
            let mut unmoved = movable.clone();
            while !unmoved.is_empty() {
                // For each unmoved part, the best of the other seven orientations.
                let mut best_of: Vec<(f64, Tx)> = Vec::with_capacity(unmoved.len());
                for &part in &unmoved {
                    let start_tx = self.c.parts[part].tx;
                    prev_tx.insert(part, start_tx);
                    let ctr = (self.place_bbox(part) * start_tx).ctr();
                    let mut best_delta = f64::INFINITY;
                    let mut best_tx = start_tx;
                    let mut starting_cost = None;
                    for _ in 0..2 {
                        for _ in 0..4 {
                            match starting_cost {
                                None => starting_cost = Some(cost(self, part)),
                                Some(start) => {
                                    if allowed[&part].contains(&tx_key(self.c.parts[part].tx)) {
                                        let delta = cost(self, part) - start;
                                        if delta < best_delta {
                                            best_delta = delta;
                                            best_tx = self.c.parts[part].tx;
                                        }
                                    }
                                }
                            }
                            let tx = self.c.parts[part].tx;
                            self.c.parts[part].tx = tx.move_by(-ctr).rot_90cw().move_by(ctr);
                        }
                        let tx = self.c.parts[part].tx;
                        self.c.parts[part].tx = tx.move_by(-ctr).flip_x().move_by(ctr);
                    }
                    self.c.parts[part].tx = start_tx;
                    best_of.push((best_delta, best_tx));
                }
                // Turn the part with the largest decrease in cost.
                let mut pick = 0;
                for i in 1..unmoved.len() {
                    if best_of[i].0 < best_of[pick].0 {
                        pick = i;
                    }
                }
                let part = unmoved.remove(pick);
                self.c.parts[part].tx = best_of[pick].1;
                moved.push((part, best_of[pick].0));
            }

            // Find where the running cost was lowest, and undo the turns after that.
            let mut cost_seq = vec![0.0];
            for (_, delta) in &moved {
                cost_seq.push(cost_seq.last().unwrap() + delta);
            }
            let mut min_index = 0;
            for (i, c) in cost_seq.iter().enumerate() {
                if *c < cost_seq[min_index] {
                    min_index = i;
                }
            }
            for (part, _) in &moved[min_index..] {
                self.c.parts[*part].tx = prev_tx[part];
            }
            if min_index == 0 {
                break;
            }
        }
        iter_cnt > 0
    }

    /// Place net terminals around parts that are already placed.
    fn place_net_terminals(&mut self, net_terminals: &[PartId], placed_parts: &[PartId], nets: &[NetId]) {
        if net_terminals.is_empty() {
            return;
        }
        let bbox = self.enclosing_bbox(placed_parts);
        let place_pt = |d: &Design, pin: PinId| {
            let p = &d.c.pins[pin];
            p.place_pt.unwrap_or(p.pt) * d.c.parts[p.part].tx
        };
        for &terminal in net_terminals {
            let pl = &self.c.parts[terminal].pl;
            let saved = (pl.anchor_pins.clone(), pl.pull_pins.clone());

            // Keep only the pull pin nearest an edge of the placed parts: it's the easiest to reach.
            let mut trimmed = pl.pull_pins.clone();
            for (_, pull_pins) in trimmed.iter_mut() {
                let mut best: Option<(f64, PinId)> = None;
                for &pin in pull_pins.iter() {
                    let pt = place_pt(self, pin);
                    for inset in [
                        (pt.x - bbox.ll().x).abs(),
                        (pt.x - bbox.lr().x).abs(),
                        (pt.y - bbox.ul().y).abs(),
                        (pt.y - bbox.ll().y).abs(),
                    ] {
                        if best.is_none_or(|b| inset < b.0) {
                            best = Some((inset, pin));
                        }
                    }
                }
                if let Some((_, pin)) = best {
                    *pull_pins = vec![pin];
                }
            }
            let pull_pin = trimmed.values().find_map(|pins| pins.first().copied());
            self.c.parts[terminal].pl.pull_pins = trimmed;
            self.c.parts[terminal].pl.saved = Some(saved);
            let Some(pull_pin) = pull_pin else { continue };

            // Point the terminal away from the nearest edge of the placed parts.
            let pull_pt = place_pt(self, pull_pin);
            let insets = [
                ((pull_pt.x - bbox.ll().x).abs(), Tx::IDENT),
                ((pull_pt.x - bbox.lr().x).abs(), Tx::IDENT.flip_x()),
                ((pull_pt.y - bbox.ul().y).abs(), Tx::IDENT.rot_90cw().rot_90cw().rot_90cw()),
                ((pull_pt.y - bbox.ll().y).abs(), Tx::IDENT.rot_90cw()),
            ];
            let mut best = insets[0];
            for inset in &insets[1..] {
                if inset.0 < best.0 {
                    best = *inset;
                }
            }
            self.c.parts[terminal].tx = best.1;

            // Move the terminal straight to its pull pin.
            if let Some(&anchor) = self.c.parts[terminal].pl.anchor_pins.values().find_map(|pins| pins.first()) {
                let anchor_pt = place_pt(self, anchor);
                let tx = self.c.parts[terminal].tx;
                self.c.parts[terminal].tx = tx.move_by(pull_pt - anchor_pt);
            }
        }

        self.evolve_placement(placed_parts, net_terminals, nets, None);

        for &terminal in net_terminals {
            if let Some((anchor, pull)) = self.c.parts[terminal].pl.saved.take() {
                self.c.parts[terminal].pl.anchor_pins = anchor;
                self.c.parts[terminal].pl.pull_pins = pull;
            }
        }
    }

    /// Group the parts of a node that are connected by its internal nets.
    ///
    /// Returns the groups of connected parts, the internal nets, and the parts
    /// that aren't connected to anything (floating).
    pub fn group_parts(&self, node: NodeId) -> (Vec<Vec<PartId>>, Vec<NetId>, Vec<PartId>) {
        let node_parts = &self.nodes[node].parts;
        if node_parts.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        let internal_nets = self.get_internal_nets(node);

        // Start with the parts on each net, then join the groups that share parts.
        let mut groups: Vec<HashSet<PartId>> = internal_nets
            .iter()
            .map(|&net| {
                self.c.nets[net].pins.iter().map(|&p| self.c.pins[p].part).filter(|&p| self.c.parts[p].node == Some(node)).collect()
            })
            .collect();
        for i in 0..groups.len().saturating_sub(1) {
            for j in i + 1..groups.len() {
                if !groups[i].is_disjoint(&groups[j]) {
                    let merged = std::mem::take(&mut groups[i]);
                    groups[j].extend(merged);
                    break;
                }
            }
        }
        groups.retain(|g| !g.is_empty());

        // Everything downstream depends on order, so the parts go back in the node's own order.
        let connected: HashSet<PartId> = groups.iter().flatten().copied().collect();
        let ordered =
            |group: &HashSet<PartId>| -> Vec<PartId> { node_parts.iter().copied().filter(|p| group.contains(p)).collect() };
        let floating: Vec<PartId> = node_parts.iter().copied().filter(|p| !connected.contains(p)).collect();
        (groups.iter().map(ordered).collect(), internal_nets, floating)
    }

    /// Place connected parts in rows, in breadth-first order from the most-connected part.
    ///
    /// This is for groups too large for force-directed placement.
    fn place_connected_parts_rowbased(&mut self, parts: &[PartId], nets: &[NetId]) {
        if parts.is_empty() {
            return;
        }
        self.add_placement_bboxes(parts, self.opts.expansion_factor);
        self.add_anchor_pull_pins(parts, nets);

        let part_set: HashSet<PartId> = parts.iter().copied().collect();
        let mut adjacency: HashMap<PartId, Vec<PartId>> = HashMap::new();
        for &net in nets {
            let net_parts: Vec<PartId> =
                self.c.nets[net].pins.iter().map(|&p| self.c.pins[p].part).filter(|p| part_set.contains(p)).collect();
            for (i, &p1) in net_parts.iter().enumerate() {
                for &p2 in &net_parts[i + 1..] {
                    if !adjacency.entry(p1).or_default().contains(&p2) {
                        adjacency.get_mut(&p1).unwrap().push(p2);
                    }
                    if !adjacency.entry(p2).or_default().contains(&p1) {
                        adjacency.get_mut(&p2).unwrap().push(p1);
                    }
                }
            }
        }
        let degree = |p: PartId| adjacency.get(&p).map_or(0, |a| a.len());
        let mut seed = parts[0];
        for &p in parts {
            if degree(p) > degree(seed) {
                seed = p;
            }
        }
        let mut visited: HashSet<PartId> = HashSet::from([seed]);
        let mut queue = VecDeque::from([seed]);
        let mut order = Vec::new();
        while let Some(part) = queue.pop_front() {
            order.push(part);
            for &neighbor in adjacency.get(&part).into_iter().flatten() {
                if visited.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }
        order.extend(parts.iter().copied().filter(|p| !visited.contains(p)));

        let total_area: f64 = order.iter().map(|&p| self.place_bbox(p).w().max(1.0) * self.place_bbox(p).h().max(1.0)).sum();
        let max_row_width = total_area.sqrt() * 2.0;
        let (mut col_x, mut row_y, mut row_max_h) = (0.0, 0.0, 0.0f64);
        for &part in &order {
            let w = self.place_bbox(part).w().max(100.0);
            let h = self.place_bbox(part).h().max(100.0);
            if col_x > 0.0 && col_x + w > max_row_width {
                row_y += row_max_h + BLK_INT_PAD;
                col_x = 0.0;
                row_max_h = 0.0;
            }
            self.c.parts[part].tx = Tx::IDENT.move_by(Point::new(col_x, row_y));
            col_x += w + BLK_INT_PAD;
            row_max_h = row_max_h.max(h);
        }
        for &part in &order {
            self.snap_part_to_grid(part);
        }

        let net_terminals: Vec<PartId> = parts.iter().copied().filter(|&p| self.c.parts[p].is_net_terminal).collect();
        let real_parts: Vec<PartId> = parts.iter().copied().filter(|&p| !self.c.parts[p].is_net_terminal).collect();
        self.place_net_terminals(&net_terminals, &real_parts, nets);
    }

    /// Place a group of parts that are connected by nets.
    fn place_connected_parts(&mut self, parts: &[PartId], nets: &[NetId]) {
        if parts.is_empty() {
            return;
        }
        let mut net_terminals: Vec<PartId> = parts.iter().copied().filter(|&p| self.c.parts[p].is_net_terminal).collect();
        let mut real_parts: Vec<PartId> = parts.iter().copied().filter(|&p| !self.c.parts[p].is_net_terminal).collect();
        let preplace = self.opts.preplace;

        // Row-based placement is for large groups, which force-directed
        // placement is too slow for. (Pre-placement isn't.)
        if !preplace && real_parts.len() > ROW_PLACE_THRESHOLD {
            return self.place_connected_parts_rowbased(parts, nets);
        }

        // Placement boxes leave room around the parts so they aren't butted against each other.
        self.add_placement_bboxes(parts, self.opts.expansion_factor);

        if preplace {
            // Put the parts where a person would instead of letting forces
            // pull them in from random starting points.

            // Fix satellites beside the pins they're wired to. Their hosts
            // carry them. Any that don't fit are placed like the other parts.
            let found = self.find_satellites(&real_parts, nets);
            let mut satellites = Vec::new();
            for (host, sats) in found {
                let attached = self.attach_satellites(host, &sats, nets);
                satellites.push((host, attached));
            }
            let riders: HashSet<PartId> = satellites.iter().flat_map(|(_, sats)| sats.iter().map(|s| s.0)).collect();
            let hosts: Vec<PartId> = real_parts.iter().copied().filter(|p| !riders.contains(p)).collect();
            let mut movable = hosts.clone();
            movable.extend(net_terminals.iter().copied());
            self.add_anchor_pull_pins(&movable, nets);

            // Put each part beside the pins it's wired to, then move the parts
            // off of each other and line up their pins.
            self.seed_placement(&hosts, nets);
            self.spread_parts(&hosts, nets);
            self.align_parts(&[], &hosts, nets);

            // Bring the satellites back, and include them when placing net terminals.
            self.release_satellites(&satellites);
            for part in real_parts.iter().copied().filter(|p| riders.contains(p)) {
                self.snap_part_to_grid(part);
            }
            self.add_anchor_pull_pins(parts, nets);

            // Put each net label beside a pin of its net. Any that can't be
            // placed are left for the forces below, with the placed ones held still.
            net_terminals = self.place_terminals(&net_terminals, &real_parts, nets);
            real_parts = parts.iter().copied().filter(|p| !net_terminals.contains(p)).collect();
        } else {
            self.add_anchor_pull_pins(parts, nets);

            // Start the connected parts at random places.
            let mut forces = self.part_forces(&[], parts, None);
            forces.random_placement(&mut self.rng);
            if self.opts.compress_before_place {
                forces.central_placement();
            }
            for (i, &part) in parts.iter().enumerate() {
                self.c.parts[part].tx = forces.bodies[i].tx;
            }

            // A first trial placement, then turn the parts and place them again if any turned.
            self.evolve_placement(&[], &real_parts, nets, None);
            if self.opts.rotate_parts && self.adjust_orientations(&real_parts, false) {
                self.evolve_placement(&[], &real_parts, nets, None);
            }
        }

        // Net terminals are placed after all the other parts.
        self.place_net_terminals(&net_terminals, &real_parts, nets);
    }

    /// Place the parts that aren't wired to anything.
    fn place_floating_parts(&mut self, parts: &[PartId]) {
        if parts.is_empty() {
            return;
        }
        if self.opts.preplace {
            // Put them in rows the way a person would.
            self.add_placement_bboxes(parts, 1.0);
            self.row_floating_parts(parts);
            return;
        }

        // A large number of floating parts is just put on a grid, to avoid
        // the cost of force-directed placement.
        if parts.len() > 20 && self.opts.auto_stub {
            self.add_placement_bboxes(parts, 1.0);
            let cols = ((parts.len() as f64).sqrt() as usize).max(1);
            for (i, &part) in parts.iter().enumerate() {
                let (row, col) = (i / cols, i % cols);
                let bbox = self.place_bbox(part);
                let w = if bbox.w() > 0.0 { bbox.w() } else { 200.0 };
                let h = if bbox.h() > 0.0 { bbox.h() } else { 200.0 };
                self.c.parts[part].tx = Tx::IDENT.move_by(Point::new(col as f64 * w * 1.2, row as f64 * h * 1.2));
            }
            return;
        }

        self.add_placement_bboxes(parts, 1.0);
        self.add_anchor_pull_pins(parts, &[]);

        // Parts that are alike attract each other, which collects them
        // together (bypass capacitors, say).
        let n = parts.len();
        let mut sim = vec![vec![0.0; n]; n];
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    // (Divided down to get the forces right-sized.)
                    sim[i][j] = self.c.similarity(parts[i], parts[j]) / 100.0;
                }
            }
        }

        let mut forces = self.part_forces(&[], parts, Some(sim.clone()));
        forces.random_placement(&mut self.rng);
        if self.opts.compress_before_place {
            forces.central_placement();
        }
        for (i, &part) in parts.iter().enumerate() {
            self.c.parts[part].tx = forces.bodies[i].tx;
        }
        self.evolve_placement(&[], parts, &[], Some(sim.clone()));

        // Floating parts have all their pins on labeled nets, so they're
        // turned to keep those labels off their bodies and each other.
        if self.opts.rotate_parts && self.adjust_orientations(parts, true) {
            self.evolve_placement(&[], parts, &[], Some(sim));
        }
    }

    /// The point of a node that's put on the grid, if it has one.
    fn node_snap_pt(&self, node: NodeId) -> Option<Point> {
        if !self.nodes[node].flattened {
            return None;
        }
        for &part in &self.nodes[node].parts {
            if let Some(pt) = self.part_snap_pt(part) {
                return Some(pt);
            }
        }
        for &child in self.nodes[node].children.values() {
            if self.nodes[child].flattened {
                if let Some(pt) = self.node_snap_pt(child) {
                    return Some(pt * self.nodes[child].tx);
                }
            }
        }
        None
    }

    /// Arrange the blocks of placed parts and the child nodes of a node.
    fn place_blocks(&mut self, node: NodeId, connected_parts: &[Vec<PartId>], floating_parts: &[PartId]) {
        let mut blocks: Vec<PartBlock> = Vec::new();

        // Each group of connected parts is a block, and the floating parts are one.
        let mut lists: Vec<(Vec<PartId>, usize)> = connected_parts.iter().map(|g| (g.clone(), 1)).collect();
        lists.push((floating_parts.to_vec(), 2));
        for (part_list, tag) in lists {
            if part_list.is_empty() {
                continue;
            }
            let mut snap_pt = None;
            let mut bbox = BBox::empty();
            for &part in &part_list {
                bbox.add(self.c.parts[part].lbl_bbox * self.c.parts[part].tx);
                if snap_pt.is_none() {
                    snap_pt = self.part_snap_pt(part);
                }
            }
            // The box is padded so blocks don't butt up against each other.
            let bbox = bbox.resize(Point::new(BLK_EXT_PAD, BLK_EXT_PAD));
            blocks.push(PartBlock { src: BlockSrc::Parts(part_list), bbox, snap_pt: snap_pt.unwrap_or(bbox.ctr()), tag });
        }

        for child in self.children(node) {
            let bbox = self.calc_bbox(child);
            // A flattened node shows its parts, so it's padded inside and
            // outside the box drawn around them.
            let pad = if self.nodes[child].flattened { BLK_INT_PAD + BLK_EXT_PAD } else { BLK_EXT_PAD };
            let bbox = bbox.resize(Point::new(pad, pad));
            let (snap_pt, tag) = match self.node_snap_pt(child) {
                Some(pt) => (pt, 3),
                None => (bbox.ctr(), 4),
            };
            blocks.push(PartBlock { src: BlockSrc::Node(child), bbox, snap_pt, tag });
        }
        if blocks.is_empty() {
            return;
        }

        let n = blocks.len();
        let mut txs = vec![Tx::IDENT; n];
        if n > ROW_PLACE_THRESHOLD {
            // A simple grid for a large number of blocks.
            let cols = ((n as f64).sqrt() as usize).max(1);
            for (i, blk) in blocks.iter().enumerate() {
                let (row, col) = (i / cols, i % cols);
                let w = if blk.bbox.w() > 0.0 { blk.bbox.w() } else { 500.0 };
                let h = if blk.bbox.h() > 0.0 { blk.bbox.h() } else { 500.0 };
                let tx = Tx::IDENT.move_by(Point::new(col as f64 * (w + BLK_EXT_PAD), row as f64 * (h + BLK_EXT_PAD)));
                txs[i] = snap_tx(tx, blk.snap_pt);
            }
        } else {
            // Blocks with the same tag attract strongly, and those with
            // adjacent tags weakly. That ties similar blocks together and
            // strings the kinds of blocks out in a line.
            let mut tags: Vec<usize> = blocks.iter().map(|b| b.tag).collect();
            tags.sort();
            tags.dedup();
            let rank = |tag: usize| tags.iter().position(|&t| t == tag).unwrap() as i64;
            let mut sim = vec![vec![0.0; n]; n];
            for i in 0..n {
                for j in 0..n {
                    if i == j {
                        continue;
                    }
                    if blocks[i].tag == blocks[j].tag {
                        sim[i][j] = 1.0;
                    } else if (rank(blocks[i].tag) - rank(blocks[j].tag)).abs() == 1 {
                        sim[i][j] = 0.1;
                    }
                }
            }
            let mut forces = Forces {
                bodies: blocks.iter().map(|b| Body { tx: Tx::IDENT, bbox: b.bbox }).collect(),
                n_anchored: 0,
                pins: blocks.iter().enumerate().map(|(i, b)| FPin { body: Some(i), pt: b.bbox.ctr() }).collect(),
                attraction: Attraction::Similarity { anchors: (0..n).collect(), pulls: (0..n).collect(), sim },
                pin_normalize: self.opts.pin_normalize,
                use_push_pull: self.opts.use_push_pull,
            };
            forces.random_placement(&mut self.rng);
            forces.push_and_pull(&mut self.rng);
            for (i, blk) in blocks.iter().enumerate() {
                txs[i] = snap_tx(forces.bodies[i].tx, blk.snap_pt);
            }
        }

        // Apply the moves of the blocks to what they hold.
        for (blk, tx) in blocks.iter().zip(txs) {
            match &blk.src {
                BlockSrc::Node(child) => self.nodes[*child].tx = tx,
                BlockSrc::Parts(parts) => {
                    for &part in parts {
                        self.c.parts[part].tx = self.c.parts[part].tx * tx;
                    }
                }
            }
        }
    }

    /// Remove what placement left on the parts and pins of a node.
    fn rmv_placement_stuff(&mut self, node: NodeId) {
        for part in self.nodes[node].parts.clone() {
            for pin in self.c.parts[part].pins.clone() {
                self.c.pins[pin].place_pt = None;
            }
            let pl = &mut self.c.parts[part].pl;
            pl.anchor_pins = IndexMap::new();
            pl.pull_pins = IndexMap::new();
            pl.pin_ctrs = IndexMap::new();
        }
    }

    fn stub_net(&mut self, net: NetId) {
        self.c.nets[net].stub = true;
        self.c.nets[net].stub_explicit = false;
        for pin in self.c.nets[net].pins.clone() {
            self.c.pins[pin].stub = true;
        }
    }

    /// With auto-stubbing, stub the nets that span more than one placement group.
    fn auto_stub_cross_group(&mut self, node: NodeId, groups: &[Vec<PartId>]) {
        let mut part_to_group: HashMap<PartId, usize> = HashMap::new();
        for (i, group) in groups.iter().enumerate() {
            for &part in group {
                part_to_group.insert(part, i);
            }
        }
        for part in self.nodes[node].parts.clone() {
            for pin in self.c.parts[part].pins.clone() {
                if !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if self.c.nets[net].stub_explicit || self.c.nets[net].stub {
                    continue;
                }
                let pin_groups: HashSet<usize> =
                    self.c.nets[net].pins.iter().filter_map(|&p| part_to_group.get(&self.c.pins[p].part).copied()).collect();
                if pin_groups.len() > 1 {
                    self.stub_net(net);
                }
            }
        }
    }

    /// With auto-stubbing, split oversized placement groups by stubbing some
    /// of the nets that chain their parts together.
    fn auto_stub_large_groups(&mut self, groups: &[Vec<PartId>], internal_nets: &[NetId]) {
        const MAX_GROUP: usize = 20;
        for group in groups {
            if group.len() <= MAX_GROUP {
                continue;
            }
            let in_group: HashSet<PartId> = group.iter().copied().collect();
            let mut chain_nets: Vec<(usize, NetId)> = Vec::new();
            for &net in internal_nets {
                if self.c.nets[net].stub_explicit || self.c.nets[net].stub {
                    continue;
                }
                let net_parts: HashSet<PartId> =
                    self.c.nets[net].pins.iter().map(|&p| self.c.pins[p].part).filter(|p| in_group.contains(p)).collect();
                if (2..=4).contains(&net_parts.len()) {
                    chain_nets.push((net_parts.len(), net));
                }
            }
            // Nets with the fewest parts are stubbed first.
            chain_nets.sort_by_key(|(n, _)| *n);
            if chain_nets.is_empty() {
                continue;
            }
            let n_cuts = ((group.len() + MAX_GROUP - 1) / MAX_GROUP).max(1);
            let step = (chain_nets.len() / (n_cuts + 1)).max(1);
            let mut i = step;
            while i < chain_nets.len() {
                self.stub_net(chain_nets[i].1);
                i += step;
            }
        }
    }

    /// Place the parts and children of a node.
    pub fn place(&mut self, node: NodeId) -> Res<()> {
        let result = self.place_node(node);
        if result.is_err() {
            self.rmv_placement_stuff(node);
        }
        result
    }

    fn place_node(&mut self, node: NodeId) -> Res<()> {
        // The children are placed first.
        for child in self.children(node) {
            self.place(child)?;
        }
        let name = if self.nodes[node].name.is_empty() { "top sheet".to_string() } else { self.nodes[node].name.clone() };
        progress::report(&format!("placing {}", name));

        // Group the parts into those connected by nets and those that float free.
        let (mut connected, mut internal_nets, mut floating) = self.group_parts(node);
        if self.opts.auto_stub {
            self.auto_stub_cross_group(node, &connected);
            self.auto_stub_large_groups(&connected, &internal_nets);
            // Stubbing may have changed what's connected.
            (connected, internal_nets, floating) = self.group_parts(node);
        }

        for group in &connected {
            self.place_connected_parts(group, &internal_nets);
        }
        self.place_floating_parts(&floating);
        self.place_blocks(node, &connected, &floating);

        self.rmv_placement_stuff(node);
        self.calc_bbox(node);

        // Forces that don't settle can throw groups of parts billions of mils
        // apart, which the router would then try to cover with tracks.
        let extent = self.internal_bbox(node);
        if !(extent.w().max(extent.h()) <= MAX_SHEET_SIZE) {
            return Err(Error::Placement(format!("Placement of {} didn't settle", self.nodes[node].name)));
        }
        Ok(())
    }
}

