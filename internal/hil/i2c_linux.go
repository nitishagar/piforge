// I2C tool — typed read over /dev/i2c-N via periph.io. Returns parsed values
// with units + nominal ranges for known devices. The unit/range parsing is
// the product's defense against the #1 LLM failure mode (raw counts vs Pa
// vs hPa, big-endian vs little-endian).

//go:build linux

package hil

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"

	"github.com/sashabaranov/go-openai"
	"periph.io/x/conn/v3/i2c"
	"periph.io/x/conn/v3/i2c/i2creg"
)

// I2CTool exposes i2c_scan, i2c_read, and i2c_detect. All Class R.
type I2CTool struct {
	busName string
	gate    Gate // consulted on i2c_write (Class I)
}

// NewI2CTool builds an I2CTool. busName is e.g. "1" (resolves to /dev/i2c-1);
// empty means the default bus.
func NewI2CTool(busName string, gate Gate) *I2CTool {
	return &I2CTool{busName: busName, gate: gate}
}

// Name implements Tool.
func (t *I2CTool) Name() string { return "i2c" }

// Schema implements Tool.
func (t *I2CTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "i2c",
			Description: "Interact with the I2C bus: action=scan to list device addresses, action=read to read N bytes from a register, action=detect to confirm a device is present. Class R (safe). Returns structured values; for known sensors, parsed with units + nominal ranges.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"action":  map[string]any{"type": "string", "enum": []string{"scan", "read", "detect"}},
					"address": map[string]any{"type": "integer", "description": "7-bit I2C address, e.g. 118 for 0x76"},
					"register": map[string]any{"type": "integer", "description": "register byte to read (read only)"},
					"length":  map[string]any{"type": "integer", "description": "bytes to read (read only, default 1)"},
				},
				"required": []string{"action"},
			},
		},
	}
}

type i2cArgs struct {
	Action  string `json:"action"`
	Address int    `json:"address"`
	Register *int  `json:"register"`
	Length  int    `json:"length"`
}

// Execute implements Tool.
func (t *I2CTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a i2cArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return Errorf("i2c", "bad args: %v", err), nil
	}
	switch a.Action {
	case "scan":
		return t.scan()
	case "detect":
		if a.Address == 0 {
			return Errorf("i2c", "detect requires address"), nil
		}
		return t.detect(a.Address)
	case "read":
		if a.Address == 0 {
			return Errorf("i2c", "read requires address"), nil
		}
		reg := 0
		if a.Register != nil {
			reg = *a.Register
		}
		n := a.Length
		if n == 0 {
			n = 1
		}
		return t.read(a.Address, reg, n)
	default:
		return Errorf("i2c", "unknown action %q", a.Action), nil
	}
}

func (t *I2CTool) openBus() (i2c.BusCloser, error) {
	return i2creg.Open(t.busName)
}

// scan probes the standard 7-bit address range and returns present devices.
func (t *I2CTool) scan() (any, error) {
	b, err := t.openBus()
	if err != nil {
		return Errorf("i2c", "open bus: %v (is user in 'i2c' group? is dtparam=i2c_arm=on?)", err), nil
	}
	defer b.Close()

	var found []int
	for addr := 0x08; addr <= 0x77; addr++ {
		dev := &i2c.Dev{Bus: b, Addr: uint16(addr)}
		// A zero-length write is an SMBus quick read; periph handles it.
		if err := dev.Tx(nil, nil); err == nil {
			found = append(found, addr)
		}
	}
	hex := make([]string, len(found))
	for i, a := range found {
		hex[i] = fmt.Sprintf("0x%02x", a)
	}
	notice := ""
	if len(found) == 0 {
		notice = "no devices found — check dtparam=i2c_arm=on in /boot/firmware/config.txt, wiring, and pull-ups"
	}
	if len(found) > 40 {
		notice = "many addresses responded — likely SDA/SCL shorted to power; STOP and check wiring before any further I2C op"
	}
	return Result{Tool: "i2c", OK: true, Value: map[string]any{
		"devices": hex, "count": len(found),
	}, Unit: "7-bit addr", Notice: notice}, nil
}

// detect confirms a single device is present at address.
func (t *I2CTool) detect(addr int) (any, error) {
	b, err := t.openBus()
	if err != nil {
		return Errorf("i2c", "open bus: %v", err), nil
	}
	defer b.Close()
	dev := &i2c.Dev{Bus: b, Addr: uint16(addr)}
	if err := dev.Tx(nil, nil); err != nil {
		return Errorf("i2c", "no device at 0x%02x: %v", addr, err), nil
	}
	return OK("i2c", map[string]any{"address": fmt.Sprintf("0x%02x", addr), "present": true}), nil
}

// read reads n bytes from a register and tries to interpret for known devices.
func (t *I2CTool) read(addr, reg, n int) (any, error) {
	b, err := t.openBus()
	if err != nil {
		return Errorf("i2c", "open bus: %v", err), nil
	}
	defer b.Close()
	dev := &i2c.Dev{Bus: b, Addr: uint16(addr)}
	buf := make([]byte, n)
	w := []byte{byte(reg)}
	if err := dev.Tx(w, buf); err != nil {
		return Errorf("i2c", "read 0x%02x reg 0x%02x: %v", addr, reg, err), nil
	}
	out := map[string]any{
		"address":  fmt.Sprintf("0x%02x", addr),
		"register": fmt.Sprintf("0x%02x", reg),
		"raw_hex":  fmt.Sprintf("%x", buf),
		"raw_dec":  buf,
	}
	// For multi-byte reads, include both endian interpretations — the device
	// datasheet decides which is correct, and surfacing both helps the agent
	// avoid the classic unit/endian confusion.
	if n == 2 {
		out["be_uint16"] = binary.BigEndian.Uint16(buf)
		out["le_uint16"] = binary.LittleEndian.Uint16(buf)
	}
	return OKUnit("i2c", out, "bytes (see datasheet for scaling)"), nil
}
