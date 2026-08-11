//! Regression for the gpiod `v2` feature.
//!
//! The hw scope path compiles to `todo!()` (panic) without gpiod's default `v2`
//! feature (`gpiod-0.3.0/src/lib.rs`). piforge inherits `v2` only because it does
//! NOT pass `default-features = false` to gpiod. With `panic = "abort"`, a
//! `todo!()` panic aborts the whole binary mid-capture. Any future change that
//! disables gpiod defaults silently reintroduces the abort — this test pins it.
//!
//! A text scan (not `cargo-metadata`) so it needs no extra dev-dependency.
#[test]
fn gpiod_keeps_default_features() {
    let manifest = include_str!("../Cargo.toml");
    let gpiod_line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("gpiod") && l.contains('='))
        .expect("a `gpiod = ...` dependency line in Cargo.toml");
    assert!(
        !gpiod_line.contains("default-features = false")
            && !gpiod_line.contains("default-features=false"),
        "gpiod must keep default features (the `v2` feature is load-bearing for \
         the scope path under `panic = \"abort\"`). Offending line:\n  {gpiod_line}"
    );
}
