//! Drive placement, routing and writing of a schematic, trying again when a step fails.

use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::geometry::{BBox, Point, Tx};
use crate::ir::{NetId, NodeId, PartId, PinId};
use crate::netcheck::find_kicad_cli;

use super::bboxes::{calc_hier_label_bbox, calc_symbol_bbox};
use super::conventions::{conventional_orientation, is_power_net};
use super::{progress, Design, Error, Fallback, Format, Res};

/// Pin count above which auto-stubbing turns a net into labels.
pub const AUTO_STUB_PINS: usize = 3;

const MM_TO_MILS: f64 = 1.0 / 0.0254;

fn fixable(error_type: &str) -> bool {
    matches!(error_type, "pin_not_connected" | "pin_not_driven" | "wire_not_connected")
}

impl Design {
    fn stub_net_and_pins(&mut self, net: NetId) {
        self.c.nets[net].stub = true;
        self.c.nets[net].stub_explicit = false;
        for pin in self.c.nets[net].pins.clone() {
            self.c.pins[pin].stub = true;
        }
    }

    /// Stub the power nets so they're drawn with power symbols instead of wires.
    ///
    /// A supply rail or ground usually goes to most of the parts on a sheet.
    /// People don't wire those together; they put a power symbol on each pin.
    /// Nets whose stubbing the user set are left alone, and with `keep_drawn`
    /// so are nets that already have a power symbol part of their own.
    pub fn stub_power_nets(&mut self, keep_drawn: bool) -> Vec<NetId> {
        let mut stubbed = Vec::new();
        for net in 0..self.c.nets.len() {
            let n = &self.c.nets[net];
            if n.stub_explicit || !n.valid || n.pins.is_empty() || !is_power_net(&self.c, net) {
                continue;
            }
            if keep_drawn && n.pins.iter().any(|&p| self.c.pin_part(p).is_power_symbol()) {
                continue;
            }
            self.stub_net_and_pins(net);
            stubbed.push(net);
        }
        stubbed
    }

    /// Stub the power nets, and mark nets with many pins to be stubbed once
    /// the parts are placed (so placement can still group connected parts).
    fn auto_stub_nets(&mut self) {
        let threshold = self.opts.auto_stub_fanout;
        self.stub_power_nets(false);
        for n in self.c.nets.iter_mut() {
            if n.stub_explicit || n.stub || !n.valid || n.pins.is_empty() {
                continue;
            }
            if n.pins.len() >= threshold {
                n.deferred_stub = true;
            }
        }
    }

    /// Stub a pin that's the only one of its net on a sheet, so it's drawn with a label.
    ///
    /// The label is what connects such a pin to the rest of its net on other
    /// sheets. Putting it on the pin saves placing a label somewhere else on
    /// the sheet and routing a wire to it.
    pub fn stub_lone_pins(&mut self) {
        for net in 0..self.c.nets.len() {
            let n = &self.c.nets[net];
            if n.stub || !n.valid || n.nc {
                continue;
            }
            let mut sheets: IndexMap<Vec<String>, Vec<PinId>> = IndexMap::new();
            for &pin in &n.pins {
                let part = self.c.pin_part(pin);
                if !part.is_net_terminal {
                    sheets.entry(part.hiertuple.clone()).or_default().push(pin);
                }
            }
            if sheets.len() < 2 && n.pins.len() != 1 {
                // All of the net is on one sheet, where it's wired.
                continue;
            }
            for pins in sheets.values() {
                // (A power symbol labels its net by itself.)
                if pins.len() == 1 && !self.c.pin_part(pins[0]).is_power_symbol() {
                    self.c.pins[pins[0]].stub = true;
                }
            }
        }
    }

    /// Get the parts ready for placement and routing: orientation, pin positions and bounding boxes.
    pub fn preprocess_circuit(&mut self) {
        for part in self.c.top_parts.clone() {
            let units = self.c.units(part);

            // Initialize the part or its units.
            for &unit in &units {
                let symtx = self.c.parts[unit].symtx.clone();
                self.c.parts[unit].tx = Tx::from_symtx(&symtx);
                let num_pins = self.c.parts[unit].pins.len();
                self.c.parts[unit].orientation_locked =
                    !symtx.is_empty() || !(1 < num_pins && num_pins <= self.opts.orientation_pin_limit);
                self.c.grab_pins(unit);
                for pin in self.c.parts[unit].pins.clone() {
                    let p = &mut self.c.pins[pin];
                    // Symbol coordinates are mm; the engine works in mils.
                    p.pt = Point::new(p.x * MM_TO_MILS, p.y * MM_TO_MILS);
                    p.routed = false;
                }
            }

            // Orient the part so rail pins face up, ground pins down, and
            // signals flow left to right. A part whose orientation the user
            // set is left alone, and a power symbol is drawn the way its
            // library draws it.
            if self.c.parts[part].symtx.is_empty() && !self.c.parts[part].is_power_symbol() {
                for &unit in &units {
                    if self.c.parts[unit].pins.len() > self.opts.dont_rotate_pin_count {
                        break;
                    }
                    self.c.parts[unit].tx = conventional_orientation(&self.c, unit);
                }
            }

            // Bounding boxes, with and without the labels of stubbed pins.
            let bare_bboxes = calc_symbol_bbox(&self.c, part);
            for (&unit, bare) in units.iter().zip(bare_bboxes) {
                let mut resize = Point::new(0.0, 0.0);
                if bare.w() < 100.0 {
                    resize.x = (100.0 - bare.w()) / 2.0;
                }
                if bare.h() < 100.0 {
                    resize.y = (100.0 - bare.h()) / 2.0;
                }
                let bare = bare.resize(resize);
                let mut lbl = BBox::empty();
                lbl.add(bare);
                for &pin in &self.c.parts[unit].pins {
                    let p = &self.c.pins[pin];
                    if p.stub {
                        let hlbl = calc_hier_label_bbox(self.c.net_name(pin), p.orientation) * Tx::IDENT.move_by(p.pt);
                        lbl.add(hlbl);
                    }
                }
                let u = &mut self.c.parts[unit];
                // The bare symbol box is used by the writers to place the Reference and Value.
                u.body_bbox = Some(bare);
                u.lbl_bbox = lbl;
                u.bbox = lbl;
            }
        }
    }

    /// Get the circuit ready for placement and build its tree of nodes.
    pub fn new_node_tree(&mut self) {
        self.stub_lone_pins();
        self.preprocess_circuit();
        self.add_circuit();
    }

    /// Restore the parts and nets once placement and routing are done with them.
    pub fn finalize_parts_and_nets(&mut self) {
        self.c.remove_net_terminals();
        for part in self.c.top_parts.clone() {
            self.c.release_pins(part);
            let p = &mut self.c.parts[part];
            p.body_bbox = None;
            p.place_bbox = None;
            p.tx = Tx::IDENT;
        }
    }

    /// Stub the deferred nets on each sheet below the top one, now that the parts are placed.
    fn apply_deferred_stubs(&mut self) {
        // Start over, so this can be done again on a retry.
        for net in 0..self.c.nets.len() {
            if self.c.nets[net].deferred_stub {
                self.c.set_net_stub(net, false);
            }
        }
        for child in self.children(self.root) {
            let mut seen: HashSet<NetId> = HashSet::new();
            for part in self.nodes[child].parts.clone() {
                for pin in self.c.parts[part].pins.clone() {
                    if !self.c.is_connected(pin) {
                        continue;
                    }
                    let net = self.c.pins[pin].net.unwrap();
                    if !seen.insert(net) || !self.c.nets[net].deferred_stub {
                        continue;
                    }
                    // Labels at pins are cleaner than routed wires for these.
                    for p in self.c.nets[net].pins.clone() {
                        if self.pin_in_node(p, child) {
                            self.c.pins[p].stub = true;
                        }
                    }
                }
            }
        }
        // A net is stubbed if all of its pins ended up stubbed.
        for n in 0..self.c.nets.len() {
            if self.c.nets[n].deferred_stub && self.c.nets[n].pins.iter().all(|&p| self.c.pins[p].stub) {
                self.c.nets[n].stub = true;
                self.c.nets[n].stub_explicit = false;
            }
        }
        // Resetting the deferred nets also unstubbed their lone pins.
        self.stub_lone_pins();
    }

    /// Once the parts are placed, stub the nets with too many pins or pins too far apart.
    fn classify_and_stub_complex_nets(&mut self) {
        let max_wire_pins = self.opts.auto_stub_max_wire_pins;
        let max_wire_dist = self.opts.auto_stub_max_wire_dist;

        let mut targets = vec![self.root];
        targets.extend(self.children(self.root));
        for &target in &targets {
            for net in self.get_internal_nets(target) {
                if self.c.nets[net].stub_explicit || self.c.nets[net].stub {
                    continue;
                }
                let pins: Vec<PinId> = self.c.nets[net]
                    .pins
                    .iter()
                    .copied()
                    .filter(|&p| self.pin_in_node(p, target) && !self.c.pin_part(p).is_net_terminal)
                    .collect();
                if pins.len() > max_wire_pins {
                    self.stub_net_and_pins(net);
                    continue;
                }
                if pins.len() >= 2 {
                    let pts: Vec<Point> = pins
                        .iter()
                        .map(|&p| {
                            let pin = &self.c.pins[p];
                            pin.place_pt.unwrap_or(pin.pt) * self.c.pin_part(p).tx
                        })
                        .collect();
                    let mut max_dist: f64 = 0.0;
                    for (i, a) in pts.iter().enumerate() {
                        for b in &pts[i + 1..] {
                            max_dist = max_dist.max((a.x - b.x).abs() + (a.y - b.y).abs());
                        }
                    }
                    if max_dist > max_wire_dist {
                        self.stub_net_and_pins(net);
                    }
                }
            }
        }

        // Second pass on the children: stub the nets that wiring can't help.
        for child in self.children(self.root) {
            let internal = self.get_internal_nets(child);
            // A net with a single pin of a real part only needs a label.
            for &net in &internal {
                if self.c.nets[net].stub {
                    continue;
                }
                let real = self.c.nets[net]
                    .pins
                    .iter()
                    .filter(|&&p| self.pin_in_node(p, child) && !self.c.pin_part(p).is_net_terminal)
                    .count();
                if real < 2 {
                    self.stub_net_and_pins(net);
                }
            }
            // With force-directed placement, a small subcircuit on a sheet of
            // its own is drawn with labels alone. (Pre-placed parts sit beside
            // the pins they're wired to, so their short wires are kept.)
            let non_stubbed: Vec<NetId> = internal.iter().copied().filter(|&n| !self.c.nets[n].stub).collect();
            if !self.opts.preplace && !self.nodes[child].flattened && non_stubbed.len() <= 6 {
                for net in non_stubbed {
                    self.stub_net_and_pins(net);
                }
            }
        }
    }

    /// Place the design and, with auto-stubbing on, decide which nets become labels.
    fn place_and_classify(&mut self, expansion_factor: f64, classify: bool) -> Res<()> {
        self.opts.expansion_factor = expansion_factor;
        self.place(self.root)?;
        if classify && self.opts.auto_stub {
            self.apply_deferred_stubs();
            self.classify_and_stub_complex_nets();
        }
        Ok(())
    }

    /// Snap two-pin parts onto the pins they connect to, if auto-stubbing left them loose.
    ///
    /// This is for force-directed placement, which drops such parts anywhere.
    /// Pre-placed parts are already beside their pins with wires routed to them.
    fn snap_unwired_parts(&mut self) {
        if self.opts.auto_stub && !self.opts.preplace {
            self.snap_two_pin_parts(self.root);
        }
    }

    /// Write the placed and routed design in the requested format.
    fn write_output(&mut self) -> Res<PathBuf> {
        progress::report("writing files");
        match self.opts.format {
            Format::Svg => self.write_top_svg(),
            Format::Kicad => self.write_top_schematic(),
        }
    }

    /// Stub the nets of the pins KiCad's ERC complained about. Returns true if any changed.
    fn stub_nets_for_erc_errors(&mut self, errors: &[(String, String, String)]) -> bool {
        let mut stubbed_any = false;
        for (error_type, symbol_ref, pin_num) in errors {
            if !fixable(error_type) {
                continue;
            }
            let Some(&part) = self.c.top_parts.iter().find(|&&p| self.c.parts[p].base_ref == *symbol_ref) else {
                continue;
            };
            for pin in self.c.parts[part].pins.clone() {
                if self.c.pins[pin].num != *pin_num {
                    continue;
                }
                if let Some(net) = self.c.pins[pin].net {
                    if !self.c.nets[net].stub_explicit {
                        // (If every pin already has a label, drawing again wouldn't change anything.)
                        if !self.c.nets[net].pins.iter().all(|&p| self.c.pins[p].stub) {
                            stubbed_any = true;
                        }
                        self.stub_net_and_pins(net);
                    }
                }
            }
        }
        stubbed_any
    }

    /// Handle a routing failure the way the auto-stub fallback option says.
    fn handle_fallback(&mut self, reason: &str) -> Res<()> {
        if self.opts.auto_stub_fallback == Fallback::Raise {
            self.finalize_parts_and_nets();
            return Err(Error::Routing(format!(
                "{}. Set auto_stub_fallback='labels' to produce labels-only output instead of crashing.",
                reason
            )));
        }

        // Draw every net the user didn't set with labels.
        for net in 0..self.c.nets.len() {
            if !self.c.nets[net].stub_explicit {
                self.c.nets[net].stub = true;
                for pin in self.c.nets[net].pins.clone() {
                    self.c.pins[pin].stub = true;
                }
            }
        }
        self.new_node_tree();
        self.opts.expansion_factor = 1.0;
        self.place(self.root)?;
        self.route(self.root)?;
        self.snap_two_pin_parts(self.root);
        let output = self.write_output()?;
        self.finalize_parts_and_nets();
        if self.opts.auto_stub_fallback == Fallback::Warn {
            eprintln!("schematizer: warning: {}. Produced labels-only schematic at {}.", reason, output.display());
        }
        Ok(())
    }

    /// Place, route and write the design, trying again with more room when routing fails.
    pub fn gen_schematic(&mut self) -> Res<()> {
        if self.opts.auto_stub {
            // Phase 1: heuristic auto-stubbing before the first pass.
            self.auto_stub_nets();
        } else if self.opts.power_symbols {
            // Draw power nets with power symbols instead of wires, as auto-stubbing does.
            self.stub_power_nets(true);
        }

        let mut expansion_factor = 1.0;
        let mut failure: Option<Error> = None;
        let base_seed = self.opts.seed.clone();
        let retries = self.opts.retries;

        for attempt in 0..retries {
            if let Some(seed) = &base_seed {
                // Each attempt gets its own stream derived from the caller's
                // seed, so a retry explores a different layout while the run
                // as a whole stays reproducible.
                self.rng.seed(&format!("{}:{}", seed, attempt));
            }
            self.new_node_tree();

            // Say which attempt this is once the first has failed.
            let _note = (attempt > 0).then(|| progress::prefixed(&format!("attempt {}/{},", attempt + 1, retries)));
            let result = (|| -> Res<()> {
                self.place_and_classify(expansion_factor, true)?;
                if self.opts.auto_stub && self.opts.label_clearance {
                    // The classifiers just settled which pins get labels, but
                    // the placement they ran on left no room for them. Place
                    // again with the labels in the parts' bounding boxes.
                    self.finalize_parts_and_nets();
                    self.new_node_tree();
                    self.place_and_classify(expansion_factor, false)?;
                }
                self.route(self.root)?;
                // Each sheet below was laid out sheet_tries times as it was
                // routed. Do the same for the top sheet.
                self.opts.expansion_factor = expansion_factor;
                self.best_layout(self.root, self.opts.sheet_tries);
                Ok(())
            })();
            drop(_note);
            match result {
                Err(e @ Error::Placement(_)) => {
                    self.finalize_parts_and_nets();
                    failure = Some(e);
                    continue;
                }
                Err(e @ Error::Routing(_)) => {
                    self.finalize_parts_and_nets();
                    expansion_factor *= 1.5;
                    failure = Some(e);
                    continue;
                }
                Err(e) => {
                    self.finalize_parts_and_nets();
                    return Err(e);
                }
                Ok(()) => {}
            }

            self.snap_unwired_parts();
            let mut output_file = self.write_output()?;
            self.finalize_parts_and_nets();

            // Phase 2: fix what KiCad's ERC still finds (only with
            // auto-stubbing, and only KiCad files can be checked).
            if self.opts.auto_stub && self.opts.format == Format::Kicad && find_kicad_cli().is_some() {
                let mut prev_errors: Option<Vec<(String, String, String)>> = None;
                for _ in 0..self.opts.erc_max_iterations {
                    progress::report("checking with KiCad ERC");
                    let mut errors = parse_erc_report(run_erc(&output_file).as_deref());
                    let fixable_errors: Vec<_> = errors.iter().filter(|e| fixable(&e.0)).cloned().collect();
                    if fixable_errors.is_empty() {
                        break;
                    }
                    // Drawing again can't help if the last pass changed nothing,
                    // or if there's nothing left to stub.
                    errors.sort();
                    if prev_errors.as_ref() == Some(&errors) || !self.stub_nets_for_erc_errors(&fixable_errors) {
                        break;
                    }
                    prev_errors = Some(errors);

                    // Draw everything again, with more room if routing fails.
                    let mut regen_ok = false;
                    for erc_expansion in [1.0, 1.5, 2.25] {
                        let result = (|| -> Res<PathBuf> {
                            self.new_node_tree();
                            self.place_and_classify(erc_expansion, true)?;
                            self.route(self.root)?;
                            self.snap_unwired_parts();
                            self.write_output()
                        })();
                        self.finalize_parts_and_nets();
                        match result {
                            Ok(path) => {
                                output_file = path;
                                regen_ok = true;
                                break;
                            }
                            Err(e) if e.is_layout_failure() => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    if !regen_ok {
                        self.handle_fallback("ERC correction regeneration failed after expansion attempts")?;
                        break;
                    }
                }
            }
            return Ok(());
        }

        // Every attempt failed.
        if failure.is_some() && self.opts.auto_stub {
            return self.handle_fallback(&format!("Routing failed after all {} retries", retries));
        }
        self.finalize_parts_and_nets();
        Err(failure.unwrap_or_else(|| Error::Runtime("Schematic generation failed for unknown reasons".into())))
    }

    /// Parts of a node, for callers that need them while changing the design.
    pub fn node_parts(&self, node: NodeId) -> Vec<PartId> {
        self.nodes[node].parts.clone()
    }
}

/// Run KiCad's ERC on a schematic file. Returns the report's path.
fn run_erc(schematic: &Path) -> Option<PathBuf> {
    let report = PathBuf::from(schematic.to_string_lossy().replace(".kicad_sch", "-erc.rpt"));
    let _ = Command::new(find_kicad_cli()?)
        .args(["sch", "erc", "--output"])
        .arg(&report)
        .arg("--severity-all")
        .arg(schematic)
        .output();
    report.exists().then_some(report)
}

/// Parse an ERC report into (error type, symbol reference, pin number) for each error.
///
/// The report has lines like:
/// ```text
/// [pin_not_connected]: Pin not connected ...
///     @(x,y): Symbol U1 Pin 3 ...
/// ```
fn parse_erc_report(report: Option<&Path>) -> Vec<(String, String, String)> {
    let Some(text) = report.and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    let mut errors = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            if let Some(end) = rest.find(']') {
                let kind = &rest[..end];
                if !kind.is_empty() && kind.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    current = Some(kind.to_string());
                    continue;
                }
            }
        }
        if let Some(kind) = &current {
            // Look for "Symbol <ref> Pin <num>".
            let words: Vec<&str> = line.split_whitespace().collect();
            for i in 0..words.len().saturating_sub(3) {
                if words[i] == "Symbol" && words[i + 2] == "Pin" {
                    errors.push((kind.clone(), words[i + 1].to_string(), words[i + 3].to_string()));
                    current = None;
                    break;
                }
            }
        }
    }
    errors
}
