//! Draw every test netlist and check the result.
//!
//! With kicad-cli installed, KiCad's netlist of each schematic is compared
//! with the input; without it, only that the files are written.

use std::path::{Path, PathBuf};

use schematizer::netcheck::find_kicad_cli;
use schematizer::quality::score_files;
use schematizer::{load_document, render, Format, Mode, RenderArgs};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/data")
}

fn netlists() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(data_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    found.sort();
    assert!(!found.is_empty());
    found
}

fn out_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("schematizer-test-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn args(dir: &Path, name: &str, seed: &str) -> RenderArgs {
    let mut args = RenderArgs { filepath: dir.to_path_buf(), top_name: Some(name.into()), ..Default::default() };
    args.options.seed = Some(seed.into());
    args.check_netlist = find_kicad_cli().is_some();
    args
}

#[test]
fn every_netlist_is_drawn_with_its_connections() {
    for path in netlists() {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let doc = load_document(&path).unwrap();
        for seed in ["1", "2"] {
            let dir = out_dir(&format!("{name}-{seed}"));
            render(&doc, &args(&dir, &name, seed)).unwrap_or_else(|e| panic!("{name} seed {seed}: {e}"));
            assert!(dir.join(format!("{name}.kicad_sch")).exists(), "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn labels_only_mode_is_exact() {
    for path in netlists() {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let doc = load_document(&path).unwrap();
        let dir = out_dir(&format!("labels-{name}"));
        let mut a = args(&dir, &name, "1");
        a.mode = Mode::Labels;
        render(&doc, &a).unwrap_or_else(|e| panic!("{name}: {e}"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn svg_pages_have_the_kicad_wiring() {
    let path = data_dir().join("span_hier.json");
    let doc = load_document(&path).unwrap();
    let (kdir, sdir) = (out_dir("kicad-span"), out_dir("svg-span"));
    let mut a = args(&kdir, "span", "3");
    a.check_netlist = false;
    render(&doc, &a).unwrap();
    a.filepath = sdir.clone();
    a.options.format = Format::Svg;
    render(&doc, &a).unwrap();
    let (k, s) = (score_files(&kdir), score_files(&sdir));
    assert!(k.segments > 0);
    assert_eq!((k.segments, k.bends, k.wirelength), (s.segments, s.bends, s.wirelength));
    let _ = std::fs::remove_dir_all(&kdir);
    let _ = std::fs::remove_dir_all(&sdir);
}

#[test]
fn a_seed_makes_the_drawing_reproducible() {
    let path = data_dir().join("esp32_audio_board.json");
    let doc = load_document(&path).unwrap();
    let read = |dir: &Path| std::fs::read_to_string(dir.join("esp.kicad_sch")).unwrap();
    let (a, b) = (out_dir("seed-a"), out_dir("seed-b"));
    let mut args_a = args(&a, "esp", "7");
    args_a.check_netlist = false;
    args_a.tries = 3;
    render(&doc, &args_a).unwrap();
    let mut args_b = args_a.clone();
    args_b.filepath = b.clone();
    render(&doc, &args_b).unwrap();
    assert_eq!(read(&a), read(&b));
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
