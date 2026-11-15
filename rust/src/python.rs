//! The Python extension module `schematizer._engine`.
//!
//! The Python package keeps its API (`schematizer.render()`, the CLI and the
//! exception classes) and calls `render()` here to do the work. Errors come
//! back as data, so the Python side raises the exception classes callers
//! already catch.

use pyo3::prelude::*;
use serde_json::Value;
use std::path::PathBuf;

use crate::engine::{progress, Error, Fallback, Format, Options};
use crate::{set_auto_stub, Mode, RenderArgs};

fn options_from(opts: &Value, base: Options) -> Result<Options, Error> {
    let mut o = base;
    let get = |k: &str| opts.get(k).filter(|v| !v.is_null());
    let as_bool = |k: &str, d: bool| get(k).map_or(d, |v| v.as_bool().unwrap_or(d));
    let as_usize = |k: &str, d: usize| get(k).and_then(|v| v.as_u64()).map_or(d, |v| v as usize);
    o.flatness = get("flatness").and_then(|v| v.as_f64()).unwrap_or(o.flatness);
    o.retries = as_usize("retries", o.retries);
    o.power_symbols = as_bool("power_symbols", o.power_symbols);
    o.preplace = as_bool("preplace", o.preplace);
    o.nudge = as_bool("nudge", o.nudge);
    o.sheet_tries = as_usize("sheet_tries", o.sheet_tries);
    o.reuse_layouts = as_bool("reuse_layouts", o.reuse_layouts);
    o.label_local_nets = as_bool("label_local_nets", o.label_local_nets);
    o.label_clearance = as_bool("label_clearance", o.label_clearance);
    o.place_fields = as_bool("place_fields", o.place_fields);
    o.svg_fit = as_bool("svg_fit", o.svg_fit);
    o.orientation_pin_limit = as_usize("orientation_pin_limit", o.orientation_pin_limit);
    o.dont_rotate_pin_count = as_usize("dont_rotate_pin_count", o.dont_rotate_pin_count);
    o.erc_max_iterations = as_usize("erc_max_iterations", o.erc_max_iterations);
    if let Some(f) = get("auto_stub_fallback").and_then(|v| v.as_str()) {
        o.auto_stub_fallback = match f {
            "labels" => Fallback::Labels,
            "raise" => Fallback::Raise,
            "warn" => Fallback::Warn,
            _ => return Err(Error::Value(format!("auto_stub_fallback must be 'labels', 'raise' or 'warn', not {:?}.", f))),
        };
    }
    // auto_stub is False, True (3 pins) or a pin count.
    match get("auto_stub") {
        None | Some(Value::Bool(false)) => o.auto_stub = false,
        Some(Value::Bool(true)) => set_auto_stub(&mut o, crate::engine::gen_schematic::AUTO_STUB_PINS)?,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(pins) if pins >= 1 => set_auto_stub(&mut o, pins as usize)?,
            _ => return Err(Error::Value(format!("auto_stub must be True, False or a pin count of at least 1, not {}.", n))),
        },
        Some(v) => return Err(Error::Value(format!("auto_stub must be True, False or a pin count, not {}.", v))),
    }
    // Thresholds given on their own win over those auto_stub sets.
    o.auto_stub_max_wire_pins = as_usize("auto_stub_max_wire_pins", o.auto_stub_max_wire_pins);
    o.auto_stub_fanout = as_usize("auto_stub_fanout", o.auto_stub_fanout);
    o.auto_stub_max_wire_dist = get("auto_stub_max_wire_dist").and_then(|v| v.as_f64()).unwrap_or(o.auto_stub_max_wire_dist);
    Ok(o)
}

/// Render a netlist document (as JSON text). See `schematizer.render()`.
///
/// Returns `(error kind, message or output directory, problems)`. The kind is
/// "" on success, else "value", "placement", "routing", "netlist" or "runtime".
#[pyfunction]
#[pyo3(signature = (doc_json, filepath, top_name, title, tool, format, mode, seed, tries, check_netlist, options_json, progress=None))]
#[allow(clippy::too_many_arguments)]
fn render(
    doc_json: &str,
    filepath: &str,
    top_name: Option<String>,
    title: String,
    tool: String,
    format: &str,
    mode: &str,
    seed: Option<String>,
    tries: usize,
    check_netlist: bool,
    options_json: &str,
    progress: Option<PyObject>,
) -> (String, String, Vec<String>) {
    let result = (|| -> Result<PathBuf, Error> {
        let doc: Value = serde_json::from_str(doc_json).map_err(|e| Error::Value(e.to_string()))?;
        let opts: Value = serde_json::from_str(options_json).map_err(|e| Error::Value(e.to_string()))?;
        let mut options = options_from(&opts, Options::default())?;
        options.tool = tool;
        options.seed = seed;
        options.format = if format == "svg" { Format::Svg } else { Format::Kicad };
        let args = RenderArgs {
            filepath: PathBuf::from(filepath),
            top_name,
            title,
            mode: if mode == "labels" { Mode::Labels } else { Mode::Normal },
            tries,
            check_netlist,
            options,
        };
        if let Some(cb) = progress {
            progress::enable_func(Box::new(move |msg: &str| {
                Python::with_gil(|py| {
                    let _ = cb.call1(py, (msg,));
                });
            }));
        }
        let out = crate::render(&doc, &args);
        progress::disable();
        out
    })();
    match result {
        Ok(path) => (String::new(), path.to_string_lossy().into_owned(), Vec::new()),
        Err(e) => {
            let kind = match &e {
                Error::Value(_) => "value",
                Error::Placement(_) => "placement",
                Error::Routing(_) => "routing",
                Error::NetlistMismatch(_) => "netlist",
                Error::Runtime(_) => "runtime",
            };
            let problems = if let Error::NetlistMismatch(p) = &e { p.clone() } else { Vec::new() };
            (kind.to_string(), e.to_string(), problems)
        }
    }
}

#[pymodule]
fn _engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(render, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
