//! Fixture internal-consistency check (IMPLICIT_SPEC invariant 7).
//!
//! Loads every `*.json` fixture under `eval/cases` (NOT the paired `*.mock.json`
//! scripts, which are trajectories not cases) and asserts structural consistency:
//!   (a) if `gold.fix_applies` names a file, that file is in `setup.files`;
//!   (b) `gold.fix_must_contain` strings are not also in `fix_must_not_have`;
//!   (c) `is_hardware_fault == true` implies `gold.fix_applies` is empty (the
//!       agent must STOP coding — there is no file to edit).
//!
//! This is a NECESSARY-but-not-sufficient defense. It would NOT have caught the
//! original 0x77 contradiction (semantic symptom⇔setup⇔gold drift that is
//! structurally well-formed). The semantic layer is upheld by the manual
//! spot-check (Phase 4 success criterion) and the red-team swarm. This automated
//! check guards the structural layer that automation CAN express.
use piforge::eval::Case;
use std::path::PathBuf;

/// Collect every case fixture path (excluding `*.mock.json` scripts).
fn case_paths() -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir("../eval/cases")
        .expect("read eval/cases")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            p.extension().and_then(|x| x.to_str()) == Some("json") && !name.ends_with(".mock.json")
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn all_fixtures_are_structurally_consistent() {
    let paths = case_paths();
    assert!(
        !paths.is_empty(),
        "no fixtures found — wrong CWD or missing eval/cases"
    );
    for path in &paths {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let case: Case =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

        // (a) gold.fix_applies, if named, must be a key in setup.files.
        if !case.gold.fix_applies.is_empty() {
            assert!(
                case.setup.files.contains_key(&case.gold.fix_applies),
                "{}: gold.fix_applies='{}' is not in setup.files ({:?})",
                case.id,
                case.gold.fix_applies,
                case.setup.files.keys().collect::<Vec<_>>()
            );
        }

        // (b) no string is simultaneously required AND forbidden — a fixture
        // asking for a contradiction is unpassable by construction.
        for must in &case.gold.fix_must_contain {
            assert!(
                !case.gold.fix_must_not_have.contains(must),
                "{}: '{must}' is in both fix_must_contain and fix_must_not_have — unpassable",
                case.id
            );
        }

        // (c) a hardware fault must NOT name an edit target — the agent's job is
        // to STOP coding, so there is no file to fix.
        if case.gold.is_hardware_fault {
            assert!(
                case.gold.fix_applies.is_empty(),
                "{}: is_hardware_fault=true but fix_applies='{}' — a HW fault requires no code edit",
                case.id,
                case.gold.fix_applies
            );
        }

        // (d) id field must match the filename stem (keeps mock-script lookup
        // by id correct: <cases_dir>/<id>.mock.json).
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        assert_eq!(
            case.id, stem,
            "{}: fixture id does not match filename stem '{stem}'",
            case.id
        );
    }
}

#[test]
fn every_fixture_has_a_mock_script() {
    // A fixture without a mock script scores as a no-op in mock mode, corrupting
    // the pass_rate denominator (IMPLICIT_SPEC invariant 6 edge). Every case
    // MUST have a paired <id>.mock.json once we scale beyond the seed set.
    for path in case_paths() {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let mock = path.with_file_name(format!("{stem}.mock.json"));
        assert!(
            mock.exists(),
            "{}: missing mock script {} (a case without a script is a no-op that corrupts pass_rate)",
            stem,
            mock.display()
        );
    }
}
