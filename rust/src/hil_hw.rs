//! Real hardware tools, Linux-only, behind the `hw` feature.
//! - GPIO get/set + scope (edge events): the `gpiod` crate (pure Rust, no
//!   libgpiod C dep; talks to /dev/gpiochipN directly).
//! - I2C read: `linux-embedded-hal` (nix-based ioctl on /dev/i2c-N).
//! - Telemetry (vcgencmd get_throttled decode): shell out (no Rust lib; low freq).
//!
//! Build with `--features hw` on Linux (the Pi). Off-Linux or without the
//! feature, the sim tools stand in (dev/eval loop).
#![cfg(feature = "hw")]

use parking_lot::Mutex;
use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::broker::ThrottledReader;
use crate::hil::{Gate, Tool, ToolResult};

/// Resolve the gpiochip name for the Pi 5 40-pin header.
pub fn resolve_chip(configured: &str) -> Result<String, String> {
    if !configured.is_empty() {
        return Ok(configured.into());
    }
    // Pi 5 => gpiochip4; fall back to gpiochip0 for older boards.
    for name in &["gpiochip4", "gpiochip0"] {
        if gpiod::Chip::new(*name).is_ok() {
            return Ok((*name).into());
        }
    }
    Err("no gpiochip found (set hardware.gpiochip in config)".into())
}

// ---------------- GPIO ----------------

pub struct GpioTool {
    chip: String,
    gate: Option<std::sync::Arc<dyn Gate>>,
}

impl GpioTool {
    pub fn new(
        chip: impl Into<String>,
        gate: Option<std::sync::Arc<dyn Gate>>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            chip: chip.into(),
            gate,
        })
    }
}

#[async_trait]
impl Tool for GpioTool {
    fn name(&self) -> &str {
        "gpio"
    }
    fn description(&self) -> &str {
        "GPIO get (safe) / set (Class I: physical, requires arming)."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["get","set"]},"pin":{"type":"integer"},"value":{"type":"integer","enum":[0,1]}},"required":["action","pin"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let chip = match gpiod::Chip::new(&self.chip) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::err(
                    "gpio",
                    format!("open {}: {e} (is user in 'gpio' group?)", self.chip),
                )
            }
        };
        match action {
            "get" => {
                let opts = gpiod::Options::input([pin]).consumer("piforge");
                let lines = match chip.request_lines(opts) {
                    Ok(l) => l,
                    Err(e) => return ToolResult::err("gpio", format!("request pin {pin}: {e}")),
                };
                let val = match lines.get_values([false]) {
                    Ok([v]) => {
                        if v {
                            1
                        } else {
                            0
                        }
                    }
                    _ => return ToolResult::err("gpio", format!("read pin {pin}")),
                };
                ToolResult::ok_unit(
                    "gpio",
                    json!({"pin":pin,"value":val,"chip":self.chip}),
                    "level(0|1)",
                )
            }
            "set" => {
                let value = args.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if value != 0 && value != 1 {
                    return ToolResult::err("gpio", "value must be 0 or 1");
                }
                if let Some(g) = &self.gate {
                    if let Err(e) = g.allow(
                        "gpio_set",
                        json!({"chip":self.chip,"pin":pin,"value":value}),
                    ) {
                        return ToolResult::err("gpio", format!("DENIED by safety gate: {e}"));
                    }
                }
                let v = value != 0;
                let opts = gpiod::Options::output([pin])
                    .values([v])
                    .consumer("piforge");
                let lines = match chip.request_lines(opts) {
                    Ok(l) => l,
                    Err(e) => {
                        return ToolResult::err("gpio", format!("request pin {pin} as output: {e}"))
                    }
                };
                // Hold the line for the duration of the call; it releases on drop.
                drop(lines);
                ToolResult::ok_unit(
                    "gpio",
                    json!({"pin":pin,"value":value,"chip":self.chip,"driven":true}),
                    "level(0|1)",
                )
            }
            other => ToolResult::err("gpio", format!("unknown action {other}")),
        }
    }
}

// ---------------- Scope (kernel edge events via a raw-fd ppoll loop) ----------------
//
// The scope path does NOT use gpiod's `Lines`/`read_event`: `read_event` blocks
// until the next edge with no timeout argument, so the old `while Instant::now()
// < deadline` loop was never re-evaluated on an idle pin — the capture hung
// today on a quiet pin. Wrapping it in `tokio::time::timeout` +
// `spawn_blocking` would instead leak a blocking thread on every idle-pin
// timeout (tokio blocking threads are not interruptible), and idle-pin scope is
// the common case (the user scopes a pin to *see if* it's toggling).
//
// Committed mechanism: open `/dev/gpiochipN` directly, issue the
// kernel's GPIO_V2 line-request ioctls (input + both edges) to obtain a line fd,
// then `ppoll` that fd with a duration-relative deadline. ppoll bounds the
// capture at the syscall level (returns on timeout) with zero thread leak, and a
// stray SIGCHLD from the agent's own i2cdetect/vcgencmd/i2cget shell-outs (or
// tokio child reaping) surfaces as EINTR and is retried against the remaining
// deadline rather than truncating a capture on a *toggling* pin. The GPIO get/set
// path stays on gpiod's `Lines` — those calls return promptly; only `read_event`
// blocks. The GPIO_V2 structs/constants/ioctl-numbers are authored in-tree
// (below) — they are NOT in nix or libc; nix contributes only the
// `ioctl_readwrite!` macro (the `gpio_get_line` wrapper) and `ppoll`.

/// In-tree `GPIO_V2` compat shim, mirrored from `gpiod-core-0.3.0/src/raw/v2.rs`
/// (itself a mirror of the kernel `uapi/linux/gpio.h` ABI). Sized for aarch64
/// Linux (the only target the `hw` feature is built for); the size-assertion
/// test guards the ioctl payload layout.
#[allow(dead_code)] // FFI ABI definitions; not every field is read by us.
pub mod gpio_v2 {
    pub const GPIO_MAGIC: u8 = 0xB4;
    pub const GPIO_MAX_NAME_SIZE: usize = 32;
    pub const GPIO_LINES_MAX: usize = 64;
    pub const GPIO_LINE_NUM_ATTRS_MAX: usize = 10;

    pub const GPIO_LINE_FLAG_INPUT: u64 = 1 << 2;
    pub const GPIO_LINE_FLAG_EDGE_RISING: u64 = 1 << 4;
    pub const GPIO_LINE_FLAG_EDGE_FALLING: u64 = 1 << 5;
    pub const GPIO_LINE_FLAG_EDGE_BOTH: u64 =
        GPIO_LINE_FLAG_EDGE_RISING | GPIO_LINE_FLAG_EDGE_FALLING;

    /// Kernel `gpio_v2_line_event.id` values.
    pub const GPIO_LINE_EVENT_RISING_EDGE: u32 = 1;
    pub const GPIO_LINE_EVENT_FALLING_EDGE: u32 = 2;

    #[derive(Clone, Copy)]
    #[repr(C)]
    pub union GpioLineAttrVal {
        pub flags: u64,
        pub values: u64,
        pub debounce_period_us: u32,
    }

    impl Default for GpioLineAttrVal {
        fn default() -> Self {
            Self { values: 0 }
        }
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct GpioLineAttr {
        pub id: u32,
        padding: u32,
        pub val: GpioLineAttrVal,
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct GpioLineConfigAttr {
        pub attr: GpioLineAttr,
        pub mask: u64,
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct GpioLineConfig {
        pub flags: u64,
        pub num_attrs: u32,
        padding: [u32; 5],
        pub attrs: [GpioLineConfigAttr; GPIO_LINE_NUM_ATTRS_MAX],
    }

    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct GpioLineRequest {
        pub offsets: [u32; GPIO_LINES_MAX],
        pub consumer: [u8; GPIO_MAX_NAME_SIZE],
        pub config: GpioLineConfig,
        pub num_lines: u32,
        pub event_buffer_size: u32,
        padding: [u32; 5],
        pub fd: i32,
    }

    impl Default for GpioLineRequest {
        fn default() -> Self {
            Self {
                offsets: [0; GPIO_LINES_MAX],
                consumer: [0; GPIO_MAX_NAME_SIZE],
                config: Default::default(),
                num_lines: 0,
                event_buffer_size: 0,
                padding: [0; 5],
                fd: 0,
            }
        }
    }

    /// Kernel `gpio_v2_line_event` — one is read from the line fd per edge.
    /// `timestamp_ns` is CLOCK_MONOTONIC; `id` is the edge kind.
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct GpioLineEvent {
        pub timestamp_ns: u64,
        pub id: u32,
        pub offset: u32,
        pub seqno: u32,
        pub line_seqno: u32,
        padding: [u32; 6],
    }

    /// GPIO_V2_GET_LINE_IOCTL: request a line and get back an fd for edge events.
    /// Mirrors gpiod-core's `nix::ioctl_readwrite!(gpio_get_line, GPIO_MAGIC, 0x07, GpioLineRequest)`.
    nix::ioctl_readwrite!(gpio_get_line, GPIO_MAGIC, 0x07, GpioLineRequest);

    #[cfg(test)]
    mod test {
        use super::*;
        use std::mem::size_of;
        // ABI size assertions — catch a struct-layout regression that would
        // corrupt the ioctl payload. Values mirror the kernel/gpiod-core sizes.
        #[test]
        fn sizes() {
            assert_eq!(size_of::<GpioLineAttr>(), 16);
            assert_eq!(size_of::<GpioLineConfigAttr>(), 24);
            assert_eq!(size_of::<GpioLineConfig>(), 272);
            assert_eq!(size_of::<GpioLineRequest>(), 592);
            assert_eq!(size_of::<GpioLineEvent>(), 48);
        }
    }
}

pub struct ScopeTool {
    chip: String,
}
impl ScopeTool {
    pub fn new(chip: impl Into<String>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { chip: chip.into() })
    }
}

#[async_trait]
impl Tool for ScopeTool {
    fn name(&self) -> &str {
        "scope"
    }
    fn description(&self) -> &str {
        "Capture GPIO edge events over a window (a logic-scope time series). Class R."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"pin":{"type":"integer"},"duration":{"type":"number","description":"capture window in milliseconds (max 5000)"}},"required":["pin","duration"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let dur_ms = args.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if dur_ms <= 0.0 || dur_ms > 5000.0 {
            return ToolResult::err(
                "scope",
                format!("duration must be 1..5000 ms, got {dur_ms}"),
            );
        }
        // Open the character device directly (raw-fd path; gpiod bypassed here).
        let chip_path = format!("/dev/{}", self.chip);
        let chip_fd = match nix::fcntl::open(
            chip_path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                return ToolResult::err(
                    "scope",
                    format!("open {chip_path}: {e} (is user in 'gpio' group?)"),
                )
            }
        };
        // Request the line for input + both-edge detection. The line offset IS
        // the agent's `pin`, passed straight through (verified against gpiod:
        // `Options::input([pin])` → `request.offsets`, so the same pin hits the
        // same line — no lookup table needed).
        let mut req = gpio_v2::GpioLineRequest::default();
        req.num_lines = 1;
        req.offsets[0] = pin;
        req.config.flags = gpio_v2::GPIO_LINE_FLAG_INPUT | gpio_v2::GPIO_LINE_FLAG_EDGE_BOTH;
        let consumer = b"piforge-scope\0";
        req.consumer[..consumer.len()].copy_from_slice(consumer);
        let line_fd_raw = match unsafe { gpio_v2::gpio_get_line(chip_fd, &mut req) } {
            Ok(_) => req.fd,
            Err(e) => {
                // No line fd was created — close the chip fd via a transient File.
                drop(unsafe { std::fs::File::from_raw_fd(chip_fd) });
                return ToolResult::err("scope", format!("request pin {pin} for edges: {e}"));
            }
        };
        // Hand both fds to std::fs::File so they close on drop (incl. across the
        // spawn_blocking task). File also gives AsFd for ppoll + Read for events.
        // `_chip_file` is held alive in this scope for the capture, then drops.
        let _chip_file = unsafe { std::fs::File::from_raw_fd(chip_fd) };
        let mut line_file = unsafe { std::fs::File::from_raw_fd(line_fd_raw) };
        let dur = Duration::from_millis(dur_ms as u64);
        // Bounded capture in a blocking task. ppoll bounds it at the syscall
        // level; the task completes on timeout, so no blocking-thread leak.
        let captured =
            tokio::task::spawn_blocking(move || capture_edges(&mut line_file, dur)).await;
        let (events, notice) = match captured {
            Ok(x) => x,
            Err(e) => return ToolResult::err("scope", format!("capture task: {e}")),
        };
        let count = events.len();
        let rate = count as f64 / (dur_ms / 1000.0);
        let ev_json: Vec<Value> = events
            .iter()
            .map(|(t, e)| json!({"t_us":t,"edge":e}))
            .collect();
        let mut out = ToolResult::ok(
            "scope",
            json!({"pin":pin,"duration_ms":dur_ms,"edges":count,"rate_hz":rate,"events":ev_json}),
        );
        if let Some(n) = notice {
            out.notice = Some(n);
        }
        out
    }
}

/// Decode the kernel event's edge id → the eval-stable string (matches sim:
/// `sim.rs` emits "rising"/"falling").
pub fn edge_label(ev: &gpio_v2::GpioLineEvent) -> &'static str {
    match ev.id {
        gpio_v2::GPIO_LINE_EVENT_RISING_EDGE => "rising",
        gpio_v2::GPIO_LINE_EVENT_FALLING_EDGE => "falling",
        _ => "edge",
    }
}

/// Microsecond offset of this event from the first event in the capture. The
/// kernel/gpiod timestamp epoch is arbitrary (CLOCK_MONOTONIC, crate-unspecified
/// base), so a within-capture offset is the monotonic, portable quantity. Uses
/// saturating_sub for robustness.
pub fn since_us(ev: &gpio_v2::GpioLineEvent, first_ns: u64) -> u64 {
    ev.timestamp_ns.saturating_sub(first_ns) / 1000
}

#[derive(Debug)]
enum PollOutcome {
    Readable,
    Timeout,
    /// Stray signal (e.g. SIGCHLD) interrupted ppoll; retry against the deadline.
    Eintr,
    /// Line fd reported POLLERR/POLLHUP (chip gone / line released).
    Hup,
    Err(nix::errno::Errno),
}

/// Bounded edge capture: ppoll the line fd for `dur`, reading one
/// `gpio_v2_line_event` per readiness. Returns `(events, notice)` where events
/// are `(since_us, edge_label)` pairs and notice is set iff the capture ended
/// early. Always returns within ~`dur` wall-clock; EINTR is retried, not fatal.
fn capture_edges(
    line: &mut std::fs::File,
    dur: Duration,
) -> (Vec<(u64, &'static str)>, Option<String>) {
    use std::io::Read;
    let start = std::time::Instant::now();
    let mut first_ns: Option<u64> = None;
    let mut events: Vec<(u64, &'static str)> = Vec::new();
    let mut notice: Option<String> = None;
    let ev_size = std::mem::size_of::<gpio_v2::GpioLineEvent>();
    loop {
        // Read the clock once and subtract saturatingly: `Duration - Duration`
        // panics on underflow (std `Sub` uses `expect`), and under `panic =
        // "abort"` that would abort the binary mid-capture. A re-entrant
        // edge/EINTR return could cross the `dur` boundary in the ns gap between
        // two `elapsed()` reads; a single read + saturating_sub closes it.
        let elapsed = start.elapsed();
        if elapsed >= dur {
            break;
        }
        let remaining = dur.saturating_sub(elapsed);
        // ppoll bounds the wait. The immutable borrow of `line` (via the PollFd)
        // ends with this block, freeing `line` for the mutable read below.
        let outcome = {
            let mut fds = [nix::poll::PollFd::new(&*line, nix::poll::PollFlags::POLLIN)];
            let timeout = nix::sys::time::TimeSpec::from(remaining);
            match nix::poll::ppoll(&mut fds, Some(timeout), None) {
                Ok(0) => PollOutcome::Timeout,
                Ok(_) => {
                    let rv = fds[0].revents().unwrap_or(nix::poll::PollFlags::empty());
                    if rv.contains(nix::poll::PollFlags::POLLIN) {
                        PollOutcome::Readable
                    } else if rv
                        .intersects(nix::poll::PollFlags::POLLERR | nix::poll::PollFlags::POLLHUP)
                    {
                        PollOutcome::Hup
                    } else {
                        PollOutcome::Timeout
                    }
                }
                Err(nix::errno::Errno::EINTR) => PollOutcome::Eintr,
                Err(e) => PollOutcome::Err(e),
            }
        };
        match outcome {
            PollOutcome::Timeout => break,
            PollOutcome::Eintr => continue, // retry against the remaining deadline
            PollOutcome::Hup => {
                notice = Some("capture ended early: line fd reported POLLERR/POLLHUP".into());
                break;
            }
            PollOutcome::Err(e) => {
                notice = Some(format!("capture ended early: ppoll: {e}"));
                break;
            }
            PollOutcome::Readable => {
                let mut ev = gpio_v2::GpioLineEvent::default();
                // SAFETY: GpioLineEvent is #[repr(C)], 48 bytes, no implicit
                // padding (size-asserted); reading its raw bytes from the kernel
                // line fd is the documented GPIO_V2 event-read path. `ev` is
                // properly aligned.
                let buf: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(&mut ev as *mut _ as *mut u8, ev_size)
                };
                match line.read(buf) {
                    Ok(n) if n == ev_size => {
                        let first = *first_ns.get_or_insert(ev.timestamp_ns);
                        events.push((since_us(&ev, first), edge_label(&ev)));
                    }
                    Ok(_) => continue, // partial/zero read (unexpected for gpio cdev); skip
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        notice = Some(format!("capture ended early: read: {e}"));
                        break;
                    }
                }
            }
        }
    }
    (events, notice)
}

// ---------------- I2C ----------------

pub struct I2cTool {
    /// Parsed bus index (e.g. "1") cached from the configured `/dev/i2c-N`
    /// path at construction so the config knob is honored in every
    /// `i2cdetect`/`i2cget` shell-out, not hardcoded.
    bus_num: String,
    gate: Option<std::sync::Arc<dyn Gate>>,
}
impl I2cTool {
    pub fn new(
        bus_path: impl Into<String>,
        gate: Option<std::sync::Arc<dyn Gate>>,
    ) -> std::sync::Arc<Self> {
        let bus_path = bus_path.into();
        std::sync::Arc::new(Self {
            bus_num: bus_num_from_path(&bus_path),
            gate,
        })
    }
}

/// Extract the bus index from a `/dev/i2c-N` path so the config knob is
/// honored end-to-end, not cosmetically. `"/dev/i2c-1"` → `"1"`,
/// `"/dev/i2c-10"` → `"10"`, `"1"` → `"1"`. Falls back to `"1"` (the Pi default)
/// when the path has no trailing digits, so a malformed config degrades to the
/// historical behavior rather than panicking.
pub fn bus_num_from_path(path: &str) -> String {
    let digits: String = path
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        "1".to_string()
    } else {
        digits
    }
}

#[async_trait]
impl Tool for I2cTool {
    fn name(&self) -> &str {
        "i2c"
    }
    fn description(&self) -> &str {
        "I2C scan/detect/read via /dev/i2c-N. Returns structured values with units."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["scan","read","detect"]},"address":{"type":"integer"},"register":{"type":"integer"},"length":{"type":"integer"}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("scan");
        let addr = args.get("address").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
        // Shell out to i2c-tools (i2cdetect/i2cget) — they're present on Pi OS and
        // handle the SMBus quick-read + register-read semantics robustly. A pure
        // nix-ioctl path is possible but reinvents i2c-tools; defer.
        match action {
            "scan" => {
                let out = tokio::process::Command::new("i2cdetect")
                    .args(["-y", self.bus_num.as_str()])
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => {
                        let txt = String::from_utf8_lossy(&o.stdout);
                        let found = parse_i2cdetect(&txt);
                        let notice = if found.len() > 40 {
                            Some(
                                "many addresses responded — likely SDA/SCL shorted to power; STOP"
                                    .into(),
                            )
                        } else if found.is_empty() {
                            Some("no devices — check dtparam=i2c_arm=on, wiring, pull-ups".into())
                        } else {
                            None
                        };
                        let hex: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                        ToolResult {
                            tool: "i2c".into(),
                            ok: true,
                            value: Some(json!({"devices":hex,"count":found.len()})),
                            unit: Some("7-bit addr".into()),
                            error: None,
                            notice,
                        }
                    }
                    Ok(o) => ToolResult::err(
                        "i2c",
                        format!(
                            "i2cdetect exited {}: {}",
                            o.status,
                            String::from_utf8_lossy(&o.stderr)
                        ),
                    ),
                    Err(e) => ToolResult::err(
                        "i2c",
                        format!("run i2cdetect: {e} (is i2c-tools installed?)"),
                    ),
                }
            }
            "detect" => {
                let out = tokio::process::Command::new("i2cdetect")
                    .args(["-y", self.bus_num.as_str()])
                    .output()
                    .await;
                let Ok(o) = out else {
                    return ToolResult::err("i2c", "i2cdetect failed");
                };
                let txt = String::from_utf8_lossy(&o.stdout);
                let found = parse_i2cdetect(&txt);
                if found.contains(&(addr as i32)) {
                    ToolResult::ok(
                        "i2c",
                        json!({"address":format!("0x{addr:02x}"),"present":true}),
                    )
                } else {
                    ToolResult::err("i2c", format!("no device at 0x{addr:02x}"))
                }
            }
            "read" => {
                let reg = args.get("register").and_then(|v| v.as_i64()).unwrap_or(0) as u8;
                let n = args
                    .get("length")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1)
                    .max(1)
                    .min(32) as usize;
                // i2cdump -y 1 <addr> b <reg> reads one byte per call; for N bytes we loop.
                // For MVP simplicity read N bytes starting at reg via repeated i2cget.
                let mut bytes = Vec::with_capacity(n);
                let addr_hex = format!("0x{addr:02x}");
                for i in 0..n {
                    let r = reg.wrapping_add(i as u8);
                    // i2cget expects the address in hex (e.g. 0x76), not decimal.
                    let out = tokio::process::Command::new("i2cget")
                        .args([
                            "-y",
                            self.bus_num.as_str(),
                            &addr_hex,
                            &format!("0x{r:02x}"),
                        ])
                        .output()
                        .await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            let v = u8::from_str_radix(t.trim_start_matches("0x"), 16).unwrap_or(0);
                            bytes.push(v);
                        }
                        _ => bytes.push(0),
                    }
                }
                let raw_hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let mut out = json!({
                    "address":format!("0x{addr:02x}"),
                    "register":format!("0x{reg:02x}"),
                    "raw_hex":raw_hex,
                    "raw_dec":bytes,
                });
                // 2-byte reads: surface both endian interpretations.
                if bytes.len() == 2 {
                    let be = u16::from_be_bytes([bytes[0], bytes[1]]);
                    let le = u16::from_le_bytes([bytes[0], bytes[1]]);
                    out["be_uint16"] = json!(be);
                    out["le_uint16"] = json!(le);
                }
                ToolResult::ok_unit("i2c", out, "bytes (see datasheet for scaling)")
            }
            other => ToolResult::err("i2c", format!("unknown action {other}")),
        }
    }
}

/// Parse `i2cdetect -y 1` output into present 7-bit addresses.
fn parse_i2cdetect(text: &str) -> Vec<i32> {
    let mut found = vec![];
    for line in text.lines() {
        // Lines look like:  "40: 40 41 42 43 44 45 46 47 ..."
        if let Some((row_hex, _)) = line.split_once(':') {
            if let Ok(row) = u8::from_str_radix(row_hex.trim(), 16) {
                for tok in line.split(':').nth(1).unwrap_or("").split_whitespace() {
                    if tok == "UU" {
                        continue;
                    }
                    if let Ok(col) = u8::from_str_radix(tok, 16) {
                        found.push((row + col) as i32);
                    }
                }
            }
        }
    }
    found
}

// ---------------- Telemetry (vcgencmd) ----------------

pub struct TelemetryTool {
    last: Mutex<ThrottledBits>,
}
#[derive(Default, Clone, Copy)]
struct ThrottledBits {
    now: bool,
    since: bool,
}

impl TelemetryTool {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            last: Mutex::new(ThrottledBits::default()),
        })
    }

    fn read_throttled() -> Option<u64> {
        let o = std::process::Command::new("vcgencmd")
            .arg("get_throttled")
            .output()
            .ok()?;
        if !o.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&o.stdout);
        let s = s.trim();
        let s = s.split('=').nth(1).unwrap_or(s).trim();
        let s = s.trim_start_matches("0x").trim_start_matches("0X");
        u64::from_str_radix(s, 16).ok()
    }
}

#[async_trait]
impl Tool for TelemetryTool {
    fn name(&self) -> &str {
        "telemetry"
    }
    fn description(&self) -> &str {
        "Read Pi health: temp, volts, decoded get_throttled bitmask, dmesg tail."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["snapshot","throttled","temp"]}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("snapshot");
        match action {
            "temp" => {
                let o = std::process::Command::new("vcgencmd")
                    .arg("measure_temp")
                    .output();
                let t = match o {
                    Ok(x) => String::from_utf8_lossy(&x.stdout).trim().to_string(),
                    Err(_) => "temp=N/A".into(),
                };
                ToolResult::ok_unit("telemetry", json!({"cpu_temp":t}), "degC")
            }
            "throttled" => {
                let val = Self::read_throttled().unwrap_or(0);
                let bits = ThrottledBits {
                    now: val & (1 << 0) != 0,
                    since: val & (1 << 16) != 0,
                };
                *self.last.lock() = bits;
                let notice = if bits.now || bits.since {
                    Some(
                        "UNDERVOLTAGE detected — STOP before adding load; use a 5V/3A+ PSU.".into(),
                    )
                } else {
                    None
                };
                ToolResult {
                    tool: "telemetry".into(),
                    ok: true,
                    value: Some(
                        json!({"raw":format!("0x{val:x}"),"decoded":{"under_voltage_now":bits.now,"currently_throttled_now":val&(1<<2)!=0,"under_voltage_since_boot":bits.since,"throttled_since_boot":val&(1<<18)!=0}}),
                    ),
                    unit: None,
                    error: None,
                    notice,
                }
            }
            _ => {
                let val = Self::read_throttled().unwrap_or(0);
                let uv = val & (1 << 0) != 0 || val & (1 << 16) != 0;
                let mut out = json!({"throttled_raw":format!("0x{val:x}"),"under_voltage":uv});
                if uv {
                    out["notice"] = json!("UNDERVOLTAGE detected — STOP before adding load.");
                }
                ToolResult::ok("telemetry", out)
            }
        }
    }
}

impl ThrottledReader for TelemetryTool {
    fn under_voltage_active(&self) -> (bool, bool) {
        let val = Self::read_throttled().unwrap_or(0);
        (val & (1 << 0) != 0, val & (1 << 16) != 0)
    }
}

// ---------------- Inventory ----------------

pub struct InventoryTool {
    /// I2C bus index (e.g. "1") for the inventory `i2cdetect` call.
    bus_num: String,
}
impl InventoryTool {
    pub fn new(bus_num: impl Into<String>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            bus_num: bus_num.into(),
        })
    }
}

#[async_trait]
impl Tool for InventoryTool {
    fn name(&self) -> &str {
        "hardware_inventory"
    }
    fn description(&self) -> &str {
        "List detected hardware: gpiochips, I2C devices, board model. Call first."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _args: &Value) -> ToolResult {
        let mut inv = json!({});
        if let Ok(model) = std::fs::read_to_string("/proc/device-tree/model") {
            inv["board"] = json!(model.trim_end_matches('\0'));
        }
        if let Ok(o) = std::process::Command::new("sh")
            .arg("-c")
            .arg("ls /dev/gpiochip* 2>/dev/null")
            .output()
        {
            inv["gpiochips"] = json!(String::from_utf8_lossy(&o.stdout).trim());
        }
        if let Ok(o) = std::process::Command::new("i2cdetect")
            .args(["-y", self.bus_num.as_str()])
            .output()
        {
            if o.status.success() {
                let found = parse_i2cdetect(&String::from_utf8_lossy(&o.stdout));
                let hex: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                inv["i2c_devices"] = json!(hex);
            }
        }
        ToolResult::ok("hardware_inventory", inv)
    }
}

/// Build the full real-hardware tool-box for the interactive agent. The shared
/// `telemetry` instance is pushed directly instead of constructing a second one,
/// so the Broker's under-voltage STOP and the agent-facing tool read the same
/// vcgencmd state. `i2c_bus` is threaded into I2cTool + InventoryTool — the
/// config knob is honored end-to-end, not hardcoded.
pub fn build_tools(
    chip: &str,
    i2c_bus: &str,
    gate: std::sync::Arc<dyn Gate>,
    telemetry: std::sync::Arc<TelemetryTool>,
) -> crate::hil::ToolVec {
    vec![
        InventoryTool::new(bus_num_from_path(i2c_bus)),
        telemetry,
        I2cTool::new(i2c_bus, Some(gate.clone())),
        GpioTool::new(chip, Some(gate.clone())),
        ScopeTool::new(chip),
        crate::sim::CodeEditTool::new("."),
    ]
}

// keep HashMap import used (for future per-pin profile expansion)
#[allow(dead_code)]
fn _hm() -> HashMap<String, String> {
    HashMap::new()
}
