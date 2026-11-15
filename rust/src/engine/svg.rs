//! Write a placed and routed circuit out as SVG pages.
//!
//! This is the sibling of the KiCad writer: same placement, same page
//! geometry, different output. Where the KiCad writer emits a symbol instance
//! that KiCad draws from its library section, this draws the symbol graphics
//! itself. Labels, wires and junctions aren't decided here: both writers draw
//! the list built by `sheet_connectivity()`.
//!
//! A flattened node draws on its parent's page. A node with a sheet of its
//! own gets its own `.svg` file, and its parent draws a rectangle carrying
//! its name, linked to its file. Each child page links back to its parent and
//! to the top sheet.

use std::f64::consts::FRAC_PI_2;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::geometry::{BBox, Point, Tx};
use crate::ir::{NodeId, PartId};
use crate::sexp::Sexp;

use super::kicad::{calc_sheet_tx, is_power_symbol_name, power_lib, reference_is_hidden, round_mm, WriterState, A_SIZES};
use super::{Design, Res};

/// Placement coordinates are mils; symbol graphics are mm.
const MM_TO_MILS: f64 = 1.0 / 0.0254;

/// Text width per character, as a fraction of the font size (for boxes only).
const CHAR_WIDTH_RATIO: f64 = 0.6;

const MARGIN: f64 = 5.0;
const TITLE_W: f64 = 80.0;
const TITLE_H: f64 = 20.0;
const NAV_H: f64 = 10.0;

// KiCad's default schematic colors, so a page looks like the .kicad_sch drawn alongside it.
const SYMBOL: &str = "#840000";
const SYMBOL_FILL: &str = "#FFFFC2";
const PIN_NAME: &str = "#0000C8";
const FIELD: &str = "#840000";
const WIRE: &str = "#008484";
const JUNCTION: &str = "#008484";
const NO_CONNECT: &str = "#0000C8";
const GLOBAL_LABEL: &str = "#840000";
const SHEET: &str = "#840084";
const SHEET_FILL: &str = "#FFFFFF";
const BORDER: &str = "#840000";
const TEXT: &str = "#000000";

const STYLE: &str = "
  text { font-family: \"DejaVu Sans\", Verdana, Arial, sans-serif; }
  a { cursor: pointer; }
  g.sheet:hover rect { stroke-width: 0.6; }
";

/// Escape text for XML character data or an attribute.
pub fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn xy(e: Option<&Sexp>) -> Point {
    e.map_or(Point::new(0.0, 0.0), |e| Point::new(e.num(1), e.num(2)))
}

fn font_size(effects: Option<&Sexp>) -> (f64, f64) {
    effects.and_then(|e| e.field("font")).and_then(|f| f.field("size")).map_or((0.0, 0.0), |s| (s.num(1), s.num(2)))
}

/// The first bare item of a command after its head, as text.
fn misc(cmd: &Sexp) -> String {
    cmd.list().and_then(|v| v.iter().skip(1).find(|e| !e.is_list())).map(|e| e.text()).unwrap_or_default()
}

fn text_bbox(text: &str, start: Point, dir: Point, char_wid: f64, char_hgt: f64) -> BBox {
    let (w, h) = (char_wid * CHAR_WIDTH_RATIO, char_hgt * CHAR_WIDTH_RATIO);
    let ortho = dir * Tx::IDENT.rot(90.0);
    let n = text.chars().count() as f64;
    let p1 = start - ortho * h / 2.0;
    let p2 = start + ortho * h / 2.0;
    BBox::from_pts(&[p1, p2, p1 + dir * w * n, p2 + dir * w * n])
}

/// Which side of the symbol a direction points toward, so text reads
/// left to right (or bottom to top) whichever way the part is turned.
fn side_of(v: Point) -> &'static str {
    if v.x > v.y && v.x > -v.y {
        "left"
    } else if v.x < v.y && v.x > -v.y {
        "top"
    } else if v.x < v.y && v.x < -v.y {
        "right"
    } else if v.x > v.y && v.x < -v.y {
        "bottom"
    } else {
        "left"
    }
}

fn text_svg(text: &str, side: &str, pt: Point, size: f64, cls: &str, color: &str, anchor_override: Option<&str>) -> String {
    let (rotate, anchor) = match side {
        "right" => (0, "end"),
        "top" => (-90, "end"),
        "bottom" => (-90, "start"),
        _ => (0, "start"),
    };
    let anchor = anchor_override.unwrap_or(anchor);
    format!(
        "<text class=\"{cls}\" x=\"{x:.3}\" y=\"{y:.3}\" transform=\"rotate({rotate} {x:.3} {y:.3})\" \
         style=\"font-size:{size:.3}px; fill:{color}\" dominant-baseline=\"central\" text-anchor=\"{anchor}\">{t}</text>\n",
        x = pt.x,
        y = pt.y,
        t = esc(text)
    )
}

fn fill_style(cmd: &Sexp) -> &'static str {
    match cmd.field("fill").and_then(|f| f.field("type")).map(|t| t.str_at(1)) {
        Some("background") => SYMBOL_FILL,
        Some("outline") => SYMBOL,
        _ => "none",
    }
}

fn pts_str(points: &[Point]) -> String {
    points.iter().map(|p| format!("{:.3},{:.3}", p.x, p.y)).collect::<Vec<_>>().join(" ")
}

/// One symbol drawing command as SVG, with its box in page mm.
///
/// `tx` maps symbol mm (Y up) to page mm (Y down). `value` is the text drawn
/// for a Value property; other properties aren't drawn.
fn draw_cmd_to_svg(cmd: &Sexp, tx: Tx, value: Option<&str>, pin_names: Option<(f64, bool)>) -> (String, BBox) {
    let empty = (String::new(), BBox::empty());
    let scale = tx.scale();
    let width = cmd.field("stroke").and_then(|s| s.field("width")).map_or(0.0, |w| w.num(1));
    let width = if width == 0.0 { 0.1524 } else { width };
    let sw = (width * scale).abs();
    let style = |fill: &str| format!("style=\"stroke:{SYMBOL}; stroke-width:{sw:.3}; fill:{fill}\"");

    match cmd.head().unwrap_or("") {
        "polyline" => {
            let points: Vec<Point> = cmd.field("pts").map(|p| p.fields("xy").map(|e| xy(Some(e)) * tx).collect()).unwrap_or_default();
            if points.is_empty() {
                return empty;
            }
            (format!("<polyline points=\"{}\" {} />\n", pts_str(&points), style(fill_style(cmd))), BBox::from_pts(&points))
        }
        "circle" => {
            let ctr = xy(cmd.field("center")) * tx;
            let r = (cmd.field("radius").map_or(0.0, |r| r.num(1)) * scale).abs();
            (
                format!("<circle cx=\"{:.3}\" cy=\"{:.3}\" r=\"{r:.3}\" {} />\n", ctr.x, ctr.y, style(fill_style(cmd))),
                BBox::from_pts(&[ctr + Point::new(r, r), ctr - Point::new(r, r)]),
            )
        }
        "rectangle" => {
            let b = BBox::new(xy(cmd.field("start")) * tx, xy(cmd.field("end")) * tx);
            (
                format!(
                    "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" {} />\n",
                    b.min.x,
                    b.min.y,
                    b.w(),
                    b.h(),
                    style(fill_style(cmd))
                ),
                b,
            )
        }
        "arc" => {
            let a = xy(cmd.field("start")) * tx;
            let b = xy(cmd.field("end")) * tx;
            let c = xy(cmd.field("mid")) * tx;
            // The radius of the circle through the three points: R = abc / 4K.
            let (la, lb, lc) = ((b - c).magnitude(), (a - c).magnitude(), (a - b).magnitude());
            let mut path = format!("M {:.3} {:.3} L {:.3} {:.3}", a.x, a.y, b.x, b.y);
            if la * lb != 0.0 {
                let angle = ((la * la + lb * lb - lc * lc) / (2.0 * la * lb)).clamp(-1.0, 1.0).acos();
                let k = 0.5 * la * lb * angle.sin();
                if k != 0.0 {
                    let r = la * lb * lc / 4.0 / k;
                    let large_arc = (FRAC_PI_2 > angle) as i32;
                    let sweep = ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x) < 0.0) as i32;
                    path = format!("M {:.3} {:.3} A {r:.3} {r:.3} 0 {large_arc} {sweep} {:.3} {:.3}", a.x, a.y, b.x, b.y);
                }
            }
            (format!("<path d=\"{path}\" {} />\n", style(fill_style(cmd))), BBox::from_pts(&[a, b, c]))
        }
        "property" => {
            let effects = cmd.field("effects");
            let name = misc(cmd).to_lowercase();
            let Some(text) = value.filter(|v| name == "value" && !v.is_empty()) else { return empty };
            if effects.is_some_and(|e| e.field("hide").is_some()) {
                return empty;
            }
            let at = cmd.field("at");
            let start = xy(at);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let justify = effects.and_then(|e| e.field("justify")).map_or("center".to_string(), |j| j.str_at(1).to_lowercase());
            let dir = if justify == "right" { Point::new(-1.0, 0.0) } else { Point::new(1.0, 0.0) } * Tx::IDENT.rot(rotation);
            let end = (start + dir) * tx;
            let start = start * tx;
            let side = side_of(dir * tx.no_translate());
            let (w, h) = font_size(effects);
            let (w, h) = (w * scale, h * scale);
            (text_svg(text, side, start, w, "field", FIELD, None), text_bbox(text, start, (end - start).norm(), w, h))
        }
        "pin" => {
            let at = cmd.field("at");
            let start = xy(at);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let length = cmd.field("length").map_or(0.0, |l| l.num(1));
            // A pin's "at" is its connection point, and its line runs from there toward the body.
            let dir = Point::new(1.0, 0.0) * Tx::IDENT.rot(rotation);
            let end = (start + dir * length) * tx;
            let start = start * tx;
            let mut bbox = BBox::new(start, end);
            let side = side_of(dir * tx.no_translate());
            let mut svg = format!("<polyline points=\"{}\" {} />\n", pts_str(&[start, end]), style("none"));

            if let Some(name) = cmd.field("name") {
                let pin_name = misc(name);
                let neffects = name.field("effects");
                let (w, h) = font_size(neffects);
                let (w, h) = (w * scale, h * scale);
                let (offset, hide) = pin_names.unwrap_or((0.0, false));
                let hidden = hide || neffects.is_some_and(|e| e.field("hide").is_some());
                // "~" is KiCad's "no name"; it's never drawn.
                if !pin_name.is_empty() && pin_name != "~" && !hidden {
                    let inward = (end - start).norm();
                    let name_pt = end + inward * (offset * scale);
                    svg += &text_svg(&pin_name, side, name_pt, w, "pin_name", PIN_NAME, None);
                    bbox.add(text_bbox(&pin_name, name_pt, inward, w, h));
                }
            }
            (svg, bbox)
        }
        "text" => {
            let at = cmd.field("at");
            let start = xy(at);
            let rotation = at.map_or(0.0, |a| a.num(3));
            let dir = Point::new(1.0, 0.0) * Tx::IDENT.rot(rotation);
            let end = (start + dir) * tx;
            let start = start * tx;
            let side = side_of(dir * tx.no_translate());
            let (w, h) = font_size(cmd.field("effects"));
            let (w, h) = (w * scale, h * scale);
            let text = misc(cmd);
            (text_svg(&text, side, start, w, "text", TEXT, None), text_bbox(&text, start, (end - start).norm(), w, h))
        }
        // An unknown command (a bezier, say) is skipped rather than spoiling the page.
        _ => empty,
    }
}

/// Text over a white backing so it stays readable across a wire.
///
/// The backing is a second copy of the text drawn underneath with a fat
/// white stroke. (`paint-order="stroke"` does it in one element, but several
/// renderers don't support it and paint the stroke over the glyphs.)
fn halo_text(cls: &str, text: &str, x: f64, y: f64, color: &str, anchor: &str, rotate: i32) -> String {
    let common = format!("x=\"{x:.3}\" y=\"{y:.3}\" transform=\"rotate({rotate} {x:.3} {y:.3})\" dominant-baseline=\"central\" text-anchor=\"{anchor}\"");
    let t = esc(text);
    format!(
        "<text class=\"{cls} halo\" {common} style=\"font-size:1.27px; fill:#FFFFFF; stroke:#FFFFFF; stroke-width:0.3; stroke-linejoin:round\">{t}</text>\n\
         <text class=\"{cls}\" {common} style=\"font-size:1.27px; fill:{color}\">{t}</text>\n"
    )
}

/// About the box a run of 1.27 mm text takes up, so the page leaves room for it.
fn text_extent(text: &str, x: f64, y: f64, anchor: &str, rotate: i32) -> BBox {
    let size = 1.27;
    let w = text.chars().count() as f64 * size * 0.6;
    let (a, b) = match anchor {
        "start" => (0.0, w),
        "end" => (-w, 0.0),
        _ => (-w / 2.0, w / 2.0),
    };
    if rotate != 0 {
        // Turned -90 degrees, the text climbs the page.
        BBox::new(Point::new(x - size / 2.0, y - b), Point::new(x + size / 2.0, y - a))
    } else {
        BBox::new(Point::new(x + a, y - size / 2.0), Point::new(x + b, y + size / 2.0))
    }
}

/// Map a symbol's mm (Y up) onto the page in mm (Y down): the same composition
/// that places pins, so the drawn pins land on the ends of the routed wires.
fn symbol_render_tx(part_tx: Tx, sheet_tx: Tx) -> Tx {
    Tx::scale_by(MM_TO_MILS) * part_tx * sheet_tx
}

/// The drawing commands to collect from a power symbol's definition.
fn collect_draw_cmds<'a>(sym: &'a Sexp, out: &mut Vec<&'a Sexp>) {
    for item in sym.list().into_iter().flatten().skip(1) {
        match item.head() {
            Some("polyline" | "rectangle" | "circle" | "arc" | "pin" | "text" | "property") => out.push(item),
            Some("symbol") => collect_draw_cmds(item, out),
            _ => {}
        }
    }
}

/// A power symbol at (x, y). Its name goes where the library symbol has it,
/// or reads up the sheet centered on `upright` if that's given.
fn power_symbol_to_svg(net: &str, x: f64, y: f64, angle: f64, upright: Option<(f64, f64)>) -> (String, BBox) {
    let Some(sym) = power_lib().get(net) else { return (String::new(), BBox::empty()) };
    let tx = Tx::lin(1.0, 0.0, 0.0, -1.0).rot(-angle).move_by(Point::new(x, y));
    let mut svg = format!("<g class=\"power\" data-net=\"{}\">\n", esc(net));
    let mut bbox = BBox::empty();
    let mut cmds = Vec::new();
    collect_draw_cmds(sym, &mut cmds);
    for cmd in cmds {
        let (s, b) = draw_cmd_to_svg(cmd, tx, if upright.is_some() { None } else { Some(net) }, None);
        if !s.is_empty() {
            svg += &s;
            bbox.add(b);
        }
    }
    if let Some((ux, uy)) = upright {
        svg += &halo_text("field", net, ux, uy, FIELD, "middle", -90);
        bbox.add(text_extent(net, ux, uy, "middle", -90));
    }
    svg += "</g>\n";
    (svg, bbox)
}

/// Draw one element of a sheet's connectivity list.
///
/// A hierarchical label has no drawing of its own: the parent page lists
/// those nets on the sheet's box.
fn element_to_svg(el: &Sexp) -> (String, BBox) {
    let at = el.field("at");
    let (ax, ay) = at.map_or((0.0, 0.0), |a| (a.num(1), a.num(2)));
    match el.head().unwrap_or("") {
        "wire" => {
            let pts: Vec<&Sexp> = el.field("pts").map(|p| p.fields("xy").collect()).unwrap_or_default();
            if pts.len() < 2 {
                return (String::new(), BBox::empty());
            }
            let (x1, y1, x2, y2) = (pts[0].num(1), pts[0].num(2), pts[1].num(1), pts[1].num(2));
            (
                format!("<line x1=\"{x1:.3}\" y1=\"{y1:.3}\" x2=\"{x2:.3}\" y2=\"{y2:.3}\" style=\"stroke:{WIRE}; stroke-width:0.1524\" />\n"),
                BBox::new(Point::new(x1, y1), Point::new(x2, y2)),
            )
        }
        "junction" => (
            format!("<circle cx=\"{ax:.3}\" cy=\"{ay:.3}\" r=\"0.45\" style=\"fill:{JUNCTION}; stroke:none\" />\n"),
            BBox::new(Point::new(ax, ay), Point::new(ax, ay)),
        ),
        "no_connect" => {
            let r = 0.635;
            (
                format!(
                    "<path class=\"no_connect\" d=\"M{:.3} {:.3} L{:.3} {:.3} M{:.3} {:.3} L{:.3} {:.3}\" style=\"stroke:{NO_CONNECT}; stroke-width:0.1524; fill:none\" />\n",
                    ax - r, ay - r, ax + r, ay + r, ax - r, ay + r, ax + r, ay - r
                ),
                BBox::new(Point::new(ax - r, ay - r), Point::new(ax + r, ay + r)),
            )
        }
        "global_label" => {
            // The anchor is the connection point, and the text leads away from it.
            let (rotate, anchor, off) = match (at.map_or(0.0, |a| a.num(3)) as i64).rem_euclid(360) {
                180 => (0, "end", (-0.8, 0.0)),
                90 => (-90, "start", (0.0, -0.8)),
                270 => (-90, "end", (0.0, 0.8)),
                _ => (0, "start", (0.8, 0.0)),
            };
            let (x, y) = (round_mm(ax + off.0), round_mm(ay + off.1));
            let name = el.str_at(1);
            (halo_text("net_label", name, x, y, GLOBAL_LABEL, anchor, rotate), text_extent(name, x, y, anchor, rotate))
        }
        "symbol" => {
            let lib_id = el.field("lib_id").map_or("", |l| l.str_at(1));
            let net = lib_id.split_once(':').map_or("", |(_, n)| n);
            // A name turned upright to clear its neighbors is drawn where the KiCad writer put it.
            let upright = el
                .fields("property")
                .find(|p| p.str_at(1) == "Value")
                .and_then(|p| p.field("at"))
                .filter(|a| a.num(3) == 90.0)
                .map(|a| (a.num(1), a.num(2)));
            power_symbol_to_svg(net, ax, ay, at.map_or(0.0, |a| a.num(3)), upright)
        }
        _ => (String::new(), BBox::empty()),
    }
}

impl Design {
    fn svg_filename(&self, node: NodeId) -> String {
        let base = &self.nodes[node].sheet_filename;
        let base = if base.is_empty() { "no_sheet_filename" } else { base.as_str() };
        let stem = match base.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => stem,
            _ => base,
        };
        format!("{stem}.svg")
    }

    /// Draw a placed part's symbol and its Reference and Value.
    fn part_to_svg(&self, part: PartId, sheet_tx: Tx) -> (String, BBox) {
        let p = &self.c.parts[part];
        let tx = symbol_render_tx(p.tx, sheet_tx);
        let mut svg = format!("<g class=\"part\" data-ref=\"{}\">\n", esc(&p.base_ref));
        let mut bbox = BBox::empty();
        // Pins go last: a filled body drawn after them would cover their names.
        let mut cmds: Vec<&Sexp> = [0, p.num].iter().flat_map(|k| p.draw_cmds.get(k).into_iter().flatten()).collect();
        cmds.sort_by_key(|c| c.head().is_some_and(|h| h.eq_ignore_ascii_case("pin")));
        for cmd in cmds {
            // The Reference and Value are drawn below, where the KiCad writer puts them.
            let (s, b) = draw_cmd_to_svg(cmd, tx, None, p.pin_names);
            if !s.is_empty() {
                svg += &s;
                bbox.add(b);
            }
        }
        if bbox.w().is_finite() && bbox.h().is_finite() {
            let (ref_spot, val_spot) = self.calc_field_layout(part, sheet_tx);
            let ref_text = if reference_is_hidden(&p.base_ref) { "" } else { p.base_ref.as_str() };
            for (text, (x, y, j)) in [(ref_text, ref_spot), (p.value.as_str(), val_spot)] {
                if text.is_empty() {
                    continue;
                }
                let anchor = match j {
                    'l' => "start",
                    'r' => "end",
                    _ => "middle",
                };
                svg += &halo_text("field", text, x, y, FIELD, anchor, 0);
                bbox.add(text_extent(text, x, y, anchor, 0));
            }
        }
        svg += "</g>\n";
        (svg, bbox)
    }

    /// Draw a child sheet as a rectangle on its parent's page, linked to the child's file.
    fn sheet_box_to_svg(&self, node: NodeId, sheet_tx: Tx) -> (String, BBox) {
        let bbox = self.sheet_rect(node) * self.nodes[node].tx * sheet_tx;
        let (x, y) = (round_mm(bbox.ll().x), round_mm(bbox.ll().y));
        let (w, h) = (round_mm(bbox.w().abs()), round_mm(bbox.h().abs()));
        let href = self.svg_filename(node);
        let mut svg = format!(
            "<a href=\"{href}\" target=\"_top\">\n<g class=\"sheet\">\n\
             <rect x=\"{x:.3}\" y=\"{y:.3}\" width=\"{w:.3}\" height=\"{h:.3}\" style=\"stroke:{SHEET}; stroke-width:0.3; fill:{SHEET_FILL}\" />\n\
             <text x=\"{x:.3}\" y=\"{:.3}\" style=\"font-size:2.0px; fill:{SHEET}; font-weight:bold\" text-anchor=\"start\">{}</text>\n\
             <text x=\"{x:.3}\" y=\"{:.3}\" style=\"font-size:1.4px; fill:{SHEET}\" text-anchor=\"start\">{}</text>\n",
            y - 1.0,
            esc(&self.nodes[node].name),
            y + h + 2.4,
            esc(&href)
        );
        // The nets crossing into the child, down the inside of the left edge:
        // the counterpart of the sheet pins the KiCad writer draws.
        let mut pin_y = y + 2.54;
        for net in self.get_boundary_nets(node) {
            let n = &self.c.nets[net];
            if is_power_symbol_name(&n.name) || n.stub {
                continue;
            }
            if pin_y > y + h - 1.0 {
                break; // Out of room; the rest stay implicit.
            }
            let _ = writeln!(
                svg,
                "<text class=\"sheet_pin\" x=\"{:.3}\" y=\"{pin_y:.3}\" style=\"font-size:1.27px; fill:{SHEET}\" dominant-baseline=\"central\" text-anchor=\"start\">{}</text>",
                x + 1.0,
                esc(&n.name)
            );
            pin_y += 2.54;
        }
        svg += "</g>\n</a>\n";
        (svg, BBox::new(Point::new(x, y - 3.0), Point::new(x + w, y + h + 3.4)))
    }

    /// Links back up the hierarchy, at the top left of a child page.
    fn nav_links_svg(&self, node: NodeId) -> String {
        let parent = self.nodes[node].parent;
        let Some(parent) = parent else { return String::new() };
        let name = |n: NodeId| if self.nodes[n].name.is_empty() { "top".to_string() } else { self.nodes[n].name.clone() };
        let mut links = vec![(format!("▲ Up: {}", name(parent)), self.svg_filename(parent))];
        if self.root != parent && self.root != node {
            links.push((format!("⌂ Top: {}", name(self.root)), self.svg_filename(self.root)));
        }
        let mut svg = String::from("<g class=\"nav\">\n");
        let mut x = 8.0;
        for (text, href) in links {
            let _ = writeln!(
                svg,
                "<a href=\"{href}\" target=\"_top\"><text x=\"{x:.3}\" y=\"6.0\" style=\"font-size:3.0px; fill:{SHEET}; text-decoration:underline\">{}</text></a>",
                esc(&text)
            );
            x += 28.0f64.max(text.chars().count() as f64 * 1.9);
        }
        svg += "</g>\n";
        svg
    }

    /// Page border and a title block in the bottom right corner.
    fn title_block_svg(&self, node: NodeId, page_w: f64, page_h: f64, top: f64) -> String {
        let m = MARGIN;
        let (bx, by) = (page_w - m - TITLE_W, page_h - m - TITLE_H);
        let top = m.max(top);
        let sheet = if self.nodes[node].name.is_empty() { "/" } else { self.nodes[node].name.as_str() };
        format!(
            "<g class=\"frame\">\n\
             <rect x=\"5.0\" y=\"{top:.3}\" width=\"{:.3}\" height=\"{:.3}\" style=\"stroke:{BORDER}; stroke-width:0.3; fill:none\" />\n\
             <rect x=\"{bx:.3}\" y=\"{by:.3}\" width=\"80.0\" height=\"20.0\" style=\"stroke:{BORDER}; stroke-width:0.3; fill:none\" />\n\
             <text x=\"{:.3}\" y=\"{:.3}\" style=\"font-size:3.5px; fill:{BORDER}\">{}</text>\n\
             <text x=\"{:.3}\" y=\"{:.3}\" style=\"font-size:2.5px; fill:{BORDER}\">Sheet: {}</text>\n\
             <text x=\"{:.3}\" y=\"{:.3}\" style=\"font-size:2.5px; fill:{BORDER}\">File: {}</text>\n</g>\n",
            page_w - 2.0 * m,
            page_h - m - top,
            bx + 2.0,
            by + 6.0,
            esc(&self.title),
            bx + 2.0,
            by + 12.0,
            esc(sheet),
            bx + 2.0,
            by + 17.0,
            esc(&self.svg_filename(node))
        )
    }

    /// Convert a node and its children to SVG.
    ///
    /// A flattened node returns its drawing and its connectivity elements for
    /// its parent's page. A node with a sheet of its own writes its file and
    /// returns a linked box for its parent to draw.
    fn node_to_svg(&mut self, node: NodeId, sheet_tx: Tx) -> Res<(String, BBox, Vec<PathBuf>, Vec<Sexp>)> {
        let flattened = self.nodes[node].flattened;
        let (tx, paper) = if flattened { (self.nodes[node].tx * sheet_tx, "A4") } else { calc_sheet_tx(&self.internal_bbox(node)) };

        let mut body = String::new();
        let mut bbox = BBox::empty();
        let mut written = Vec::new();
        let mut sheet_elements = Vec::new();

        // Children first, so their boxes go under this page's wires.
        for child in self.children(node) {
            let (svg, b, w, els) = self.node_to_svg(child, tx)?;
            written.extend(w);
            sheet_elements.extend(els);
            if !svg.is_empty() {
                body += &svg;
                bbox.add(b);
            }
        }
        for part in self.nodes[node].parts.clone() {
            if !self.c.parts[part].is_net_terminal {
                let (svg, b) = self.part_to_svg(part, tx);
                body += &svg;
                bbox.add(b);
            }
        }
        let mut sheet_elements = self.sheet_connectivity(node, sheet_elements, tx, None);
        if flattened {
            return Ok((body, bbox, written, sheet_elements));
        }

        self.deconflict_sheet_labels(&mut sheet_elements, node, tx);
        for el in &sheet_elements {
            let (svg, b) = element_to_svg(el);
            if !svg.is_empty() {
                body += &svg;
                bbox.add(b);
            }
        }

        // The page is shrunk to the drawing, since an SVG shows at the size it
        // declares. The drawing is only moved, never scaled.
        let nav = self.nav_links_svg(node);
        let (pw, ph) = A_SIZES.iter().find(|(n, _)| *n == paper).map_or((297.0, 210.0), |(_, s)| *s);
        let (page_w, page_h, dx, dy) = if !self.opts.svg_fit || !(bbox.w().is_finite() && bbox.h().is_finite()) {
            (pw, ph, 0.0, 0.0)
        } else {
            let nav_h = if nav.is_empty() { 0.0 } else { NAV_H };
            let content_w = bbox.w() + 2.0 * MARGIN;
            let content_h = bbox.h() + 2.0 * MARGIN;
            let total_w = content_w.max(TITLE_W + 2.0 * MARGIN);
            (
                total_w,
                nav_h + content_h + TITLE_H + MARGIN,
                -bbox.min.x + MARGIN + (total_w - content_w) / 2.0,
                -bbox.min.y + MARGIN + nav_h,
            )
        };
        let mut page = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{page_w:.3}mm\" height=\"{page_h:.3}mm\" viewBox=\"0 0 {page_w:.3} {page_h:.3}\">\n\
             <style>{STYLE}</style>\n<rect width=\"{page_w:.3}\" height=\"{page_h:.3}\" style=\"fill:#FFFFFF\" />\n"
        );
        page += &self.title_block_svg(node, page_w, page_h, if nav.is_empty() { 0.0 } else { NAV_H });
        page += &nav;
        let _ = write!(page, "<g transform=\"translate({dx:.3} {dy:.3})\">\n{body}</g>\n</svg>\n");

        let path = self.filepath.join(self.svg_filename(node));
        std::fs::write(&path, page)?;
        written.push(path);

        let (svg, b) = self.sheet_box_to_svg(node, sheet_tx);
        Ok((svg, b, written, Vec::new()))
    }

    /// Write the design as one or more linked SVG pages. Returns the top page's path.
    pub fn write_top_svg(&mut self) -> Res<PathBuf> {
        let root = self.root;
        let top = if self.top_name.is_empty() { "schematic".to_string() } else { self.top_name.clone() };
        self.kc = WriterState { project_name: top.clone(), ..Default::default() };
        self.nodes[root].title = self.title.clone();
        self.nodes[root].sheet_filename = top;
        // The top node has no parent to take its contents, so it always gets a page.
        self.nodes[root].flattened = false;
        if self.opts.place_fields {
            self.plan_field_layouts(root, true);
            self.kc.pwr_counter = 0;
        }
        let (_, _, written, _) = self.node_to_svg(root, Tx::IDENT)?;
        let top_file = self.filepath.join(self.svg_filename(root));
        Ok(if written.contains(&top_file) { top_file } else { written.last().cloned().unwrap_or_default() })
    }
}
