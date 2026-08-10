// Package sim provides simulated hardware tools for the capability eval.
//
// The eval gate must run headless (no real Pi hardware, no real sensors), so
// the agent's HIL tool calls are served from the Case fixture's Setup: an
// i2c_read returns the fixture's register bytes, a gpio_get returns the
// fixture's pin value, telemetry returns the fixture's throttled bitmask, and
// edits land in a per-case temp workspace that the scorer reads back.
//
// The simulated tools implement the same hil.Tool interface as the real ones,
// so the agent loop is identical between live and eval runs.
package sim

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"sync"

	"github.com/sashabaranov/go-openai"

	"github.com/nitishagar/piforge/internal/hil"
)

// State is the simulated board state, loaded from a Case fixture's Setup.
// All fields are safe to read concurrently (the agent loop dispatches one tool
// call at a time today, but the mutex guards future parallelism).
type State struct {
	mu        sync.Mutex
	Board     string
	I2C       *I2CBus
	GPIO      map[int]*Pin
	DmesgTail []string
	Throttled uint64
	// Files holds the workspace files the agent edits; keyed by relative path.
	Files map[string]string
}

// I2CBus models the bus for the scan/detect/read tools.
type I2CBus struct {
	// Devices maps the 7-bit address (int) to a device model.
	Devices map[int]*I2CDevice
	// ScanPattern overrides per-device presence for scan(): "all" means every
	// address responds (the shorted-bus fault case); "" means use Devices.
	ScanPattern string
}

// I2CDevice models a single device's registers.
type I2CDevice struct {
	Chip      string         // e.g. "BME280"
	Registers map[int][]byte // reg -> bytes returned for a read of that reg
}

// Pin models a GPIO pin's mode + value.
type Pin struct {
	Mode  string // "input" | "output"
	Value int    // 0 | 1
}

// --- the simulated tools ---

// I2CTool simulates i2c scan/detect/read against State.I2C.
type I2CTool struct {
	st *State
}

// NewI2CTool builds a simulated I2C tool.
func NewI2CTool(st *State) *I2CTool { return &I2CTool{st: st} }

// Name implements hil.Tool.
func (t *I2CTool) Name() string { return "i2c" }

// Schema mirrors the real I2CTool schema (keeps the agent prompt identical).
func (t *I2CTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "i2c",
			Description: "Interact with the I2C bus: action=scan to list device addresses, action=read to read N bytes from a register, action=detect to confirm a device is present. Class R (safe). Returns structured values.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"action":   map[string]any{"type": "string", "enum": []string{"scan", "read", "detect"}},
					"address":  map[string]any{"type": "integer", "description": "7-bit I2C address, e.g. 118 for 0x76"},
					"register": map[string]any{"type": "integer", "description": "register byte to read (read only)"},
					"length":   map[string]any{"type": "integer", "description": "bytes to read (read only, default 1)"},
				},
				"required": []string{"action"},
			},
		},
	}
}

type i2cArgs struct {
	Action   string `json:"action"`
	Address  int    `json:"address"`
	Register *int   `json:"register"`
	Length   int    `json:"length"`
}

// Execute implements hil.Tool.
func (t *I2CTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a i2cArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return hil.Errorf("i2c", "bad args: %v", err), nil
	}
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	switch a.Action {
	case "scan":
		return t.scan(), nil
	case "detect":
		return t.detect(a.Address), nil
	case "read":
		reg := 0
		if a.Register != nil {
			reg = *a.Register
		}
		n := a.Length
		if n == 0 {
			n = 1
		}
		return t.read(a.Address, reg, n), nil
	default:
		return hil.Errorf("i2c", "unknown action %q", a.Action), nil
	}
}

func (t *I2CTool) scan() hil.Result {
	if t.st.I2C == nil {
		return hil.OK("i2c", map[string]any{"devices": []string{}, "count": 0})
	}
	if t.st.I2C.ScanPattern == "all" {
		// The "every address responds" shorted-bus fault case.
		hex := make([]string, 0, 0x70)
		for addr := 0x08; addr <= 0x77; addr++ {
			hex = append(hex, fmt.Sprintf("0x%02x", addr))
		}
		return hil.Result{
			Tool: "i2c", OK: true, Unit: "7-bit addr",
			Value:  map[string]any{"devices": hex, "count": len(hex)},
			Notice: "many addresses responded — likely SDA/SCL shorted to power; STOP and check wiring before any further I2C op",
		}
	}
	var found []string
	for addr := range t.st.I2C.Devices {
		found = append(found, fmt.Sprintf("0x%02x", addr))
	}
	notice := ""
	if len(found) == 0 {
		notice = "no devices found — check dtparam=i2c_arm=on, wiring, and pull-ups"
	}
	return hil.Result{
		Tool: "i2c", OK: true, Unit: "7-bit addr",
		Value: map[string]any{"devices": found, "count": len(found)}, Notice: notice,
	}
}

func (t *I2CTool) detect(addr int) hil.Result {
	if t.st.I2C == nil {
		return hil.Errorf("i2c", "no device at 0x%02x", addr)
	}
	if _, ok := t.st.I2C.Devices[addr]; ok {
		return hil.OK("i2c", map[string]any{"address": fmt.Sprintf("0x%02x", addr), "present": true})
	}
	return hil.Errorf("i2c", "no device at 0x%02x", addr)
}

func (t *I2CTool) read(addr, reg, n int) hil.Result {
	if t.st.I2C == nil {
		return hil.Errorf("i2c", "no device at 0x%02x", addr)
	}
	dev, ok := t.st.I2C.Devices[addr]
	if !ok {
		return hil.Errorf("i2c", "no device at 0x%02x", addr)
	}
	// Default to zero bytes if the fixture didn't specify this register.
	src := dev.Registers[reg]
	out := make([]byte, n)
	for i := 0; i < n && i < len(src); i++ {
		out[i] = src[i]
	}
	res := map[string]any{
		"address":  fmt.Sprintf("0x%02x", addr),
		"register": fmt.Sprintf("0x%02x", reg),
		"raw_hex":  fmt.Sprintf("%x", out),
		"raw_dec":  out,
	}
	if dev.Chip != "" {
		res["chip"] = dev.Chip
	}
	return hil.OKUnit("i2c", res, "bytes (see datasheet for scaling)")
}

// --- GPIO ---

// GPIOTool simulates gpio get/set.
type GPIOTool struct {
	st   *State
	gate hil.Gate
}

// NewGPIOTool builds a simulated GPIO tool. gate (may be nil) is consulted on set.
func NewGPIOTool(st *State, gate hil.Gate) *GPIOTool { return &GPIOTool{st: st, gate: gate} }

// Name implements hil.Tool.
func (t *GPIOTool) Name() string { return "gpio" }

// Schema mirrors the real GPIOTool schema.
func (t *GPIOTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "gpio",
			Description: "Read or set a GPIO pin. action=get to read (safe); action=set to drive (Class I: physical). Returns {pin, value, unit:'level(0|1)'}.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"action": map[string]any{"type": "string", "enum": []string{"get", "set"}},
					"pin":    map[string]any{"type": "integer", "description": "BCM pin number"},
					"value":  map[string]any{"type": "integer", "enum": []int{0, 1}},
				},
				"required": []string{"action", "pin"},
			},
		},
	}
}

type gpioArgs struct {
	Action string `json:"action"`
	Pin    int    `json:"pin"`
	Value  int    `json:"value"`
}

// Execute implements hil.Tool.
func (t *GPIOTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a gpioArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return hil.Errorf("gpio", "bad args: %v", err), nil
	}
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	switch a.Action {
	case "get":
		p, ok := t.st.GPIO[a.Pin]
		if !ok {
			return hil.Errorf("gpio", "pin %d not in profile", a.Pin), nil
		}
		return hil.OKUnit("gpio", map[string]any{"pin": a.Pin, "value": p.Value}, "level(0|1)"), nil
	case "set":
		if a.Value != 0 && a.Value != 1 {
			return hil.Errorf("gpio", "value must be 0 or 1"), nil
		}
		if t.gate != nil {
			if err := t.gate.Allow("gpio_set", map[string]any{"pin": a.Pin, "value": a.Value}); err != nil {
				return hil.Errorf("gpio", "DENIED by safety gate: %v", err), nil
			}
		}
		if _, ok := t.statePin(a.Pin); !ok {
			return hil.Errorf("gpio", "pin %d not in profile", a.Pin), nil
		}
		t.statePinSet(a.Pin, a.Value)
		return hil.OKUnit("gpio", map[string]any{"pin": a.Pin, "value": a.Value, "driven": true}, "level(0|1)"), nil
	default:
		return hil.Errorf("gpio", "unknown action %q", a.Action), nil
	}
}

// statePin returns the pin if present.
func (t *GPIOTool) statePin(pin int) (*Pin, bool) {
	p, ok := t.st.GPIO[pin]
	return p, ok
}

// statePinSet drives the pin (mode becomes output).
func (t *GPIOTool) statePinSet(pin, v int) {
	if t.st.GPIO[pin] == nil {
		t.st.GPIO[pin] = &Pin{}
	}
	t.st.GPIO[pin].Mode = "output"
	t.st.GPIO[pin].Value = v
}

// --- scope (simulated edge events) ---

// ScopeTool simulates a time-series capture. Without a real edge model it
// returns the pin's static value as a single edge — enough for the eval loop.
type ScopeTool struct {
	st *State
}

// NewScopeTool builds a simulated scope tool.
func NewScopeTool(st *State) *ScopeTool { return &ScopeTool{st: st} }

func (ScopeTool) Name() string { return "scope" }

func (ScopeTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "scope",
			Description: "Capture GPIO edge events over a time window. Class R.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"pin":      map[string]any{"type": "integer"},
					"duration": map[string]any{"type": "number"},
				},
				"required": []string{"pin", "duration"},
			},
		},
	}
}

type scopeArgs struct {
	Pin      int     `json:"pin"`
	Duration float64 `json:"duration"`
}

func (t *ScopeTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a scopeArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return hil.Errorf("scope", "bad args: %v", err), nil
	}
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	p, ok := t.st.GPIO[a.Pin]
	if !ok {
		return hil.Errorf("scope", "pin %d not in profile", a.Pin), nil
	}
	edge := "rising"
	if p.Value == 0 {
		edge = "falling"
	}
	return hil.OK("scope", map[string]any{
		"pin": a.Pin, "duration_ms": a.Duration, "edges": 1, "rate_hz": 0,
		"events": []map[string]any{{"t_us": 0, "edge": edge}},
	}), nil
}

// --- telemetry ---

// TelemetryTool simulates the telemetry snapshot from State.
type TelemetryTool struct {
	st *State
}

func NewTelemetryTool(st *State) *TelemetryTool { return &TelemetryTool{st: st} }

func (TelemetryTool) Name() string { return "telemetry" }

func (TelemetryTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "telemetry",
			Description: "Read Pi health telemetry. action=snapshot returns temp/volts/throttled/dmesg; action=throttled returns the decoded bitmask; action=temp returns CPU temp.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"action": map[string]any{"type": "string", "enum": []string{"snapshot", "throttled", "temp"}},
				},
				"required": []string{"action"},
			},
		},
	}
}

type telArgs struct {
	Action string `json:"action"`
}

func (t *TelemetryTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a telArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return hil.Errorf("telemetry", "bad args: %v", err), nil
	}
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	switch a.Action {
	case "temp":
		return hil.OKUnit("telemetry", map[string]any{"cpu_temp": "temp=48.5'C"}, "degC"), nil
	case "throttled":
		return t.throttled(), nil
	case "snapshot":
		return t.snapshot(), nil
	default:
		return hil.Errorf("telemetry", "unknown action %q", a.Action), nil
	}
}

// ThrottledBits mirrors the real telemetry decoder's output shape.
type ThrottledBits struct {
	UnderVoltageNow       bool `json:"under_voltage_now"`
	CurrentlyThrottledNow bool `json:"currently_throttled_now"`
	UnderVoltageSinceBoot bool `json:"under_voltage_since_boot"`
	ThrottledSinceBoot    bool `json:"throttled_since_boot"`
}

func (t *TelemetryTool) throttled() hil.Result {
	val := t.st.Throttled
	bits := ThrottledBits{
		UnderVoltageNow:       val&(1<<0) != 0,
		CurrentlyThrottledNow: val&(1<<2) != 0,
		UnderVoltageSinceBoot: val&(1<<16) != 0,
		ThrottledSinceBoot:    val&(1<<18) != 0,
	}
	notice := ""
	if bits.UnderVoltageNow || bits.UnderVoltageSinceBoot {
		notice = "UNDERVOLTAGE detected — STOP before adding load; brownout risks SD-card corruption. Use a 5V/3A+ official PSU."
	}
	return hil.Result{
		Tool: "telemetry", OK: true,
		Value:  map[string]any{"raw": fmt.Sprintf("0x%x", val), "decoded": bits},
		Notice: notice,
	}
}

func (t *TelemetryTool) snapshot() hil.Result {
	out := map[string]any{
		"cpu_temp":   "temp=48.5'C",
		"core_volts": "volt=1.0V",
		"board":      t.st.Board,
	}
	r := t.throttled()
	out["throttled"] = r.Value
	if r.Notice != "" {
		out["notice"] = r.Notice
	}
	if len(t.st.DmesgTail) > 0 {
		out["dmesg_tail"] = strings.Join(t.st.DmesgTail, "\n")
	}
	return hil.OK("telemetry", out)
}

// UnderVoltageActive reports whether the under-voltage stop condition is set.
// Used by the broker gate in eval runs.
func (t *TelemetryTool) UnderVoltageActive() (now, sinceBoot bool, err error) {
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	val := t.st.Throttled
	return val&(1<<0) != 0, val&(1<<16) != 0, nil
}

// --- inventory ---

// InventoryTool lists the simulated board's detected hardware.
type InventoryTool struct {
	st *State
}

func NewInventoryTool(st *State) *InventoryTool { return &InventoryTool{st: st} }

func (InventoryTool) Name() string { return "hardware_inventory" }

func (InventoryTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "hardware_inventory",
			Description: "List detected hardware: I2C devices, GPIO pins in profile, board model. Call this first to ground yourself.",
			Parameters:  map[string]any{"type": "object", "properties": map[string]any{}},
		},
	}
}

func (t *InventoryTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	t.st.mu.Lock()
	defer t.st.mu.Unlock()
	inv := map[string]any{"board": t.st.Board}
	if t.st.I2C != nil {
		var addrs []string
		for a := range t.st.I2C.Devices {
			addrs = append(addrs, fmt.Sprintf("0x%02x", a))
		}
		inv["i2c_devices"] = addrs
	}
	if len(t.st.GPIO) > 0 {
		pins := make([]int, 0, len(t.st.GPIO))
		for p := range t.st.GPIO {
			pins = append(pins, p)
		}
		inv["gpio_pins"] = pins
	}
	return hil.OK("hardware_inventory", inv), nil
}

// --- helpers ---

// StateFromSetup builds a sim.State from an eval Case's Setup.
func StateFromSetup(s Setup) *State {
	st := &State{
		Board:     s.Board,
		GPIO:      map[int]*Pin{},
		Files:     map[string]string{},
		Throttled: parseHex(s.Throttled),
		DmesgTail: s.DmesgTail,
	}
	if s.I2CDevices != nil || s.ScanPattern != "" {
		st.I2C = &I2CBus{Devices: map[int]*I2CDevice{}, ScanPattern: s.ScanPattern}
		for addr, chip := range s.I2CDevices {
			if addr == 0 {
				continue
			}
			st.I2C.Devices[addr] = &I2CDevice{Chip: chip, Registers: map[int][]byte{}}
		}
	}
	for pin, mode := range s.GPIOPins {
		st.GPIO[pin] = &Pin{Mode: mode, Value: 0}
	}
	for path, content := range s.Files {
		st.Files[path] = content
	}
	return st
}

func parseHex(s string) uint64 {
	s = strings.TrimSpace(s)
	if i := strings.Index(s, "="); i >= 0 {
		s = s[i+1:]
	}
	s = strings.TrimSpace(s)
	var v uint64
	for _, ch := range s {
		switch {
		case ch >= '0' && ch <= '9':
			v = v*16 + uint64(ch-'0')
		case ch >= 'a' && ch <= 'f':
			v = v*16 + uint64(ch-'a'+10)
		case ch >= 'A' && ch <= 'F':
			v = v*16 + uint64(ch-'A'+10)
		case ch == 'x' || ch == 'X':
			// allow 0x prefix
		}
	}
	return v
}

// Setup is the eval fixture's setup (re-declared here to avoid an import cycle
// eval -> sim -> eval; the eval package converts from its own Case.Setup).
type Setup struct {
	Board       string            `json:"board"`
	I2CDevices  map[int]string    `json:"i2c_devices"`
	ScanPattern string            `json:"scan_pattern"` // "all" => every address responds
	GPIOPins    map[int]string    `json:"gpio_pins"`
	Files       map[string]string `json:"files"`
	DmesgTail   []string          `json:"dmesg_tail"`
	Throttled   string            `json:"throttled"`
}
