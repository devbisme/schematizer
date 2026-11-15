//! Snap two-pin parts onto the pins they connect to.
//!
//! After placement and routing, two-pin parts (resistors, caps, LEDs, ...)
//! are moved so one of their pins lands on the pin of a connected IC, or on
//! the free pin of a two-pin part already snapped. That turns a sea of
//! labeled stubs into pins that touch and short wires, which the writers
//! draw without the labels that would be redundant.
//!
//! This is only done for force-directed placement with auto-stubbing, which
//! drops such parts anywhere. It moves `part.tx` and records on the node the
//! wires, junctions and pins whose labels the writers should leave out.

use indexmap::IndexMap;
use std::collections::HashSet;

use crate::geometry::{Point, Tx};
use crate::ir::{NetId, NodeId, PartId, PinId};

use super::conventions::is_power_net_name;
use super::Design;

/// Each two-pin part on a fan.
type FanEntry = (PartId, PinId, PinId, PinId, PartId);

fn is_gnd_name(name: &str) -> bool {
    let u = name.to_uppercase();
    matches!(u.as_str(), "GND" | "AGND" | "DGND" | "PGND" | "VSS" | "VEE" | "GROUND")
}

fn perp(dir: char) -> char {
    match dir {
        'R' => 'D',
        'L' => 'U',
        'U' => 'R',
        _ => 'L',
    }
}

fn opposite(dir: char) -> char {
    match dir {
        'U' => 'D',
        'D' => 'U',
        'L' => 'R',
        _ => 'L',
    }
}

fn dir_vec(dir: char) -> (f64, f64) {
    match dir {
        'R' => (1.0, 0.0),
        'L' => (-1.0, 0.0),
        'U' => (0.0, -1.0),
        _ => (0.0, 1.0),
    }
}

/// The transformation that puts `my_pin` of a two-pin part at a point, with
/// its other pin extending from it in the given direction.
fn compute_snap_tx(my_pt: Point, other_pt: Point, target: Point, extend_dir: char) -> Tx {
    let (dx, dy) = (other_pt.x - my_pt.x, other_pt.y - my_pt.y);
    let symtx = match extend_dir {
        'R' => {
            if dx.abs() >= dy.abs() {
                if dx > 0.0 { "" } else { "H" }
            } else if dy > 0.0 {
                "R"
            } else {
                "L"
            }
        }
        'L' => {
            if dx.abs() >= dy.abs() {
                if dx < 0.0 { "" } else { "H" }
            } else if dy > 0.0 {
                "L"
            } else {
                "R"
            }
        }
        'U' => {
            if dy.abs() >= dx.abs() {
                if dy > 0.0 { "" } else { "V" }
            } else if dx > 0.0 {
                "L"
            } else {
                "R"
            }
        }
        'D' => {
            if dy.abs() >= dx.abs() {
                if dy < 0.0 { "" } else { "V" }
            } else if dx > 0.0 {
                "R"
            } else {
                "L"
            }
        }
        _ => "",
    };
    let tx = Tx::from_symtx(symtx);
    let placed = my_pt * tx;
    tx.move_by(Point::new(target.x - placed.x, target.y - placed.y))
}

impl Design {
    fn is_two_pin(&self, part: PartId) -> bool {
        !self.c.parts[part].is_net_terminal && self.c.parts[part].pins.len() == 2
    }

    fn is_power(&self, net: NetId) -> bool {
        is_power_net_name(&self.c.nets[net].name)
    }

    /// The direction away from a pin on the page, once its part's transformation is applied.
    fn pin_world_orient(&self, pin: PinId) -> char {
        let p = &self.c.pins[pin];
        let (vx, vy) = match p.orientation {
            'L' => (-1.0, 0.0),
            'U' => (0.0, -1.0),
            'D' => (0.0, 1.0),
            _ => (1.0, 0.0),
        };
        let tx = self.c.parts[p.part].tx;
        let (wx, wy) = (tx.a * vx + tx.b * vy, tx.c * vx + tx.d * vy);
        let world = if wx.abs() >= wy.abs() {
            if wx > 0.0 { 'R' } else { 'L' }
        } else if wy > 0.0 {
            'D'
        } else {
            'U'
        };
        opposite(world)
    }

    fn world_pt(&self, pin: PinId) -> Point {
        self.c.pins[pin].pt * self.c.pin_part(pin).tx
    }

    /// Stub the nets of a part that's been moved after routing: its wires are stale.
    fn stub_snapped_part(&mut self, part: PartId) {
        for pin in self.c.parts[part].pins.clone() {
            let Some(net) = self.c.pins[pin].net else { continue };
            if self.c.nets[net].stub_explicit {
                continue;
            }
            self.c.pins[pin].stub = true;
            self.c.nets[net].stub = true;
            for p in self.c.nets[net].pins.clone() {
                self.c.pins[p].stub = true;
            }
        }
    }

    fn snap_to(&mut self, part: PartId, my_pin: PinId, target: Point, dir: char) {
        let other = *self.c.parts[part].pins.iter().find(|&&p| p != my_pin).unwrap();
        self.c.parts[part].tx = compute_snap_tx(self.c.pins[my_pin].pt, self.c.pins[other].pt, target, dir);
    }

    /// Snap two-pin parts onto their connected IC pins or onto parts already snapped.
    ///
    /// First onto IC pins, one part per pin. Then, a few times over, onto the
    /// free pins of parts already snapped, building chains (IC, resistor,
    /// LED). Then onto IC pins already taken, at right angles to the first
    /// part. Child nodes are done first.
    pub fn snap_two_pin_parts(&mut self, node: NodeId) {
        for child in self.children(node) {
            self.snap_two_pin_parts(child);
        }
        let parts = self.nodes[node].parts.clone();
        let in_node: HashSet<PartId> = parts.iter().copied().collect();
        let mut snapped: HashSet<PartId> = HashSet::new();
        let mut occupied: HashSet<PinId> = HashSet::new();
        // Decoupling caps already on each supply pin, so more fan out instead of stacking.
        let mut caps_on_pin: IndexMap<PinId, (Point, Vec<Point>)> = IndexMap::new();

        let pins_of = |d: &Design, part: PartId| -> Option<[(PinId, NetId); 2]> {
            let pins = &d.c.parts[part].pins;
            Some([(pins[0], d.c.pins[pins[0]].net?), (pins[1], d.c.pins[pins[1]].net?)])
        };

        for &part in &parts {
            if !self.is_two_pin(part) {
                continue;
            }
            let Some(pins) = pins_of(self, part) else { continue };
            let both_power = self.is_power(pins[0].1) && self.is_power(pins[1].1);
            let min_target_pins = if both_power { 8 } else { 2 };
            let mut target = None;
            'search: for &(my_pin, net) in &pins {
                if self.is_power(net) && !both_power {
                    continue;
                }
                // Decoupling caps anchor to the supply pin, never ground.
                if both_power && is_gnd_name(&self.c.nets[net].name) {
                    continue;
                }
                for &np in &self.c.nets[net].pins {
                    let other = self.c.pins[np].part;
                    if other != part
                        && in_node.contains(&other)
                        && !self.c.parts[other].is_net_terminal
                        && self.c.parts[other].pins.len() > min_target_pins
                        && !occupied.contains(&np)
                    {
                        target = Some((np, my_pin));
                        break 'search;
                    }
                }
            }
            let Some((target_pin, my_pin)) = target else { continue };
            let target_world = self.world_pt(target_pin);
            let dir = self.pin_world_orient(target_pin);
            self.snap_to(part, my_pin, target_world, dir);
            if both_power {
                // Caps on one supply pin go out from it, each one fanned
                // sideways from the last, so they sit in a row.
                let entry = caps_on_pin.entry(target_pin).or_insert((target_world, Vec::new()));
                let n = entry.1.len() as f64;
                let (ax, ay): (f64, f64) = match dir {
                    'L' => (-200.0, 0.0),
                    'U' => (0.0, 200.0),
                    'D' => (0.0, -200.0),
                    _ => (200.0, 0.0),
                };
                let (px, py) = ((-ay / 200.0).floor(), (ax / 200.0).floor());
                const FAN_STEP: f64 = 300.0;
                let tx = self.c.parts[part].tx;
                self.c.parts[part].tx = tx.move_by(Point::new(ax + px * FAN_STEP * n, ay + py * FAN_STEP * n));
                let cap_pt = self.world_pt(my_pin);
                caps_on_pin.get_mut(&target_pin).unwrap().1.push(cap_pt);
                // A wire is drawn to the cap, so its label would be redundant.
                self.nodes[node].power_cap_suppressed_pins.insert(my_pin);
            }
            self.stub_snapped_part(part);
            snapped.insert(part);
            // Caps share their supply pin; any other part claims its pin.
            if !both_power {
                occupied.insert(target_pin);
            }
        }

        // The caps on a supply pin are wired as a bus: a connector from the
        // pin to the first cap, a trunk along the caps, and a junction at each tap.
        for (_, (t, caps)) in caps_on_pin {
            let n = &mut self.nodes[node];
            n.power_cap_wires.push((t.x, t.y, caps[0].x, caps[0].y));
            for w in caps.windows(2) {
                n.power_cap_wires.push((w[0].x, w[0].y, w[1].x, w[1].y));
            }
            n.power_cap_junctions.extend(caps[..caps.len() - 1].iter().copied());
        }

        // Chains: onto the free pins of parts already snapped.
        for _ in 0..5 {
            let mut newly: Vec<PartId> = Vec::new();
            for &part in &parts {
                if snapped.contains(&part) || !self.is_two_pin(part) {
                    continue;
                }
                let Some(pins) = pins_of(self, part) else { continue };
                let both_power = self.is_power(pins[0].1) && self.is_power(pins[1].1);
                let mut target = None;
                'search2: for &(my_pin, net) in &pins {
                    if self.is_power(net) && !both_power {
                        continue;
                    }
                    for &np in &self.c.nets[net].pins {
                        let other = self.c.pins[np].part;
                        if other != part && snapped.contains(&other) && !occupied.contains(&np) {
                            target = Some((np, my_pin));
                            break 'search2;
                        }
                    }
                }
                let Some((target_pin, my_pin)) = target else { continue };
                let (target_world, dir) = (self.world_pt(target_pin), self.pin_world_orient(target_pin));
                self.snap_to(part, my_pin, target_world, dir);
                self.stub_snapped_part(part);
                newly.push(part);
                occupied.insert(target_pin);
            }
            if newly.is_empty() {
                break;
            }
            snapped.extend(newly);
        }

        // Onto IC pins already taken, at right angles to the part already there.
        for &part in &parts {
            if snapped.contains(&part) || !self.is_two_pin(part) {
                continue;
            }
            let Some(pins) = pins_of(self, part) else { continue };
            let mut target = None;
            'search3: for &(my_pin, net) in &pins {
                for &np in &self.c.nets[net].pins {
                    let other = self.c.pins[np].part;
                    if other != part
                        && in_node.contains(&other)
                        && !self.c.parts[other].is_net_terminal
                        && self.c.parts[other].pins.len() > 2
                        && occupied.contains(&np)
                    {
                        target = Some((np, my_pin));
                        break 'search3;
                    }
                }
            }
            let Some((target_pin, my_pin)) = target else { continue };
            let (target_world, dir) = (self.world_pt(target_pin), perp(self.pin_world_orient(target_pin)));
            self.snap_to(part, my_pin, target_world, dir);
            self.stub_snapped_part(part);
            snapped.insert(part);
        }

        self.stagger_tjunctions(node, &in_node, &mut snapped, &occupied);
    }

    /// Spread out the parts that share IC pins, so they fan out from the IC.
    ///
    /// The ICs are moved apart first so their fans won't overlap, then each
    /// fan's parts are put on a wire running out from its IC pin.
    fn stagger_tjunctions(&mut self, node: NodeId, in_node: &HashSet<PartId>, snapped: &mut HashSet<PartId>, occupied: &HashSet<PinId>) {
        let parts = self.nodes[node].parts.clone();

        // The two-pin parts on each IC pin that's taken.
        let mut on_pin: IndexMap<PinId, Vec<FanEntry>> = IndexMap::new();
        for &part in &parts {
            if !self.is_two_pin(part) {
                continue;
            }
            let pins = &self.c.parts[part].pins;
            let (p1, p2) = (pins[0], pins[1]);
            let (Some(n1), Some(n2)) = (self.c.pins[p1].net, self.c.pins[p2].net) else { continue };
            'outer: for (my_pin, net, other_pin) in [(p1, n1, p2), (p2, n2, p1)] {
                if self.is_power(net) {
                    continue;
                }
                for &np in &self.c.nets[net].pins {
                    let ic = self.c.pins[np].part;
                    if ic != part
                        && in_node.contains(&ic)
                        && !self.c.parts[ic].is_net_terminal
                        && self.c.parts[ic].pins.len() > 2
                        && occupied.contains(&np)
                    {
                        on_pin.entry(np).or_default().push((part, my_pin, other_pin, np, ic));
                        break 'outer;
                    }
                }
            }
        }
        let mut by_ic: IndexMap<PartId, Vec<(PinId, Vec<FanEntry>)>> = IndexMap::new();
        for (pin, list) in on_pin {
            by_ic.entry(list[0].4).or_default().push((pin, list));
        }

        struct Plan {
            ic: PartId,
            matching: Vec<(PinId, Vec<FanEntry>)>,
            ic_dir: char,
            step: (f64, f64),
            step_size: f64,
            extent: f64,
            dominant: usize,
            deps: Vec<PartId>,
        }
        let mut plans: Vec<Plan> = Vec::new();
        for (ic, entries) in by_ic {
            // The most common number of parts per pin (the smallest, on a tie).
            let counts: Vec<usize> = entries.iter().map(|(_, l)| l.len()).collect();
            let mut values = counts.clone();
            values.sort();
            values.dedup();
            let mut dominant = values[0];
            for &v in &values {
                if counts.iter().filter(|&&c| c == v).count() > counts.iter().filter(|&&c| c == dominant).count() {
                    dominant = v;
                }
            }
            if dominant < 2 {
                continue;
            }
            let matching: Vec<(PinId, Vec<FanEntry>)> = entries.into_iter().filter(|(_, l)| l.len() == dominant).collect();
            let ic_dir = self.pin_world_orient(matching[0].0);
            let mut max_span: f64 = 0.0;
            for (_, list) in &matching {
                for e in list {
                    let pts: Vec<Point> = self.c.parts[e.0].pins.iter().map(|&p| self.c.pins[p].pt).collect();
                    let xs = pts.iter().map(|p| p.x);
                    let ys = pts.iter().map(|p| p.y);
                    let span = (xs.clone().fold(f64::NEG_INFINITY, f64::max) - xs.fold(f64::INFINITY, f64::min))
                        .max(ys.clone().fold(f64::NEG_INFINITY, f64::max) - ys.fold(f64::INFINITY, f64::min));
                    max_span = max_span.max(span);
                }
            }
            let step_size = 100f64.max(max_span.trunc() + 50.0);
            let extent = step_size * matching.len() as f64 + max_span;
            plans.push(Plan { ic, matching, ic_dir, step: dir_vec(ic_dir), step_size, extent, dominant, deps: Vec::new() });
        }

        if plans.len() > 1 {
            // Move the ICs, with the parts already snapped to them, apart vertically so the fans won't overlap.
            for plan in plans.iter_mut() {
                plan.deps = parts
                    .iter()
                    .copied()
                    .filter(|&p| p != plan.ic && snapped.contains(&p) && self.is_two_pin(p))
                    .filter(|&p| {
                        self.c.parts[p].pins.iter().any(|&pin| {
                            self.c.pins[pin].net.is_some_and(|n| self.c.nets[n].pins.iter().any(|&np| self.c.pins[np].part == plan.ic))
                        })
                    })
                    .collect();
            }
            let span_y = |d: &Design, plan: &Plan| -> (f64, f64) {
                let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                for &part in std::iter::once(&plan.ic).chain(plan.deps.iter()) {
                    for &pin in &d.c.parts[part].pins {
                        let y = d.world_pt(pin).y;
                        lo = lo.min(y);
                        hi = hi.max(y);
                    }
                }
                (lo, hi)
            };
            let mut keyed: Vec<(f64, Plan)> = plans.into_iter().map(|p| (span_y(self, &p).0, p)).collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
            plans = keyed.into_iter().map(|(_, p)| p).collect();
            const MARGIN: f64 = 200.0;
            let mut prev_max: Option<f64> = None;
            for plan in &plans {
                let (mut lo, hi) = span_y(self, plan);
                let mut group_max = hi.max(lo + plan.extent);
                if let Some(prev) = prev_max {
                    if lo < prev + MARGIN {
                        let shift = prev + MARGIN - lo;
                        let mut moved: HashSet<PartId> = HashSet::new();
                        for &part in std::iter::once(&plan.ic).chain(plan.deps.iter()) {
                            if moved.insert(part) {
                                let tx = self.c.parts[part].tx;
                                self.c.parts[part].tx = tx.move_by(Point::new(0.0, shift));
                            }
                        }
                        lo += shift;
                        group_max += shift;
                    }
                }
                let _ = lo;
                prev_max = Some(group_max);
            }
        }

        // Put each fan's parts on a wire running out from its IC pin.
        let mut suppressed: HashSet<PinId> = HashSet::new();
        for plan in &mut plans {
            let perp_dir = perp(plan.ic_dir);
            // Shift the fan out past the IC pins' labels, which run the same way.
            const LABEL_CHAR_W: f64 = 50.0;
            let mut label_clearance: f64 = 0.0;
            for (pin, _) in &plan.matching {
                if let Some(net) = self.c.pins[*pin].net {
                    if self.c.pins[*pin].stub {
                        label_clearance = label_clearance.max((self.c.nets[net].name.chars().count() + 1) as f64 * LABEL_CHAR_W);
                    }
                }
            }
            let label_clearance = label_clearance.min(800.0);
            let horizontal = matches!(plan.ic_dir, 'L' | 'R');
            plan.matching.sort_by(|a, b| {
                let (wa, wb) = (self.world_pt(a.0), self.world_pt(b.0));
                if horizontal { wa.y.total_cmp(&wb.y) } else { wa.x.total_cmp(&wb.x) }
            });
            let extend_dirs: Vec<char> = if plan.dominant >= 2 { vec![perp_dir, opposite(perp_dir)] } else { vec![perp_dir] };
            for (idx, (ic_pin, list)) in plan.matching.iter_mut().enumerate() {
                let w = self.world_pt(*ic_pin);
                list.sort_by_cached_key(|e| self.c.parts[e.0].ref_());
                let off = label_clearance + plan.step_size * (idx + 1) as f64;
                let (ox, oy) = (w.x + plan.step.0 * off, w.y + plan.step.1 * off);
                for (k, e) in list.iter().enumerate() {
                    let dir = extend_dirs[k % extend_dirs.len()];
                    self.snap_to(e.0, e.1, Point::new(ox, oy), dir);
                    self.stub_snapped_part(e.0);
                    snapped.insert(e.0);
                    suppressed.insert(e.1);
                }
                self.nodes[node].tjunction_wires.push((w.x, w.y, ox, oy));
            }
        }
        self.nodes[node].tjunction_suppressed_pins = suppressed;
    }
}
