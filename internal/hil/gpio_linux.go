// GPIO tool — typed read/set over the character device via go-gpiocdev.
// On a Pi 5 the 40-pin header lives on gpiochip4 (auto-discovered if unset).

//go:build linux

package hil

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/sashabaranov/go-openai"
	"github.com/warthog618/go-gpiocdev"
)

// GPIOTool exposes gpio_get and gpio_set. Reads are Class R (auto-approve);
// writes are Class I (physical; require arming via the broker).
type GPIOTool struct {
	chip string
	// gate, if non-nil, is consulted before any Class I (write) op.
	gate Gate
}

// NewGPIOTool builds a GPIOTool. chip is the gpiochip name; if empty it's
// auto-discovered at first use (Pi 5 => gpiochip4).
func NewGPIOTool(chip string, gate Gate) *GPIOTool {
	return &GPIOTool{chip: chip, gate: gate}
}

// Name implements Tool.
func (g *GPIOTool) Name() string { return "gpio" }

// Schema implements Tool.
func (g *GPIOTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "gpio",
			Description: "Read or set a GPIO pin on the Raspberry Pi 40-pin header. Use action=get to read a pin value (Class R, safe). Use action=set to drive a pin high(1)/low(0) (Class I: physical, requires arming). Returns {pin, value, unit:'level(0|1)'}.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"action": map[string]any{
						"type": "string",
						"enum": []string{"get", "set"},
					},
					"pin":   map[string]any{"type": "integer", "description": "BCM pin number, e.g. 17"},
					"value": map[string]any{"type": "integer", "enum": []int{0, 1}, "description": "0=low, 1=high (set only)"},
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

// Execute implements Tool.
func (g *GPIOTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a gpioArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return Errorf("gpio", "bad args: %v", err), nil
	}
	chip := g.chip
	if chip == "" {
		chip = "gpiochip4" // Pi 5 default; ResolveChip sets this in the constructor
	}

	switch a.Action {
	case "get":
		return g.get(ctx, chip, a.Pin)
	case "set":
		return g.set(ctx, chip, a.Pin, a.Value)
	default:
		return Errorf("gpio", "unknown action %q (use get|set)", a.Action), nil
	}
}

func (g *GPIOTool) get(ctx context.Context, chip string, pin int) (any, error) {
	line, err := gpiocdev.RequestLine(chip, pin, gpiocdev.AsInput)
	if err != nil {
		return Errorf("gpio", "request pin %d on %s: %v (is user in 'gpio' group?)", pin, chip, err), nil
	}
	defer line.Close()
	val, err := line.Value()
	if err != nil {
		return Errorf("gpio", "read pin %d: %v", pin, err), nil
	}
	return OKUnit("gpio", map[string]any{"pin": pin, "value": val, "chip": chip}, "level(0|1)"), nil
}

func (g *GPIOTool) set(ctx context.Context, chip string, pin, value int) (any, error) {
	if value != 0 && value != 1 {
		return Errorf("gpio", "value must be 0 or 1, got %d", value), nil
	}
	// Class I: consult the broker gate before touching hardware.
	if g.gate != nil {
		if err := g.gate.Allow("gpio_set", map[string]any{
			"chip": chip, "pin": pin, "value": value,
		}); err != nil {
			return Errorf("gpio", "DENIED by safety gate: %v", err), nil
		}
	}
	line, err := gpiocdev.RequestLine(chip, pin, gpiocdev.AsOutput(value))
	if err != nil {
		return Errorf("gpio", "request pin %d as output: %v", pin, err), nil
	}
	defer line.Close()
	return OKUnit("gpio", map[string]any{"pin": pin, "value": value, "chip": chip, "driven": true}, "level(0|1)"), nil
}

// ---- Scope / Monitor (the genuine moat) ----

// ScopeTool captures a time-series of edge events on a GPIO line using the
// kernel's character-device edge interrupts (in-process, sub-ms). This is the
// capability that the cloud-over-SSH path cannot match: WAN jitter (10s of ms)
// makes sustained edge capture impractical remotely.
type ScopeTool struct {
	chip string
}

// NewScopeTool builds a ScopeTool.
func NewScopeTool(chip string) *ScopeTool { return &ScopeTool{chip: chip} }

// Name implements Tool.
func (s *ScopeTool) Name() string { return "scope" }

// Schema implements Tool.
func (s *ScopeTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "scope",
			Description: "Capture GPIO edge events over a time window (a logic-analyzer-style time series). Returns timestamps of rising/falling edges. Class R. This is the tool that catches glitches a single sample misses.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"pin":      map[string]any{"type": "integer", "description": "BCM pin number to monitor"},
					"duration": map[string]any{"type": "number", "description": "capture window in milliseconds (max 5000)"},
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

// EdgeEvent is a single edge in a scope capture.
type EdgeEvent struct {
	T    int64  `json:"t_us"` // monotonic microseconds since capture start
	Edge string `json:"edge"` // "rising" | "falling"
}

// Execute implements Tool.
func (s *ScopeTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a scopeArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return Errorf("scope", "bad args: %v", err), nil
	}
	if a.Duration <= 0 || a.Duration > 5000 {
		return Errorf("scope", "duration must be 1..5000 ms, got %v", a.Duration), nil
	}
	chip := s.chip
	if chip == "" {
		chip = "gpiochip4" // Pi 5 default
	}

	var events []EdgeEvent
	handler := func(evt gpiocdev.LineEvent) {
		edge := "falling"
		if evt.Type == gpiocdev.LineEventRisingEdge {
			edge = "rising"
		}
		// evt.Timestamp is time.Duration (CLOCK_MONOTONIC since kernel boot);
		// record microseconds-since-start for relative timing between edges.
		events = append(events, EdgeEvent{T: evt.Timestamp.Microseconds(), Edge: edge})
	}

	line, err := gpiocdev.RequestLine(chip, a.Pin,
		gpiocdev.AsInput,
		gpiocdev.WithBothEdges,
		gpiocdev.WithEventHandler(handler),
	)
	if err != nil {
		return Errorf("scope", "request pin %d for edges: %v", a.Pin, err), nil
	}
	defer line.Close()

	dur := time.Duration(a.Duration * float64(time.Millisecond))
	timer := time.NewTimer(dur)
	defer timer.Stop()
	select {
	case <-timer.C:
	case <-ctx.Done():
		return Errorf("scope", "cancelled"), nil
	}

	rate := 0.0
	if dur > 0 {
		rate = float64(len(events)) / dur.Seconds()
	}
	return OK("scope", map[string]any{
		"pin":         a.Pin,
		"duration_ms": a.Duration,
		"edges":       len(events),
		"rate_hz":     rate,
		"events":      events,
	}), nil
}

// ResolveChip finds the gpiochip name for the Pi 5 40-pin header. If chip is
// non-empty it is returned as-is; otherwise the common Pi 5 chips are probed.
func ResolveChip(chip string) (string, error) {
	if chip != "" {
		return chip, nil
	}
	for _, name := range []string{"gpiochip4", "gpiochip0"} {
		c, err := gpiocdev.NewChip(name)
		if err == nil {
			c.Close()
			return name, nil
		}
	}
	return "", fmt.Errorf("no gpiochip found (set hardware.gpiochip in config)")
}
