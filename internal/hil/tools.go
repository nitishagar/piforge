// Package hil implements the hardware-in-the-loop tool surface: the typed
// wrappers over GPIO/I2C/sysfs/vcgencmd that give the agent structured reads
// with units, ranges, and (for scope) time-series the cloud-over-SSH path
// cannot match.
//
// Tool selection (from the verified research):
//   - GPIO + scope: github.com/warthog618/go-gpiocdev (pure Go, Pi-5-aware,
//     in-process edge events — preserves the moat's latency).
//   - I2C: periph.io/x/conn/v3/i2c (pure Go, bus-agnostic via /dev/i2c-N).
//   - vcgencmd/get_throttled/dmesg: exec shell-out (no Go lib; low frequency).
package hil

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/sashabaranov/go-openai"
)

// Tool is the interface every HIL tool implements. A tool carries its OpenAI
// function schema (so the agent can call it) and an Execute method.
type Tool interface {
	// Name is the function name the model calls.
	Name() string
	// Schema returns the OpenAI tool definition (type "function").
	Schema() openai.Tool
	// Execute runs the tool with JSON arguments and returns a JSON result
	// (which the agent sees as the tool-call output) and an error.
	Execute(ctx context.Context, args json.RawMessage) (any, error)
}

// Result is the wrapper for a tool's structured output.
type Result struct {
	Tool   string `json:"tool"`
	OK     bool   `json:"ok"`
	Value  any    `json:"value,omitempty"`
	Unit   string `json:"unit,omitempty"`
	Error  string `json:"error,omitempty"`
	Notice string `json:"notice,omitempty"`
}

// MarshalJSON renders a Result compactly. Empty fields are omitted so the
// agent's context isn't polluted with "unit":"" noise.
func (r Result) String() string {
	b, _ := json.Marshal(r)
	return string(b)
}

// Gate is the safety gate the broker implements. A tool consults it before
// any Class I (physical/irreversible) operation. Defined here (untagged) so
// both the Linux and non-Linux stub builds can reference it.
type Gate interface {
	// Allow returns nil if the op is permitted, else an error describing why.
	// "physical" is e.g. "gpio_set", "pwm_start", "i2c_write", "motor_drive".
	Allow(physical string, detail map[string]any) error
}

// Errorf returns a Result indicating failure. Tools return this rather than a
// Go error so the agent can read the failure and decide what to do.
func Errorf(tool, format string, args ...any) Result {
	return Result{Tool: tool, OK: false, Error: fmt.Sprintf(format, args...)}
}

// OK returns a successful Result with a value.
func OK(tool string, value any) Result {
	return Result{Tool: tool, OK: true, Value: value}
}

// OKUnit returns a successful Result with a value and unit.
func OKUnit(tool string, value any, unit string) Result {
	return Result{Tool: tool, OK: true, Value: value, Unit: unit}
}
