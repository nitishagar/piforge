//go:build !linux

// Stubs for non-Linux builds (macOS dev). The real hardware tools live in
// gpio_linux.go and i2c_linux.go and compile only on Linux (go-gpiocdev's uapi
// package is Linux-only). On a non-Linux host the constructors return stubs
// that error at runtime, so the agent loop still compiles and runs against a
// remote llama-server for development.

package hil

import (
	"context"
	"encoding/json"

	"github.com/sashabaranov/go-openai"
)

// stubTool implements Tool by returning a "Linux-only" error.
type stubTool struct {
	name string
	desc string
}

func (s stubTool) Name() string { return s.name }
func (s stubTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        s.name,
			Description: s.desc + " (Linux-only stub on this build)",
			Parameters:  map[string]any{"type": "object", "properties": map[string]any{}},
		},
	}
}
func (s stubTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	return Errorf(s.name, "this tool is Linux-only — build with GOOS=linux for hardware access"), nil
}

// GPIOTool is a stub on non-Linux.
type GPIOTool = stubTool

// NewGPIOTool returns a stub on non-Linux.
func NewGPIOTool(chip string, gate Gate) *GPIOTool {
	return &GPIOTool{name: "gpio", desc: "Read or set a GPIO pin"}
}

// ScopeTool is a stub on non-Linux.
type ScopeTool = stubTool

// NewScopeTool returns a stub on non-Linux.
func NewScopeTool(chip string) *ScopeTool {
	return &ScopeTool{name: "scope", desc: "Capture GPIO edge events"}
}

// I2CTool is a stub on non-Linux.
type I2CTool = stubTool

// NewI2CTool returns a stub on non-Linux.
func NewI2CTool(busName string, gate Gate) *I2CTool {
	return &I2CTool{name: "i2c", desc: "Interact with the I2C bus"}
}

// ResolveChip returns the configured chip or a default on non-Linux.
func ResolveChip(chip string) (string, error) {
	if chip != "" {
		return chip, nil
	}
	return "gpiochip4", nil
}
