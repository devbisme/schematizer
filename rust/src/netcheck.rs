//! Check that a written KiCad schematic connects what its netlist says.
//!
//! KiCad exports the netlist of the schematic that was written, and each
//! net's pins are compared with the pins of the nets in the netlist document.
//! This catches what the router's own checks can't see: two nets joined by
//! touching wires, or a label, sheet pin or power symbol that doesn't connect.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use crate::engine::Error;

/// How many differing nets an error message lists.
pub const MAX_REPORTED: usize = 8;

type PinRef = (String, String);
type PinSet = BTreeSet<PinRef>;

/// KiCad leaves "#" references (power symbols, flags) out of its netlists.
fn counts(pin: &PinRef) -> bool {
    !pin.0.starts_with('#')
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The pins of each net of a netlist document, by net name.
///
/// The no-connect net and nets with fewer than two pins are left out, since
/// they connect nothing.
pub fn source_nets(doc: &Value) -> BTreeMap<String, PinSet> {
    let mut nets = BTreeMap::new();
    for net in doc.get("nets").and_then(|n| n.as_array()).into_iter().flatten() {
        if net.get("nc").and_then(|b| b.as_bool()).unwrap_or(false) {
            continue;
        }
        let pins: PinSet = net
            .get("nodes")
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
            .map(|node| (text(&node["ref"]), text(&node["pin"])))
            .filter(counts)
            .collect();
        if pins.len() > 1 {
            let name = match net.get("name").and_then(|n| n.as_str()) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => format!("net {}", net.get("code").map(text).unwrap_or_default()),
            };
            nets.insert(name, pins);
        }
    }
    nets
}

/// Find `kicad-cli` on the PATH.
pub fn find_kicad_cli() -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "kicad-cli.exe" } else { "kicad-cli" };
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join(exe)).find(|p| p.is_file()))
}

/// The value of the first quoted string after `key` in `text`.
fn quoted_after<'a>(text: &'a str, key: &str) -> Option<(&'a str, usize)> {
    let start = text.find(key)? + key.len();
    let end = text[start..].find('"')? + start;
    Some((&text[start..end], end))
}

/// The pin sets of the nets KiCad finds in a schematic file.
pub fn exported_nets(schematic_file: &Path) -> Result<Vec<PinSet>, Error> {
    let kicad_cli = find_kicad_cli()
        .ok_or_else(|| Error::Runtime("Checking the netlist needs kicad-cli, which isn't installed.".into()))?;
    // A directory of its own, since checks can run at the same time on different threads.
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work_dir = std::env::temp_dir().join(format!("schematizer-netcheck-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&work_dir)?;
    let netlist_file = work_dir.join("exported.net");
    let result = Command::new(kicad_cli)
        .args(["sch", "export", "netlist", "--format", "kicadsexpr", "-o"])
        .arg(&netlist_file)
        .arg(schematic_file)
        .output();
    let text = std::fs::read_to_string(&netlist_file);
    let _ = std::fs::remove_dir_all(&work_dir);
    let text = match (result, text) {
        (Err(e), _) => return Err(Error::Runtime(format!("kicad-cli couldn't export the netlist: {}", e))),
        (Ok(out), Err(_)) => {
            let detail = if out.stderr.is_empty() { out.stdout } else { out.stderr };
            return Err(Error::Runtime(format!(
                "kicad-cli couldn't export the netlist: {}",
                String::from_utf8_lossy(&detail).trim()
            )));
        }
        (Ok(_), Ok(text)) => text,
    };

    let mut nets = Vec::new();
    let Some(start) = text.find("(nets") else { return Ok(nets) };
    for block in text[start..].split("(net (code").skip(1) {
        let mut pins = PinSet::new();
        let mut rest = block;
        while let Some((ref_, end)) = quoted_after(rest, "(node (ref \"") {
            rest = &rest[end..];
            if let Some(tail) = rest.strip_prefix("\") (pin \"") {
                if let Some(e) = tail.find('"') {
                    let pin = (ref_.to_string(), tail[..e].to_string());
                    if counts(&pin) {
                        pins.insert(pin);
                    }
                }
            }
        }
        if pins.len() > 1 {
            nets.push(pins);
        }
    }
    Ok(nets)
}

fn pins_text(pins: &PinSet) -> String {
    let limit = 4;
    let mut names: Vec<String> = pins.iter().map(|(r, n)| format!("{}.{}", r, n)).collect();
    names.sort();
    let more = names.len() as i64 - limit as i64;
    let shown = names[..names.len().min(limit)].join(", ");
    if more > 0 {
        format!("{} and {} more", shown, more)
    } else {
        shown
    }
}

/// A sentence for each way the drawn nets differ from the source nets.
pub fn compare_nets(source: &BTreeMap<String, PinSet>, drawn: &[PinSet]) -> Vec<String> {
    let mut net_of: HashMap<&PinRef, &String> = HashMap::new();
    for (name, pins) in source {
        for pin in pins {
            net_of.insert(pin, name);
        }
    }
    let mut group_of: HashMap<&PinRef, usize> = HashMap::new();
    for (i, pins) in drawn.iter().enumerate() {
        for pin in pins {
            group_of.insert(pin, i);
        }
    }
    let mut problems = Vec::new();
    let mut reported_shorts: HashSet<usize> = HashSet::new();
    for (name, pins) in source {
        if drawn.contains(pins) {
            continue;
        }
        let groups: BTreeSet<usize> = pins.iter().filter_map(|p| group_of.get(p).copied()).collect();
        // Joined to pins of other nets.
        for &i in &groups {
            let extra: PinSet = drawn[i].difference(pins).cloned().collect();
            let mut others: Vec<String> =
                extra.iter().filter_map(|p| net_of.get(p).map(|n| n.to_string())).collect::<BTreeSet<_>>().into_iter().collect();
            let stray: PinSet = extra.iter().filter(|p| !net_of.contains_key(p)).cloned().collect();
            if !stray.is_empty() {
                others.push(pins_text(&stray));
            }
            if !others.is_empty() && reported_shorts.insert(i) {
                problems.push(format!("{} is shorted to {}", name, others.join(", ")));
            }
        }
        // Split up, or missing pins.
        let loose = pins.iter().filter(|p| !group_of.contains_key(p)).count();
        let pieces = groups.len() + loose;
        if groups.is_empty() {
            problems.push(format!("none of the pins of {} are connected ({})", name, pins_text(pins)));
        } else if pieces > 1 {
            let biggest: PinSet = groups
                .iter()
                .map(|&g| drawn[g].intersection(pins).cloned().collect::<PinSet>())
                .max_by_key(|s| s.len())
                .unwrap_or_default();
            let apart: PinSet = pins.difference(&biggest).cloned().collect();
            problems.push(format!("{} is in {} pieces ({} not connected to the rest)", name, pieces, pins_text(&apart)));
        }
    }
    // Pins the netlist leaves alone that the drawing joined to each other.
    for (i, pins) in drawn.iter().enumerate() {
        if !reported_shorts.contains(&i) && !pins.iter().any(|p| net_of.contains_key(p)) {
            problems.push(format!("{} are connected but shouldn't be", pins_text(pins)));
        }
    }
    problems
}

/// The message for a schematic that doesn't match its netlist.
pub fn mismatch_message(problems: &[String]) -> String {
    let shown = &problems[..problems.len().min(MAX_REPORTED)];
    let more = problems.len() - shown.len();
    let tail = if more > 0 { format!("; and {} more", more) } else { String::new() };
    format!("The schematic doesn't match its netlist: {}{}.", shown.join("; "), tail)
}

/// Fail with `Error::NetlistMismatch` if the schematic's connections differ from the document's.
pub fn check_netlist(doc: &Value, schematic_file: &Path) -> Result<(), Error> {
    let problems = compare_nets(&source_nets(doc), &exported_nets(schematic_file)?);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::NetlistMismatch(problems))
    }
}
