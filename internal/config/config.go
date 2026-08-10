// Package config holds PiForge's configuration: model + server settings,
// hardware profile, safety policy. Loaded from a repo-root TOML file
// (piforge.toml) with env-var overrides.
package config

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/BurntSushi/toml"
)

// Config is the top-level configuration object.
type Config struct {
	Model      ModelConfig      `toml:"model"`
	Server     ServerConfig     `toml:"server"`
	Agent      AgentConfig      `toml:"agent"`
	Hardware   HardwareConfig   `toml:"hardware"`
	Safety     SafetyConfig     `toml:"safety"`
	Eval       EvalConfig       `toml:"eval"`
}

// ModelConfig selects the model and quantization.
type ModelConfig struct {
	// Path to the GGUF model file on disk (used only to launch a server in
	// "managed" mode; in "external" mode the server is started by the user).
	Path string `toml:"path"`
	// Quant hint for documentation/cache-keying (e.g. "Q4_K_M").
	Quant string `toml:"quant"`
	// Context window in tokens. On an 8GB Pi 5 with Q4 weights, 8192 is a
	// realistic ceiling once OS + model + KV cache are accounted for.
	Context int `toml:"context"`
	// ThinkingMode: "off" (default; non-thinking model) or "on".
	// Note: the Qwen3-4B-Thinking-2507 variant is thinking-only and ignores
	// "off". Use Qwen3-4B-Instruct-2507 for the interactive non-thinking path.
	ThinkingMode string `toml:"thinking_mode"`
}

// ServerConfig configures the local llama-server client.
type ServerConfig struct {
	// BaseURL of the llama-server OpenAI-compat endpoint, e.g. http://127.0.0.1:8080/v1.
	BaseURL string `toml:"base_url"`
	// APIKey is unused by llama-server but required by the OpenAI client shape.
	// Defaults to a dummy value.
	APIKey string `toml:"api_key"`
	// MaxTokens caps generation. Set conservatively on a slow Pi.
	MaxTokens int `toml:"max_tokens"`
	// Temperature for sampling.
	Temperature float32 `toml:"temperature"`
}

// AgentConfig configures the agent loop.
type AgentConfig struct {
	// MaxTurns caps a single task's tool-use loop.
	MaxTurns int `toml:"max_turns"`
	// TelemetryPreload: if true, capture a telemetry snapshot (vcgencmd, dmesg,
	// i2cdetect, pin state) and inject into the first turn. Reduces round-trips.
	TelemetryPreload bool `toml:"telemetry_preload"`
}

// HardwareConfig is the auto-generated board tier of the hardware profile.
// The human-declared electrical tier lives per-pin in Profile.Pins.
type HardwareConfig struct {
	// Board model, e.g. "Raspberry Pi 5 Model B Rev 1.0".
	Board string `toml:"board"`
	// GPIOChipName for the 40-pin header on Pi 5, e.g. "gpiochip4".
	// Auto-discovered at runtime if empty.
	GPIOChip string `toml:"gpiochip"`
	// I2CBus device path, e.g. "/dev/i2c-1".
	I2CBus string `toml:"i2c_bus"`
}

// SafetyConfig is the policy layer enforced by the broker.
type SafetyConfig struct {
	// Arming mode: "confirm" (default; per-Class-I action human confirm),
	// "auto" (DANGEROUS; auto-approve all — for eval/batch only).
	ArmMode string `toml:"arm_mode"`
	// PerPinMaxCurrentMA: RP1 max drive is 12mA. Do not set above 12.
	PerPinMaxCurrentMA int `toml:"per_pin_max_current_ma"`
	// RailBudgetMA: conservative self-imposed 3.3V rail guideline.
	// Not an RP1 spec (none published); treat as a deliberate safety margin.
	RailBudgetMA int `toml:"rail_budget_ma"`
	// StopOnUnderVoltage: if vcgencmd get_throttled bit0/bit16 set, refuse
	// Class I ops until cleared.
	StopOnUnderVoltage bool `toml:"stop_on_under_voltage"`
}

// EvalConfig configures the capability eval harness.
type EvalConfig struct {
	// CasesDir holds the ~40 case fixtures.
	CasesDir string `toml:"cases_dir"`
	// Models to evaluate (paths/ids).
	Models []string `toml:"models"`
	// PassRateThreshold: fractional fix-rate to clear the gate (default 0.55).
	PassRateThreshold float64 `toml:"pass_rate_threshold"`
	// HallucinationThreshold: max fractional register/pin hallucination
	// (default 0.10).
	HallucinationThreshold float64 `toml:"hallucination_threshold"`
}

// Load reads piforge.toml from path, then applies env-var overrides.
// Missing fields are filled with defaults via WithDefaults.
func Load(path string) (*Config, error) {
	c := Default()
	if path == "" {
		return c, nil
	}
	if _, err := toml.DecodeFile(path, c); err != nil {
		return nil, fmt.Errorf("decode %s: %w", path, err)
	}
	c.applyEnv()
	return c, nil
}

// Default returns a sensible-default config for a Pi 5 8GB with llama-server
// on localhost:8080 and Qwen3-4B-Instruct-2507 at Q4_K_M.
func Default() *Config {
	return &Config{
		Model: ModelConfig{
			Path:         "",
			Quant:        "Q4_K_M",
			Context:      8192,
			ThinkingMode: "off",
		},
		Server: ServerConfig{
			BaseURL:     "http://127.0.0.1:8080/v1",
			APIKey:      "dummy",
			MaxTokens:   1024,
			Temperature: 0.2,
		},
		Agent: AgentConfig{
			MaxTurns:         12,
			TelemetryPreload: true,
		},
		Hardware: HardwareConfig{
			Board:   "",
			GPIOChip: "",
			I2CBus:  "/dev/i2c-1",
		},
		Safety: SafetyConfig{
			ArmMode:             "confirm",
			PerPinMaxCurrentMA:  12, // RP1 register max; do not raise
			RailBudgetMA:        50, // conservative guideline, not an RP1 spec
			StopOnUnderVoltage:  true,
		},
		Eval: EvalConfig{
			CasesDir:               "eval/cases",
			PassRateThreshold:      0.55,
			HallucinationThreshold: 0.10,
		},
	}
}

// applyEnv overrides selected fields from the environment.
func (c *Config) applyEnv() {
	if v := os.Getenv("PIFORGE_BASE_URL"); v != "" {
		c.Server.BaseURL = v
	}
	if v := os.Getenv("PIFORGE_MODEL_PATH"); v != "" {
		c.Model.Path = v
	}
	if v := os.Getenv("PIFORGE_CONTEXT"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			c.Model.Context = n
		}
	}
	if v := os.Getenv("PIFORGE_MAX_TOKENS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			c.Server.MaxTokens = n
		}
	}
	if v := os.Getenv("PIFORGE_GPIO_CHIP"); v != "" {
		c.Hardware.GPIOChip = v
	}
	if v := os.Getenv("PIFORGE_I2C_BUS"); v != "" {
		c.Hardware.I2CBus = v
	}
	if v := os.Getenv("PIFORGE_ARM_MODE"); v != "" {
		if v == "auto" || v == "confirm" {
			c.Safety.ArmMode = v
		}
	}
}

// Validate checks the config for obvious errors.
func (c *Config) Validate() error {
	if c.Server.BaseURL == "" {
		return fmt.Errorf("server.base_url must be set")
	}
	if !strings.HasPrefix(c.Server.BaseURL, "http") {
		return fmt.Errorf("server.base_url must be an http(s) URL")
	}
	if c.Model.Context < 512 {
		return fmt.Errorf("model.context %d too small (min 512)", c.Model.Context)
	}
	if c.Safety.PerPinMaxCurrentMA > 12 {
		return fmt.Errorf("safety.per_pin_max_current_ma %d exceeds RP1 max of 12mA",
			c.Safety.PerPinMaxCurrentMA)
	}
	if c.Safety.ArmMode == "auto" && os.Getenv("PIFORGE_ALLOW_AUTO_ARM") != "1" {
		return fmt.Errorf("safety.arm_mode=auto requires PIFORGE_ALLOW_AUTO_ARM=1 (DANGEROUS: auto-approves all hardware actions)")
	}
	return nil
}
