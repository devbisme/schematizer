//! The placement, routing and writing engine.

use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::geometry::{BBox, Point, Segment, Tx};
use crate::ir::{Circuit, NetId, NodeId, PartId, PinId};

pub mod bboxes;
pub mod constants;
pub mod conventions;
pub mod decisions;
pub mod gen_schematic;
pub mod kicad;
pub mod labels;
pub mod layouts;
pub mod node;
pub mod nudge;
pub mod place;
pub mod preplace;
pub mod progress;
pub mod rng;
pub mod snap;
pub mod svg;
pub mod route;

/// Why a drawing couldn't be made.
#[derive(Clone, Debug)]
pub enum Error {
    /// A bad argument or option.
    Value(String),
    /// The parts couldn't be placed.
    Placement(String),
    /// The wires couldn't be routed.
    Routing(String),
    /// KiCad's netlist of the schematic differs from the input.
    NetlistMismatch(Vec<String>),
    /// Anything else (files that can't be read or written, a missing tool).
    Runtime(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Value(s) | Error::Placement(s) | Error::Routing(s) | Error::Runtime(s) => write!(f, "{}", s),
            Error::NetlistMismatch(problems) => write!(f, "{}", problems.join("; ")),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Runtime(e.to_string())
    }
}

pub type Res<T> = Result<T, Error>;

impl Error {
    /// True for a failure to place or route, which another try may get past.
    pub fn is_layout_failure(&self) -> bool {
        matches!(self, Error::Placement(_) | Error::Routing(_))
    }
}

/// What to do when routing fails with auto-stubbing on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fallback {
    Labels,
    Raise,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Kicad,
    Svg,
}

/// Options for placement, routing and writing.
#[derive(Clone, Debug)]
pub struct Options {
    pub tool: String,
    pub format: Format,
    pub flatness: f64,
    pub retries: usize,
    pub seed: Option<String>,
    pub auto_stub: bool,
    pub auto_stub_fanout: usize,
    pub auto_stub_max_wire_pins: usize,
    pub auto_stub_max_wire_dist: f64,
    pub auto_stub_fallback: Fallback,
    pub erc_max_iterations: usize,
    pub power_symbols: bool,
    pub preplace: bool,
    pub nudge: bool,
    pub sheet_tries: usize,
    pub reuse_layouts: bool,
    pub label_local_nets: bool,
    pub label_clearance: bool,
    pub place_fields: bool,
    pub svg_fit: bool,
    pub orientation_pin_limit: usize,
    pub dont_rotate_pin_count: usize,
    pub use_push_pull: bool,
    pub rotate_parts: bool,
    pub pt_to_pt_mult: f64,
    pub pin_normalize: bool,
    pub expansion_factor: f64,
    pub route_sheets: bool,
    pub compress_before_place: bool,
    pub fanout_attenuation: bool,
    pub normalize: bool,
    pub remove_power: bool,
    pub allow_routing_failure: bool,
    pub seed_ratio: Option<f64>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            tool: "kicad9".into(),
            format: Format::Kicad,
            flatness: 0.0,
            retries: 2,
            seed: None,
            auto_stub: false,
            auto_stub_fanout: 5,
            auto_stub_max_wire_pins: 3,
            auto_stub_max_wire_dist: 2000.0,
            auto_stub_fallback: Fallback::Labels,
            erc_max_iterations: 3,
            power_symbols: true,
            preplace: true,
            nudge: true,
            sheet_tries: 1,
            reuse_layouts: true,
            label_local_nets: false,
            label_clearance: false,
            place_fields: true,
            svg_fit: true,
            orientation_pin_limit: 44,
            dont_rotate_pin_count: 10000,
            use_push_pull: true,
            rotate_parts: true,
            pt_to_pt_mult: 5.0,
            pin_normalize: true,
            expansion_factor: 1.0,
            route_sheets: true,
            compress_before_place: false,
            fanout_attenuation: false,
            normalize: false,
            remove_power: false,
            allow_routing_failure: false,
            seed_ratio: None,
        }
    }
}

/// A node in the circuit hierarchy: the parts of one subcircuit and the nodes below it.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub parts: Vec<PartId>,
    pub parent: Option<NodeId>,
    pub children: IndexMap<String, NodeId>,
    pub sheet_filename: String,
    pub title: String,
    /// True if the node is drawn on its parent's sheet instead of one of its own.
    pub flattened: bool,
    pub wires: IndexMap<NetId, Vec<Segment>>,
    pub junctions: IndexMap<NetId, Vec<Point>>,
    pub tx: Tx,
    pub bbox: BBox,
    pub complexity: usize,
    pub uuid: String,
    /// Names of the hierarchical labels the node's sheet holds, for its sheet pins.
    pub hier_label_names: Vec<String>,
    /// Where the pins on the node's sheet are (mm x 100).
    pub pin_anchor_pts: HashSet<(i64, i64)>,
    // Extra wires, junctions and suppressed labels from snapping parts onto pins.
    pub tjunction_wires: Vec<(f64, f64, f64, f64)>,
    pub tjunction_junctions: Vec<Point>,
    pub tjunction_suppressed_pins: HashSet<PinId>,
    pub power_cap_wires: Vec<(f64, f64, f64, f64)>,
    pub power_cap_junctions: Vec<Point>,
    pub power_cap_suppressed_pins: HashSet<PinId>,
}

/// A circuit, its tree of nodes, and everything needed to draw it.
pub struct Design {
    pub c: Circuit,
    pub nodes: Vec<Node>,
    pub root: NodeId,
    pub opts: Options,
    pub rng: rng::Rng,
    pub filepath: PathBuf,
    pub top_name: String,
    pub title: String,
    pub kc: kicad::WriterState,
}
