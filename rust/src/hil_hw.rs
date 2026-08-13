//! Real hardware tools, Linux-only, behind the `hw` feature.
//! - GPIO get/set + scope (edge events): the `gpiod` crate (pure Rust, no
//!   libgpiod C dep; talks to /dev/gpiochipN directly).
//! - I2C scan/detect/read: in-process `nix` ioctl on `/dev/i2c-N` (SMBus QUICK
//!   scan + one `I2C_RDWR` write-reg+read; no `i2c-tools` PATH dependency).
//! - Telemetry (vcgencmd get_throttled decode): shell out (no Rust lib; low freq).
//!
//! Build with `--features hw` on Linux (the Pi). Off-Linux or without the
//! feature, the sim tools stand in (dev/eval loop).
#![cfg(feature = "hw")]

use parking_lot::Mutex;
use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::broker::{ThrottledReader, UvReading};
use crate::hil::{self, Gate, Tool, ToolResult};

/// Resolve the gpiochip name for the Pi 5 40-pin header.
///
/// Current Pi OS names the RP1 40-pin chip `gpiochip0` and keeps `gpiochip4` as
/// a compat symlink. `gpiod::Chip::new` rejects the symlink ("not a character
/// device"), so we canonicalize `/dev/<name>` when it exists. Configured names
/// that do not exist on this host (macOS / CI) are returned verbatim.
pub fn resolve_chip(configured: &str) -> Result<String, String> {
    if !configured.is_empty() {
        return Ok(canonical_chip_name(configured).unwrap_or_else(|_| configured.into()));
    }
    // Prefer the historic Pi 5 name; follow symlink / fall back to gpiochip0.
    for name in &["gpiochip4", "gpiochip0"] {
        let candidate = canonical_chip_name(name).unwrap_or_else(|_| (*name).into());
        if gpiod::Chip::new(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err("no gpiochip found (set hardware.gpiochip in config)".into())
}

fn canonical_chip_name(name: &str) -> Result<String, String> {
    let path = PathBuf::from("/dev").join(name.trim_start_matches("/dev/"));
    let canon = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
    canon
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("gpiochip path {} has no file name", canon.display()))
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
// stray SIGCHLD from `vcgencmd`/`dmesg` (or tokio child reaping) surfaces as
// EINTR and is retried against the remaining deadline rather than truncating a
// capture on a *toggling* pin. The GPIO get/set
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
        "Capture GPIO edge events over a window (Class R)."
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

// ---------------- I2C (in-process ioctl on /dev/i2c-N) ----------------
//
// Scan uses SMBus QUICK (i2cdetect -y). I2C_TIMEOUT=10 (100 ms/transfer, Linux
// 10 ms units) plus a 2 s Instant check between probes inside spawn_blocking.
// The async side joins that task (never tokio::time::timeout, which would
// abandon the JoinHandle while ioctl still holds the adapter). Read is one
// I2C_RDWR write-reg + read-N. A shared Mutex serializes I2cTool + InventoryTool.

mod i2c_ioctl {
    pub const I2C_M_RD: u16 = 0x0001;
    pub const I2C_SMBUS_WRITE: u8 = 0;
    pub const I2C_SMBUS_QUICK: i32 = 0;

    #[repr(C)]
    pub struct I2cSmbusIoctlData {
        pub read_write: u8,
        pub command: u8,
        pub size: i32,
        pub data: *mut u8,
    }

    #[repr(C)]
    pub struct I2cMsg {
        pub addr: u16,
        pub flags: u16,
        pub len: u16,
        pub buf: *mut u8,
    }

    #[repr(C)]
    pub struct I2cRdwrIoctlData {
        pub msgs: *mut I2cMsg,
        pub nmsgs: u32,
    }

    nix::ioctl_write_int_bad!(i2c_timeout, 0x0702);
    nix::ioctl_write_int_bad!(i2c_slave, 0x0703);
    nix::ioctl_readwrite_bad!(i2c_smbus, 0x0720, I2cSmbusIoctlData);
    nix::ioctl_readwrite_bad!(i2c_rdwr, 0x0707, I2cRdwrIoctlData);
}

/// Shared in-process helper for `/dev/i2c-N`. Construction does not open the
/// node (`schema()` / tests use a dummy path).
pub struct I2cAdapter {
    path: PathBuf,
    lock: Mutex<()>,
}

impl I2cAdapter {
    pub fn new(path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            path: path.into(),
            lock: Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn scan(self: &Arc<Self>) -> Result<(Vec<u8>, Option<String>), String> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _g = this.lock.lock();
            scan_bus_blocking(&this.path)
        })
        .await
        .map_err(|e| format!("i2c scan task: {e}"))?
    }

    async fn detect(self: &Arc<Self>, addr: u16) -> Result<bool, String> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _g = this.lock.lock();
            detect_blocking(&this.path, addr)
        })
        .await
        .map_err(|e| format!("i2c detect task: {e}"))?
    }

    async fn read(self: &Arc<Self>, addr: u16, reg: u8, n: usize) -> Result<Vec<u8>, String> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _g = this.lock.lock();
            read_blocking(&this.path, addr, reg, n)
        })
        .await
        .map_err(|e| format!("i2c read task: {e}"))?
    }
}

fn trailing_digits(path: &str) -> String {
    path.chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Extract the bus index from a `/dev/i2c-N` path. Empty trailing digits is
/// `Err`, not a silent fallback to `"1"`.
pub fn bus_num_from_path(path: &str) -> Result<String, String> {
    let digits = trailing_digits(path);
    if digits.is_empty() {
        Err(format!(
            "I2C path {path:?} has no trailing decimal bus index"
        ))
    } else {
        Ok(digits)
    }
}

fn list_i2c_dev_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev") {
        for e in rd.flatten() {
            let s = e.file_name().to_string_lossy().into_owned();
            if s.starts_with("i2c-") {
                names.push(s);
            }
        }
    }
    names.sort();
    names
}

/// Fail-closed bus resolve: trailing digits + `Path::exists`. Missing node
/// lists `/dev/i2c-*`. Call from hw `build_tools` before constructing tools.
/// Existence is not checked in `config.validate()` (macOS has no node).
pub fn resolve_i2c_bus(configured: &str) -> Result<PathBuf, String> {
    let available = || {
        let names = list_i2c_dev_names();
        if names.is_empty() {
            "(none)".into()
        } else {
            names.join(", ")
        }
    };
    if bus_num_from_path(configured).is_err() {
        return Err(format!(
            "I2C bus {configured:?} has no trailing decimal index; available: {}",
            available()
        ));
    }
    let path = PathBuf::from(configured);
    if !path.exists() {
        return Err(format!(
            "I2C bus {configured} not found; available: {}",
            available()
        ));
    }
    Ok(path)
}

fn open_i2c(path: &Path) -> Result<std::fs::File, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    unsafe {
        i2c_ioctl::i2c_timeout(file.as_raw_fd(), 10).map_err(|e| format!("I2C_TIMEOUT: {e}"))?;
    }
    Ok(file)
}

fn smbus_quick(file: &std::fs::File, addr: u16) -> bool {
    let fd = file.as_raw_fd();
    if unsafe { i2c_ioctl::i2c_slave(fd, addr as _) }.is_err() {
        return false;
    }
    let mut data = i2c_ioctl::I2cSmbusIoctlData {
        read_write: i2c_ioctl::I2C_SMBUS_WRITE,
        command: 0,
        size: i2c_ioctl::I2C_SMBUS_QUICK,
        data: std::ptr::null_mut(),
    };
    unsafe { i2c_ioctl::i2c_smbus(fd, &mut data) }.is_ok()
}

fn scan_bus_blocking(path: &Path) -> Result<(Vec<u8>, Option<String>), String> {
    let file = open_i2c(path)?;
    let start = Instant::now();
    let mut found = Vec::new();
    let mut timed_out = false;
    for addr in 0x08u8..=0x77 {
        if smbus_quick(&file, addr as u16) {
            found.push(addr);
        }
        if start.elapsed() >= Duration::from_secs(2) {
            timed_out = true;
            break;
        }
    }
    let notice = if timed_out {
        Some("scan timed out".into())
    } else if found.len() > 40 {
        Some("many addresses responded — likely SDA/SCL shorted to power; STOP".into())
    } else if found.is_empty() {
        Some("no devices — check dtparam=i2c_arm=on, wiring, pull-ups".into())
    } else {
        None
    };
    Ok((found, notice))
}

fn detect_blocking(path: &Path, addr: u16) -> Result<bool, String> {
    let file = open_i2c(path)?;
    Ok(smbus_quick(&file, addr))
}

fn read_blocking(path: &Path, addr: u16, reg: u8, n: usize) -> Result<Vec<u8>, String> {
    let file = open_i2c(path)?;
    let mut reg_buf = [reg];
    let mut data = vec![0u8; n];
    let mut msgs = [
        i2c_ioctl::I2cMsg {
            addr,
            flags: 0,
            len: 1,
            buf: reg_buf.as_mut_ptr(),
        },
        i2c_ioctl::I2cMsg {
            addr,
            flags: i2c_ioctl::I2C_M_RD,
            len: n as u16,
            buf: data.as_mut_ptr(),
        },
    ];
    let mut ioctl_data = i2c_ioctl::I2cRdwrIoctlData {
        msgs: msgs.as_mut_ptr(),
        nmsgs: 2,
    };
    unsafe { i2c_ioctl::i2c_rdwr(file.as_raw_fd(), &mut ioctl_data) }
        .map_err(|e| format!("read 0x{addr:02x} reg 0x{reg:02x}: {e}"))?;
    Ok(data)
}

pub struct I2cTool {
    bus: Arc<I2cAdapter>,
}
impl I2cTool {
    pub fn new(bus: Arc<I2cAdapter>) -> Arc<Self> {
        Arc::new(Self { bus })
    }
}

#[async_trait]
impl Tool for I2cTool {
    fn name(&self) -> &str {
        "i2c"
    }
    fn description(&self) -> &str {
        "I2C scan/detect/read. Returns structured values with units."
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
        match action {
            "scan" => match self.bus.scan().await {
                Ok((found, notice)) => {
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
                Err(e) => ToolResult::err("i2c", e),
            },
            "detect" => match self.bus.detect(addr).await {
                Ok(true) => ToolResult::ok(
                    "i2c",
                    json!({"address":format!("0x{addr:02x}"),"present":true}),
                ),
                Ok(false) => ToolResult::err("i2c", format!("no device at 0x{addr:02x}")),
                Err(e) => ToolResult::err("i2c", e),
            },
            "read" => {
                let reg = args.get("register").and_then(|v| v.as_i64()).unwrap_or(0) as u8;
                let n = args
                    .get("length")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1)
                    .clamp(1, 32) as usize;
                match self.bus.read(addr, reg, n).await {
                    Ok(bytes) => {
                        let raw_hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                        let mut out = json!({
                            "address":format!("0x{addr:02x}"),
                            "register":format!("0x{reg:02x}"),
                            "raw_hex":raw_hex,
                            "raw_dec":bytes,
                        });
                        if bytes.len() == 2 {
                            let be = u16::from_be_bytes([bytes[0], bytes[1]]);
                            let le = u16::from_le_bytes([bytes[0], bytes[1]]);
                            out["be_uint16"] = json!(be);
                            out["le_uint16"] = json!(le);
                        }
                        ToolResult::ok_unit("i2c", out, "bytes (see datasheet for scaling)")
                    }
                    Err(e) => ToolResult::err("i2c", e),
                }
            }
            other => ToolResult::err("i2c", format!("unknown action {other}")),
        }
    }
}

// ---------------- Telemetry (vcgencmd) ----------------

pub struct TelemetryTool;

impl TelemetryTool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
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

    fn read_temp() -> String {
        let o = std::process::Command::new("vcgencmd")
            .arg("measure_temp")
            .output();
        match o {
            Ok(x) if x.status.success() => {
                let t = String::from_utf8_lossy(&x.stdout).trim().to_string();
                if t.is_empty() {
                    "N/A".into()
                } else {
                    t
                }
            }
            _ => "N/A".into(),
        }
    }

    fn read_dmesg_tail() -> Option<String> {
        let o = std::process::Command::new("dmesg").output().ok()?;
        if !o.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&o.stdout);
        let mut lines: Vec<&str> = s.lines().rev().take(20).collect();
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        let joined = lines.join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

#[async_trait]
impl Tool for TelemetryTool {
    fn name(&self) -> &str {
        "telemetry"
    }
    fn description(&self) -> &str {
        "Read Pi health telemetry: temp, volts, throttled bitmask, dmesg tail."
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
                let t = Self::read_temp();
                ToolResult::ok_unit("telemetry", json!({"cpu_temp":t}), "degC")
            }
            "throttled" => match Self::read_throttled() {
                Some(val) => {
                    let now = val & (1 << 0) != 0;
                    let since = val & (1 << 16) != 0;
                    let notice = if now || since {
                        Some(
                            "UNDERVOLTAGE detected — STOP before adding load; use a 5V/3A+ PSU."
                                .into(),
                        )
                    } else {
                        None
                    };
                    ToolResult {
                        tool: "telemetry".into(),
                        ok: true,
                        value: Some(
                            json!({"raw":format!("0x{val:x}"),"decoded":{"under_voltage_now":now,"currently_throttled_now":val&(1<<2)!=0,"under_voltage_since_boot":since,"throttled_since_boot":val&(1<<18)!=0}}),
                        ),
                        unit: None,
                        error: None,
                        notice,
                    }
                }
                None => ToolResult {
                    tool: "telemetry".into(),
                    ok: true,
                    value: Some(json!({"raw": Value::Null, "decoded": Value::Null})),
                    unit: None,
                    error: None,
                    notice: None,
                },
            },
            _ => {
                let throttled = Self::read_throttled();
                let cpu_temp = Self::read_temp();
                let dmesg = Self::read_dmesg_tail();
                let uv = matches!(
                    throttled,
                    Some(val) if val & (1 << 0) != 0 || val & (1 << 16) != 0
                );
                let out = hil::telemetry_snapshot(throttled, &cpu_temp, dmesg.as_deref());
                let notice = if uv {
                    Some("UNDERVOLTAGE detected — STOP before adding load.".into())
                } else {
                    None
                };
                ToolResult {
                    tool: "telemetry".into(),
                    ok: true,
                    value: Some(out),
                    unit: None,
                    error: None,
                    notice,
                }
            }
        }
    }
}

impl ThrottledReader for TelemetryTool {
    fn under_voltage_active(&self) -> UvReading {
        match Self::read_throttled() {
            Some(val) => UvReading {
                known: true,
                now: val & (1 << 0) != 0,
                since: val & (1 << 16) != 0,
            },
            None => UvReading {
                known: false,
                now: false,
                since: false,
            },
        }
    }
}

// ---------------- Inventory ----------------

pub struct InventoryTool {
    bus: Arc<I2cAdapter>,
}
impl InventoryTool {
    pub fn new(bus: Arc<I2cAdapter>) -> Arc<Self> {
        Arc::new(Self { bus })
    }
}

fn gpiochip_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev") {
        for e in rd.flatten() {
            let s = e.file_name().to_string_lossy().into_owned();
            if s.starts_with("gpiochip") {
                names.push(s);
            }
        }
    }
    names.sort();
    names
}

fn one_wire_ids() -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/w1/devices") else {
        return ids;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("w1_bus_master") {
            continue;
        }
        ids.push(n);
    }
    ids.sort();
    ids
}

fn iio_devices() -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/iio/devices") else {
        return out;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let name = std::fs::read_to_string(path.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| e.file_name().to_string_lossy().into_owned());
        let raw = std::fs::read_to_string(path.join("in_voltage0_raw"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let scale = std::fs::read_to_string(path.join("in_voltage_scale"))
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        out.push(json!({"name": name, "raw": raw, "scale": scale}));
    }
    out
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
        let board = std::fs::read_to_string("/proc/device-tree/model")
            .map(|s| s.trim_end_matches('\0').to_string())
            .unwrap_or_default();
        let (i2c_devices, notice) = match self.bus.scan().await {
            Ok((found, notice)) => {
                let hex: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                (hex, notice)
            }
            Err(e) => return ToolResult::err("hardware_inventory", format!("i2c scan: {e}")),
        };
        let inv = json!({
            "board": board,
            "gpiochips": gpiochip_names(),
            "i2c_devices": i2c_devices,
            "i2c_bus": self.bus.path().display().to_string(),
            "one_wire": one_wire_ids(),
            "iio": iio_devices(),
        });
        let mut out = ToolResult::ok("hardware_inventory", inv);
        out.notice = notice;
        out
    }
}

/// Build the full real-hardware tool-box for the interactive agent. The shared
/// `telemetry` instance is pushed directly instead of constructing a second one,
/// so the Broker's under-voltage STOP and the agent-facing tool read the same
/// vcgencmd state. I2cTool + InventoryTool share one `I2cAdapter` Mutex.
pub fn build_tools(
    chip: &str,
    i2c_bus: &str,
    gate: Arc<dyn Gate>,
    telemetry: Arc<TelemetryTool>,
) -> crate::hil::ToolVec {
    let bus = I2cAdapter::new(i2c_bus);
    vec![
        InventoryTool::new(bus.clone()),
        telemetry,
        I2cTool::new(bus),
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
