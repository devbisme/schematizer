//! Labels-only schematic writer.
//!
//! Writes KiCad schematics whose connectivity is exact by construction, for
//! use when correctness matters more than a readable drawing. There is no
//! placement optimization and no wire routing:
//!
//! - Each part unit is placed unrotated on a deterministic shelf-packed grid.
//! - Every connected pin gets a short wire stub ending in a power symbol (if
//!   the net name matches one), a global label (net on more than one sheet) or
//!   a local label (net on one sheet).
//! - Pins on the no-connect net get a no-connect flag.
//!
//! Every distinct hierarchy path that holds parts becomes one sheet directly
//! under the root sheet. Coordinates here are sheet mm (Y down).

use indexmap::IndexMap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::geometry::{pyround, round_to, Tx};
use crate::ir::{PartId, PinId};
use crate::sexp::{self, Sexp};
use crate::sx;
use crate::uuid::gen_uuid;

use super::kicad::{
    self, create_title_block_sexp, file_version, is_power_symbol_name, part_to_lib_symbol_definition,
    power_symbol_instance_sexp, power_symbol_pin_angle, write_sexp_schematic, A_SIZES,
};
use super::{Design, Res};

const GRID: f64 = 1.27; // KiCad schematic connection grid (mm).
const STUB: f64 = 2.54; // Length of the wire stub from a pin to its label (mm).
const MARGIN: f64 = 20.0; // Page margin (mm).
const FONT: f64 = 1.27; // Label/field font size (mm).

fn snap(v: f64) -> f64 {
    round_to(pyround(v / GRID) * GRID, 4)
}

/// Rough drawn length (mm) of label text, plus the label's arrow.
fn text_len(text: &str) -> f64 {
    0.8 * FONT * text.chars().count() as f64 + 2.0
}

/// Outward direction (sheet coordinates, Y down) of a pin, from the direction it points into its body.
fn pin_dir(orientation: char) -> (i32, i32) {
    match orientation {
        'L' => (1, 0),
        'R' => (-1, 0),
        'U' => (0, 1),
        _ => (0, -1),
    }
}

/// Label angle so the text extends away from the pin.
fn label_angle(d: (i32, i32)) -> f64 {
    match d {
        (1, 0) => 0.0,
        (-1, 0) => 180.0,
        (0, -1) => 90.0,
        _ => 270.0,
    }
}

/// The outward direction as an angle in symbol (Y up) degrees.
fn outward_deg(d: (i32, i32)) -> f64 {
    match d {
        (1, 0) => 0.0,
        (0, -1) => 90.0,
        (-1, 0) => 180.0,
        _ => 270.0,
    }
}

/// Symbol-local points from drawing commands, for the body's box.
fn graphic_points(cmd: &Sexp, pts: &mut Vec<(f64, f64)>) {
    let Some(items) = cmd.list() else { return };
    let Some(head) = cmd.head() else { return };
    if matches!(head, "at" | "start" | "end" | "xy" | "center" | "mid") && items.len() >= 3 {
        if let (Some(x), Some(y)) = (items[1].as_f64(), items[2].as_f64()) {
            pts.push((x, y));
        }
        return;
    }
    if matches!(head, "property" | "text") {
        return;
    }
    for sub in &items[1..] {
        graphic_points(sub, pts);
    }
}

/// One placed part unit.
struct Item {
    part: PartId,
    unit: PartId,
    unit_num: i64,
    pins: Vec<PinId>,
    ext: (f64, f64, f64, f64),
    w: f64,
    h: f64,
    origin: (f64, f64),
}

struct Writer<'a> {
    d: &'a mut Design,
    project: String,
    title: String,
    root_uuid: String,
    pwr_count: usize,
}

impl Writer<'_> {
    fn uid(&self, names: &[&str]) -> String {
        let mut all = vec![self.project.as_str()];
        all.extend(names);
        gen_uuid(&format!("labels:{}", all.join(":")))
    }

    /// (unit, unit number, pins) for a part, in unit order.
    fn units(&self, part: PartId) -> Vec<(PartId, i64, Vec<PinId>)> {
        let c = &self.d.c;
        if c.parts[part].units.is_empty() {
            return vec![(part, 1, c.parts[part].pins.clone())];
        }
        let mut units = c.parts[part].units.clone();
        units.sort_by_key(|&u| c.parts[u].num);
        let common: Vec<PinId> =
            units.iter().filter(|&&u| c.parts[u].num == 0).flat_map(|&u| c.parts[u].pins.clone()).collect();
        units
            .iter()
            .filter(|&&u| c.parts[u].num != 0)
            .map(|&u| {
                let mut pins = c.parts[u].pins.clone();
                pins.extend(common.iter().copied());
                (u, c.parts[u].num, pins)
            })
            .collect()
    }

    fn item(&self, part: PartId, unit: PartId, unit_num: i64, pins: Vec<PinId>) -> Item {
        let c = &self.d.c;
        let mut pts = Vec::new();
        for key in [unit_num, 0] {
            for cmd in c.parts[part].draw_cmds.get(&key).into_iter().flatten() {
                graphic_points(cmd, &mut pts);
            }
        }
        pts.extend(pins.iter().map(|&p| (c.pins[p].x, c.pins[p].y)));
        if pts.is_empty() {
            pts = vec![(-2.54, -2.54), (2.54, 2.54)];
        }
        let (x0, y0, x1, y1) = crate::quality::bounds(&pts);
        // Room for the stub and label (or power symbol) on each side the pins leave from.
        let mut m: HashMap<(i32, i32), f64> = [((1, 0), 2.54), ((-1, 0), 2.54), ((0, 1), 2.54), ((0, -1), 2.54)].into();
        for &pin in &pins {
            let d = pin_dir(c.pins[pin].orientation);
            let need = STUB + c.pins[pin].net.map_or(2.0, |n| text_len(&c.nets[n].name));
            let e = m.get_mut(&d).unwrap();
            *e = e.max(need);
        }
        // Extents relative to the symbol origin, in sheet coordinates, with
        // room for the Reference above and the Value below.
        let ext =
            (x0 - m[&(-1, 0)], -y1 - m[&(0, -1)] - 2.54, x1 + m[&(1, 0)], -y0 + m[&(0, 1)] + 2.54);
        Item { part, unit, unit_num, pins, ext, w: ext.2 - ext.0, h: ext.3 - ext.1, origin: (0.0, 0.0) }
    }

    /// Stub wires, labels, power symbols and no-connect flags for one unit.
    fn stub_elements(&mut self, it: &Item, net_sheets: &HashMap<String, HashSet<Vec<String>>>, uuid_path: &str) -> Vec<Sexp> {
        let mut out = Vec::new();
        let (ox, oy) = it.origin;
        let ref_ = self.d.c.parts[it.part].base_ref.clone();
        let unit_num = it.unit_num.to_string();
        for &pin in &it.pins {
            let p = &self.d.c.pins[pin];
            let (px, py) = (snap(ox + p.x), snap(oy - p.y));
            let key = [ref_.as_str(), unit_num.as_str(), p.num.as_str()];
            let Some(net) = p.net else { continue }; // An unconnected pin is left for ERC to report.
            let key_with = |kind: &'static str| -> Vec<&str> {
                let mut k = vec![kind];
                k.extend(key);
                k
            };
            if self.d.c.nets[net].nc {
                out.push(sx!["no_connect", sx!["at", px, py], sx!["uuid", self.uid(&key_with("nc"))]]);
                continue;
            }
            let (dx, dy) = pin_dir(p.orientation);
            let (ex, ey) = (snap(px + STUB * dx as f64), snap(py + STUB * dy as f64));
            out.push(sx![
                "wire",
                sx!["pts", sx!["xy", px, py], sx!["xy", ex, ey]],
                sx!["stroke", sx!["width", 0], sx!["type", "default"]],
                sx!["uuid", self.uid(&key_with("wire"))]
            ]);
            let name = self.d.c.nets[net].name.clone();
            if is_power_symbol_name(&name) {
                self.pwr_count += 1;
                let angle = (outward_deg((dx, dy)) - power_symbol_pin_angle(&name)).rem_euclid(360.0);
                out.push(power_symbol_instance_sexp(
                    &self.project,
                    &name,
                    ex,
                    ey,
                    angle,
                    &format!("#PWR{:03}", self.pwr_count),
                    uuid_path,
                    &format!("labels:{}:{}", self.project, key.join(":")),
                ));
                continue;
            }
            let angle = label_angle((dx, dy));
            let justify = if angle == 0.0 || angle == 90.0 { "left" } else { "right" };
            if net_sheets.get(&name).map_or(0, |s| s.len()) > 1 {
                out.push(sx![
                    "global_label",
                    name.as_str(),
                    sx!["shape", "bidirectional"],
                    sx!["at", ex, ey, angle],
                    sx!["fields_autoplaced", "yes"],
                    sx!["effects", sx!["font", sx!["size", FONT, FONT]], sx!["justify", justify]],
                    sx!["uuid", self.uid(&key_with("glabel"))]
                ]);
            } else {
                out.push(sx![
                    "label",
                    name.as_str(),
                    sx!["at", ex, ey, angle],
                    sx!["fields_autoplaced", "yes"],
                    sx!["effects", sx!["font", sx!["size", FONT, FONT]], sx!["justify", justify, "bottom"]],
                    sx!["uuid", self.uid(&key_with("label"))]
                ]);
            }
        }
        out
    }

    fn sheet_block(&self, name: &str, fname: &str, sheet_uuid: &str, page: usize, index: usize) -> Sexp {
        let x = snap(25.4 + (index % 4) as f64 * 63.5);
        let y = snap(38.1 + (index / 4) as f64 * 38.1);
        let (w, h) = (50.8, 25.4);
        let font = || sx!["font", sx!["size", FONT, FONT]];
        sx![
            "sheet",
            sx!["at", x, y],
            sx!["size", w, h],
            sx!["exclude_from_sim", "no"],
            sx!["in_bom", "yes"],
            sx!["on_board", "yes"],
            sx!["dnp", "no"],
            sx!["fields_autoplaced", "yes"],
            sx!["stroke", sx!["width", 0.1524], sx!["type", "solid"]],
            sx!["fill", sx!["color", 0, 0, 0, 0]],
            sx!["uuid", sheet_uuid],
            sx![
                "property",
                "Sheetname",
                name,
                sx!["at", x, snap(y - 1.27), 0],
                sx!["effects", font(), sx!["justify", "left", "bottom"]]
            ],
            sx![
                "property",
                "Sheetfile",
                fname,
                sx!["at", x, snap(y + h + 1.27), 0],
                sx!["effects", font(), sx!["justify", "left", "top"]]
            ],
            sx![
                "instances",
                sx![
                    "project",
                    self.project.as_str(),
                    sx!["path", format!("/{}", self.root_uuid), sx!["page", sexp::q(&page.to_string())]]
                ]
            ]
        ]
    }

    #[allow(clippy::too_many_arguments)]
    fn write_sheet(
        &mut self,
        fname: &str,
        sheet_uuid: &str,
        title: &str,
        parts: &[PartId],
        net_sheets: &HashMap<String, HashSet<Vec<String>>>,
        uuid_path: &str,
        extra: Vec<Sexp>,
    ) -> Res<PathBuf> {
        let mut sorted = parts.to_vec();
        sorted.sort_by(|&a, &b| {
            let (pa, pb) = (&self.d.c.parts[a], &self.d.c.parts[b]);
            pb.pins.len().cmp(&pa.pins.len()).then_with(|| pa.base_ref.cmp(&pb.base_ref))
        });
        let mut items = Vec::new();
        for part in sorted {
            for (unit, num, pins) in self.units(part) {
                items.push(self.item(part, unit, num, pins));
            }
        }
        let paper = if items.is_empty() { "A4" } else { pack(&mut items) };

        let mut lib_symbols: BTreeMap<String, PartId> = BTreeMap::new();
        let mut elements = Vec::new();
        for it in &items {
            let p = &self.d.c.parts[it.part];
            lib_symbols.entry(format!("{}:{}", p.lib, p.name)).or_insert(it.part);
            self.d.c.parts[it.unit].tx = Tx::translate(it.origin.0, it.origin.1);
            elements.push(self.d.part_to_sexp(it.unit, uuid_path, Tx::IDENT));
            elements.extend(self.stub_elements(it, net_sheets, uuid_path));
        }

        let tool = self.d.opts.tool.clone();
        let mut schematic = sx![
            "kicad_sch",
            sx!["version", file_version(&tool)],
            sx!["generator", "schematizer"],
            sx!["generator_version", kicad::VERSION],
            sx!["uuid", sheet_uuid],
            sx!["paper", paper]
        ];
        schematic.push(create_title_block_sexp(title));
        let mut libs = sx!["lib_symbols"];
        for &part in lib_symbols.values() {
            libs.push(part_to_lib_symbol_definition(&self.d.c.parts[part], &tool));
        }
        let mut pwr: Vec<String> = elements
            .iter()
            .filter(|e| e.is("symbol"))
            .filter_map(|e| e.field("lib_id"))
            .map(|l| l.str_at(1).to_string())
            .filter(|l| l.starts_with("power:"))
            .collect();
        pwr.sort();
        pwr.dedup();
        for lib_id in pwr {
            if !lib_symbols.contains_key(&lib_id) {
                if let Some(sym) = kicad::power_lib().get(&lib_id["power:".len()..]) {
                    let mut sym = sym.clone();
                    sym.list_mut().unwrap()[1] = Sexp::A(lib_id.clone());
                    libs.push(sym);
                }
            }
        }
        schematic.push(libs);
        schematic.list_mut().unwrap().extend(elements);
        schematic.list_mut().unwrap().extend(extra);
        if uuid_path == format!("/{}", self.root_uuid) {
            schematic.push(sx!["sheet_instances", sx!["path", "/", sx!["page", sexp::q("1")]]]);
        }
        schematic.push(sx!["embedded_fonts", "no"]);
        let path = self.d.filepath.join(fname);
        write_sexp_schematic(schematic, &path)?;
        // Leave the part transforms as the engine expects them.
        for it in &items {
            self.d.c.parts[it.unit].tx = Tx::IDENT;
        }
        Ok(path)
    }

    fn write(&mut self) -> Res<PathBuf> {
        // The parts on each sheet; the root holds the top-level parts.
        let mut by_sheet: BTreeMap<Vec<String>, Vec<PartId>> = BTreeMap::new();
        for &part in &self.d.c.top_parts {
            by_sheet.entry(self.d.c.parts[part].hiertuple.clone()).or_default().push(part);
        }
        let root_key = vec![String::new()];
        let mut names: IndexMap<Vec<String>, String> = IndexMap::new();
        let mut used: HashSet<String> = HashSet::new();
        for ht in by_sheet.keys() {
            if *ht == root_key {
                continue;
            }
            let levels: Vec<&String> = ht.iter().filter(|h| !h.is_empty()).collect();
            let mut name = levels.last().map_or("sheet".to_string(), |s| s.to_string());
            if used.contains(&name) {
                name = levels.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("_");
            }
            used.insert(name.clone());
            names.insert(ht.clone(), name);
        }

        // Which sheets each net appears on decides between global and local labels.
        let mut net_sheets: HashMap<String, HashSet<Vec<String>>> = HashMap::new();
        for (ht, parts) in &by_sheet {
            for &part in parts {
                for &pin in &self.d.c.parts[part].pins {
                    if let Some(net) = self.d.c.pins[pin].net {
                        if !self.d.c.nets[net].nc {
                            net_sheets.entry(self.d.c.nets[net].name.clone()).or_default().insert(ht.clone());
                        }
                    }
                }
            }
        }

        let mut order: Vec<Vec<String>> = names.keys().cloned().collect();
        order.sort_by(|a, b| names[a].cmp(&names[b]));
        let mut blocks = Vec::new();
        for (index, ht) in order.iter().enumerate() {
            let name = names[ht].clone();
            let fname = format!("{}_{}.kicad_sch", self.project, name);
            let mut key = vec!["sheet"];
            key.extend(ht.iter().map(|s| s.as_str()));
            let sheet_uuid = self.uid(&key);
            let title = format!("{} - {}", self.title, name);
            let path = format!("/{}/{}", self.root_uuid, sheet_uuid);
            self.write_sheet(&fname, &sheet_uuid, &title, &by_sheet[ht], &net_sheets, &path, Vec::new())?;
            blocks.push(self.sheet_block(&name, &fname, &sheet_uuid, index + 2, index));
        }

        let fname = format!("{}.kicad_sch", self.project);
        let root_uuid = self.root_uuid.clone();
        let title = self.title.clone();
        let root_parts = by_sheet.get(&root_key).cloned().unwrap_or_default();
        self.write_sheet(&fname, &root_uuid, &title, &root_parts, &net_sheets, &format!("/{}", root_uuid), blocks)
    }
}

/// Shelf-pack items onto the smallest A-size page that holds them.
fn pack(items: &mut [Item]) -> &'static str {
    let area: f64 = items.iter().map(|it| it.w * it.h).sum();
    let widest = items.iter().map(|it| it.w).fold(0.0, f64::max);
    let mut idx = A_SIZES.len() - 1;
    for (i, (_, (pw, ph))) in A_SIZES.iter().enumerate() {
        if area * 1.6 < (pw - 2.0 * MARGIN) * (ph - 2.0 * MARGIN) && widest < pw - 2.0 * MARGIN {
            idx = i;
            break;
        }
    }
    loop {
        let (name, (pw, ph)) = A_SIZES[idx];
        let (mut x, mut y, mut shelf_h) = (MARGIN, MARGIN + 5.0, 0.0f64);
        for it in items.iter_mut() {
            if x + it.w > pw - MARGIN && x > MARGIN {
                x = MARGIN;
                y += shelf_h;
                shelf_h = 0.0;
            }
            it.origin = (snap(x - it.ext.0), snap(y - it.ext.1));
            x += it.w;
            shelf_h = shelf_h.max(it.h);
        }
        // The largest page may still overflow; KiCad accepts items off the page.
        if y + shelf_h <= ph - MARGIN || idx == A_SIZES.len() - 1 {
            return name;
        }
        idx += 1;
    }
}

/// Write a labels-only KiCad schematic for a design's circuit. Returns the top file's path.
pub fn write_labels_schematic(d: &mut Design) -> Res<PathBuf> {
    let top_name = d.top_name.clone();
    d.kc = kicad::WriterState { project_name: top_name.clone(), ..Default::default() };
    std::fs::create_dir_all(&d.filepath)?;
    let title = d.title.clone();
    let root_uuid = gen_uuid(&format!("labels:{}:root", top_name));
    Writer { d, project: top_name, title, root_uuid, pwr_count: 0 }.write()
}
