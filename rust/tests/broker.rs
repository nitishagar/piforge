//! Broker safety-gate regression tests.
//! The key regression: under-voltage STOP must fire even in auto-arm mode.
use piforge::broker::{Broker, ThrottledReader, UvReading};
use piforge::config::SafetyConfig;
use piforge::hil::{self, Gate}; // bring allow() into scope
use serde_json::json;
use std::io::IsTerminal;
use std::sync::Arc;

struct StubThrottled {
    known: bool,
    now: bool,
    since: bool,
}
impl ThrottledReader for StubThrottled {
    fn under_voltage_active(&self) -> UvReading {
        UvReading {
            known: self.known,
            now: self.now,
            since: self.since,
        }
    }
}

fn stub(known: bool, now: bool, since: bool) -> Arc<StubThrottled> {
    Arc::new(StubThrottled { known, now, since })
}

fn auto_cfg() -> SafetyConfig {
    SafetyConfig {
        arm_mode: "auto".into(),
        stop_on_under_voltage: true,
        per_pin_max_current_ma: 12,
        rail_budget_ma: 50,
    }
}

#[test]
fn under_voltage_blocks_even_in_auto_mode() {
    // Regression: the auto short-circuit must NOT bypass the under-voltage STOP.
    let g = Broker::new(
        auto_cfg(),
        Some(stub(true, true, false)),
        Broker::always_deny(),
    );
    let err = g
        .allow("gpio_set", json!({"pin":17,"value":1}))
        .unwrap_err();
    assert!(err.contains("under-voltage"), "{err}");

    // since-boot only must also block.
    let g = Broker::new(
        auto_cfg(),
        Some(stub(true, false, true)),
        Broker::always_deny(),
    );
    assert!(g.allow("gpio_set", json!({"pin":17})).is_err());
}

#[test]
fn auto_arm_allows_when_voltage_ok() {
    let g = Broker::new(
        auto_cfg(),
        Some(stub(true, false, false)),
        Broker::always_deny(),
    );
    g.allow("gpio_set", json!({"pin":17,"value":1}))
        .expect("healthy power + auto should allow");
}

#[test]
fn confirm_deny_blocks() {
    let cfg = SafetyConfig {
        arm_mode: "confirm".into(),
        stop_on_under_voltage: true,
        ..Default::default()
    };
    let g = Broker::new(cfg, Some(stub(true, false, false)), Arc::new(|_| false));
    assert!(g.allow("gpio_set", json!({"pin":17,"value":1})).is_err());
}

#[test]
fn scoped_arm_re_approves_same_pin_within_window() {
    let cfg = SafetyConfig {
        arm_mode: "confirm".into(),
        stop_on_under_voltage: true,
        ..Default::default()
    };
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c2 = counter.clone();
    let g = Broker::new(
        cfg,
        Some(stub(true, false, false)),
        Arc::new(move |_| {
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }),
    );
    g.allow("gpio_set", json!({"pin":17,"value":1})).unwrap();
    // Same pin within window => no re-confirm.
    g.allow("gpio_set", json!({"pin":17,"value":0})).unwrap();
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "scoped arm should not re-confirm same pin"
    );
    // Different pin => re-confirm.
    g.allow("gpio_set", json!({"pin":18,"value":1})).unwrap();
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "different pin should re-confirm"
    );
}

#[test]
fn unknown_throttled_denies_in_auto() {
    let g = Broker::new(
        auto_cfg(),
        Some(stub(false, false, false)),
        Broker::always_deny(),
    );
    assert!(g.allow("gpio_set", json!({"pin":17,"value":1})).is_err());
}

#[test]
fn missing_telemetry_denies_class_i() {
    let g = Broker::new(auto_cfg(), None, Broker::always_deny());
    assert!(g.allow("gpio_set", json!({"pin":17,"value":1})).is_err());
}

#[test]
fn temp_na_does_not_set_telemetry_known_false() {
    let v = hil::telemetry_snapshot(Some(0), "N/A", None);
    assert_eq!(v["cpu_temp"], json!("N/A"));
    assert_eq!(v["telemetry_known"], json!(true));
    assert_eq!(v["throttled_raw"], json!("0x0"));
}

#[test]
fn confirm_line_accepts_yes() {
    assert!(Broker::confirm_line("yes"));
    assert!(Broker::confirm_line("YES"));
    assert!(Broker::confirm_line("y"));
    assert!(Broker::confirm_line("Y"));
    assert!(Broker::confirm_line(" yes "));
    assert!(!Broker::confirm_line("no"));
    assert!(!Broker::confirm_line("Yes"));
}

#[test]
fn stdin_confirmer_denies_non_tty_without_read() {
    assert!(
        !std::io::stdin().is_terminal(),
        "this test must run with non-TTY stdin"
    );
    let f = Broker::stdin_confirmer();
    assert!(!f("approve?"));
}
