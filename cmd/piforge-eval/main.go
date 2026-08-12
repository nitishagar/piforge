package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/sashabaranov/go-openai"

	"github.com/nitishagar/piforge/internal/config"
	"github.com/nitishagar/piforge/internal/eval"
	"github.com/nitishagar/piforge/internal/provider"
)

func main() {
	cfgPath := flag.String("config", "piforge.toml", "path to piforge.toml")
	casesDir := flag.String("cases", "", "override eval.cases_dir")
	mock := flag.Bool("mock", false, "use scripted mock provider (CI; no llama-server)")
	flag.Parse()

	cfg, err := config.Load(*cfgPath)
	if err != nil {
		die(err)
	}
	if *casesDir != "" {
		cfg.Eval.CasesDir = *casesDir
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	var runner *eval.Runner
	if *mock {
		// Headless: scripted turns per case. This exercises the runner, the
		// sim-served tools, the broker gate, and the scorer — NOT the model.
		runner = eval.NewMockRunner(cfg.Agent.MaxTurns, mockScript)
		fmt.Fprintf(os.Stderr, "piforge-eval: MOCK mode (scripted provider, no model)\n")
	} else {
		client := provider.New(cfg.Server)
		if err := client.HealthCheck(ctx); err != nil {
			die(err)
		}
		runner = eval.NewRunner(client, cfg.Agent.MaxTurns)
	}

	fmt.Fprintf(os.Stderr, "piforge-eval: running cases from %s\n", cfg.Eval.CasesDir)
	verdicts, err := runner.RunAll(ctx, cfg.Eval.CasesDir, func(v eval.Verdict) {
		status := "FAIL"
		if v.Pass {
			status = "PASS"
		}
		extra := ""
		if v.Hallucination {
			extra = " [HALLUC]"
		}
		fmt.Fprintf(os.Stderr, "  %-32s %s%s  turns=%d  %s\n",
			v.CaseID, status, extra, v.Turns, oneLine(v.Notes))
	})
	if err != nil {
		die(err)
	}

	if len(verdicts) == 0 {
		fmt.Fprintln(os.Stderr, "no cases found — add fixtures under eval/cases/*.json")
		os.Exit(0)
	}

	s := eval.Summarize(verdicts)
	decision := eval.Decide(s, cfg.Eval.PassRateThreshold, cfg.Eval.HallucinationThreshold)
	fmt.Printf("\n=== Summary ===\n")
	fmt.Printf("cases=%d passed=%d partial=%d hallucinated=%d\n", s.Total, s.Passed, s.Partial, s.Hallucinated)
	fmt.Printf("pass_rate=%.0f%% halluc_rate=%.0f%% median_turns=%d mean_cache_hit=%.0f%%\n",
		s.PassRate*100, s.HallucinationRate*100, s.MedianTurns, s.MeanCacheHitRate*100)
	fmt.Printf("threshold: pass>=%.0f%% halluc<=%.0f%%\n",
		cfg.Eval.PassRateThreshold*100, cfg.Eval.HallucinationThreshold*100)
	fmt.Printf("DECISION: %s\n", decision)
}

func die(err error) {
	fmt.Fprintf(os.Stderr, "piforge-eval: %v\n", err)
	os.Exit(1)
}

func oneLine(s string) string {
	for i, r := range s {
		if r == '\n' {
			return s[:i] + "..."
		}
	}
	return s
}

// mockScript returns canned assistant turns for the two seed cases so the
// mock eval produces a deterministic PASS/PASS result in CI. For any case
// without a script, it returns a single empty-text terminal turn (PASS=false).
func mockScript(caseID string) []eval.MockTurn {
	switch caseID {
	case "wrong-i2c-address-bme280-0x76":
		// Turn 0: scan; Turn 1: detect at 0x76; Turn 2: edit the file; Turn 3: terminal.
		return []eval.MockTurn{
			{ToolCalls: []openai.ToolCall{call("i2c", `{"action":"scan"}`)}, PromptTokens: 120, CachedTokens: 0},
			{ToolCalls: []openai.ToolCall{call("i2c", `{"action":"detect","address":118}`)}, PromptTokens: 200, CachedTokens: 180},
			{ToolCalls: []openai.ToolCall{call("edit_file", `{"path":"bme_simpletest.py","content":"from board import *\nfrom adafruit_bme280 import basic as adafruit_bme280\ni2c = I2C(scl, sda)\nbme = adafruit_bme280.Adafruit_BME280_I2C(i2c, address=0x76)\nprint(bme.humidity)\n"}`)}, PromptTokens: 280, CachedTokens: 260},
			{Text: "Fixed: BME280 is at 0x76, not the default 0x77. Set address=0x76 in the constructor.", PromptTokens: 320, CachedTokens: 300},
		}
	case "i2c-bus-scan-all-addresses":
		return []eval.MockTurn{
			{ToolCalls: []openai.ToolCall{call("i2c", `{"action":"scan"}`)}, PromptTokens: 120, CachedTokens: 100},
			{Text: "i2cdetect shows every address responding — that means SDA/SCL are shorted to power. This is a hardware/wiring fault. STOP coding; check the wiring and pull-ups before any further I2C op.", PromptTokens: 200, CachedTokens: 180},
		}
	case "undervoltage-brownout":
		return []eval.MockTurn{
			{ToolCalls: []openai.ToolCall{call("telemetry", `{"action":"snapshot"}`)}, PromptTokens: 130, CachedTokens: 110},
			{Text: "vcgencmd get_throttled shows undervoltage has occurred (bit 16). This is a power-supply problem, not code — use a 5V/3A+ PSU and don't power servos from the 3.3V rail. STOP coding.", PromptTokens: 210, CachedTokens: 190},
		}
	}
	return []eval.MockTurn{{Text: "", PromptTokens: 100}}
}

// call builds an OpenAI tool_call shape the mock provider emits.
func call(name, argsJSON string) openai.ToolCall {
	return openai.ToolCall{
		ID:       name + "-1",
		Type:     openai.ToolTypeFunction,
		Function: openai.FunctionCall{Name: name, Arguments: argsJSON},
	}
}

// keep the json import used when openai shapes need raw args (currently the
// scripts above use string literals; this guards future extension).
var _ = json.RawMessage(nil)
