//! Broker safety-gate regression tests.
//! The key regression: under-voltage STOP must fire even in auto-arm mode.
use piforge::broker::{Broker, ThrottledReader};
use piforge::config::SafetyConfig;
use piforge::hil::Gate; // bring allow() into scope
use serde_json::json;
use std::sync::Arc;

struct StubThrottled {
    now: bool,
    since: bool,
}
impl ThrottledReader for StubThrottled {
    fn under_voltage_active(&self) -> (bool, bool) {
        (self.now, self.since)
    }
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
        Some(Arc::new(StubThrottled {
            now: true,
            since: false,
        })),
        Broker::always_deny(),
    );
    let err = g
        .allow("gpio_set", json!({"pin":17,"value":1}))
        .unwrap_err();
    assert!(err.contains("under-voltage"), "{err}");

    // since-boot only must also block.
    let g = Broker::new(
        auto_cfg(),
        Some(Arc::new(StubThrottled {
            now: false,
            since: true,
        })),
        Broker::always_deny(),
    );
    assert!(g.allow("gpio_set", json!({"pin":17})).is_err());
}

#[test]
fn auto_arm_allows_when_voltage_ok() {
    let g = Broker::new(
        auto_cfg(),
        Some(Arc::new(StubThrottled {
            now: false,
            since: false,
        })),
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
    let g = Broker::new(
        cfg,
        Some(Arc::new(StubThrottled {
            now: false,
            since: false,
        })),
        Arc::new(|_| false),
    );
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
        Some(Arc::new(StubThrottled {
            now: false,
            since: false,
        })),
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
