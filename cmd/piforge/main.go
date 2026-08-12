package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/nitishagar/piforge/internal/agent"
	"github.com/nitishagar/piforge/internal/broker"
	"github.com/nitishagar/piforge/internal/config"
	"github.com/nitishagar/piforge/internal/hil"
	"github.com/nitishagar/piforge/internal/provider"
)

func main() {
	cfgPath := flag.String("config", "piforge.toml", "path to piforge.toml (or 'none' for defaults)")
	task := flag.String("task", "", "single task to run (non-interactive)")
	flag.Parse()

	cfg, err := config.Load(*cfgPath)
	if err != nil {
		die(err)
	}
	if err := cfg.Validate(); err != nil {
		die(err)
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	client := provider.New(cfg.Server)
	if err := client.HealthCheck(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		fmt.Fprintf(os.Stderr, "start llama-server first, e.g.:\n  llama-server -m %s --jinja --port 8080 -c %d -ctk q8_0 -ctv q8_0\n",
			"<qwen3-4b-instruct-2507-q4_k_m.gguf>", cfg.Model.Context)
		os.Exit(1)
	}

	// Build the safety gate (broker). The telemetry tool backs the under-voltage
	// stop check (it implements broker.ThrottledReader); the confirmer reads stdin.
	tel := hil.NewTelemetryTool()
	gate := broker.New(cfg.Safety, tel, broker.StdinConfirmer(ctx))

	gpioChip, err := hil.ResolveChip(cfg.Hardware.GPIOChip)
	if err != nil {
		fmt.Fprintf(os.Stderr, "warn: %v\n", err)
		gpioChip = "gpiochip4"
	}

	tools := []hil.Tool{
		hil.NewInventoryTool(),
		tel,
		hil.NewI2CTool(cfg.Hardware.I2CBus, gate),
		hil.NewGPIOTool(gpioChip, gate),
		hil.NewScopeTool(gpioChip),
		hil.NewCodeEditTool("."),
	}

	ag := agent.New(client, tools, cfg.Agent.MaxTurns)

	taskText := *task
	if taskText == "" {
		fmt.Fprintln(os.Stderr, "no --task given; interactive mode not yet implemented. Use --task \"<symptom>\"")
		os.Exit(2)
	}

	res, err := ag.Run(ctx, taskText, func(s string) {
		fmt.Fprintln(os.Stdout, s)
	})
	if err != nil {
		die(err)
	}

	fmt.Fprintf(os.Stderr, "\n[turns=%d cache_hit=%.0f%% prompt_tok=%d completion_tok=%d]\n",
		res.Turns, res.CacheHitRate()*100, res.PromptTokens, res.Completion)
}

func die(err error) {
	fmt.Fprintf(os.Stderr, "piforge: %v\n", err)
	os.Exit(1)
}
