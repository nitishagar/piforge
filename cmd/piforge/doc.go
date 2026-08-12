// Command piforge runs the interactive PiForge agent against a local
// llama-server. Configure via piforge.toml or PIFORGE_* env vars.
//
// This Go tree (cmd/ + internal/) is the reference implementation: functional
// and tested (go test -race ./...), but frozen. The Rust implementation under
// rust/ is the primary, active path. See README.md and bench/RUST_VS_GO.md.
package main
