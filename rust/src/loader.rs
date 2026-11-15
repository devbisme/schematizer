//! Build the circuit from a generic netlist document.
//!
//! No KiCad symbol libraries are read: the document carries each symbol's
//! drawing commands and pin geometry.

use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::ir::{Circuit, Net, Part, Pin};
use crate::sexp::Sexp;

pub const SCHEMA_NAME: &str = "skidl-generic-netlist";
pub const SCHEMA_VERSION: i64 = 1;

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// A value that may be a string or a number, as text.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
    }
}

fn check_document(doc: &Value) -> Result<(), String> {
    let fmt = doc.get("format").and_then(|f| f.as_str()).unwrap_or("");
    if fmt != SCHEMA_NAME {
        return Err(format!("Unrecognized netlist format {:?}; expected {:?}.", fmt, SCHEMA_NAME));
    }
    if doc.get("version").and_then(|v| v.as_i64()) != Some(SCHEMA_VERSION) {
        return Err(format!(
            "Unsupported netlist schema version {}; this tool understands version {}.",
            doc.get("version").map(|v| v.to_string()).unwrap_or_else(|| "None".into()),
            SCHEMA_VERSION
        ));
    }
    Ok(())
}

fn orientation(v: Option<&Value>) -> char {
    match v {
        Some(Value::String(s)) => s.chars().next().unwrap_or('R'),
        Some(Value::Number(n)) => {
            match (n.as_f64().unwrap_or(0.0) as i64).rem_euclid(360) {
                90 => 'U',
                180 => 'L',
                270 => 'D',
                _ => 'R',
            }
        }
        _ => 'R',
    }
}

fn make_part(comp: &Value, symbols: &Value, nets: &HashMap<String, usize>, c: &mut Circuit) {
    let lib_id = s(comp, "lib_id");
    let null = Value::Null;
    let sym = symbols.get(&lib_id).unwrap_or(&null);

    let name = {
        let n = s(sym, "name");
        if n.is_empty() {
            lib_id.split_once(':').map_or(lib_id.clone(), |(_, n)| n.to_string())
        } else {
            n
        }
    };
    let mut draw_cmds: IndexMap<i64, Vec<Sexp>> = IndexMap::new();
    if let Some(Value::Object(cmds)) = sym.get("draw_cmds") {
        for (unit, list) in cmds {
            let list = list.as_array().map(|a| a.iter().map(Sexp::from_json).collect()).unwrap_or_default();
            draw_cmds.insert(unit.parse().unwrap_or(0), list);
        }
    }
    let pin_names = sym.get("pin_names").and_then(|p| p.as_object()).map(|p| {
        (
            p.get("offset").and_then(|o| o.as_f64()).unwrap_or(0.0),
            p.get("hide").map(|h| h.as_bool().unwrap_or(!h.is_null())).unwrap_or(false),
        )
    });
    let mut fields = Vec::new();
    if let Some(Value::Object(f)) = comp.get("fields") {
        for (k, v) in f {
            fields.push((k.clone(), text(v)));
        }
    }
    let hiertuple: Vec<String> = match comp.get("hiertuple").and_then(|h| h.as_array()) {
        Some(a) => a.iter().map(text).collect(),
        None => vec![String::new()],
    };
    let ref_prefix = sym.get("ref_prefix").and_then(|r| r.as_str()).unwrap_or("U").to_string();

    let part = Part {
        name,
        base_ref: s(comp, "ref"),
        ref_prefix,
        value: comp.get("value").map(text).unwrap_or_default(),
        footprint: s(comp, "footprint"),
        description: s(sym, "description"),
        datasheet: s(sym, "datasheet"),
        lib: lib_id.split_once(':').map_or(lib_id.clone(), |(l, _)| l.to_string()),
        symtx: s(comp, "symtx"),
        orientation_locked: comp.get("orientation_locked").and_then(|o| o.as_bool()).unwrap_or(false),
        draw_cmds: Arc::new(draw_cmds),
        pin_names,
        fields,
        hiertuple,
        num: 1,
        alive: true,
        ..Default::default()
    };
    let part_id = c.add_part(part);
    c.top_parts.push(part_id);

    // Which net each pin connects to, by pin number.
    let mut net_of_pin: HashMap<String, String> = HashMap::new();
    if let Some(pins) = comp.get("pins").and_then(|p| p.as_array()) {
        for p in pins {
            if let Some(net) = p.get("net").and_then(|n| n.as_str()) {
                net_of_pin.insert(p.get("num").map(text).unwrap_or_default(), net.to_string());
            }
        }
    }

    // Pin geometry lives in the embedded symbol, per unit.
    let mut pins_by_unit: Vec<(i64, Vec<usize>)> = Vec::new();
    if let Some(Value::Object(units)) = sym.get("units") {
        let mut keys: Vec<&String> = units.keys().collect();
        keys.sort();
        for key in keys {
            let unit_num: i64 = key.parse().unwrap_or(1);
            let empty = Vec::new();
            for pg in units[key].get("pins").and_then(|p| p.as_array()).unwrap_or(&empty) {
                let num = pg.get("num").map(text).unwrap_or_default();
                let pin = Pin {
                    num: num.clone(),
                    name: s(pg, "name"),
                    x: pg.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    y: pg.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    orientation: orientation(pg.get("orient")),
                    part: part_id,
                    owner: part_id,
                    ..Default::default()
                };
                let pin_id = c.add_pin(pin);
                c.parts[part_id].pins.push(pin_id);
                match pins_by_unit.iter_mut().find(|(n, _)| *n == unit_num) {
                    Some((_, v)) => v.push(pin_id),
                    None => pins_by_unit.push((unit_num, vec![pin_id])),
                }
                if let Some(net_name) = net_of_pin.get(&num) {
                    if let Some(&net) = nets.get(net_name) {
                        c.connect(pin_id, net);
                    }
                }
            }
        }
    }

    // A part with more than one unit is placed as separate units sharing one reference.
    if pins_by_unit.len() > 1 {
        pins_by_unit.sort_by_key(|(n, _)| *n);
        for (num, pins) in pins_by_unit {
            let label = if num >= 1 {
                format!("u{}", (b'A' + (num - 1) as u8) as char)
            } else {
                format!("u{}", num)
            };
            let parent = &c.parts[part_id];
            let unit = Part {
                name: parent.name.clone(),
                base_ref: parent.base_ref.clone(),
                ref_prefix: parent.ref_prefix.clone(),
                value: parent.value.clone(),
                footprint: parent.footprint.clone(),
                description: parent.description.clone(),
                datasheet: parent.datasheet.clone(),
                lib: parent.lib.clone(),
                draw_cmds: parent.draw_cmds.clone(),
                pin_names: parent.pin_names,
                fields: parent.fields.clone(),
                symtx: parent.symtx.clone(),
                orientation_locked: parent.orientation_locked,
                hiertuple: parent.hiertuple.clone(),
                parent: Some(part_id),
                label,
                num,
                pins,
                alive: true,
                ..Default::default()
            };
            let unit_id = c.add_part(unit);
            c.parts[part_id].units.push(unit_id);
        }
    }
}

/// Build a circuit from a parsed generic netlist document.
pub fn load_netlist(doc: &Value) -> Result<Circuit, String> {
    check_document(doc)?;

    let mut c = Circuit::new();
    let null = Value::Null;
    let symbols = doc.get("symbols").unwrap_or(&null);
    let empty = Vec::new();
    let doc_nets = doc.get("nets").and_then(|n| n.as_array()).unwrap_or(&empty);

    // Create all the nets first so the pins of any component can attach to them.
    let mut nets: HashMap<String, usize> = HashMap::new();
    for n in doc_nets {
        let name = s(n, "name");
        let net = Net {
            name: name.clone(),
            code: n.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
            implicit: n.get("implicit").and_then(|b| b.as_bool()).unwrap_or(false),
            netio: s(n, "netio").to_lowercase(),
            nc: n.get("nc").and_then(|b| b.as_bool()).unwrap_or(false),
            valid: true,
            ..Default::default()
        };
        c.nets.push(net);
        nets.insert(name, c.nets.len() - 1);
    }

    for comp in doc.get("components").and_then(|c| c.as_array()).unwrap_or(&empty) {
        make_part(comp, symbols, &nets, &mut c);
    }

    // Record each pin's electrical type; placement uses it for signal flow.
    let mut pins: HashMap<(String, String), usize> = HashMap::new();
    for &part in &c.top_parts {
        for &pin in &c.parts[part].pins {
            pins.insert((c.parts[part].base_ref.clone(), c.pins[pin].num.clone()), pin);
        }
    }
    for n in doc_nets {
        for node in n.get("nodes").and_then(|x| x.as_array()).unwrap_or(&empty) {
            let key = (s(node, "ref"), node.get("pin").map(text).unwrap_or_default());
            if let Some(&pin) = pins.get(&key) {
                c.pins[pin].func = s(node, "pintype").to_uppercase();
            }
        }
    }

    // Apply explicit stub hints once the pins are connected, so they reach the pins.
    for n in doc_nets {
        if n.get("stub_explicit").and_then(|b| b.as_bool()).unwrap_or(false) {
            let net = nets[&s(n, "name")];
            c.nets[net].stub_explicit = true;
            c.set_net_stub(net, n.get("stub").and_then(|b| b.as_bool()).unwrap_or(false));
        }
    }

    Ok(c)
}
