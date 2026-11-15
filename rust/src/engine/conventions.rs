//! Drafting conventions people follow when drawing schematics.
//!
//! * Supply rails are at the top and grounds at the bottom, so a pin on a
//!   supply net should have its wire leave upward and a pin on a ground net
//!   downward.
//! * Signals flow left to right, so wires should enter input pins from the
//!   left and leave output pins to the right.
//! * ICs are drawn the way their symbols were designed: they may be mirrored
//!   but not turned on their sides.

use crate::geometry::{pyround, Point, Tx};
use crate::ir::{Circuit, NetId, PartId, PinId};

pub const UP: (i32, i32) = (0, 1);
pub const DOWN: (i32, i32) = (0, -1);
pub const LEFT: (i32, i32) = (-1, 0);
pub const RIGHT: (i32, i32) = (1, 0);

pub const RAIL_MIN_PINS: usize = 4;
/// Parts with more pins than this are ICs that don't get turned on their sides.
pub const MAX_ROTATED_PINS: usize = 3;
pub const SIDEWAYS_IC_COST: i64 = 1000;
pub const RAIL_COST: i64 = 10;
pub const FLOW_COST: i64 = 1;

const POWER_NAMES: [&str; 14] = [
    "GND", "AGND", "DGND", "PGND", "VCC", "VDD", "VSS", "VEE", "VBUS", "VBAT", "AVCC", "AVDD", "DVCC", "DVDD",
];

/// True if a net with this name is a supply rail or a ground.
pub fn is_power_net_name(name: &str) -> bool {
    name.starts_with('+') || POWER_NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// True if a net with this name is a ground or a negative supply.
pub fn is_ground_net_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.contains("GND") || matches!(upper.as_str(), "VSS" | "VEE" | "GROUND") || name.starts_with('-')
}

/// Direction a wire leaves a pin, by pin orientation, in unrotated part coordinates.
pub fn pin_exit_vec(orientation: char) -> Point {
    match orientation {
        'U' => Point::new(0.0, -1.0),
        'D' => Point::new(0.0, 1.0),
        'L' => Point::new(1.0, 0.0),
        'R' => Point::new(-1.0, 0.0),
        _ => Point::new(0.0, 0.0),
    }
}

fn rail_names(c: &Circuit, net: NetId) -> Vec<String> {
    let mut names = vec![c.nets[net].name.clone()];
    for &pin in &c.nets[net].pins {
        let part = c.pin_part(pin);
        if part.is_power_symbol() {
            names.push(if part.value.is_empty() { part.name.clone() } else { part.value.clone() });
        }
    }
    names
}

/// True if a net is a supply rail or a ground: named like one, or going to a
/// power pin and enough other pins that it's distributing power.
pub fn is_power_net(c: &Circuit, net: NetId) -> bool {
    let n = &c.nets[net];
    if is_power_net_name(&n.name) {
        return true;
    }
    n.pins.len() >= RAIL_MIN_PINS
        && n.pins.iter().any(|&p| matches!(c.pins[p].func.as_str(), "POWER-IN" | "POWER-OUT"))
}

/// Which way a net's power symbol points: UP for a rail, DOWN for a ground, None for a signal.
pub fn rail_dir(c: &Circuit, net: Option<NetId>) -> Option<(i32, i32)> {
    let net = net?;
    let names = rail_names(c, net);
    if names.iter().any(|n| is_ground_net_name(n)) {
        return Some(DOWN);
    }
    if is_power_net(c, net) || names.iter().any(|n| is_power_net_name(n)) {
        return Some(UP);
    }
    None
}

/// The rotation/flip part of a transform, without its translation.
pub fn linear(tx: Tx) -> Tx {
    tx.no_translate()
}

/// The direction a pin's wire leaves it when its part has the given transform.
pub fn pin_exit(c: &Circuit, pin: PinId, tx: Tx) -> Point {
    let lin = linear(tx);
    let p = &c.pins[pin];
    (p.pt + pin_exit_vec(p.orientation)) * lin - p.pt * lin
}

/// The eight orientations (four rotations, each mirrored or not) of a transform.
pub fn orientations(tx: Tx) -> Vec<Tx> {
    let mut lin = linear(tx);
    let mut txs = Vec::with_capacity(8);
    for _ in 0..2 {
        for _ in 0..4 {
            txs.push(lin);
            lin = lin.rot_90cw();
        }
        lin = lin.flip_x();
    }
    txs
}

fn wanted_exit(c: &Circuit, pin: PinId) -> (Option<(i32, i32)>, i64) {
    let p = &c.pins[pin];
    if let Some(rail) = rail_dir(c, p.net) {
        return (Some(rail), RAIL_COST);
    }
    if p.net.is_some() {
        match p.func.as_str() {
            "INPUT" => return (Some(LEFT), FLOW_COST),
            "OUTPUT" => return (Some(RIGHT), FLOW_COST),
            _ => {}
        }
    }
    (None, 0)
}

/// Score how badly an orientation of a part breaks the drafting conventions (0 is best).
pub fn orientation_cost(c: &Circuit, part: PartId, tx: Tx) -> i64 {
    let p = &c.parts[part];
    let mut cost = 0;
    if p.pins.len() > MAX_ROTATED_PINS && (tx.b != 0.0 || tx.c != 0.0) {
        cost += SIDEWAYS_IC_COST;
    }
    for &pin in &p.pins {
        let (wanted, pin_cost) = wanted_exit(c, pin);
        if let Some(w) = wanted {
            let e = pin_exit(c, pin, tx);
            // Sideways costs once; backwards (dot product of -1) costs twice.
            let along = pyround(e.x) as i64 * w.0 as i64 + pyround(e.y) as i64 * w.1 as i64;
            cost += pin_cost * (1 - along);
        }
    }
    // Satellites fixed beside the part (see preplace) turn with it.
    for &rider in &p.pl.riders {
        cost += orientation_cost(c, rider, c.parts[rider].pl.rel_tx * tx);
    }
    cost
}

/// The orientations of a part that break the conventions the least, its current one first if it's among them.
pub fn best_orientations(c: &Circuit, part: PartId) -> Vec<Tx> {
    let costed: Vec<(i64, Tx)> =
        orientations(c.parts[part].tx).into_iter().map(|tx| (orientation_cost(c, part, tx), tx)).collect();
    let least = costed.iter().map(|(c, _)| *c).min().unwrap_or(0);
    costed.into_iter().filter(|(c, _)| *c == least).map(|(_, tx)| tx).collect()
}

/// The orientation closest to the part's current one that best follows the conventions.
pub fn conventional_orientation(c: &Circuit, part: PartId) -> Tx {
    best_orientations(c, part)[0]
}
