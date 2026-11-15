//! Decide what to draw on a sheet besides its parts and routed wires: which
//! labels are redundant, which power pins share a bus wire, where no-connect
//! flags go, and how labels are moved off part bodies.
//!
//! All coordinates here are sheet mm, as the pins are drawn.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

use crate::geometry::{pyround, round_to, BBox, Point, Tx};
use crate::ir::{NetId, NodeId, PartId, PinId};

use super::kicad::{is_power_symbol_name, kicad_pin_pos, render_xy, round_mm};
use super::Design;

/// Size (mm) of the box a net label is taken to fill.
pub const LABEL_BOX: (f64, f64) = (10.0, 2.0);

impl Design {
    fn real_pin_in_node(&self, pin: PinId, node: NodeId) -> bool {
        let part = &self.c.parts[self.c.pins[pin].part];
        part.node == Some(node) && !part.is_net_terminal
    }

    /// Pins whose labels are redundant because snapping made them coincide.
    ///
    /// Overlapping pins of a net form clusters. Each cluster needs only one
    /// label to connect it to the rest of the net, and none if it holds the
    /// whole net.
    pub fn find_overlapping_pins(&self, node: NodeId, sheet_tx: Tx) -> HashSet<PinId> {
        let mut wired: HashSet<PinId> = HashSet::new();
        let mut seen: HashSet<NetId> = HashSet::new();
        for &part in &self.nodes[node].parts {
            if self.c.parts[part].is_net_terminal {
                continue;
            }
            for &pin in &self.c.parts[part].pins {
                if !self.c.pins[pin].stub || !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if !seen.insert(net) {
                    continue;
                }
                let is_power = is_power_symbol_name(&self.c.nets[net].name);

                // For power nets, all the pins on this sheet count. For
                // others, only the stubbed pins (which get labels).
                let pins_in_node: Vec<PinId> = self.c.nets[net]
                    .pins
                    .iter()
                    .copied()
                    .filter(|&p| self.real_pin_in_node(p, node) && (is_power || self.c.pins[p].stub))
                    .collect();
                if pins_in_node.len() < 2 {
                    continue;
                }
                let positions: Vec<(f64, f64)> = pins_in_node
                    .iter()
                    .map(|&p| kicad_pin_pos(&self.c.pins[p], self.c.pin_part(p).tx, sheet_tx))
                    .collect();

                // Union-find to cluster the overlapping pins.
                let n = positions.len();
                let mut parent: Vec<usize> = (0..n).collect();
                fn find(parent: &mut [usize], mut i: usize) -> usize {
                    while parent[i] != i {
                        parent[i] = parent[parent[i]];
                        i = parent[i];
                    }
                    i
                }
                for i in 0..n {
                    for j in i + 1..n {
                        let d = ((positions[i].0 - positions[j].0).powi(2) + (positions[i].1 - positions[j].1).powi(2)).sqrt();
                        if d < 0.01 {
                            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                            if ri != rj {
                                parent[ri] = rj;
                            }
                        }
                    }
                }
                let mut clusters: IndexMap<usize, Vec<usize>> = IndexMap::new();
                for i in 0..n {
                    let r = find(&mut parent, i);
                    clusters.entry(r).or_default().push(i);
                }

                let all_real = self.c.nets[net].pins.iter().filter(|&&p| !self.c.pin_part(p).is_net_terminal).count();
                let has_pins_outside = all_real > pins_in_node.len();
                for members in clusters.values() {
                    if members.len() < 2 {
                        continue;
                    }
                    if !has_pins_outside && pins_in_node.len() == members.len() {
                        // The whole net is in this cluster, so it needs no labels.
                        wired.extend(members.iter().map(|&i| pins_in_node[i]));
                    } else {
                        // Keep one label to connect to the rest of the net.
                        wired.extend(members[1..].iter().map(|&i| pins_in_node[i]));
                    }
                }
            }
        }
        wired
    }

    /// The box of a part's body in sheet mm.
    fn part_render_bbox(&self, part: PartId, sheet_tx: Tx) -> ((f64, f64), (f64, f64)) {
        let p = &self.c.parts[part];
        let bbox = p.place_bbox.unwrap_or(p.lbl_bbox);
        let corners = [
            render_xy(bbox.min.x, bbox.min.y, p.tx, sheet_tx),
            render_xy(bbox.max.x, bbox.min.y, p.tx, sheet_tx),
            render_xy(bbox.min.x, bbox.max.y, p.tx, sheet_tx),
            render_xy(bbox.max.x, bbox.max.y, p.tx, sheet_tx),
        ];
        let xs = corners.iter().map(|c| c.0);
        let ys = corners.iter().map(|c| c.1);
        (
            (xs.clone().fold(f64::INFINITY, f64::min), ys.clone().fold(f64::INFINITY, f64::min)),
            (xs.fold(f64::NEG_INFINITY, f64::max), ys.fold(f64::NEG_INFINITY, f64::max)),
        )
    }

    /// Runs of three or more power-net pins in a line, to be joined by a bus wire.
    ///
    /// Returns the wire segments (x1, y1, x2, y2, net name) and the pins that
    /// don't get power symbols of their own.
    #[allow(clippy::type_complexity)]
    pub fn find_power_bus_runs(
        &self,
        node: NodeId,
        sheet_tx: Tx,
    ) -> (Vec<(f64, f64, f64, f64, String)>, HashSet<PinId>) {
        const MAX_GAP_MM: f64 = 10.0;
        let mut bus_pins: HashSet<PinId> = HashSet::new();
        let mut segments = Vec::new();

        // Part bodies, so no bus segment is run straight through a part.
        let bodies: Vec<((f64, f64), (f64, f64), PartId)> = self.nodes[node]
            .parts
            .iter()
            .filter(|&&p| !self.c.parts[p].is_net_terminal)
            .map(|&p| {
                let (lo, hi) = self.part_render_bbox(p, sheet_tx);
                (lo, hi, p)
            })
            .collect();

        let mut seen: HashSet<NetId> = HashSet::new();
        for &part in &self.nodes[node].parts {
            if self.c.parts[part].is_net_terminal {
                continue;
            }
            for &pin in &self.c.parts[part].pins {
                if !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if !seen.insert(net) {
                    continue;
                }
                let net_name = self.c.nets[net].name.clone();
                if !is_power_symbol_name(&net_name) {
                    continue;
                }
                let pins_in_node: Vec<PinId> = self.c.nets[net]
                    .pins
                    .iter()
                    .copied()
                    .filter(|&p| {
                        let pp = self.c.pin_part(p);
                        self.real_pin_in_node(p, node)
                            && pp.pins.len() == 2
                            && !pp
                                .pins
                                .iter()
                                .all(|&q| self.c.is_connected(q) && is_power_symbol_name(self.c.net_name(q)))
                    })
                    .collect();
                if pins_in_node.len() < 3 {
                    continue;
                }
                let positions: Vec<(f64, f64, PinId)> = pins_in_node
                    .iter()
                    .map(|&p| {
                        let (x, y) = kicad_pin_pos(&self.c.pins[p], self.c.pin_part(p).tx, sheet_tx);
                        (round_mm(x), round_mm(y), p)
                    })
                    .collect();

                let key = |v: f64| (round_to(v, 1) * 10.0).round() as i64;
                let mut x_groups: IndexMap<i64, Vec<(f64, f64, PinId)>> = IndexMap::new();
                let mut y_groups: IndexMap<i64, Vec<(f64, f64, PinId)>> = IndexMap::new();
                for pos in &positions {
                    x_groups.entry(key(pos.0)).or_default().push(*pos);
                    y_groups.entry(key(pos.1)).or_default().push(*pos);
                }

                let axis_of = |p: &(f64, f64, PinId), axis: usize| if axis == 0 { p.0 } else { p.1 };
                let split_runs = |group: &[(f64, f64, PinId)], axis: usize| -> Vec<Vec<(f64, f64, PinId)>> {
                    let mut runs = vec![vec![group[0]]];
                    for j in 1..group.len() {
                        let gap = (axis_of(&group[j], axis) - axis_of(&group[j - 1], axis)).abs();
                        if gap <= MAX_GAP_MM {
                            runs.last_mut().unwrap().push(group[j]);
                        } else {
                            runs.push(vec![group[j]]);
                        }
                    }
                    runs.into_iter().filter(|r| r.len() >= 3).collect()
                };
                let seg_clear = |a: &(f64, f64, PinId), b: &(f64, f64, PinId)| -> bool {
                    let (pa, pb) = (self.c.pins[a.2].part, self.c.pins[b.2].part);
                    bodies
                        .iter()
                        .all(|(lo, hi, bp)| *bp == pa || *bp == pb || !seg_crosses_box((a.0, a.1), (b.0, b.1), *lo, *hi, 0.5))
                };
                let mut emit_run = |run: &[(f64, f64, PinId)]| {
                    // Split the run wherever a segment would cross a part body.
                    let mut subruns = vec![vec![run[0]]];
                    for j in 0..run.len() - 1 {
                        if seg_clear(&run[j], &run[j + 1]) {
                            subruns.last_mut().unwrap().push(run[j + 1]);
                        } else {
                            subruns.push(vec![run[j + 1]]);
                        }
                    }
                    for sub in subruns {
                        if sub.len() < 2 {
                            continue; // A pin by itself gets its own power symbol.
                        }
                        for j in 0..sub.len() - 1 {
                            segments.push((sub[j].0, sub[j].1, sub[j + 1].0, sub[j + 1].1, net_name.clone()));
                        }
                        for p in &sub[..sub.len() - 1] {
                            bus_pins.insert(p.2);
                        }
                    }
                };

                for (_, group) in x_groups.iter_mut() {
                    if group.len() < 3 {
                        continue;
                    }
                    group.sort_by(|a, b| a.1.total_cmp(&b.1));
                    for run in split_runs(group, 1) {
                        emit_run(&run);
                    }
                }
                for (_, group) in y_groups.iter_mut() {
                    if group.len() < 3 {
                        continue;
                    }
                    group.sort_by(|a, b| a.0.total_cmp(&b.0));
                    for run in split_runs(group, 0) {
                        emit_run(&run);
                    }
                }
            }
        }
        (segments, bus_pins)
    }

    /// Decide which net labels to move off part bodies.
    ///
    /// `labels` holds (index, net name, x, y, angle) for each global label and
    /// `occupied_seed` the grid cells already taken, each by a net's label or
    /// (None) by a wire end. Returns the moves (index, x, y) and the short
    /// wires (ax, ay, x, y) that keep each moved label connected.
    #[allow(clippy::type_complexity)]
    pub fn deconflict_labels(
        &self,
        labels: &[(usize, String, f64, f64, f64)],
        occupied_seed: &[((i64, i64), Option<String>)],
        node: NodeId,
        sheet_tx: Tx,
    ) -> (Vec<(usize, f64, f64)>, Vec<(f64, f64, f64, f64)>) {
        const MARGIN_MM: f64 = 1.27;
        const GRID: f64 = 1.27;
        let snap = |v: f64| round_to(pyround(v / GRID) * GRID, 2);
        let cell = |x: f64, y: f64| (pyround(x / GRID) as i64, pyround(y / GRID) as i64);
        let on_grid = |v: f64| (v / GRID - pyround(v / GRID)).abs() < 0.02;

        // A stubbed net's label sits on its pin and connects by name, so it stays put.
        let mut stub_net_names: HashSet<&str> = HashSet::new();
        let mut comp_bboxes: Vec<BBox> = Vec::new();
        for &part in &self.nodes[node].parts {
            let p = &self.c.parts[part];
            if p.is_net_terminal {
                continue;
            }
            for &pin in &p.pins {
                if let Some(net) = self.c.pins[pin].net {
                    if self.c.pins[pin].stub || self.c.nets[net].stub {
                        stub_net_names.insert(&self.c.nets[net].name);
                    }
                }
            }
            let bbox = p.place_bbox.unwrap_or(p.lbl_bbox);
            let tb = bbox * (p.tx * sheet_tx);
            comp_bboxes.push(BBox::new(tb.min, tb.max));
        }
        if comp_bboxes.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let mut occupied: HashMap<(i64, i64), Option<String>> = HashMap::new();
        for (c, owner) in occupied_seed {
            occupied.insert(*c, owner.clone());
        }

        let (label_w, label_h) = LABEL_BOX;
        let mut moves = Vec::new();
        let mut new_wires = Vec::new();
        for (idx, net, lx, ly, langle) in labels {
            let (lx, ly) = (*lx, *ly);
            if stub_net_names.contains(net.as_str()) {
                continue;
            }
            // Only labels whose anchor (a pin) is on the grid are moved.
            if !(on_grid(lx) && on_grid(ly)) {
                continue;
            }
            // KiCad label angles are counter-clockwise, but sheet Y increases downward.
            let r = langle.to_radians();
            let (dx, dy) = (r.cos(), -r.sin());
            let (x_end, y_end) = (lx + dx * label_w, ly + dy * label_w);
            let lbl_bbox = BBox::new(
                Point::new(lx.min(x_end) - label_h / 2.0, ly.min(y_end) - label_h / 2.0),
                Point::new(lx.max(x_end) + label_h / 2.0, ly.max(y_end) + label_h / 2.0),
            );
            for cb in &comp_bboxes {
                if !lbl_bbox.intersects(cb) {
                    continue;
                }
                // A label always grazes the part whose pin it's on. That isn't a conflict.
                if cb.min.x - 0.05 <= lx && lx <= cb.max.x + 0.05 && cb.min.y - 0.05 <= ly && ly <= cb.max.y + 0.05 {
                    continue;
                }
                let (ax, ay) = (lx, ly);
                let (nx, ny);
                if dx.abs() > dy.abs() {
                    let offset = if dx > 0.0 { cb.max.x - lx + MARGIN_MM } else { cb.min.x - lx - MARGIN_MM };
                    nx = snap(lx + offset);
                    ny = snap(ly);
                } else {
                    let offset = if dy > 0.0 { cb.max.y - ly + MARGIN_MM } else { cb.min.y - ly - MARGIN_MM };
                    nx = snap(lx);
                    ny = snap(ly + offset);
                }
                if (nx, ny) == (ax, ay) {
                    break;
                }
                // A move onto another net's label or a wire end could short them.
                if let Some(owner) = occupied.get(&cell(nx, ny)) {
                    if owner.as_deref() != Some(net.as_str()) {
                        break;
                    }
                }
                // The connecting wire mustn't cut through a part body.
                if comp_bboxes
                    .iter()
                    .any(|b| seg_crosses_box((ax, ay), (nx, ny), (b.min.x, b.min.y), (b.max.x, b.max.y), 0.5))
                {
                    break;
                }
                moves.push((*idx, nx, ny));
                occupied.insert(cell(nx, ny), Some(net.clone()));
                new_wires.push((ax, ay, nx, ny));
                break;
            }
        }
        (moves, new_wires)
    }

    /// Where the no-connect flags go: (x, y, part reference, pin number) for each pin on the no-connect net.
    pub fn find_no_connect_pins(&self, node: NodeId, sheet_tx: Tx) -> Vec<(f64, f64, String, String)> {
        let mut flags = Vec::new();
        for &part in &self.nodes[node].parts {
            let p = &self.c.parts[part];
            if p.is_net_terminal {
                continue;
            }
            for &pin in &p.pins {
                let pn = &self.c.pins[pin];
                if pn.net.is_some_and(|n| self.c.nets[n].nc) {
                    let pt = pn.pt * p.tx * sheet_tx;
                    flags.push((pt.x, pt.y, p.ref_(), pn.num.clone()));
                }
            }
        }
        flags
    }
}

/// True if the segment a-b passes through the inside of a box shrunk by `inset` (Liang-Barsky).
pub fn seg_crosses_box(a: (f64, f64), b: (f64, f64), bmin: (f64, f64), bmax: (f64, f64), inset: f64) -> bool {
    let (xmin, ymin) = (bmin.0 + inset, bmin.1 + inset);
    let (xmax, ymax) = (bmax.0 - inset, bmax.1 - inset);
    if xmin >= xmax || ymin >= ymax {
        return false;
    }
    let (x1, y1) = a;
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, x1 - xmin), (dx, xmax - x1), (-dy, y1 - ymin), (dy, ymax - y1)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return false;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return false;
                }
                t1 = t1.min(r);
            }
        }
    }
    t0 <= t1
}
