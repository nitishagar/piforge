//! Hardware-tool unit tests — run only under `--features hw` (i.e. on the Pi).
//! Covers the pure-logic helpers that don't need real hardware: the kernel-event
//! decoders (`edge_label`/`since_us`), the I2C bus-index parser
//! (`bus_num_from_path`), and `resolve_chip`'s non-empty (configured) branch.
#![cfg(feature = "hw")]

use piforge::hil_hw;
use piforge::hil_hw::gpio_v2::{self, GpioLineEvent};

fn ev(id: u32, ns: u64) -> GpioLineEvent {
    let mut e = GpioLineEvent::default();
    e.id = id;
    e.timestamp_ns = ns;
    e
}

#[test]
fn edge_label_decodes_kernel_ids() {
    assert_eq!(
        hil_hw::edge_label(&ev(gpio_v2::GPIO_LINE_EVENT_RISING_EDGE, 0)),
        "rising"
    );
    assert_eq!(
        hil_hw::edge_label(&ev(gpio_v2::GPIO_LINE_EVENT_FALLING_EDGE, 0)),
        "falling"
    );
    // Unknown id (neither rising nor falling) degrades to the bare "edge" label,
    // matching the sim tool's neutral fallback rather than panicking.
    assert_eq!(hil_hw::edge_label(&ev(0, 0)), "edge");
    assert_eq!(hil_hw::edge_label(&ev(99, 0)), "edge");
}

#[test]
fn since_us_offsets_from_first_event() {
    // t_us = (timestamp_ns - first_ns) / 1000, saturating on any reorder.
    // First event seeds first_ns, so its own offset is 0.
    assert_eq!(hil_hw::since_us(&ev(1, 1_000_000), 1_000_000), 0);
    // 2.5ms later => 2500 us.
    assert_eq!(hil_hw::since_us(&ev(1, 3_500_000), 1_000_000), 2500);
    // A timestamp before first_ns (clock skew/reorder) saturates to 0, not wrap.
    assert_eq!(hil_hw::since_us(&ev(1, 500_000), 1_000_000), 0);
}

#[test]
fn bus_num_from_path_extracts_index() {
    assert_eq!(hil_hw::bus_num_from_path("/dev/i2c-1").unwrap(), "1");
    assert_eq!(hil_hw::bus_num_from_path("/dev/i2c-10").unwrap(), "10");
    assert_eq!(hil_hw::bus_num_from_path("1").unwrap(), "1");
    // No trailing digits => Err, not a silent fallback to "1".
    assert!(hil_hw::bus_num_from_path("/dev/i2c-").is_err());
    assert!(hil_hw::bus_num_from_path("").is_err());
}

#[test]
fn resolve_chip_passes_configured_through() {
    // Hosts without `/dev/gpiochipN` (CI, macOS) keep the configured name.
    // When the node exists, `resolve_chip` canonicalizes symlinks (Pi OS
    // `gpiochip4` → `gpiochip0`); that path is covered by hallpi evidence.
    assert_eq!(hil_hw::resolve_chip("gpiochip4").unwrap(), "gpiochip4");
    assert_eq!(hil_hw::resolve_chip("gpiochip0").unwrap(), "gpiochip0");
}
