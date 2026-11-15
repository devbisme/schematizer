//! Save, compare and copy the layouts of hierarchical sheets.
//!
//! A sheet's layout is where its parts are and how its wires run, for the
//! sheet's node and every node below it. Two things are done with one:
//!
//! * The same subcircuit used twice (the two channels of a stereo amplifier)
//!   gets the same drawing. Once the first copy is placed and routed,
//!   `reuse_layout()` gives its layout to the second instead of laying that
//!   out from scratch, where it would come out looking different.
//! * A sheet is laid out several times and the best layout kept.
//!   `best_layout()` does the extra tries, comparing them by `layout_cost()`.
//!
//! Both rely on a sheet being drawn as a box whose size doesn't depend on
//! what's inside it, so its insides can change without disturbing the sheet
//! it's on.

use indexmap::IndexMap;
use std::collections::HashMap;

use crate::geometry::{BBox, Point, Segment, Tx};
use crate::ir::{NetId, NodeId, PartId};
use crate::quality::{self, Score};

use super::{progress, Design};

const MILS_TO_MM: f64 = 0.0254;

/// The placement and routing of a node and the nodes below it.
#[derive(Clone, Debug)]
pub struct Layout {
    node: NodeId,
    /// What placement leaves on each part: where it is, and the room it was given.
    parts: Vec<(PartId, Tx, Option<BBox>)>,
    wires: IndexMap<NetId, Vec<Segment>>,
    junctions: IndexMap<NetId, Vec<Point>>,
    children: Vec<(Tx, BBox, Layout)>,
}

/// How the parts, nets and nodes of one node map onto those of another like it.
#[derive(Default)]
pub struct Match {
    parts: HashMap<PartId, PartId>,
    nets: HashMap<NetId, NetId>,
    nets_back: HashMap<NetId, NetId>,
    nodes: HashMap<NodeId, NodeId>,
}

impl Match {
    /// Pair two nets. Returns false if either is already paired with another.
    fn pair_nets(&mut self, a: NetId, b: NetId) -> bool {
        *self.nets.entry(a).or_insert(b) == b && *self.nets_back.entry(b).or_insert(a) == a
    }
}

fn rounded(b: &BBox) -> (i64, i64, i64, i64) {
    let r = |v: f64| if v.is_finite() { v.round() as i64 } else { i64::MAX };
    (r(b.min.x), r(b.min.y), r(b.max.x), r(b.max.y))
}

impl Design {
    /// Record the layout of a node and the nodes below it.
    pub fn save_layout(&self, node: NodeId) -> Layout {
        let n = &self.nodes[node];
        Layout {
            node,
            parts: n.parts.iter().map(|&p| (p, self.c.parts[p].tx, self.c.parts[p].place_bbox)).collect(),
            wires: n.wires.clone(),
            junctions: n.junctions.clone(),
            children: n
                .children
                .values()
                .map(|&ch| (self.nodes[ch].tx, self.nodes[ch].bbox, self.save_layout(ch)))
                .collect(),
        }
    }

    /// Give a layout back to its node, or (with a match) to another node like it.
    ///
    /// The node's own position and outline on its parent's sheet are left alone.

    fn apply_layout_to(&mut self, layout: &Layout, node: NodeId, matched: Option<&Match>) {
        let part_of = |p: PartId| matched.map_or(p, |m| m.parts[&p]);
        let net_of = |n: NetId| matched.map_or(n, |m| m.nets[&n]);
        for &(part, tx, place_bbox) in &layout.parts {
            let p = &mut self.c.parts[part_of(part)];
            p.tx = tx;
            p.place_bbox = place_bbox;
        }
        self.nodes[node].wires = layout.wires.iter().map(|(&net, segs)| (net_of(net), segs.clone())).collect();
        self.nodes[node].junctions = layout.junctions.iter().map(|(&net, pts)| (net_of(net), pts.clone())).collect();
        for (tx, bbox, child) in &layout.children {
            let other = matched.map_or(child.node, |m| m.nodes[&child.node]);
            self.nodes[other].tx = *tx;
            self.nodes[other].bbox = *bbox;
            self.apply_layout_to(child, other, matched);
        }
    }

    /// True if two parts are drawn the same and take up the same room.
    fn same_shape(&self, a: PartId, b: PartId) -> bool {
        let (pa, pb) = (&self.c.parts[a], &self.c.parts[b]);
        if pa.is_net_terminal != pb.is_net_terminal
            || pa.pins.len() != pb.pins.len()
            || pa.name != pb.name
            || pa.num != pb.num
            || pa.symtx != pb.symtx
            || pa.orientation_locked != pb.orientation_locked
            || rounded(&pa.bbox) != rounded(&pb.bbox)
            || rounded(&pa.lbl_bbox) != rounded(&pb.lbl_bbox)
        {
            return false;
        }
        pa.pins.iter().zip(&pb.pins).all(|(&x, &y)| {
            let (x, y) = (&self.c.pins[x], &self.c.pins[y]);
            x.num == y.num && x.orientation == y.orientation && x.stub == y.stub && x.pt == y.pt
        })
    }

    /// Pair the nets on the pins of two parts. Returns false if they can't be paired.
    fn pair_pins(&self, a: PartId, b: PartId, m: &mut Match) -> bool {
        for (&x, &y) in self.c.parts[a].pins.iter().zip(&self.c.parts[b].pins) {
            match (self.c.pins[x].net, self.c.pins[y].net) {
                (None, None) => {}
                (Some(n1), Some(n2)) => {
                    if self.c.nets[n1].nc != self.c.nets[n2].nc
                        || self.c.nets[n1].stub != self.c.nets[n2].stub
                        || !m.pair_nets(n1, n2)
                    {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn match_into(&self, n1: NodeId, n2: NodeId, m: &mut Match) -> bool {
        let (a, b) = (&self.nodes[n1], &self.nodes[n2]);
        if a.flattened != b.flattened || a.children.len() != b.children.len() {
            return false;
        }
        // The parts of a subcircuit are made in the same order each time it's used.
        let real = |n: &super::Node| -> Vec<PartId> { n.parts.iter().copied().filter(|&p| !self.c.parts[p].is_net_terminal).collect() };
        let (parts1, parts2) = (real(a), real(b));
        if parts1.len() != parts2.len() {
            return false;
        }
        for (&p1, &p2) in parts1.iter().zip(&parts2) {
            if !self.same_shape(p1, p2) || !self.pair_pins(p1, p2, m) {
                return false;
            }
            m.parts.insert(p1, p2);
        }
        for (&c1, &c2) in a.children.values().zip(b.children.values()) {
            if !self.match_into(c1, c2, m) {
                return false;
            }
            m.nodes.insert(c1, c2);
        }
        // Net terminals are made in the order of the nets, which needn't
        // agree, so they're paired by the nets they label.
        let terminals = |n: &super::Node| -> HashMap<NetId, PartId> {
            n.parts
                .iter()
                .copied()
                .filter(|&p| self.c.parts[p].is_net_terminal)
                .filter_map(|p| self.c.pins[self.c.parts[p].pins[0]].net.map(|net| (net, p)))
                .collect()
        };
        let (t1, t2) = (terminals(a), terminals(b));
        if t1.len() != t2.len() {
            return false;
        }
        for (net1, &term1) in &t1 {
            let Some(&term2) = m.nets.get(net1).and_then(|n2| t2.get(n2)) else { return false };
            if !self.same_shape(term1, term2) {
                return false;
            }
            m.parts.insert(term1, term2);
        }
        true
    }

    /// See if two nodes hold the same circuit, drawn with the same symbols.
    ///
    /// They do if their parts pair off in order, each pair having the same
    /// symbol, size and pins, with the nets on those pins pairing off one to
    /// one, and the same goes for the nodes below them. Part values and net
    /// names can differ.
    pub fn match_nodes(&self, n1: NodeId, n2: NodeId) -> Option<Match> {
        let mut m = Match::default();
        if !self.match_into(n1, n2, &mut m) {
            return None;
        }
        m.nodes.insert(n1, n2);
        // Every wire of the first node must have a net to belong to in the second.
        for node in self.all_nodes(n1) {
            if self.nodes[node].wires.keys().any(|net| !m.nets.contains_key(net)) {
                return None;
            }
        }
        Some(m)
    }

    /// Give a node the layout of an earlier node that holds the same circuit.
    ///
    /// Returns true if the node took another's layout and needs no routing.
    pub fn reuse_layout(&mut self, node: NodeId, routed: &[NodeId]) -> bool {
        if self.nodes[node].flattened || !self.all_nodes(node).iter().any(|&n| !self.nodes[n].parts.is_empty()) {
            return false;
        }
        for &other in routed {
            let Some(m) = self.match_nodes(other, node) else { continue };
            let before = self.save_layout(node);
            let layout = self.save_layout(other);
            self.apply_layout_to(&layout, node, Some(&m));
            let ok = self.all_nodes(node).into_iter().all(|sub| {
                let nets = self.get_internal_nets(sub);
                self.check_pin_wires(sub, &nets).is_ok()
            });
            if !ok {
                self.apply_layout_to(&before, node, None);
                continue;
            }
            progress::report(&format!("drawing {} like {}", self.nodes[node].name, self.nodes[other].name));
            return true;
        }
        false
    }

    /// Score the drawing of a placed and routed node's sheet, as its file would be scored.
    ///
    /// The bends here include wire crossings.
    pub fn layout_score(&mut self, node: NodeId) -> Score {
        let (mut bends, mut length) = (0, 0.0);
        for n in self.sheet_nodes(node) {
            let cost = self.wiring_cost(n);
            bends += cost.0;
            length += cost.1;
        }
        let (part_overlaps, text_overlaps) = self.sheet_overlaps(node);
        Score { bends, wirelength: length * MILS_TO_MM, part_overlaps, text_overlaps, ..Default::default() }
    }

    /// The cost of the drawing of a node's sheet: lower is better.
    ///
    /// It's the cost whole drawings are compared by: wire length, with bends,
    /// crossings and overlapping parts and text charged as extra length.
    pub fn layout_cost(&mut self, node: NodeId) -> f64 {
        quality::layout_cost(&self.layout_score(node))
    }

    /// Lay out a routed sheet some more times and keep the layout with the lowest cost.
    ///
    /// Only what's drawn on the node's own sheet is laid out again. The sheets
    /// below it keep the layouts they have. `tries` counts the layout the node
    /// already has. Returns which of the tries was kept, counting from 0.
    pub fn best_layout(&mut self, node: NodeId, tries: usize) -> usize {
        if tries < 2 || self.nodes[node].flattened || !self.sheet_nodes(node).iter().any(|&n| !self.nodes[n].wires.is_empty()) {
            return 0;
        }
        let sheets_below: Vec<NodeId> =
            self.all_nodes(node).into_iter().filter(|&n| n != node && !self.nodes[n].flattened).collect();
        let kept: Vec<Layout> = sheets_below.iter().map(|&n| self.save_layout(n)).collect();

        let mut best = self.save_layout(node);
        let mut best_cost = self.layout_cost(node);
        let mut best_try = 0;
        for i in 1..tries {
            // A crowded sheet often fails to route, so each try gets a second go.
            for _ in 0..2 {
                let ok = {
                    let _note = progress::prefixed(&format!("layout {}/{},", i + 1, tries));
                    let placed = self.place(node).is_ok();
                    // Placing the node places the sheets below it again too.
                    for layout in &kept {
                        self.apply_layout_to(layout, layout.node, None);
                    }
                    placed && self.route_node(node, false).is_ok()
                };
                if !ok {
                    continue;
                }
                let cost = self.layout_cost(node);
                if cost < best_cost {
                    best = self.save_layout(node);
                    best_cost = cost;
                    best_try = i;
                }
                break;
            }
        }
        self.apply_layout_to(&best, node, None);
        best_try
    }
}
