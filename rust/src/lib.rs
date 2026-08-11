//! PiForge — the agent that understands live hardware state.
//!
//! A coding harness for Raspberry Pi 5 that closes the loop between your code
//! and the live sensor / GPIO / I2C state. Rust port (parallel to the Go
//! implementation on `main`). Must beat the Go baseline on binary size, RSS,
//! and scope jitter to justify a merge.
//!
//! Architecture mirrors the Go packages:
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
