//! Build and query the tree of nodes that mirrors the circuit's hierarchy.

use std::collections::HashSet;

use crate::geometry::{BBox, Point, Tx};
use crate::ir::{Circuit, NetId, NodeId, Part, PartId, Pin, PinId};

use super::bboxes::calc_hier_label_bbox;
use super::constants::GRID;
use super::{Design, Node};

/// Estimated length (mils) of a net label: per character of its name, plus its flag's ends.
pub const LABEL_CHAR_W: f64 = 45.0;
pub const LABEL_END: f64 = 150.0;

/// Mils per character of the 1.27 mm sheet-file text, as KiCad draws it.
const FILENAME_SZ: f64 = 45.0;
const NAME_SZ: f64 = 40.0;

/// Make a net terminal: a one-pin part that labels a net where it leaves a node.
pub fn new_net_terminal(c: &mut Circuit, net: NetId) -> PartId {
    let base_ref = c.next_ref("NT");
    let name = c.nets[net].name.clone();
    let netio = c.nets[net].netio.to_lowercase();

    // The bounding box is calculated as if the pin pointed right, so the label
    // text goes to the left. It's resized to a whole number of grids, then
    // extended so any attached routing comes straight in.
    let mut bbox = calc_hier_label_bbox(&name, 'R').snap_resize(GRID);
    bbox.max = bbox.max + Point::new(GRID, 0.0);

    let mut tx = Tx::IDENT;
    if netio == "o" {
        // An output net has its label on the right.
        tx = tx.flip_x();
    }

    let part = Part {
        name: "NT".into(),
        base_ref,
        ref_prefix: "NT".into(),
        hiertuple: vec![String::new()],
        num: 1,
        is_net_terminal: true,
        alive: true,
        tx,
        bbox,
        lbl_bbox: bbox,
        orientation_locked: netio == "i" || netio == "o",
        ..Default::default()
    };
    let part_id = c.add_part(part);
    c.top_parts.push(part_id);

    // The pin is at (0, 0), pointing left toward the label, so routing goes off to the right.
    let pin = Pin {
        num: "1".into(),
        name: "~".into(),
        orientation: 'L',
        part: part_id,
        owner: part_id,
        ..Default::default()
    };
    let pin_id = c.add_pin(pin);
    c.parts[part_id].pins.push(pin_id);
    c.connect(pin_id, net);
    part_id
}

impl Design {
    pub fn new_node(&mut self, parent: Option<NodeId>) -> NodeId {
        self.nodes.push(Node { parent, title: self.title.clone(), ..Default::default() });
        self.nodes.len() - 1
    }

    pub fn get_or_add_child(&mut self, node: NodeId, name: &str) -> NodeId {
        if let Some(&child) = self.nodes[node].children.get(name) {
            return child;
        }
        let child = self.new_node(Some(node));
        self.nodes[node].children.insert(name.to_string(), child);
        child
    }

    /// The node that holds a part, going by the part's place in the hierarchy.
    pub fn find_node_with_part(&mut self, part: PartId) -> NodeId {
        let levels = self.c.parts[part].hiertuple.clone();
        let mut node = self.root;
        for name in levels.iter().skip(1) {
            node = self.get_or_add_child(node, name);
        }
        node
    }

    fn add_part(&mut self, node: NodeId, part: PartId, level: usize) {
        let level_names = self.c.parts[part].hiertuple.clone();
        let part_level = level_names.len() - 1;

        self.nodes[node].name = level_names[level].clone();
        // The top level always has a blank name, so it's skipped in the file name.
        let mut names = vec![self.top_name.clone()];
        names.extend(level_names[1..=level].iter().cloned());
        self.nodes[node].sheet_filename = names.join("_") + ".sch";

        if part_level == level {
            for unit in self.c.units(part) {
                self.nodes[node].parts.push(unit);
                self.c.parts[unit].node = Some(node);
            }
        } else {
            let child = self.get_or_add_child(node, &level_names[level + 1]);
            self.add_part(child, part, level + 1);
        }
    }

    /// Build the tree of nodes from the circuit's parts and add the terminals
    /// of the nets that cross between nodes.
    pub fn add_circuit(&mut self) {
        self.nodes.clear();
        self.root = self.new_node(None);
        for part in self.c.top_parts.clone() {
            self.add_part(self.root, part, 0);
        }

        for net in 0..self.c.nets.len() {
            // A stubbed net has no wire to attach a terminal to.
            if self.c.nets[net].stub {
                continue;
            }
            let pins = self.c.nets[net].pins.clone();
            let hier = |c: &Circuit, pin: PinId| c.parts[c.pins[pin].part].hiertuple.clone();
            let spans_nodes = pins.windows(2).any(|w| hier(&self.c, w[0]) != hier(&self.c, w[1]));
            if !spans_nodes {
                let lone_pin = pins.len() == 1;
                let named = !self.c.nets[net].implicit && self.opts.label_local_nets;
                // A net with several pins, all in one node, needs no terminal.
                // (Routing a wire to a label just to show the net's name makes a
                // sheet that much harder to route, so that's only done if asked for.)
                if !(lone_pin || named) {
                    continue;
                }
            }

            // Add one terminal to each node holding pins of the net. A node with
            // an explicit power symbol for the net needs none: the symbol
            // already labels the net there.
            let mut visited: Vec<Vec<String>> = pins
                .iter()
                .filter(|&&p| self.c.pin_part(p).is_power_symbol())
                .map(|&p| hier(&self.c, p))
                .collect();
            for &pin in &pins {
                if self.c.pins[pin].stub {
                    continue;
                }
                let part = self.c.pins[pin].part;
                let ht = self.c.parts[part].hiertuple.clone();
                if visited.contains(&ht) {
                    continue;
                }
                let node = self.find_node_with_part(part);
                self.add_terminal(node, net);
                visited.push(ht);
            }
        }

        let flatness = self.opts.flatness;
        self.flatten(self.root, flatness);
    }

    pub fn add_terminal(&mut self, node: NodeId, net: NetId) {
        let nt = new_net_terminal(&mut self.c, net);
        self.nodes[node].parts.push(nt);
        self.c.parts[nt].node = Some(node);
    }

    /// True if a pin's part is one of the node's own.
    pub fn pin_in_node(&self, pin: PinId, node: NodeId) -> bool {
        self.c.parts[self.c.pins[pin].part].node == Some(node)
    }

    /// The nets that have at least one unstubbed pin on a part of this node.
    pub fn get_internal_nets(&self, node: NodeId) -> Vec<NetId> {
        let mut processed: HashSet<NetId> = HashSet::new();
        let mut internal = Vec::new();
        for &part in &self.nodes[node].parts {
            for &pin in &self.c.parts[part].pins {
                if self.c.pins[pin].stub || !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if !processed.insert(net) {
                    continue;
                }
                if self.c.nets[net].stub {
                    continue;
                }
                if self.c.nets[net].pins.iter().any(|&p| self.pin_in_node(p, node)) {
                    internal.push(net);
                }
            }
        }
        internal
    }

    /// The unstubbed pins of a net that are on parts of this node.
    pub fn get_internal_pins(&self, node: NodeId, net: NetId) -> Vec<PinId> {
        if self.c.nets[net].stub {
            return Vec::new();
        }
        self.c.nets[net]
            .pins
            .iter()
            .copied()
            .filter(|&p| !self.c.pins[p].stub && self.pin_in_node(p, node))
            .collect()
    }

    /// The parts of this node and of every node below it.
    pub fn subtree_parts(&self, node: NodeId) -> Vec<PartId> {
        let mut parts = self.nodes[node].parts.clone();
        for &child in self.nodes[node].children.values() {
            parts.extend(self.subtree_parts(child));
        }
        parts
    }

    /// The children of a node, in order.
    pub fn children(&self, node: NodeId) -> Vec<NodeId> {
        self.nodes[node].children.values().copied().collect()
    }

    /// The nets that connect parts inside this node (or below it) to parts outside.
    pub fn get_boundary_nets(&self, node: NodeId) -> Vec<NetId> {
        let parts = self.subtree_parts(node);
        let inside: HashSet<PartId> = parts.iter().copied().collect();
        let mut boundary = Vec::new();
        let mut seen: HashSet<NetId> = HashSet::new();
        for &part in &parts {
            for &pin in &self.c.parts[part].pins {
                if !self.c.is_connected(pin) {
                    continue;
                }
                let net = self.c.pins[pin].net.unwrap();
                if !seen.insert(net) {
                    continue;
                }
                if self.c.nets[net].pins.iter().any(|&p| !inside.contains(&self.c.pins[p].part)) {
                    boundary.push(net);
                }
            }
        }
        boundary
    }

    /// Width (mils) left of a sheet for the labels on its sheet pins, rounded up to the grid.
    pub fn sheet_pin_margin(&self, node: NodeId) -> f64 {
        let longest = self
            .get_boundary_nets(node)
            .iter()
            .filter(|&&n| !self.c.nets[n].stub)
            .map(|&n| self.c.nets[n].name.chars().count())
            .max();
        match longest {
            None => 0.0,
            Some(longest) => {
                let width = longest as f64 * LABEL_CHAR_W + LABEL_END;
                (width / 50.0).ceil() * 50.0
            }
        }
    }

    /// The sheet symbol's rectangle, in node coordinates: its bounding box less the label margin.
    pub fn sheet_rect(&self, node: NodeId) -> BBox {
        let bbox = self.nodes[node].bbox;
        BBox::new(Point::new(bbox.ll().x + self.sheet_pin_margin(node), bbox.ll().y), bbox.ur())
    }

    /// The bounding box of a hierarchical sheet as its parent sees it.
    pub fn external_bbox(&self, node: NodeId) -> BBox {
        let n = &self.nodes[node];
        let mut bbox = BBox::new(Point::new(0.0, 0.0), Point::new(500.0, 500.0));
        bbox.add_pt(Point::new(("File: ".len() + n.sheet_filename.chars().count()) as f64 * FILENAME_SZ, 0.0));
        bbox.add_pt(Point::new(("Sheet: ".len() + n.name.chars().count()) as f64 * NAME_SZ, 0.0));

        // The sheet is made tall enough for its pins, which the writer puts
        // one grid apart down its left side.
        let num_pins = self.get_boundary_nets(node).iter().filter(|&&net| !self.c.nets[net].stub).count();
        bbox.add_pt(Point::new(0.0, num_pins as f64 * 100.0));

        bbox = bbox.resize(Point::new(100.0, 100.0));

        // Room on the left for the labels on the sheet pins.
        bbox.add_pt(Point::new(bbox.ll().x - self.sheet_pin_margin(node), bbox.ll().y));
        bbox
    }

    /// The bounding box of the circuitry inside a node.
    pub fn internal_bbox(&self, node: NodeId) -> BBox {
        let mut bbox = BBox::empty();
        for &part in &self.nodes[node].parts {
            let p = &self.c.parts[part];
            bbox.add(p.bbox * p.tx);
        }
        for &child in self.nodes[node].children.values() {
            let ch = &self.nodes[child];
            bbox.add(ch.bbox * ch.tx);
        }
        bbox.resize(Point::new(100.0, 100.0))
    }

    pub fn calc_bbox(&mut self, node: NodeId) -> BBox {
        let bbox = if self.nodes[node].flattened { self.internal_bbox(node) } else { self.external_bbox(node) };
        self.nodes[node].bbox = bbox;
        bbox
    }

    /// Decide which child nodes are drawn on their parent's sheet.
    ///
    /// A node's complexity is the number of part pins it holds. The sum of
    /// the child complexities times the flatness is the number of pins that
    /// can be shown on the sheet before hierarchy is used. Children are taken
    /// by type, smallest first, so simple ones are more likely to be flattened.
    pub fn flatten(&mut self, node: NodeId, flatness: f64) {
        let children = self.children(node);
        for &child in &children {
            self.flatten(child, flatness);
        }

        let mut complexity: usize = self.nodes[node].parts.iter().map(|&p| self.c.parts[p].pins.len()).sum();
        let child_complexity: usize = children.iter().map(|&ch| self.nodes[ch].complexity).sum();
        let mut slack = child_complexity as f64 * flatness;

        // Group the children by type: their names less any trailing instance number.
        let mut types: indexmap::IndexMap<String, Vec<NodeId>> = indexmap::IndexMap::new();
        for (name, &child) in &self.nodes[node].children {
            let kind = name.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
            types.entry(kind).or_default().push(child);
        }
        let mut sizes: Vec<(String, usize)> = types
            .iter()
            .map(|(kind, nodes)| (kind.clone(), nodes.iter().map(|&n| self.nodes[n].complexity).sum()))
            .collect();
        sizes.sort_by_key(|(_, size)| *size);

        for (kind, size) in sizes {
            let flat = slack > 0.0 && size as f64 <= slack;
            for &child in &types[&kind] {
                self.nodes[child].flattened = flat;
            }
            if flat {
                slack -= size as f64;
                complexity += size;
            }
        }
        self.nodes[node].complexity = complexity;
    }

    /// The node and the nodes below it that are drawn on its sheet.
    pub fn sheet_nodes(&self, node: NodeId) -> Vec<NodeId> {
        let mut nodes = vec![node];
        for &child in self.nodes[node].children.values() {
            if self.nodes[child].flattened {
                nodes.extend(self.sheet_nodes(child));
            }
        }
        nodes
    }

    /// The node and every node below it.
    pub fn all_nodes(&self, node: NodeId) -> Vec<NodeId> {
        let mut nodes = vec![node];
        for &child in self.nodes[node].children.values() {
            nodes.extend(self.all_nodes(child));
        }
        nodes
    }
}
