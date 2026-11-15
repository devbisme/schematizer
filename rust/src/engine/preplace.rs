//! Put parts where a person would, instead of letting forces pull them in
//! from random starting points.
//!
//! * Satellites: a two-pin part that hangs off the rest of the circuit by one
//!   pin (a pull-up, a series resistor, an LED with its resistor) is fixed
//!   beside the pin it's wired to. Its host carries it through placement.
//! * Seed: each remaining part goes on the side of the pin it's wired to, with
//!   the two pins level so the wire between them is straight.
//! * Spread: seeded parts that landed on each other are moved apart.
//!
//! Once the parts are down, each net label is put beside a pin of its net.
//!
//! Parts with no wires at all (a bank of decoupling caps whose pins all go to
//! power symbols) are put in tidy rows, grouped by the rails they're on.

use std::collections::{HashMap, HashSet};

use crate::geometry::{pyround, BBox, Point, Tx};
use crate::ir::{NetId, PartId, PinId};

use super::constants::GRID;
use super::conventions::{best_orientations, is_ground_net_name, linear, orientation_cost, orientations, pin_exit};
use super::Design;

// Gap between parts in the rows of parts with no wires, and how many times
// wider than tall the rows are made.
const FLOAT_GAP: f64 = 2.0 * GRID;
const FLOAT_ASPECT: f64 = 4.0;

// Size (mils) of the reference and value text drawn beside a part: the gap
// between it and the part, the width of a character, and the line pitch.
const FIELD_GAP: f64 = 35.0;
const FIELD_CHAR_W: f64 = 30.0;
const FIELD_PITCH: f64 = 50.0;

/// Gap left between the placement boxes of two parts joined by a straight wire.
const SEED_GAP: f64 = 2.0 * GRID;
/// Most pins a net can have and still have every pair of them pull together.
const SEED_MAX_CLIQUE: usize = 10;

// How far (in grid steps) a part can be moved at a time to get it off of
// other parts, how far out the search goes if that isn't enough, and the wire
// length a bend or a wire doubling back is worth when choosing where it goes.
const SPREAD_MAX_STEPS: i64 = 20;
const SPREAD_MAX_RADIUS: i64 = 1000;
const SPREAD_BEND_COST: f64 = 4.0 * GRID;
const SPREAD_UTURN_COST: f64 = 8.0 * GRID;
/// Times each part gets to move again after all the parts are down.
const SPREAD_PASSES: usize = 2;

// Search limits (in grid steps) for a spot beside a pin.
const SAT_MAX_OUT: i64 = 40;
const SAT_MAX_SIDE: i64 = 10;

// Wire length (in grid steps) that a satellite's wire is penalized for
// stepping sideways, for each bend, and for doubling back on itself.
const SAT_SIDE_COST: i64 = 3;
const SAT_BEND_COST: i64 = 2;
const SAT_UTURN_COST: i64 = 8;

/// Wire length (in grid steps) a satellite is penalized per unit of orientation cost.
const SAT_CONVENTION_COST: f64 = 1.0;

/// Space kept between a satellite and other parts so the router can get a wire through.
const SAT_CLEARANCE: f64 = GRID;
/// Space a wire needs between it and a part, or the router has no track to put it on.
const WIRE_CLEARANCE: f64 = GRID / 2.0;
/// Extra space left around a part that's put in open space because there was no room beside a pin.
const FALLBACK_CLEARANCE: f64 = 2.0 * GRID;
/// Length of the corridor kept clear where a wire leaves a part.
const SAT_EXIT_CLEARANCE: f64 = 12.0 * GRID;
/// The same for net labels, which only need to stay off the ends of other pins.
const TERMINAL_EXIT_CLEARANCE: f64 = 3.0 * GRID;
/// Space left between a net label and the pin it's beside when other pins on
/// the net have to be wired to that pin too.
const TERMINAL_JOIN_GAP: f64 = 2.0 * GRID;

/// A satellite, its wired pin, and the pin it's wired to.
pub type Satellite = (PartId, PinId, PinId);

/// Invert a transformation made of rotations, flips and a translation.
fn inverse(tx: Tx) -> Tx {
    let lin = Tx::lin(tx.a, tx.c, tx.b, tx.d); // The transpose undoes a rotation or flip.
    let back = Point::new(-tx.dx, -tx.dy) * lin;
    lin.move_by(back)
}

fn grow(bbox: BBox, amount: f64) -> BBox {
    BBox::new(bbox.min - Point::new(amount, amount), bbox.max + Point::new(amount, amount))
}

fn unit(v: Point) -> Point {
    Point::new(pyround(v.x), pyround(v.y))
}

/// Where a part's pin could go relative to the pin it's wired to.
///
/// `out` is the direction a wire leaves the pin it's wired to and `exit_vec`
/// the direction a wire leaves the part's own pin. Returns (wire cost, offset
/// is leftward, offset is downward, offset in grid steps, corner of the wire
/// in grid steps), cheapest first.
#[allow(clippy::type_complexity)]
fn spot_offsets(out: (i64, i64), exit_vec: (i64, i64)) -> Vec<(i64, bool, bool, (i64, i64), Option<(i64, i64)>)> {
    let side = (-out.1, out.0);
    let facing = exit_vec.0 * out.0 + exit_vec.1 * out.1;
    let mut offsets: Vec<(i64, (i64, i64), Option<(i64, i64)>)> = Vec::new();
    if facing < 0 {
        // The pins face each other: a straight wire, or a jog if stepped sideways.
        for k in 2..=SAT_MAX_OUT {
            for j in -SAT_MAX_SIDE..=SAT_MAX_SIDE {
                offsets.push((k + SAT_SIDE_COST * j.abs(), (out.0 * k + side.0 * j, out.1 * k + side.1 * j), None));
            }
        }
    } else if facing == 0 {
        // The pins are at right angles: the wire goes out, then turns into the part.
        for k in 1..=SAT_MAX_OUT {
            for m in 1..=SAT_MAX_SIDE {
                offsets.push((
                    k + m + SAT_BEND_COST,
                    (out.0 * k - exit_vec.0 * m, out.1 * k - exit_vec.1 * m),
                    Some((out.0 * k, out.1 * k)),
                ));
            }
        }
    } else {
        // The pins point the same way: the wire has to double back.
        for k in 0..=SAT_MAX_OUT {
            for j in -SAT_MAX_SIDE..=SAT_MAX_SIDE {
                if j.abs() >= 2 {
                    offsets.push((k + j.abs() + SAT_UTURN_COST, (out.0 * k + side.0 * j, out.1 * k + side.1 * j), None));
                }
            }
        }
    }
    // Ties break the same way every time: positive side first.
    let mut keyed: Vec<_> =
        offsets.into_iter().enumerate().map(|(n, (cost, off, corner))| (cost, off.0 < 0, off.1 < 0, n, off, corner)).collect();
    keyed.sort_by_key(|e| (e.0, e.1, e.2, e.3));
    keyed.into_iter().map(|(cost, left, down, _, off, corner)| (cost, left, down, off, corner)).collect()
}

/// Who a keepout box belongs to. Its owner is allowed to touch it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Owner {
    Part(PartId),
    Pin(PinId),
}

/// The boxes that parts placed beside pins, and their wires, have to keep out of.
///
/// Parts keep their distance from other parts so the router can get wires
/// between them. A wire can run closer to a part than that.
struct Keepouts {
    clearance: f64,
    exit_clearance: f64,
    for_parts: Vec<(Owner, BBox)>,
    for_wires: Vec<(Owner, BBox)>,
    #[allow(clippy::type_complexity)]
    offsets: HashMap<((i64, i64), (i64, i64)), Vec<(i64, bool, bool, (i64, i64), Option<(i64, i64)>)>>,
}

impl Keepouts {
    fn new(clearance: f64, exit_clearance: f64) -> Keepouts {
        Keepouts { clearance, exit_clearance, for_parts: Vec::new(), for_wires: Vec::new(), offsets: HashMap::new() }
    }

    fn add_part(&mut self, part: PartId, bbox: BBox) {
        self.for_parts.push((Owner::Part(part), grow(bbox, self.clearance)));
        self.for_wires.push((Owner::Part(part), grow(bbox, WIRE_CLEARANCE)));
    }

    fn add_wire(&mut self, pin: PinId, bbox: BBox) {
        self.for_parts.push((Owner::Pin(pin), bbox));
        self.for_wires.push((Owner::Pin(pin), bbox));
    }

    /// Keep clear of where a wire leaves a pin and passes its part's labels.
    fn add_exit(&mut self, d: &Design, pin: PinId, tx: Tx) {
        let pt = d.c.pins[pin].pt * tx;
        let out = unit(pin_exit(&d.c, pin, tx));
        let bbox = d.c.pin_part(pin).lbl_bbox * tx;
        let reach = ((bbox.max.x - pt.x) * out.x)
            .max((bbox.min.x - pt.x) * out.x)
            .max((bbox.max.y - pt.y) * out.y)
            .max((bbox.min.y - pt.y) * out.y);
        let tip = pt + out * (reach + self.exit_clearance);
        self.add_wire(pin, grow(BBox::new(pt, tip), GRID / 2.0));
    }

    /// Find the best free spot for a part beside the pin it's wired to, and take it.
    ///
    /// `lins` are the orientations the part can have, `same_net` the other
    /// pins whose wires the part can share the way with, and `min_gap` the
    /// space to leave between the pin and the part so other wires can join
    /// this one. Orientations that break the drafting conventions cost extra,
    /// so a decoupling cap stays upright unless that pushes it far from its pin.
    #[allow(clippy::too_many_arguments)]
    fn find_spot(
        &mut self,
        d: &Design,
        part: PartId,
        pin: PinId,
        host_pin: PinId,
        host_tx: Tx,
        lins: &[Tx],
        same_net: &[PinId],
        min_gap: f64,
    ) -> Option<Tx> {
        let host_pt = d.c.pins[host_pin].pt * host_tx;
        let out = unit(pin_exit(&d.c, host_pin, host_tx));
        let out_key = (out.x as i64, out.y as i64);
        let host_part = d.c.pins[host_pin].part;
        let pin_pt = d.c.pins[pin].pt;
        let lbl_bbox = d.c.parts[part].lbl_bbox;

        // The spots for every orientation, best first.
        let costs: Vec<i64> = lins.iter().map(|&lin| orientation_cost(&d.c, part, lin)).collect();
        let least_broken = costs.iter().copied().min().unwrap_or(0);
        let mut spots: Vec<(f64, bool, bool, usize, usize, (i64, i64), Option<(i64, i64)>)> = Vec::new();
        for (turn, &lin) in lins.iter().enumerate() {
            let broken = (costs[turn] - least_broken) as f64 * SAT_CONVENTION_COST;
            let exit_vec = unit(pin_exit(&d.c, pin, lin));
            let key = (out_key, (exit_vec.x as i64, exit_vec.y as i64));
            let offsets = self.offsets.entry(key).or_insert_with(|| spot_offsets(key.0, key.1));
            for (n, &(cost, left, down, offset, corner)) in offsets.iter().enumerate() {
                spots.push((cost as f64 + broken, left, down, turn, n, offset, corner));
            }
        }
        spots.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| (a.1, a.2, a.3, a.4).cmp(&(b.1, b.2, b.3, b.4))));

        let allowed = |owner: &Owner| *owner == Owner::Pin(host_pin) || matches!(owner, Owner::Pin(p) if same_net.contains(p));
        for (_, _, _, turn, _, offset, corner) in spots {
            let lin = lins[turn];
            let shift = host_pt + Point::new(offset.0 as f64, offset.1 as f64) * GRID - pin_pt * lin;
            let tx = lin.move_by(shift);
            let bbox = lbl_bbox * tx;
            if min_gap != 0.0 && gap_ahead(host_pt, out, &bbox) < min_gap {
                continue;
            }
            let mut wire = BBox::new(host_pt, pin_pt * tx);
            if let Some(c) = corner {
                wire.add_pt(host_pt + Point::new(c.0 as f64, c.1 as f64) * GRID);
            }
            let wire = grow(wire, 1.0);
            // The wire has to touch the part it comes from and can share the
            // way with other wires from the same pin, but nothing else.
            if self.for_parts.iter().any(|(owner, b)| !allowed(owner) && bbox.intersects(b)) {
                continue;
            }
            if self
                .for_wires
                .iter()
                .any(|(owner, b)| !allowed(owner) && *owner != Owner::Part(host_part) && wire.intersects(b))
            {
                continue;
            }
            self.add_part(part, bbox);
            self.add_wire(host_pin, wire);
            return Some(tx);
        }
        None
    }

    /// Find the nearest free spot for a part around a point, and take it.
    ///
    /// This is for a part there's no spot for beside a pin. It only finds
    /// open space; the part's wire is left for the router to work out.
    fn find_room(&mut self, d: &Design, part: PartId, pin: PinId, near: Point, lins: &[Tx]) -> Option<Tx> {
        let near = near.snap(GRID);
        let pin_pt = d.c.pins[pin].pt;
        let lbl_bbox = d.c.parts[part].lbl_bbox;
        for radius in 1..SPREAD_MAX_RADIUS {
            for (dx, dy) in ring(radius) {
                for &lin in lins {
                    let tx = lin.move_by(near + Point::new(dx, dy) - pin_pt * lin);
                    let bbox = lbl_bbox * tx;
                    // Extra room is left all around, and the wires leaving
                    // pins are kept clear, so the router can get to it.
                    let roomy = grow(bbox, FALLBACK_CLEARANCE);
                    if !self.for_parts.iter().chain(self.for_wires.iter()).any(|(_, b)| roomy.intersects(b)) {
                        self.add_part(part, bbox);
                        return Some(tx);
                    }
                }
            }
        }
        None
    }
}

/// Distance from a point to a box, measured in the direction out from the point.
fn gap_ahead(pt: Point, out: Point, bbox: &BBox) -> f64 {
    let dot = |corner: Point| (corner - pt).x * out.x + (corner - pt).y * out.y;
    dot(bbox.min).min(dot(bbox.max))
}

/// Order for trying moves: nearest first, then positive side first.
fn move_key(v: (i64, i64)) -> (i64, bool, bool, i64) {
    (v.0.abs() + v.1.abs(), v.1 < 0, v.0 < 0, v.0.abs())
}

/// Moves (mils) to the grid points on a square this many grid steps out.
fn ring(radius: i64) -> Vec<(f64, f64)> {
    let mut edge: Vec<(i64, i64)> = Vec::new();
    for i in -radius..=radius {
        for j in [-radius, radius] {
            edge.push((i, j));
        }
    }
    for i in [-radius, radius] {
        for j in (-radius + 1)..radius {
            edge.push((i, j));
        }
    }
    edge.sort_by_key(|&v| move_key(v));
    edge.into_iter().map(|(i, j)| (i as f64 * GRID, j as f64 * GRID)).collect()
}

/// The net-label orientations: one for each direction its wire can leave it.
fn terminal_lins() -> [Tx; 4] {
    [Tx::IDENT, Tx::IDENT.flip_x(), Tx::IDENT.rot_90cw(), Tx::IDENT.rot_90cw().rot_90cw().rot_90cw()]
}

/// Solve the least-squares problem of the seed placement.
///
/// Each row says that the difference between the positions of two parts
/// should be `want`, with a weight. Returns the positions that come closest,
/// with each connected set of parts centered on the origin (the least-norm solution).
fn solve_positions(n: usize, rows: &[(usize, usize, f64, (f64, f64))]) -> Vec<(f64, f64)> {
    // Normal equations: a weighted graph Laplacian.
    let mut lap = vec![vec![0.0f64; n]; n];
    let mut rhs = vec![(0.0f64, 0.0f64); n];
    let mut comp: Vec<usize> = (0..n).collect();
    fn find(comp: &mut [usize], mut i: usize) -> usize {
        while comp[i] != i {
            comp[i] = comp[comp[i]];
            i = comp[i];
        }
        i
    }
    for &(a, b, w, want) in rows {
        let w2 = w * w;
        lap[a][a] += w2;
        lap[b][b] += w2;
        lap[a][b] -= w2;
        lap[b][a] -= w2;
        // The row is w * (x_b - x_a) = w * want.
        rhs[a].0 -= w2 * want.0;
        rhs[a].1 -= w2 * want.1;
        rhs[b].0 += w2 * want.0;
        rhs[b].1 += w2 * want.1;
        let (ra, rb) = (find(&mut comp, a), find(&mut comp, b));
        if ra != rb {
            comp[ra] = rb;
        }
    }

    let mut pos = vec![(0.0, 0.0); n];
    let roots: Vec<usize> = (0..n).map(|i| find(&mut comp, i)).collect();
    let mut done: HashSet<usize> = HashSet::new();
    for &root in &roots {
        if !done.insert(root) {
            continue;
        }
        let members: Vec<usize> = (0..n).filter(|&i| roots[i] == root).collect();
        if members.len() < 2 {
            continue;
        }
        // Fix the first member at zero and solve for the rest by elimination.
        let free = &members[1..];
        let m = free.len();
        let mut a: Vec<Vec<f64>> = free.iter().map(|&i| free.iter().map(|&j| lap[i][j]).collect()).collect();
        let mut bx: Vec<f64> = free.iter().map(|&i| rhs[i].0).collect();
        let mut by: Vec<f64> = free.iter().map(|&i| rhs[i].1).collect();
        for col in 0..m {
            let mut pivot = col;
            for r in col + 1..m {
                if a[r][col].abs() > a[pivot][col].abs() {
                    pivot = r;
                }
            }
            if a[pivot][col].abs() < 1e-12 {
                continue;
            }
            a.swap(col, pivot);
            bx.swap(col, pivot);
            by.swap(col, pivot);
            for r in col + 1..m {
                let factor = a[r][col] / a[col][col];
                if factor != 0.0 {
                    for c in col..m {
                        a[r][c] -= factor * a[col][c];
                    }
                    bx[r] -= factor * bx[col];
                    by[r] -= factor * by[col];
                }
            }
        }
        let mut x = vec![0.0; m];
        let mut y = vec![0.0; m];
        for r in (0..m).rev() {
            if a[r][r].abs() < 1e-12 {
                continue;
            }
            let (mut sx, mut sy) = (bx[r], by[r]);
            for c in r + 1..m {
                sx -= a[r][c] * x[c];
                sy -= a[r][c] * y[c];
            }
            x[r] = sx / a[r][r];
            y[r] = sy / a[r][r];
        }
        let mean_x = x.iter().sum::<f64>() / members.len() as f64;
        let mean_y = y.iter().sum::<f64>() / members.len() as f64;
        pos[members[0]] = (-mean_x, -mean_y);
        for (k, &i) in free.iter().enumerate() {
            pos[i] = (x[k] - mean_x, y[k] - mean_y);
        }
    }
    pos
}

impl Design {
    /// True if a pin gets a wire: connected, not stubbed, and on one of the nets being placed.
    fn is_wired(&self, pin: PinId, nets: &HashSet<NetId>) -> bool {
        self.c.is_connected(pin) && !self.c.pins[pin].stub && self.c.pins[pin].net.is_some_and(|n| nets.contains(&n))
    }

    fn wired_pins(&self, part: PartId, nets: &HashSet<NetId>) -> Vec<PinId> {
        self.c.parts[part].pins.iter().copied().filter(|&p| self.is_wired(p, nets)).collect()
    }

    /// Find two-pin parts that can ride along beside the pin they're wired to.
    ///
    /// A satellite has one pin wired toward the rest of the circuit. Its other
    /// pin is unwired, or leads only to its own satellites (so an LED and its
    /// series resistor hang off an IC pin as a chain). Returns each host with
    /// its satellites, each of which comes after the one it's wired to.
    pub fn find_satellites(&self, parts: &[PartId], nets: &[NetId]) -> Vec<(PartId, Vec<Satellite>)> {
        let nets: HashSet<NetId> = nets.iter().copied().collect();
        let part_set: HashSet<PartId> = parts.iter().copied().collect();
        // Satellite -> (its wired pin, the pin it's wired to).
        let mut wired_to: HashMap<PartId, (PinId, PinId)> = HashMap::new();

        // Wired pins on a pin's net that are on parts still in the circuit.
        let other_pins = |pin: PinId, wired_to: &HashMap<PartId, (PinId, PinId)>| -> Vec<PinId> {
            let own = self.c.pins[pin].part;
            self.c.nets[self.c.pins[pin].net.unwrap()]
                .pins
                .iter()
                .copied()
                .filter(|&p| {
                    let part = self.c.pins[p].part;
                    part != own && part_set.contains(&part) && !wired_to.contains_key(&part) && self.is_wired(p, &nets)
                })
                .collect()
        };

        // Strip satellites off the circuit until no more can be found.
        // Removing one can turn the part it was wired to into a satellite.
        let mut found = true;
        while found {
            found = false;
            for &part in parts {
                let p = &self.c.parts[part];
                if wired_to.contains_key(&part) || p.pins.len() != 2 || p.orientation_locked {
                    continue;
                }
                let live: Vec<PinId> =
                    self.wired_pins(part, &nets).into_iter().filter(|&pin| !other_pins(pin, &wired_to).is_empty()).collect();
                if live.len() != 1 {
                    continue;
                }
                let others = other_pins(live[0], &wired_to);
                // Parts on the same pin of an IC (a pull-up and a filter cap,
                // say) are satellites of the IC, not of each other.
                let ics: Vec<PinId> = others.iter().copied().filter(|&o| self.c.pin_part(o).pins.len() > 2).collect();
                let hosts = if ics.is_empty() { others } else { ics };
                if hosts.len() == 1 {
                    wired_to.insert(part, (live[0], hosts[0]));
                    found = true;
                }
            }
        }

        let host_of = |mut part: PartId| {
            while let Some(&(_, host_pin)) = wired_to.get(&part) {
                part = self.c.pins[host_pin].part;
            }
            part
        };
        let mut satellites: Vec<(PartId, Vec<Satellite>)> = Vec::new();
        let mut done: HashSet<PartId> = HashSet::new();
        fn add(
            sat: PartId,
            d: &Design,
            wired_to: &HashMap<PartId, (PinId, PinId)>,
            done: &mut HashSet<PartId>,
            satellites: &mut Vec<(PartId, Vec<Satellite>)>,
            host_of: &dyn Fn(PartId) -> PartId,
        ) {
            if !done.insert(sat) {
                return;
            }
            let (sat_pin, host_pin) = wired_to[&sat];
            let host_part = d.c.pins[host_pin].part;
            if wired_to.contains_key(&host_part) {
                add(host_part, d, wired_to, done, satellites, host_of);
            }
            let host = host_of(sat);
            match satellites.iter_mut().find(|(h, _)| *h == host) {
                Some((_, list)) => list.push((sat, sat_pin, host_pin)),
                None => satellites.push((host, vec![(sat, sat_pin, host_pin)])),
            }
        }
        for &part in parts {
            if wired_to.contains_key(&part) {
                add(part, self, &wired_to, &mut done, &mut satellites, &host_of);
            }
        }
        satellites
    }

    /// Fix each satellite beside the pin it's wired to and grow the host to cover them.
    ///
    /// Each satellite gets a `rel_tx` that maps it into its host's
    /// coordinates, so after placement its transformation is `rel_tx * host.tx`.
    /// Returns the satellites that were attached. One that there was no room
    /// for is left out (with any wired to it), to be placed like any other part.
    pub fn attach_satellites(&mut self, host: PartId, satellites: &[Satellite], nets: &[NetId]) -> Vec<Satellite> {
        let nets: HashSet<NetId> = nets.iter().copied().collect();
        let mut keepouts = Keepouts::new(SAT_CLEARANCE, SAT_EXIT_CLEARANCE);

        // Everything is laid out around the host as it's oriented now, with
        // the host's origin at (0, 0).
        let host_lin = linear(self.c.parts[host].tx);
        let to_host = inverse(host_lin);
        let mut txs: HashMap<PartId, Tx> = HashMap::from([(host, host_lin)]);

        keepouts.add_part(host, self.c.parts[host].lbl_bbox * host_lin);
        for pin in self.wired_pins(host, &nets) {
            keepouts.add_exit(self, pin, host_lin);
        }

        let host_bbox = self.place_bbox(host);
        self.c.parts[host].pl.preplace_bbox = Some(host_bbox);
        let mut bbox = host_bbox;
        self.c.parts[host].pl.riders = Vec::new();
        let mut attached = Vec::new();
        for &(sat, sat_pin, host_pin) in satellites {
            // If what it's wired to didn't fit, it has nothing to sit beside.
            let Some(&host_pin_tx) = txs.get(&self.c.pins[host_pin].part) else { continue };
            let lins = orientations(self.c.parts[sat].tx);
            let Some(tx) = keepouts.find_spot(self, sat, sat_pin, host_pin, host_pin_tx, &lins, &[], 0.0) else {
                continue;
            };
            txs.insert(sat, tx);
            self.c.parts[sat].pl.rel_tx = tx * to_host;
            self.c.parts[host].pl.riders.push(sat);
            attached.push((sat, sat_pin, host_pin));
            for pin in self.wired_pins(sat, &nets) {
                if pin != sat_pin {
                    keepouts.add_exit(self, pin, tx);
                }
            }
            bbox.add(self.place_bbox(sat) * self.c.parts[sat].pl.rel_tx);
        }
        self.c.parts[host].place_bbox = Some(bbox);
        attached
    }

    /// Put satellites where their placed hosts carried them, and shrink the hosts.
    pub fn release_satellites(&mut self, hosts: &[(PartId, Vec<Satellite>)]) {
        for (host, satellites) in hosts {
            let pl = &mut self.c.parts[*host].pl;
            let bbox = pl.preplace_bbox.take();
            pl.riders = Vec::new();
            self.c.parts[*host].place_bbox = bbox;
            let host_tx = self.c.parts[*host].tx;
            for &(sat, _, _) in satellites {
                self.c.parts[sat].tx = self.c.parts[sat].pl.rel_tx * host_tx;
                self.c.parts[sat].pl.rel_tx = Tx::IDENT;
            }
        }
    }

    /// Put each net label beside one of the pins of its net.
    ///
    /// The label goes straight out from a pin if there's room, so its wire is
    /// short. Pins nearer the outside of the placed parts are tried first. If
    /// there's no room beside any of them, it goes in the nearest open space.
    /// Returns the terminals that couldn't be placed.
    pub fn place_terminals(&mut self, terminals: &[PartId], parts: &[PartId], nets: &[NetId]) -> Vec<PartId> {
        if terminals.is_empty() {
            return Vec::new();
        }
        let net_set: HashSet<NetId> = nets.iter().copied().collect();
        let part_set: HashSet<PartId> = parts.iter().copied().collect();
        let mut keepouts = Keepouts::new(SAT_CLEARANCE, TERMINAL_EXIT_CLEARANCE);
        let mut area = BBox::empty();
        for &part in parts {
            let tx = self.c.parts[part].tx;
            let bbox = self.c.parts[part].lbl_bbox * tx;
            keepouts.add_part(part, bbox);
            area.add(bbox);
            for pin in self.wired_pins(part, &net_set) {
                keepouts.add_exit(self, pin, tx);
            }
        }

        // How far inside the placed parts a wire leaving a pin starts.
        let inset = |d: &Design, pin: PinId| -> f64 {
            let tx = d.c.pin_part(pin).tx;
            let pt = d.c.pins[pin].pt * tx;
            let out = unit(pin_exit(&d.c, pin, tx));
            let inf = f64::INFINITY;
            (if out.x > 0.0 { area.max.x - pt.x } else { inf })
                .min(if out.x < 0.0 { pt.x - area.min.x } else { inf })
                .min(if out.y > 0.0 { area.max.y - pt.y } else { inf })
                .min(if out.y < 0.0 { pt.y - area.min.y } else { inf })
        };

        let mut unplaced = Vec::new();
        for &terminal in terminals {
            let pin = self.c.parts[terminal].pins[0];
            // An input or output label keeps its direction.
            let lins: Vec<Tx> = if self.c.parts[terminal].orientation_locked {
                vec![linear(self.c.parts[terminal].tx)]
            } else {
                terminal_lins().to_vec()
            };
            let Some(net) = self.c.pins[pin].net else {
                unplaced.push(terminal);
                continue;
            };
            let mut host_pins: Vec<PinId> = self.c.nets[net]
                .pins
                .iter()
                .copied()
                .filter(|&p| part_set.contains(&self.c.pins[p].part) && self.is_wired(p, &net_set))
                .collect();
            host_pins.sort_by(|&a, &b| inset(self, a).total_cmp(&inset(self, b)));
            let min_gap = if host_pins.len() > 1 { TERMINAL_JOIN_GAP } else { 0.0 };
            let mut tx = None;
            for &host_pin in &host_pins {
                let host_tx = self.c.pin_part(host_pin).tx;
                tx = keepouts.find_spot(self, terminal, pin, host_pin, host_tx, &lins, &host_pins, min_gap);
                if tx.is_some() {
                    break;
                }
            }
            if tx.is_none() && !host_pins.is_empty() {
                // No room beside any pin, so use the nearest open space to one.
                let near = self.c.pins[host_pins[0]].pt * self.c.pin_part(host_pins[0]).tx;
                tx = keepouts.find_room(self, terminal, pin, near, &lins);
            }
            match tx {
                Some(tx) => self.c.parts[terminal].tx = tx,
                None => unplaced.push(terminal),
            }
        }
        unplaced
    }

    /// Move parts to starting positions.
    ///
    /// For each pair of wired pins on different parts, the second part wants
    /// to be where a short, straight wire would join the pins: on the side of
    /// the first part that its pin is on, with the two pins level. Those wants
    /// can't all be met, so the positions that come closest (in the
    /// least-squares sense) are found with one linear solve. Parts can end up
    /// on top of each other; `spread_parts()` fixes that.
    pub fn seed_placement(&mut self, parts: &[PartId], nets: &[NetId]) {
        if parts.len() < 2 {
            return;
        }
        let index: HashMap<PartId, usize> = parts.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let net_set: HashSet<NetId> = nets.iter().copied().collect();
        let lins: Vec<Tx> = parts.iter().map(|&p| linear(self.c.parts[p].tx)).collect();

        let mut rows: Vec<(usize, usize, f64, (f64, f64))> = Vec::new();
        for &net in nets {
            let pins: Vec<PinId> = self.c.nets[net]
                .pins
                .iter()
                .copied()
                .filter(|&p| index.contains_key(&self.c.pins[p].part) && self.is_wired(p, &net_set))
                .collect();
            let num_parts = pins.iter().map(|&p| self.c.pins[p].part).collect::<HashSet<_>>().len();
            if num_parts < 2 {
                continue;
            }
            // A net with many parts shouldn't pull harder than one with two.
            let mut weight = (1.0 / (num_parts as f64 - 1.0)).sqrt();
            let mut pairs: Vec<(PinId, PinId)> = Vec::new();
            if pins.len() > SEED_MAX_CLIQUE {
                // Pairing every pin with every other gets out of hand for a
                // big net, so each is just paired with the next.
                pairs.extend(pins.windows(2).map(|w| (w[0], w[1])));
                weight = 1.0;
            } else {
                for i in 0..pins.len() {
                    for j in i + 1..pins.len() {
                        pairs.push((pins[i], pins[j]));
                    }
                }
            }
            for (pin_a, pin_b) in pairs {
                let (a, b) = (index[&self.c.pins[pin_a].part], index[&self.c.pins[pin_b].part]);
                if a == b {
                    continue;
                }
                let exit_a = unit(pin_exit(&self.c, pin_a, lins[a]));
                let exit_b = unit(pin_exit(&self.c, pin_b, lins[b]));
                // Part b goes in the direction pin a's wire leaves, which is
                // also the direction pin b's wire arrives from.
                let mut toward = exit_a - exit_b;
                if toward.magnitude() != 0.0 {
                    toward = toward / toward.magnitude();
                }
                let place_pt = |pin: PinId| self.c.pins[pin].place_pt.unwrap_or(self.c.pins[pin].pt);
                let want = place_pt(pin_a) * lins[a] - place_pt(pin_b) * lins[b] + toward * SEED_GAP;
                rows.push((a, b, weight, (want.x, want.y)));
            }
        }
        if rows.is_empty() {
            return;
        }
        let pos = solve_positions(parts.len(), &rows);
        for (i, &part) in parts.iter().enumerate() {
            self.c.parts[part].tx = lins[i].move_by(Point::new(pos[i].0, pos[i].1));
        }
    }

    /// A part's transformation, then that for each other orientation it's allowed.
    ///
    /// The part is turned about the center of its placement box and put back on the grid.
    fn turned(&self, part: PartId) -> Vec<Tx> {
        let p = &self.c.parts[part];
        let mut txs = vec![p.tx];
        if p.orientation_locked {
            return txs;
        }
        let now = linear(p.tx);
        let bbox = self.place_bbox(part);
        let ctr = bbox.ctr() * p.tx;
        for lin in best_orientations(&self.c, part) {
            if (lin.a, lin.b, lin.c, lin.d) == (now.a, now.b, now.c, now.d) {
                continue;
            }
            let tx = lin.move_by(ctr - bbox.ctr() * lin);
            let pt = self.c.pins[p.pins[0]].pt * tx;
            txs.push(tx.move_by(pt.snap(GRID) - pt));
        }
        txs
    }

    /// Find the free spot near a part's position that needs the least wiring.
    ///
    /// The wiring for a spot is estimated as the distance from each of the
    /// part's wired pins to the nearest pin it connects to, plus penalties if
    /// that wire needs a bend or has to double back behind its pin. Among
    /// equal spots, the order of the offsets makes the nearest one win.
    fn best_spot(
        &self,
        part: PartId,
        tx: Tx,
        offsets: &[(f64, f64)],
        partners: &[(PinId, Vec<Point>)],
        boxes: &[(f64, f64, f64, f64)],
    ) -> (f64, Option<Point>) {
        let bbox = self.place_bbox(part) * tx;
        let pins: Vec<(Point, Point, &Vec<Point>)> = partners
            .iter()
            .map(|(pin, others)| (self.c.pins[*pin].pt * tx, unit(pin_exit(&self.c, *pin, tx)), others))
            .collect();
        let mut best_cost = f64::INFINITY;
        let mut best = None;
        for &(x, y) in offsets {
            let (x1, y1, x2, y2) = (bbox.min.x + x, bbox.min.y + y, bbox.max.x + x, bbox.max.y + y);
            if boxes.iter().any(|b| x1 < b.2 && x2 > b.0 && y1 < b.3 && y2 > b.1) {
                continue;
            }
            let mut cost = 0.0;
            for (pt, out, others) in &pins {
                let mut least = f64::INFINITY;
                for o in others.iter() {
                    let (dx, dy) = (o.x - (pt.x + x), o.y - (pt.y + y));
                    let mut wire = dx.abs() + dy.abs();
                    if dx.abs() > 1.0 && dy.abs() > 1.0 {
                        wire += SPREAD_BEND_COST;
                    }
                    if dx * out.x + dy * out.y < -1.0 {
                        wire += SPREAD_UTURN_COST;
                    }
                    least = least.min(wire);
                }
                cost += least;
                if cost >= best_cost {
                    break;
                }
            }
            if cost < best_cost {
                best_cost = cost;
                best = Some(Point::new(x, y));
            }
        }
        (best_cost, best)
    }

    /// Move seeded parts off of each other and onto the grid.
    ///
    /// Parts are taken biggest first. Each goes to the free spot near its
    /// seeded position that needs the least wire to the parts already down,
    /// favoring spots where its wires are straight, and is turned whichever
    /// way the drafting conventions allow that does that best. Then each part
    /// gets a chance to move again now that it can see all the others.
    pub fn spread_parts(&mut self, parts: &[PartId], nets: &[NetId]) {
        let net_set: HashSet<NetId> = nets.iter().copied().collect();
        let wired: HashMap<PartId, Vec<PinId>> = parts.iter().map(|&p| (p, self.wired_pins(p, &net_set))).collect();
        let mut order = parts.to_vec();
        order.sort_by(|&a, &b| {
            wired[&b].len().cmp(&wired[&a].len()).then_with(|| self.place_bbox(b).area().total_cmp(&self.place_bbox(a).area()))
        });

        // Moves to try from a part's position, nearest first.
        let mut steps: Vec<(i64, i64)> = Vec::new();
        for i in -SPREAD_MAX_STEPS..=SPREAD_MAX_STEPS {
            for j in -SPREAD_MAX_STEPS..=SPREAD_MAX_STEPS {
                steps.push((i, j));
            }
        }
        steps.sort_by_key(|&v| move_key(v));
        let offsets: Vec<(f64, f64)> = steps.into_iter().map(|(i, j)| (i as f64 * GRID, j as f64 * GRID)).collect();

        // The placement boxes of the parts that are down, in the order they went down.
        let mut placed: Vec<(PartId, (f64, f64, f64, f64))> = Vec::new();

        let mut passes: Vec<(PartId, bool)> = order.iter().map(|&p| (p, true)).collect();
        for _ in 0..SPREAD_PASSES {
            passes.extend(order.iter().rev().map(|&p| (p, false)));
        }
        for (part, first) in passes {
            if first {
                // Put the part's snap point on the grid; moves of whole grid steps keep it there.
                let pt = self.c.pins[self.c.parts[part].pins[0]].pt * self.c.parts[part].tx;
                self.c.parts[part].tx = self.c.parts[part].tx.move_by(pt.snap(GRID) - pt);
            }

            // Move and turn the part to its best spot among the parts that are placed.
            let is_placed = |p: PartId| placed.iter().any(|(q, _)| *q == p);
            let mut partners: Vec<(PinId, Vec<Point>)> = Vec::new();
            for &pin in &wired[&part] {
                let net = self.c.pins[pin].net.unwrap();
                let pts: Vec<Point> = self.c.nets[net]
                    .pins
                    .iter()
                    .copied()
                    .filter(|&o| {
                        let op = self.c.pins[o].part;
                        op != part && is_placed(op) && self.is_wired(o, &net_set)
                    })
                    .map(|o| self.c.pins[o].pt * self.c.pin_part(o).tx)
                    .collect();
                if !pts.is_empty() {
                    partners.push((pin, pts));
                }
            }
            let boxes: Vec<(f64, f64, f64, f64)> = placed.iter().filter(|(p, _)| *p != part).map(|(_, b)| *b).collect();
            let mut best_cost = f64::INFINITY;
            for tx in self.turned(part) {
                // The part's current orientation is tried first and wins ties.
                let (cost, offset) = self.best_spot(part, tx, &offsets, &partners, &boxes);
                if cost < best_cost {
                    best_cost = cost;
                    self.c.parts[part].tx = tx.move_by(offset.unwrap());
                }
            }
            if best_cost == f64::INFINITY {
                // No room nearby, so look farther and farther out until there is.
                let mut radius = SPREAD_MAX_STEPS + 1;
                while radius < SPREAD_MAX_RADIUS {
                    let tx = self.c.parts[part].tx;
                    if let (_, Some(offset)) = self.best_spot(part, tx, &ring(radius), &partners, &boxes) {
                        self.c.parts[part].tx = tx.move_by(offset);
                        break;
                    }
                    radius += 2;
                }
            }
            let bbox = (self.place_bbox(part) * self.c.parts[part].tx).tuple();
            match placed.iter_mut().find(|(p, _)| *p == part) {
                Some(entry) => entry.1 = bbox,
                None => placed.push((part, bbox)),
            }
        }
    }

    /// Put parts that have no wires in rows, grouped by the nets on their pins.
    ///
    /// These are mostly decoupling caps whose pins all go to power symbols,
    /// and a person draws them as a row of caps with the ones on the same rail
    /// side by side. The rows are kept a few times wider than they are tall.
    /// The parts keep their orientations.
    pub fn row_floating_parts(&mut self, parts: &[PartId]) {
        let rails = |d: &Design, part: PartId| -> Vec<String> {
            let mut names: Vec<String> = d.c.parts[part]
                .pins
                .iter()
                .filter_map(|&pin| d.c.pins[pin].net)
                .map(|n| d.c.nets[n].name.clone())
                .filter(|name| !is_ground_net_name(name))
                .collect();
            names.sort();
            names.dedup();
            names
        };
        let mut sorted = parts.to_vec();
        sorted.sort_by_cached_key(|&p| {
            let part = &self.c.parts[p];
            (rails(self, p), part.pins.len(), part.value.clone(), part.ref_())
        });

        // The box of each part and its labels, plus room for its reference and
        // value. The text goes on a side with no pins: to the right of a part
        // with pins only on its top and bottom, otherwise above and below it.
        let boxes: Vec<BBox> = sorted
            .iter()
            .map(|&p| {
                let part = &self.c.parts[p];
                let lin = linear(part.tx);
                let bbox = part.lbl_bbox * lin;
                let chars = part.ref_().chars().count().max(part.value.chars().count()) as f64;
                if part.pins.iter().all(|&pin| pyround(pin_exit(&self.c, pin, lin).x) == 0.0) {
                    BBox::new(bbox.min, bbox.max + Point::new(FIELD_GAP + chars * FIELD_CHAR_W, 0.0))
                } else {
                    bbox.resize(Point::new(0.0, FIELD_GAP + 2.0 * FIELD_PITCH))
                }
            })
            .collect();
        let area: f64 = boxes.iter().map(|b| (b.w() + FLOAT_GAP) * (b.h() + FLOAT_GAP)).sum();
        let widest = boxes.iter().map(|b| b.w()).fold(0.0, f64::max);
        let width = widest.max((area * FLOAT_ASPECT).sqrt());

        let (mut x, mut top, mut row_height) = (0.0, 0.0, 0.0f64);
        for (&part, bbox) in sorted.iter().zip(&boxes) {
            if x != 0.0 && x + bbox.w() > width {
                // Start a new row below the last one.
                x = 0.0;
                top = top - row_height - FLOAT_GAP;
                row_height = 0.0;
            }
            let tx = linear(self.c.parts[part].tx).move_by(Point::new(x - bbox.min.x, top - bbox.max.y));
            self.c.parts[part].tx = tx;
            if let Some(&pin) = self.c.parts[part].pins.first() {
                let pt = self.c.pins[pin].pt * tx;
                self.c.parts[part].tx = tx.move_by(pt.snap(GRID) - pt);
            }
            x += bbox.w() + FLOAT_GAP;
            row_height = row_height.max(bbox.h());
        }
    }
}
