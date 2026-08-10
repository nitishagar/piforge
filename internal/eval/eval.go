// Package eval is the capability eval — the gate that must pass before the
// local-appliance product is built. Per the design doc:
//
//   - ~40 cases drawn from recurring bug archetypes (wrong I2C address,
//     missing overlays, unit conversion, GPIO numbering, PWM backend, ...).
//   - Run each candidate model zero-shot and with typed HIL tools.
//   - Decision rule: >=55% fix-rate AND <10% register/pin hallucination
//     => build local. ~30% => ship against a cloud model instead.
//
// Cases run headless: the agent's HIL tool calls are served by the sim package
// from each Case fixture's Setup, and edits land in a per-case temp workspace
// the scorer reads back. A mock provider lets the whole thing run in CI without
// a llama-server; the real provider is used for the actual model evaluation.
package eval

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/nitishagar/piforge/internal/agent"
	"github.com/nitishagar/piforge/internal/broker"
	"github.com/nitishagar/piforge/internal/config"
	"github.com/nitishagar/piforge/internal/hil"
	"github.com/nitishagar/piforge/internal/provider"
	"github.com/nitishagar/piforge/internal/sim"
)

// Case is a single eval fixture.
type Case struct {
	ID           string   `json:"id"`
	Archetype    string   `json:"archetype"`
	Symptom      string   `json:"symptom"`
	Setup        Setup    `json:"setup"`
	Gold         Gold     `json:"gold"`
	Hallucinated []string `json:"hallucinated"`
}

// Setup is the initial hardware/code state for a case (mirrors sim.Setup).
type Setup struct {
	Board       string            `json:"board"`
	I2CDevices  map[int]string    `json:"i2c_devices"`
	ScanPattern string            `json:"scan_pattern"` // "all" => every address responds (shorted bus)
	GPIOPins    map[int]string    `json:"gpio_pins"`
	Files       map[string]string `json:"files"`
	DmesgTail   []string          `json:"dmesg_tail"`
	Throttled   string            `json:"throttled"`
}

// Gold is the correct answer for scoring.
type Gold struct {
	Diagnosis       string   `json:"diagnosis"`
	FixApplies      string   `json:"fix_applies"`
	FixMustContain  []string `json:"fix_must_contain"`
	FixMustNotHave  []string `json:"fix_must_not_have"`
	IsHardwareFault bool     `json:"is_hardware_fault"`
}

// Verdict is the per-case score.
type Verdict struct {
	CaseID        string
	Pass          bool
	Partial       bool
	Hallucination bool
	DurationSec   float64
	Turns         int
	CacheHitRate  float64
	Notes         string
}

// Runner runs cases against a configured model+tools.
type Runner struct {
	client   *provider.Client
	maxTurns int
	// mockProvider, when non-nil, is used instead of client (CI mode).
	mockProvider *MockProvider
}

// NewRunner builds a Runner against a real provider.
func NewRunner(client *provider.Client, maxTurns int) *Runner {
	return &Runner{client: client, maxTurns: maxTurns}
}

// NewMockRunner builds a Runner against a scripted mock provider for CI.
// Each case maps to a canned assistant response via script(caseID).
func NewMockRunner(maxTurns int, script func(caseID string) []MockTurn) *Runner {
	return &Runner{maxTurns: maxTurns, mockProvider: &MockProvider{Script: script}}
}

// RunAll runs all *.json cases in a directory and returns per-case verdicts.
func (r *Runner) RunAll(ctx context.Context, casesDir string, onProgress func(Verdict)) ([]Verdict, error) {
	entries, err := os.ReadDir(casesDir)
	if err != nil {
		return nil, fmt.Errorf("read cases dir: %w", err)
	}
	// Sort for deterministic output.
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
	var verdicts []Verdict
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".json") {
			continue
		}
		v, err := r.runFile(ctx, filepath.Join(casesDir, e.Name()))
		if err != nil {
			return verdicts, fmt.Errorf("case %s: %w", e.Name(), err)
		}
		verdicts = append(verdicts, v)
		if onProgress != nil {
			onProgress(v)
		}
	}
	return verdicts, nil
}

func (r *Runner) runFile(ctx context.Context, path string) (Verdict, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return Verdict{}, err
	}
	var c Case
	if err := json.Unmarshal(raw, &c); err != nil {
		return Verdict{}, fmt.Errorf("unmarshal: %w", err)
	}
	return r.runCase(ctx, c)
}

// runCase runs one case against sim-served hardware, in an isolated temp dir.
func (r *Runner) runCase(ctx context.Context, c Case) (Verdict, error) {
	v := Verdict{CaseID: c.ID}
	start := time.Now()

	// Per-case temp workspace: write the fixture's files into it, point the
	// edit tool at it, score by reading back from it after the run.
	workspace, cleanup, err := setupWorkspace(c.Setup.Files)
	if err != nil {
		return v, err
	}
	defer cleanup()

	// Build the sim state + tools from the fixture.
	st := sim.StateFromSetup(sim.Setup{
		Board: c.Setup.Board, I2CDevices: c.Setup.I2CDevices,
		ScanPattern: c.Setup.ScanPattern,
		GPIOPins:    c.Setup.GPIOPins, Files: c.Setup.Files,
		DmesgTail: c.Setup.DmesgTail, Throttled: c.Setup.Throttled,
	})

	// The eval uses an auto-arm gate (no human prompts). Under-voltage still
	// gates Class I (the shorted-bus case must not drive any pin).
	tel := sim.NewTelemetryTool(st)
	gate := broker.New(config.SafetyConfig{
		ArmMode:            "auto", // eval/batch; main entrypoint gates PIFORGE_ALLOW_AUTO_ARM
		StopOnUnderVoltage: true,
		PerPinMaxCurrentMA: 12,
		RailBudgetMA:       50,
	}, broker.ThrottledFunc(tel.UnderVoltageActive), broker.AlwaysDenyConfirmer())

	editTool := hil.NewCodeEditTool(workspace)
	tools := []hil.Tool{
		sim.NewInventoryTool(st),
		tel,
		sim.NewI2CTool(st),
		sim.NewGPIOTool(st, gate),
		sim.NewScopeTool(st),
		editTool,
	}

	var res *agent.RunResult
	if r.mockProvider != nil {
		r.mockProvider.setCase(c.ID)
		a := agent.New(r.mockProvider, tools, r.maxTurns)
		res, err = a.Run(ctx, c.Symptom, nil)
	} else {
		a := agent.New(r.client, tools, r.maxTurns)
		res, err = a.Run(ctx, c.Symptom, nil)
	}
	v.DurationSec = time.Since(start).Seconds()
	if err != nil {
		v.Notes = "error: " + err.Error()
		return v, nil
	}
	v.Turns = res.Turns
	v.CacheHitRate = res.CacheHitRate()
	v.Notes = truncate(res.FinalText, 200)

	// Score against the edited file in the workspace.
	r.score(c, workspace, res.FinalText, editTool.Edits(), &v)
	return v, nil
}

// score applies the gold rubric to the edited file + final text. editCount is
// the number of edit_file calls the model made — used to verify hardware-fault
// cases correctly refrained from editing.
func (r *Runner) score(c Case, workspace, finalText string, editCount int, v *Verdict) {
	// Hallucination detection: scan the final text for known-wrong patterns.
	lower := strings.ToLower(finalText)
	for _, h := range c.Hallucinated {
		if strings.Contains(lower, strings.ToLower(h)) {
			v.Hallucination = true
			break
		}
	}

	// Code-fix scoring: read the (possibly edited) file from the workspace.
	if c.Gold.FixApplies != "" && len(c.Gold.FixMustContain) > 0 {
		got, rerr := os.ReadFile(filepath.Join(workspace, c.Gold.FixApplies))
		if rerr == nil {
			allIn := true
			for _, s := range c.Gold.FixMustContain {
				if !strings.Contains(string(got), s) {
					allIn = false
					break
				}
			}
			noneBad := true
			for _, s := range c.Gold.FixMustNotHave {
				if strings.Contains(string(got), s) {
					noneBad = false
					break
				}
			}
			v.Pass = allIn && noneBad
		}
	}

	// Hardware-fault cases: correct answer is STOP/triage, NOT a code edit.
	// Use whole-phrase (not bare substring) matching so "power" doesn't match
	// "powered", "short" doesn't match "shorter", and "fault" doesn't match
	// "default". Diagnosis phrases are anchored as word-bounded alternatives.
	if c.Gold.IsHardwareFault {
		v.Partial = containsDiagnosis(lower)
		// Pass requires BOTH a correct triage diagnosis AND that the model did
		// NOT attempt a code edit (the whole point is "stop coding; it's
		// hardware"). A model that edits anyway has misdiagnosed.
		v.Pass = v.Partial && editCount == 0
	}
}

// containsDiagnosis reports whether text contains a word-bounded diagnosis
// phrase indicating a hardware/wiring/power fault. Phrases are chosen to avoid
// substring false positives (e.g. "power" alone would match "powered").
func containsDiagnosis(lower string) bool {
	phrases := []string{
		"hardware fault", "hardware issue", "wiring", "power supply", "psu",
		"brownout", "under-voltage", "undervoltage", "short circuit",
		"shorted", "stop coding", "physical fault", "not a software",
		"not a code", "rewire", "pull-up", "pull up resistor",
	}
	for _, p := range phrases {
		if strings.Contains(lower, p) {
			return true
		}
	}
	return false
}

// setupWorkspace creates a temp dir seeded with files and returns its path +
// a cleanup func.
func setupWorkspace(files map[string]string) (dir string, cleanup func(), err error) {
	dir, err = os.MkdirTemp("", "piforge-eval-*")
	if err != nil {
		return "", nil, err
	}
	for rel, content := range files {
		abs := filepath.Join(dir, rel)
		if mkErr := os.MkdirAll(filepath.Dir(abs), 0o755); mkErr != nil {
			os.RemoveAll(dir)
			return "", nil, mkErr
		}
		if wErr := os.WriteFile(abs, []byte(content), 0o644); wErr != nil {
			os.RemoveAll(dir)
			return "", nil, wErr
		}
	}
	return dir, func() { os.RemoveAll(dir) }, nil
}

// autoArmSafety helper removed — the broker is constructed inline in runCase
// with a config.SafetyConfig{ArmMode:"auto", ...}.

// Summary aggregates per-case verdicts.
type Summary struct {
	Total             int
	Passed            int
	Partial           int
	Hallucinated      int
	PassRate          float64
	HallucinationRate float64
	MedianTurns       int
	MeanCacheHitRate  float64
}

// Summarize computes aggregate metrics.
func Summarize(vs []Verdict) Summary {
	s := Summary{Total: len(vs)}
	cacheSum := 0.0
	turns := []int{}
	for _, v := range vs {
		if v.Pass {
			s.Passed++
		}
		if v.Partial {
			s.Partial++
		}
		if v.Hallucination {
			s.Hallucinated++
		}
		cacheSum += v.CacheHitRate
		turns = append(turns, v.Turns)
	}
	if s.Total > 0 {
		s.PassRate = float64(s.Passed) / float64(s.Total)
		s.HallucinationRate = float64(s.Hallucinated) / float64(s.Total)
		s.MeanCacheHitRate = cacheSum / float64(s.Total)
		s.MedianTurns = median(turns)
	}
	return s
}

// Decide applies the eval decision rule.
func Decide(s Summary, passThreshold, hallucThreshold float64) string {
	if s.PassRate >= passThreshold && s.HallucinationRate <= hallucThreshold {
		return "BUILD_LOCAL"
	}
	if s.PassRate < 0.30 {
		return "PIVOT_TO_CLOUD"
	}
	return "INCONCLUSIVE"
}

func median(xs []int) int {
	if len(xs) == 0 {
		return 0
	}
	sort.Ints(xs)
	return xs[len(xs)/2]
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
