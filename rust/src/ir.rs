//! The circuit as the engine sees it: parts, pins and nets held in arenas and
//! referred to by index.

use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::geometry::{BBox, Point, Tx};
use crate::sexp::Sexp;

pub type PartId = usize;
pub type PinId = usize;
pub type NetId = usize;
pub type NodeId = usize;

/// Separator between hierarchy levels in a part's hierarchical name.
pub const HIER_SEP: &str = ".";

/// Where a part's Reference and Value go: (x, y, justification) for each, in sheet mm.
/// The justification is 'l', 'r' or 'c' (centered).
pub type FieldSpot = (f64, f64, char);
pub type FieldLayout = (FieldSpot, FieldSpot);

#[derive(Clone, Debug, Default)]
pub struct Pin {
    pub num: String,
    pub name: String,
    /// Symbol coordinates (mm).
    pub x: f64,
    pub y: f64,
    /// 'R', 'U', 'L' or 'D': the direction from the pin's end into the part body.
    pub orientation: char,
    /// The part (or unit) the pin is placed with.
    pub part: PartId,
    /// The whole part the pin belongs to.
    pub owner: PartId,
    pub net: Option<NetId>,
    /// Electrical type from the netlist ("INPUT", "OUTPUT", "PASSIVE", ...), or "".
    pub func: String,
    pub stub: bool,
    pub routed: bool,
    /// Position in part coordinates (mils).
    pub pt: Point,
    /// Placement state.
    pub place_pt: Option<Point>,
    pub route_pt: Point,
    pub face: Option<usize>,
    pub orientation_bias: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Net {
    pub name: String,
    pub code: i64,
    pub pins: Vec<PinId>,
    /// The net is drawn with labels instead of wires.
    pub stub: bool,
    pub stub_explicit: bool,
    pub implicit: bool,
    pub netio: String,
    pub valid: bool,
    /// The no-connect net.
    pub nc: bool,
    pub deferred_stub: bool,
}

pub type DrawCmds = Arc<IndexMap<i64, Vec<Sexp>>>;

#[derive(Clone, Debug, Default)]
pub struct Part {
    pub name: String,
    /// Reference of a whole part ("U1"); for a unit, the parent's.
    pub base_ref: String,
    pub ref_prefix: String,
    pub value: String,
    pub footprint: String,
    pub description: String,
    pub datasheet: String,
    /// Library name ("Device" in "Device:R").
    pub lib: String,
    pub pins: Vec<PinId>,
    /// Units of a multi-unit part (empty for a single-unit part).
    pub units: Vec<PartId>,
    /// The whole part a unit belongs to.
    pub parent: Option<PartId>,
    /// A unit's label ("uA").
    pub label: String,
    /// Unit number.
    pub num: i64,
    pub fields: Vec<(String, String)>,
    pub symtx: String,
    pub orientation_locked: bool,
    pub tx: Tx,
    pub draw_cmds: DrawCmds,
    /// How the symbol shows pin names: (offset, hidden).
    pub pin_names: Option<(f64, bool)>,
    pub hiertuple: Vec<String>,
    pub is_net_terminal: bool,
    /// False once the part has been removed from the circuit.
    pub alive: bool,
    /// The node the part is drawn in.
    pub node: Option<NodeId>,

    // Set by preprocessing.
    pub bbox: BBox,
    pub lbl_bbox: BBox,
    pub body_bbox: Option<BBox>,
    // Set by placement.
    pub place_bbox: Option<BBox>,
    // Set by the writers.
    pub field_layout: Option<FieldLayout>,

    /// Placement scratch state (see engine::place).
    pub pl: crate::engine::place::PartPlace,
}

impl Part {
    /// True for a KiCad power symbol (GND, VCC, ...): it labels its net by itself.
    pub fn is_power_symbol(&self) -> bool {
        self.ref_prefix == "#PWR" && self.pins.len() == 1
    }

    /// The reference that tells this part or unit from every other ("U1" or "U1.uA").
    pub fn ref_(&self) -> String {
        if self.parent.is_some() {
            format!("{}{}{}", self.base_ref, HIER_SEP, self.label)
        } else {
            self.base_ref.clone()
        }
    }

    /// Hierarchical name: the hierarchy path joined with the reference.
    pub fn hiername(&self) -> String {
        let mut names: Vec<String> = self.hiertuple.clone();
        names.push(self.ref_());
        names.join(HIER_SEP)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Circuit {
    pub parts: Vec<Part>,
    pub pins: Vec<Pin>,
    pub nets: Vec<Net>,
    /// The whole parts and net terminals, in the order they were made.
    pub top_parts: Vec<PartId>,
    ref_counts: HashMap<String, usize>,
}

impl Circuit {
    pub fn new() -> Circuit {
        Circuit::default()
    }

    /// Assign a unique reference to a part made without one (a net terminal).
    pub fn next_ref(&mut self, prefix: &str) -> String {
        let n = self.ref_counts.entry(prefix.to_string()).or_insert(0);
        *n += 1;
        format!("{}{}", prefix, n)
    }

    pub fn add_part(&mut self, part: Part) -> PartId {
        self.parts.push(part);
        self.parts.len() - 1
    }

    pub fn add_pin(&mut self, pin: Pin) -> PinId {
        self.pins.push(pin);
        self.pins.len() - 1
    }

    /// Attach a pin to a net.
    pub fn connect(&mut self, pin: PinId, net: NetId) {
        self.pins[pin].net = Some(net);
        if !self.nets[net].pins.contains(&pin) {
            self.nets[net].pins.push(pin);
        }
    }

    /// True if the pin is attached to a real (not the no-connect) net.
    pub fn is_connected(&self, pin: PinId) -> bool {
        match self.pins[pin].net {
            Some(n) => !self.nets[n].nc,
            None => false,
        }
    }

    /// Mark a net as stubbed or not, along with every pin on it.
    pub fn set_net_stub(&mut self, net: NetId, value: bool) {
        self.nets[net].stub = value;
        for i in 0..self.nets[net].pins.len() {
            let p = self.nets[net].pins[i];
            self.pins[p].stub = value;
        }
    }

    /// The parts that get placed for a whole part: its units, or itself.
    pub fn units(&self, part: PartId) -> Vec<PartId> {
        if self.parts[part].units.is_empty() {
            vec![part]
        } else {
            self.parts[part].units.clone()
        }
    }

    /// Point each pin at the unit it's placed with.
    pub fn grab_pins(&mut self, unit: PartId) {
        for i in 0..self.parts[unit].pins.len() {
            let p = self.parts[unit].pins[i];
            self.pins[p].part = unit;
        }
    }

    /// Hand the pins of a part's units back to the part.
    pub fn release_pins(&mut self, part: PartId) {
        for u in self.parts[part].units.clone() {
            for i in 0..self.parts[u].pins.len() {
                let p = self.parts[u].pins[i];
                self.pins[p].part = part;
            }
        }
    }

    /// Remove the net terminals, taking their pins off their nets.
    pub fn remove_net_terminals(&mut self) {
        let terminals: Vec<PartId> =
            self.top_parts.iter().copied().filter(|&p| self.parts[p].is_net_terminal).collect();
        for part in &terminals {
            for pin in self.parts[*part].pins.clone() {
                if let Some(net) = self.pins[pin].net.take() {
                    self.nets[net].pins.retain(|&p| p != pin);
                }
            }
            self.parts[*part].alive = false;
        }
        self.top_parts.retain(|p| !terminals.contains(p));
    }

    pub fn pin_part(&self, pin: PinId) -> &Part {
        &self.parts[self.pins[pin].part]
    }

    pub fn net_name(&self, pin: PinId) -> &str {
        match self.pins[pin].net {
            Some(n) => &self.nets[n].name,
            None => "",
        }
    }

    /// Heuristic similarity of two parts (higher is more alike), so the placer groups like parts.
    pub fn similarity(&self, a: PartId, b: PartId) -> f64 {
        let (pa, pb) = (&self.parts[a], &self.parts[b]);
        let score_pins = || -> f64 {
            let mut s = 0.0;
            if pa.pins.len() == pb.pins.len() {
                let mut o1 = pa.pins.clone();
                let mut o2 = pb.pins.clone();
                o1.sort_by(|x, y| self.pins[*x].num.cmp(&self.pins[*y].num));
                o2.sort_by(|x, y| self.pins[*x].num.cmp(&self.pins[*y].num));
                for (x, y) in o1.iter().zip(o2.iter()) {
                    if self.pins[*x].net.is_some() && self.pins[*x].net == self.pins[*y].net {
                        s += 1.0;
                    }
                }
            }
            s
        };
        let mut score = 1.0;
        if pa.description == pb.description {
            score += 5.0;
        }
        if pa.name == pb.name {
            score += 5.0;
            if pa.value == pb.value {
                score += 2.0;
            }
            score += score_pins();
        } else if pa.ref_prefix == pb.ref_prefix {
            score += 3.0;
            if pa.value == pb.value {
                score += 2.0;
            }
            score += score_pins();
        }
        score / 3.0
    }
}
