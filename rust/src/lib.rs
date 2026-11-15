//! schematizer: turn a generic hierarchical netlist (JSON) into a KiCad
//! schematic or an SVG drawing of one.
//!
//! [`render`] is the entry point. The netlist document carries each
//! component's symbol, so no KiCad symbol libraries are needed.

pub mod engine;
pub mod geometry;
pub mod ir;
pub mod loader;
pub mod netcheck;
#[cfg(feature = "python")]
mod python;
pub mod quality;
pub mod sexp;
pub mod uuid;

use serde_json::Value;
use std::path::{Path, PathBuf};

pub use engine::{Error, Fallback, Format, Options};
use engine::{kicad, progress, rng::Rng, Design};

pub const SUPPORTED_TOOLS: [&str; 5] = ["kicad6", "kicad7", "kicad8", "kicad9", "kicad10"];
pub const SUPPORTED_FORMATS: [&str; 2] = ["kicad", "svg"];
pub const SUPPORTED_MODES: [&str; 2] = ["normal", "labels"];

/// How to lay the drawing out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Placed and routed, for people to read.
    Normal,
    /// Labels only: parts on a plain grid, every pin stubbed to a label,
    /// power symbol or no-connect flag. KiCad output only.
    Labels,
}

/// What `render()` is asked to do.
#[derive(Clone, Debug)]
pub struct RenderArgs {
    /// Output directory.
    pub filepath: PathBuf,
    /// Base name for the output files. Defaults to the document's, then "schematic".
    pub top_name: Option<String>,
    /// Title-block text.
    pub title: String,
    pub mode: Mode,
    /// Number of layouts to generate, keeping the one with the lowest cost.
    pub tries: usize,
    /// Have KiCad export the written schematic's netlist and compare it with the input.
    pub check_netlist: bool,
    /// Placement, routing and output options.
    pub options: Options,
}

impl Default for RenderArgs {
    fn default() -> RenderArgs {
        RenderArgs {
            filepath: PathBuf::from("."),
            top_name: None,
            title: "SKiDL-Generated Schematic".into(),
            mode: Mode::Normal,
            tries: 1,
            check_netlist: false,
            options: Options::default(),
        }
    }
}

/// Turn an `auto_stub` pin count into the options the engine reads.
///
/// A net with more than `pins` pins becomes labels.
pub fn set_auto_stub(options: &mut Options, pins: usize) -> Result<(), Error> {
    if pins < 1 {
        return Err(Error::Value(format!(
            "auto_stub must be True, False or a pin count of at least 1, not {}.",
            pins
        )));
    }
    options.auto_stub = true;
    options.auto_stub_max_wire_pins = pins;
    options.auto_stub_fanout = pins + 1;
    Ok(())
}

/// Read a netlist document from a JSON file.
pub fn load_document(path: &Path) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Runtime(format!("{}: {}", path.display(), e)))?;
    serde_json::from_str(&text).map_err(|e| Error::Value(format!("{}: {}", path.display(), e)))
}

fn doc_str<'a>(doc: &'a Value, key: &str) -> Option<&'a str> {
    doc.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Generate schematic files from a generic netlist document. Returns the output directory.
pub fn render(doc: &Value, args: &RenderArgs) -> Result<PathBuf, Error> {
    let tool = args.options.tool.as_str();
    if tool == "kicad5" {
        return Err(Error::Value(
            "KiCad 5 uses the legacy EESCHEMA '.sch' format, which this tool does not write. \
             Use KiCad 6 or later for schematic generation."
                .into(),
        ));
    }
    if !SUPPORTED_TOOLS.contains(&tool) {
        return Err(Error::Value(format!("Unsupported tool {:?}; choose one of {}.", tool, SUPPORTED_TOOLS.join(", "))));
    }
    if args.mode == Mode::Labels && args.options.format != Format::Kicad {
        return Err(Error::Value("Labels-only mode writes KiCad files only (format='kicad').".into()));
    }
    if args.tries < 1 {
        return Err(Error::Value(format!("tries must be at least 1, not {}.", args.tries)));
    }
    if args.check_netlist && args.options.format != Format::Kicad {
        return Err(Error::Value("check_netlist needs KiCad files (format='kicad').".into()));
    }

    let out = render_checked(doc, args)?;
    if args.check_netlist {
        progress::report("checking the netlist");
        let name = args.top_name.as_deref().or(doc_str(doc, "top_name")).unwrap_or("schematic");
        netcheck::check_netlist(doc, &out.join(format!("{}.kicad_sch", name)))?;
    }
    Ok(out)
}

fn render_checked(doc: &Value, args: &RenderArgs) -> Result<PathBuf, Error> {
    if args.tries > 1 && args.mode == Mode::Normal {
        return render_best_layout(doc, args);
    }

    let top_name = args.top_name.as_deref().or(doc_str(doc, "top_name")).unwrap_or("schematic").to_string();
    let title = if args.title.is_empty() {
        doc_str(doc, "title").unwrap_or("SKiDL-Generated Schematic").to_string()
    } else {
        args.title.clone()
    };
    std::fs::create_dir_all(&args.filepath)?;

    let circuit = loader::load_netlist(doc).map_err(Error::Value)?;
    let rng = match &args.options.seed {
        Some(seed) => Rng::from_str(seed),
        None => Rng::from_entropy(),
    };
    let mut design = Design {
        c: circuit,
        nodes: Vec::new(),
        root: 0,
        opts: args.options.clone(),
        rng,
        filepath: args.filepath.clone(),
        top_name,
        title,
        kc: kicad::WriterState::default(),
    };

    if args.mode == Mode::Labels {
        progress::report("writing labels-only schematic");
        engine::labels::write_labels_schematic(&mut design)?;
        return Ok(args.filepath.clone());
    }
    design.gen_schematic()?;
    Ok(args.filepath.clone())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Render several layouts and keep the files of the one with the best score.
fn render_best_layout(doc: &Value, args: &RenderArgs) -> Result<PathBuf, Error> {
    let seed = match &args.options.seed {
        Some(s) => s.clone(),
        None => Rng::from_entropy().getrandbits64().to_string(),
    };

    // A labels-only fallback has no wires, so it would always score best. Such
    // a layout fails here instead; it's the last resort if none can be routed.
    // Overlaps can only be scored from KiCad files, so an SVG is drawn from the
    // seed of the best layout once that's known.
    let mut trial = args.clone();
    trial.tries = 1;
    trial.check_netlist = false;
    trial.options.auto_stub_fallback = Fallback::Raise;
    trial.options.format = Format::Kicad;

    let work_dir = std::env::temp_dir().join(format!("schematizer-{}-{:x}", std::process::id(), Rng::from_entropy().getrandbits64()));
    let result = (|| -> Result<PathBuf, Error> {
        // The layouts are independent, so each is made on a thread of its own.
        // Their progress messages come back here to be shown.
        let trials: Vec<RenderArgs> = (0..args.tries)
            .map(|i| {
                let mut t = trial.clone();
                t.filepath = work_dir.join(i.to_string());
                t.options.seed = Some(format!("{}:{}", seed, i));
                t
            })
            .collect();
        let show = progress::is_enabled();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let outcomes: Vec<Result<PathBuf, Error>> = std::thread::scope(|scope| {
            let handles: Vec<_> = trials
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let tx = tx.clone();
                    let tries = args.tries;
                    scope.spawn(move || {
                        if show {
                            progress::enable_channel(tx);
                        }
                        let _note = progress::prefixed(&format!("layout {}/{},", i + 1, tries));
                        render_checked(doc, t)
                    })
                })
                .collect();
            drop(tx);
            // Pass the messages on until every thread has finished.
            for msg in rx {
                progress::report_raw(&msg);
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(Error::Runtime("a layout thread panicked".into()))))
                .collect()
        });

        let mut best: Option<(PathBuf, String, f64)> = None;
        for (t, outcome) in trials.iter().zip(outcomes) {
            match outcome {
                Ok(_) => {}
                Err(e) if e.is_layout_failure() => continue,
                Err(e) => return Err(e),
            }
            let cost = quality::layout_cost(&quality::score_files(&t.filepath));
            if best.as_ref().is_none_or(|b| cost < b.2) {
                best = Some((t.filepath.clone(), t.options.seed.clone().unwrap(), cost));
            }
        }

        let mut last = args.clone();
        last.tries = 1;
        last.check_netlist = false;
        match best {
            None => {
                // Every layout failed, so handle that as the caller asked.
                last.options.seed = Some(seed.clone());
                render_checked(doc, &last)
            }
            Some((_, best_seed, _)) if args.options.format != Format::Kicad => {
                last.options.seed = Some(best_seed);
                last.options.auto_stub_fallback = Fallback::Raise;
                render_checked(doc, &last)
            }
            Some((best_dir, _, _)) => {
                copy_dir(&best_dir, &args.filepath)?;
                Ok(args.filepath.clone())
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&work_dir);
    result
}
