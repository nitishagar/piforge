// Telemetry tool — the agent's "instrument panel" before any physical action.
// Wraps vcgencmd (temp/volts/get_throttled) and dmesg/journalctl tail.
//
// The get_throttled decoder is load-bearing: a non-zero value is a STOP signal
// before adding load. Bitmask verified from the official Raspberry Pi docs
// (documentation/asciidoc/computers/os/graphics-utilities.adoc):
//
//	bit 0  (0x1)     undervoltage detected NOW
//	bit 1  (0x2)     arm frequency capped NOW
//	bit 2  (0x4)     currently throttled NOW
//	bit 3  (0x8)     soft temperature limit active NOW
//	bit 16 (0x10000) undervoltage has occurred since boot
//	bit 17 (0x20000) arm frequency capping has occurred
//	bit 18 (0x40000) throttling has occurred
//	bit 19 (0x80000) soft temperature limit has occurred
package hil

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strconv"
	"strings"

	"github.com/sashabaranov/go-openai"
)

// TelemetryTool exposes a single telemetry tool that returns a full snapshot.
// All sub-reads are Class R.
type TelemetryTool struct{}

// NewTelemetryTool builds a TelemetryTool.
func NewTelemetryTool() *TelemetryTool { return &TelemetryTool{} }

// Name implements Tool.
func (TelemetryTool) Name() string { return "telemetry" }

// Schema implements Tool.
func (TelemetryTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "telemetry",
			Description: "Read Raspberry Pi health telemetry. action=snapshot returns CPU temp, core volts, throttled state (undervoltage/throttle bits), dmesg tail, and free memory — the instrument panel before any physical action. action=throttled returns just the decoded throttled bitmask.",
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

type telemetryArgs struct {
	Action string `json:"action"`
}

// Execute implements Tool.
func (TelemetryTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a telemetryArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return Errorf("telemetry", "bad args: %v", err), nil
	}
	switch a.Action {
	case "temp":
		t, err := vcgencmd("measure_temp")
		if err != nil {
			return Errorf("telemetry", "measure_temp: %v", err), nil
		}
		return OKUnit("telemetry", map[string]any{"cpu_temp": t}, "degC"), nil
	case "throttled":
		raw, bits, err := throttled()
		if err != nil {
			return Errorf("telemetry", "get_throttled: %v", err), nil
		}
		notice := ""
		if bits.UnderVoltageNow || bits.UnderVoltageSinceBoot {
			notice = "UNDERVOLTAGE detected — STOP before adding load; brownout risks SD-card corruption and crashes. Use a 5V/3A+ official PSU."
		}
		return Result{Tool: "telemetry", OK: true, Value: map[string]any{
			"raw": raw, "decoded": bits,
		}, Notice: notice}, nil
	case "snapshot":
		return snapshot(ctx)
	default:
		return Errorf("telemetry", "unknown action %q", a.Action), nil
	}
}

// ThrottledBits is the decoded get_throttled bitmask.
type ThrottledBits struct {
	UnderVoltageNow        bool `json:"under_voltage_now"`
	ArmFreqCappedNow       bool `json:"arm_freq_capped_now"`
	CurrentlyThrottledNow  bool `json:"currently_throttled_now"`
	SoftTempLimitNow       bool `json:"soft_temp_limit_now"`
	UnderVoltageSinceBoot  bool `json:"under_voltage_since_boot"`
	ArmFreqCappedSinceBoot bool `json:"arm_freq_capped_since_boot"`
	ThrottledSinceBoot     bool `json:"throttled_since_boot"`
	SoftTempLimitSinceBoot bool `json:"soft_temp_limit_since_boot"`
}

// snapshot captures the full instrument panel.
func snapshot(ctx context.Context) (any, error) {
	out := map[string]any{}

	if t, err := vcgencmd("measure_temp"); err == nil {
		out["cpu_temp"] = t
	}
	if v, err := vcgencmd("measure_volts", "core"); err == nil {
		out["core_volts"] = v
	}
	if raw, bits, err := throttled(); err == nil {
		out["throttled_raw"] = raw
		out["throttled"] = bits
		if bits.UnderVoltageNow || bits.UnderVoltageSinceBoot {
			out["notice"] = "UNDERVOLTAGE detected — STOP before adding load."
		}
	}
	if dmesg, err := tail("dmesg"); err == nil {
		out["dmesg_tail"] = dmesg
	}
	if free, err := tail("free", "-m"); err == nil {
		out["free_m"] = free
	}
	if df, err := tail("df", "-h", "/"); err == nil {
		out["df_root"] = df
	}
	if model, err := readFile("/proc/device-tree/model"); err == nil {
		out["board"] = model
	}
	return OK("telemetry", out), nil
}

// throttled reads + decodes vcgencmd get_throttled.
func throttled() (string, ThrottledBits, error) {
	s, err := vcgencmd("get_throttled")
	if err != nil {
		return "", ThrottledBits{}, err
	}
	s = strings.TrimSpace(s)
	// vcgencmd prints e.g. "throttled=0x0" or just "0x0"
	if i := strings.Index(s, "="); i >= 0 {
		s = s[i+1:]
	}
	s = strings.TrimSpace(s)
	val, err := strconv.ParseUint(s, 0, 64) // base 0 => handle 0x prefix
	if err != nil {
		return s, ThrottledBits{}, fmt.Errorf("parse %q: %v", s, err)
	}
	bits := ThrottledBits{
		UnderVoltageNow:        val&(1<<0) != 0,
		ArmFreqCappedNow:       val&(1<<1) != 0,
		CurrentlyThrottledNow:  val&(1<<2) != 0,
		SoftTempLimitNow:       val&(1<<3) != 0,
		UnderVoltageSinceBoot:  val&(1<<16) != 0,
		ArmFreqCappedSinceBoot: val&(1<<17) != 0,
		ThrottledSinceBoot:     val&(1<<18) != 0,
		SoftTempLimitSinceBoot: val&(1<<19) != 0,
	}
	return s, bits, nil
}

// UnderVoltageActive reports whether the under-voltage stop condition is set.
// Used by the broker gate to refuse Class I ops before adding load. Returns
// (now, sinceBoot, err): either bit stops the agent from driving hardware.
func (TelemetryTool) UnderVoltageActive() (now, sinceBoot bool, err error) {
	_, bits, err := throttled()
	if err != nil {
		return false, false, err
	}
	return bits.UnderVoltageNow, bits.UnderVoltageSinceBoot, nil
}

// vcgencmd runs `vcgencmd <args...>` and returns trimmed stdout.
func vcgencmd(args ...string) (string, error) {
	return run("vcgencmd", args...)
}

// tail runs a command and returns up to N lines / its raw output.
func run(name string, args ...string) (string, error) {
	out, err := exec.Command(name, args...).Output()
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(string(out)), nil
}

// tail returns the full stdout of a command (used for dmesg/free/df).
func tail(name string, args ...string) (string, error) {
	fullArgs := append([]string(nil), args...)
	// For dmesg, limit lines via head.
	if name == "dmesg" {
		out, err := exec.Command("sh", "-c", "dmesg | tail -n 30").Output()
		if err != nil {
			return "", err
		}
		return strings.TrimSpace(string(out)), nil
	}
	return run(name, fullArgs...)
}

// readFile reads a flat file like /proc/device-tree/model.
func readFile(path string) (string, error) {
	out, err := run("cat", path)
	if err != nil {
		return "", err
	}
	// device-tree strings are null-terminated
	return strings.TrimRight(out, "\x00"), nil
}
