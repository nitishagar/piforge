// Code-edit tool — whole-file replacement (the Aider-validated edit format for
// small coders; NOT SEARCH/REPLACE). Small models mangle SEARCH/REPLACE blocks
// far more than they botch whole-file rewrites. Class B (bounded; file edit).
package hil

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/sashabaranov/go-openai"
)

// CodeEditTool edits files by whole-file replacement within an allowed root.
type CodeEditTool struct {
	root string // workspace root; edits confined under it
}

// NewCodeEditTool builds a CodeEditTool confined to root.
func NewCodeEditTool(root string) *CodeEditTool {
	return &CodeEditTool{root: root}
}

// Name implements Tool.
func (c *CodeEditTool) Name() string { return "edit_file" }

// Schema implements Tool.
func (c *CodeEditTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "edit_file",
			Description: "Write the full new contents of a file (whole-file edit, not patch). The path must be relative to the workspace root. The file is created if missing, overwritten if present. Read the file first, then return its entire new content here. Class B (file edit).",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"path":    map[string]any{"type": "string", "description": "path relative to workspace root"},
					"content": map[string]any{"type": "string", "description": "full new file contents"},
				},
				"required": []string{"path", "content"},
			},
		},
	}
}

type editArgs struct {
	Path    string `json:"path"`
	Content string `json:"content"`
}

// Execute implements Tool.
func (c *CodeEditTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	var a editArgs
	if err := json.Unmarshal(raw, &a); err != nil {
		return Errorf("edit_file", "bad args: %v", err), nil
	}
	if a.Path == "" {
		return Errorf("edit_file", "path required"), nil
	}
	abs, err := c.safe(a.Path)
	if err != nil {
		return Errorf("edit_file", "%v", err), nil
	}
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return Errorf("edit_file", "mkdir: %v", err), nil
	}
	if err := os.WriteFile(abs, []byte(a.Content), 0o644); err != nil {
		return Errorf("edit_file", "write: %v", err), nil
	}
	return OK("edit_file", map[string]any{
		"path": a.Path, "bytes": len(a.Content),
	}), nil
}

// safe resolves rel under root and rejects escapes (path traversal).
func (c *CodeEditTool) safe(rel string) (string, error) {
	if filepath.IsAbs(rel) {
		return "", errors.New("path must be relative to workspace root")
	}
	abs := filepath.Join(c.root, rel)
	cleanRoot, _ := filepath.Abs(c.root)
	cleanAbs, _ := filepath.Abs(abs)
	if !within(cleanRoot, cleanAbs) {
		return "", fmt.Errorf("path %q escapes workspace root", rel)
	}
	return cleanAbs, nil
}

func within(parent, child string) bool {
	rel, err := filepath.Rel(parent, child)
	if err != nil {
		return false
	}
	return rel != ".." && !startsWith(rel, "..")
}

func startsWith(s, prefix string) bool {
	return len(s) >= len(prefix) && s[:len(prefix)] == prefix
}

// ---- hardware_inventory ----

// InventoryTool lists the board's detected hardware: gpiochips, I2C devices,
// 1-wire sensors, IIO devices. This is the agent's first call to ground itself.
// Class R.
type InventoryTool struct{}

func NewInventoryTool() *InventoryTool { return &InventoryTool{} }

func (InventoryTool) Name() string { return "hardware_inventory" }

func (InventoryTool) Schema() openai.Tool {
	return openai.Tool{
		Type: openai.ToolTypeFunction,
		Function: &openai.FunctionDefinition{
			Name:        "hardware_inventory",
			Description: "List detected hardware on this Raspberry Pi: gpiochips, I2C bus devices, 1-wire sensors (/sys/bus/w1), and IIO devices (/sys/bus/iio). Call this first to ground yourself before reading sensors or editing drivers.",
			Parameters:  map[string]any{"type": "object", "properties": map[string]any{}},
		},
	}
}

// Execute implements Tool.
func (InventoryTool) Execute(ctx context.Context, raw json.RawMessage) (any, error) {
	inv := map[string]any{}
	if s, err := run("ls", "/dev/gpiochip*"); err == nil {
		inv["gpiochips"] = s
	} else if s, err := run("sh", "-c", "ls /dev/gpiochip* 2>/dev/null"); err == nil {
		inv["gpiochips"] = s
	}
	if s, err := run("sh", "-c", "ls /sys/bus/w1/devices/ 2>/dev/null"); err == nil && s != "" {
		inv["one_wire"] = s
	}
	if s, err := run("sh", "-c", "ls /sys/bus/iio/devices/ 2>/dev/null"); err == nil && s != "" {
		inv["iio"] = s
	}
	if s, err := readFile("/proc/device-tree/model"); err == nil {
		inv["board"] = s
	}
	return OK("hardware_inventory", inv), nil
}
