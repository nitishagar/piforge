//! PiForge — Raspberry Pi 5 hardware-fault-clearance harness.
//!
//! Distinguishes faults that look like broken hardware (pin numbering, I2C
//! address, library, overlay, IIO scale) from STOP hardware faults (shorted
//! I2C, undervoltage). May edit workspace-relative driver/config files when
//! the gold fix is software.
//!
//! Modules:
//!   - [`config`] — TOML config + defaults + validation
//!   - [`provider`] — llama-server (OpenAI-compat) client
//!   - [`hil`] — typed HIL tool surface (real on Linux `hw` feature; stubs off)
//!   - [`sim`] — simulated tools driven by eval Case fixtures
//!   - [`broker`] — safety gate (R/B/I op classification, under-voltage stop)
//!   - [`agent`] — ReAct loop with prefix-cache discipline
//!   - [`eval`] — capability eval gate + scorer + decision rule
pub mod agent;
pub mod broker;
pub mod config;
pub mod eval;
pub mod hil;
pub mod provider;
pub mod sim;

#[cfg(feature = "hw")]
pub mod hil_hw;
