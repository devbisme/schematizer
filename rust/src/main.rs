//! Command-line interface: generic netlist JSON -> schematic files.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use schematizer::engine::gen_schematic::AUTO_STUB_PINS;
use schematizer::engine::progress;
use schematizer::{
    load_document, netcheck, render, set_auto_stub, Error, Format, Mode, RenderArgs, SUPPORTED_FORMATS,
    SUPPORTED_MODES, SUPPORTED_TOOLS,
};

const USAGE: &str = "usage: schematizer [-h] [-t TOOL] [-f FORMAT] [-o OUTPUT] [--top-name TOP_NAME]
                   [--title TITLE] [-m MODE] [--flatness FLATNESS] [--seed SEED]
                   [--tries TRIES] [--sheet-tries N] [--no-reuse] [--no-preplace]
                   [--no-power-symbols] [--no-nudge] [--auto-stub [PINS]]
                   [--check-netlist] [--progress] [--no-progress] [--version]
                   netlist";

fn help() -> String {
    format!(
        "{USAGE}

Generate KiCad schematic or SVG files from a generic hierarchical netlist (JSON).

positional arguments:
  netlist               Path to the generic netlist JSON file.

options:
  -h, --help            Show this help message and exit.
  -t, --tool TOOL       Target KiCad version: {tools} (default: kicad9).
  -f, --format FORMAT   Output format: editable .kicad_sch files (kicad), or static
                        cross-linked .svg pages (svg) (default: kicad).
  -o, --output OUTPUT   Output directory for the generated files (default: .).
  --top-name TOP_NAME   Base name for output files (default: from the netlist).
  --title TITLE         Schematic title-block text.
  -m, --mode MODE       Layout mode: 'normal' (placed and routed) or 'labels'
                        (labels-only, exact connectivity, no routing) (default: normal).
  --flatness FLATNESS   Hierarchy flattening, 0.0 (fully hierarchical, one file per
                        sheet) .. 1.0 (everything on one sheet).
  --seed SEED           Seed the placer/router so the drawing is reproducible: the
                        same netlist, seed and options give the same output. Omit
                        for a different (equally valid) layout each run.
  --tries TRIES         Generate this many layouts and keep the one with the best
                        wiring score. Takes about this many times as long.
  --sheet-tries N       Lay out each sheet this many times and keep its best layout,
                        scored like --tries scores a drawing. Unlike --tries, each
                        sheet gets the best of its own tries.
  --no-reuse            Lay out every sheet separately. Normally a sheet holding the
                        same circuit as an earlier one is drawn the same way.
  --no-preplace         Place connected parts with force-directed placement from
                        random starting positions, instead of putting each part
                        beside the pins it's wired to.
  --no-power-symbols    Wire power nets (GND, VCC, +3V3, ...) like any other net
                        instead of putting a power symbol on each of their pins.
  --no-nudge            Leave parts where placement put them. Normally, once a sheet
                        is routed, parts are moved a step or two to straighten wires
                        that jog sideways and the sheet is routed again.
  --auto-stub [PINS]    Draw hard-to-route nets with labels instead of wires: nets
                        with more than PINS pins (default: {pins}) and nets whose pins
                        are far apart. Fixes what KiCad's ERC still finds (needs
                        kicad-cli), and writes a labels-only schematic if routing
                        still fails.
  --check-netlist       After writing, have KiCad export the schematic's netlist and
                        compare it with the input. Exits with status 1 if they differ
                        (needs kicad-cli; KiCad format only).
  --progress            Show which sheet is being placed or routed (default: on when
                        stderr is a terminal).
  --no-progress         Don't show progress.
  --version             Show the program's version number and exit.",
        tools = SUPPORTED_TOOLS.join(", "),
        pins = AUTO_STUB_PINS,
    )
}

struct Cli {
    netlist: PathBuf,
    args: RenderArgs,
    progress: Option<bool>,
    format_name: String,
}

fn usage_error(msg: &str) -> String {
    format!("{USAGE}\nschematizer: error: {msg}")
}

fn parse(argv: &[String]) -> Result<Option<Cli>, String> {
    let mut args = RenderArgs::default();
    let mut netlist: Option<PathBuf> = None;
    let mut progress = None;
    let mut format_name = "kicad".to_string();
    let mut i = 0;

    let value = |i: &mut usize, name: &str, inline: Option<String>| -> Result<String, String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        argv.get(*i).cloned().ok_or_else(|| usage_error(&format!("argument {name}: expected one argument")))
    };

    while i < argv.len() {
        let arg = argv[i].clone();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        match name.as_str() {
            "-h" | "--help" => {
                println!("{}", help());
                return Ok(None);
            }
            "--version" => {
                println!("schematizer {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-t" | "--tool" => {
                let v = value(&mut i, "-t/--tool", inline)?;
                if !SUPPORTED_TOOLS.contains(&v.as_str()) {
                    return Err(usage_error(&format!(
                        "argument -t/--tool: invalid choice: '{v}' (choose from {})",
                        SUPPORTED_TOOLS.join(", ")
                    )));
                }
                args.options.tool = v;
            }
            "-f" | "--format" => {
                let v = value(&mut i, "-f/--format", inline)?;
                args.options.format = match v.as_str() {
                    "kicad" => Format::Kicad,
                    "svg" => Format::Svg,
                    _ => {
                        return Err(usage_error(&format!(
                            "argument -f/--format: invalid choice: '{v}' (choose from {})",
                            SUPPORTED_FORMATS.join(", ")
                        )))
                    }
                };
                format_name = v;
            }
            "-o" | "--output" => args.filepath = PathBuf::from(value(&mut i, "-o/--output", inline)?),
            "--top-name" => args.top_name = Some(value(&mut i, "--top-name", inline)?),
            "--title" => args.title = value(&mut i, "--title", inline)?,
            "-m" | "--mode" => {
                let v = value(&mut i, "-m/--mode", inline)?;
                args.mode = match v.as_str() {
                    "normal" => Mode::Normal,
                    "labels" => Mode::Labels,
                    _ => {
                        return Err(usage_error(&format!(
                            "argument -m/--mode: invalid choice: '{v}' (choose from {})",
                            SUPPORTED_MODES.join(", ")
                        )))
                    }
                };
            }
            "--flatness" => {
                let v = value(&mut i, "--flatness", inline)?;
                args.options.flatness =
                    v.parse().map_err(|_| usage_error(&format!("argument --flatness: invalid float value: '{v}'")))?;
            }
            "--seed" => {
                let v = value(&mut i, "--seed", inline)?;
                v.parse::<i64>().map_err(|_| usage_error(&format!("argument --seed: invalid int value: '{v}'")))?;
                args.options.seed = Some(v);
            }
            "--tries" => {
                let v = value(&mut i, "--tries", inline)?;
                args.tries = v.parse().map_err(|_| usage_error(&format!("argument --tries: invalid int value: '{v}'")))?;
            }
            "--sheet-tries" => {
                let v = value(&mut i, "--sheet-tries", inline)?;
                args.options.sheet_tries =
                    v.parse().map_err(|_| usage_error(&format!("argument --sheet-tries: invalid int value: '{v}'")))?;
            }
            "--no-reuse" => args.options.reuse_layouts = false,
            "--no-preplace" => args.options.preplace = false,
            "--no-power-symbols" => args.options.power_symbols = false,
            "--no-nudge" => args.options.nudge = false,
            "--auto-stub" => {
                // The pin count is optional: take the next argument only if it's a number.
                let pins = match inline {
                    Some(v) => v.parse().map_err(|_| usage_error(&format!("argument --auto-stub: invalid int value: '{v}'")))?,
                    None => match argv.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                        Some(n) => {
                            i += 1;
                            n
                        }
                        None => AUTO_STUB_PINS,
                    },
                };
                set_auto_stub(&mut args.options, pins).map_err(|e| format!("schematizer: error: {e}"))?;
            }
            "--check-netlist" => args.check_netlist = true,
            "--progress" => progress = Some(true),
            "--no-progress" => progress = Some(false),
            _ if name.starts_with('-') && name.len() > 1 => {
                return Err(usage_error(&format!("unrecognized arguments: {arg}")));
            }
            _ => {
                if netlist.is_some() {
                    return Err(usage_error(&format!("unrecognized arguments: {arg}")));
                }
                netlist = Some(PathBuf::from(arg));
            }
        }
        i += 1;
    }
    let netlist = netlist.ok_or_else(|| usage_error("the following arguments are required: netlist"))?;
    Ok(Some(Cli { netlist, args, progress, format_name }))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(&argv) {
        Ok(Some(cli)) => cli,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    if cli.progress.unwrap_or_else(|| std::io::stderr().is_terminal()) {
        progress::enable_stderr();
    }
    let result = load_document(&cli.netlist).and_then(|doc| render(&doc, &cli.args));
    progress::disable();

    match result {
        Ok(out) => {
            println!("Wrote {} files to {}", cli.format_name, out.display());
            ExitCode::SUCCESS
        }
        Err(e @ (Error::Placement(_) | Error::Routing(_))) => {
            let stage = if matches!(e, Error::Placement(_)) { "placed" } else { "routed" };
            let text = e.to_string();
            let detail = if text.is_empty() { String::new() } else { format!(" ({text})") };
            eprintln!(
                "schematizer: error: the drawing couldn't be {stage}{detail}. \
                 Try --auto-stub, more --tries, another --seed, or --mode labels."
            );
            ExitCode::from(1)
        }
        Err(Error::NetlistMismatch(problems)) => {
            eprintln!("schematizer: error: {}", netcheck::mismatch_message(&problems));
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("schematizer: error: {e}");
            ExitCode::from(1)
        }
    }
}
