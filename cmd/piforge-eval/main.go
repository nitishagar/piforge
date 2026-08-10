// Command piforge-eval runs the capability eval gate: loads cases, runs each
// configured model + tools, scores fix-rate + hallucination-rate, and prints
// the build/pivot decision.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/nitishagar/piforge/internal/broker"
	"github.com/nitishagar/piforge/internal/config"
	"github.com/nitishagar/piforge/internal/eval"
	"github.com/nitishagar/piforge/internal/hil"
	"github.com/nitishagar/piforge/internal/provider"
)

func main() {
	cfgPath := flag.String("config", "piforge.toml", "path to piforge.toml")
	casesDir := flag.String("cases", "", "override eval.cases_dir")
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

	client := provider.New(cfg.Server)
	if err := client.HealthCheck(ctx); err != nil {
		die(err)
	}

	// For the eval, wire an auto-arm gate (eval/batch only). The main
	// config.Validate gates PIFORGE_ALLOW_AUTO_ARM=1 for arm_mode=auto.
	evalCfg := cfg.Safety
	evalCfg.ArmMode = "auto"
	gate := broker.New(evalCfg, nil, broker.AlwaysDenyConfirmer())
	gpioChip, _ := hil.ResolveChip(cfg.Hardware.GPIOChip)

	tools := []hil.Tool{
		hil.NewInventoryTool(),
		hil.NewTelemetryTool(),
		hil.NewI2CTool(cfg.Hardware.I2CBus, gate),
		hil.NewGPIOTool(gpioChip, gate),
		hil.NewScopeTool(gpioChip),
		hil.NewCodeEditTool("."),
	}

	runner := eval.NewRunner(client, tools, cfg.Agent.MaxTurns)

	fmt.Fprintf(os.Stderr, "piforge-eval: running cases from %s\n", cfg.Eval.CasesDir)
	verdicts, err := runner.RunAll(ctx, cfg.Eval.CasesDir, func(v eval.Verdict) {
		status := "FAIL"
		if v.Pass {
			status = "PASS"
		}
		fmt.Fprintf(os.Stderr, "  %-20s %s  turns=%d cache=%.0f%%  %s\n",
			v.CaseID, status, v.Turns, v.CacheHitRate*100, oneLine(v.Notes))
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
