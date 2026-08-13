//! Fixture-consistency: every eval case is either a hardware-fault STOP with
//! empty fix, or a non-fault with a scorable file gold (I8 / I9).
use std::fs;
use std::path::PathBuf;

use piforge::eval::Case;

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eval/cases")
}

fn load_cases() -> Vec<(PathBuf, Case)> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(cases_dir())
        .expect("eval/cases")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let path = e.path();
        let raw = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let case: Case = serde_json::from_str(&raw)
            .unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
        out.push((path, case));
    }
    out
}

#[test]
fn every_case_is_hw_fault_or_scorable_file_fix() {
    let cases = load_cases();
    assert_eq!(cases.len(), 13, "expected the 13 eval fixtures");
    for (path, c) in &cases {
        let stem = path.file_stem().and_then(|s| s.to_str());
        assert_eq!(
            stem,
            Some(c.id.as_str()),
            "{}: filename stem must equal case id {}",
            path.display(),
            c.id
        );
        if c.gold.is_hardware_fault {
            assert!(
                c.gold.fix_applies.is_empty() && c.gold.fix_must_contain.is_empty(),
                "{}: hardware-fault gold must have empty fix_applies and fix_must_contain",
                path.display()
            );
        } else {
            assert!(
                !c.gold.fix_applies.is_empty(),
                "{}: non-fault gold must have non-empty fix_applies",
                path.display()
            );
            assert!(
                !c.gold.fix_must_contain.is_empty(),
                "{}: non-fault gold must have non-empty fix_must_contain",
                path.display()
            );
            assert!(
                c.setup.files.contains_key(&c.gold.fix_applies),
                "{}: setup.files must contain gold.fix_applies {:?}",
                path.display(),
                c.gold.fix_applies
            );
        }
    }
}

#[test]
fn i2c_wrong_address_0x77_is_wiring_stop_without_device_119() {
    let cases = load_cases();
    let c = cases
        .iter()
        .map(|(_, c)| c)
        .find(|c| c.id == "i2c-wrong-address-0x77")
        .expect("i2c-wrong-address-0x77 fixture");
    assert!(c.gold.is_hardware_fault);
    assert!(c.gold.fix_applies.is_empty());
    assert!(c.gold.fix_must_contain.is_empty());
    assert!(
        c.setup.i2c_bus_present,
        "0x77 wiring STOP keeps the bus node present (distinct from i2c-not-enabled)"
    );
    assert!(
        !c.setup.i2c_devices.contains_key(&119),
        "setup.i2c_devices must not contain 119/0x77, got {:?}",
        c.setup.i2c_devices
    );
    assert!(
        c.setup.i2c_devices.is_empty(),
        "0x77 wiring STOP must have empty i2c_devices"
    );
}
