//! Unit tests of the pieces that have to agree exactly with the Python version.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use schematizer::geometry::{pyround, round_to, BBox, Point, Tx};
use schematizer::netcheck::compare_nets;
use schematizer::quality::{count_crossings, layout_cost, Score};
use schematizer::sexp::Sexp;
use schematizer::uuid::gen_uuid;

#[test]
fn uuids_match_python_uuid5() {
    // From Python: uuid.uuid5(UUID("7026fcc6-e1a0-409e-aaf4-6a17ea82654f"), name).
    assert_eq!(gen_uuid("R1"), "74098eae-8838-5d57-af42-a6aec810bd0d");
    assert_eq!(gen_uuid("wire:N1:1.27:2.54:3.81:2.54"), "544d4a7c-ee57-55f6-a48b-6e7a1803a5b9");
}

#[test]
fn rounding_matches_python() {
    assert_eq!(pyround(2.5), 2.0);
    assert_eq!(pyround(-0.5), 0.0);
    assert_eq!(round_to(1.005, 2), 1.0);
    assert_eq!(round_to(2.675, 2), 2.67);
    assert_eq!(round_to(0.125, 2), 0.12);
}

#[test]
fn transforms_compose_like_the_python_ones() {
    let tx = Tx::from_symtx("RH");
    let p = Point::new(1.0, 2.0) * tx;
    // R turns (1, 2) to (2, -1), then H mirrors it to (-2, -1).
    assert_eq!((p.x, p.y), (-2.0, -1.0));
    let (angle, mx, my) = Tx::from_symtx("H").analyze_transform();
    // (As in the Python version, a horizontal mirror comes out as a turn plus an X mirror.)
    assert_eq!((angle, mx, my), (180.0, true, false));
    let b = BBox::new(Point::new(0.0, 0.0), Point::new(2.0, 1.0)) * Tx::ROT_90;
    assert_eq!((b.w(), b.h()), (1.0, 2.0));
}

#[test]
fn sexp_round_trips_with_kicad_quoting() {
    let mut s = Sexp::parse(r#"(kicad_sch (version 20250114) (property "Reference" "R1" (at 1.27 -2.5 0)) (pin "1" (uuid abc)))"#).unwrap();
    s.add_kicad_quotes();
    let text = s.to_text();
    assert!(text.contains(r#"(property "Reference" "R1""#), "{text}");
    assert!(text.contains("(at 1.27 -2.5 0)"), "{text}");
    assert!(text.contains(r#"(pin "1""#), "{text}");
    let again = Sexp::parse(&text).unwrap();
    assert_eq!(again.field("property").unwrap().str_at(2), "R1");
}

#[test]
fn crossings_dont_count_corners_tees_or_junctions() {
    let cross = ((0.0, 5.0), (10.0, 5.0));
    let vert = ((5.0, 0.0), (5.0, 10.0));
    assert_eq!(count_crossings(&[cross, vert], &HashSet::new()), 1);
    // A tee: the vertical segment ends on the horizontal one.
    assert_eq!(count_crossings(&[cross, ((5.0, 5.0), (5.0, 10.0))], &HashSet::new()), 0);
    // Joined by a junction.
    let junctions: HashSet<(i64, i64)> = [(500, 500)].into();
    assert_eq!(count_crossings(&[cross, vert], &junctions), 0);
}

#[test]
fn layout_cost_charges_bends_crossings_and_overlaps() {
    let s = Score { wirelength: 100.0, bends: 2, crossings: 1, part_overlaps: 1, text_overlaps: 1, ..Default::default() };
    assert_eq!(layout_cost(&s), 100.0 + 5.0 * 2.0 + 5.0 + 10.0 * 2.0);
}

#[test]
fn netlist_differences_are_described() {
    let pin = |r: &str, n: &str| (r.to_string(), n.to_string());
    let mut source = BTreeMap::new();
    source.insert("SDA".to_string(), BTreeSet::from([pin("U1", "1"), pin("R1", "1")]));
    source.insert("SCL".to_string(), BTreeSet::from([pin("U1", "2"), pin("R2", "1")]));
    // Drawn with SDA and SCL joined.
    let drawn = vec![BTreeSet::from([pin("U1", "1"), pin("R1", "1"), pin("U1", "2"), pin("R2", "1")])];
    let problems = compare_nets(&source, &drawn);
    assert_eq!(problems, vec!["SCL is shorted to SDA".to_string()]);
    assert!(compare_nets(&source, &source.values().cloned().collect::<Vec<_>>()).is_empty());
}
