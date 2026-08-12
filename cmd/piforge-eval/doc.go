// Command piforge-eval runs the capability eval gate: loads cases, runs each
// configured model + sim-served tools, scores fix-rate + hallucination-rate,
// and prints the build/pivot decision.
//
// Use --mock for headless CI runs (no llama-server required): the runner plays
// back canned assistant scripts to exercise the tool dispatch + scorer. The
// real model evaluation omits --mock and points at a running llama-server.
//
// This Go tree (cmd/ + internal/) is the reference implementation: functional
// and tested (go test -race ./...), but frozen. The Rust implementation under
// rust/ is the primary, active path. See README.md and bench/RUST_VS_GO.md.
package main
