// Package eval is the capability eval — the gate that must pass before the
// local-appliance product is built. Per the design doc:
//
//   - ~40 cases drawn from recurring bug archetypes (wrong I2C address,
//     missing overlays, unit conversion, GPIO numbering, PWM backend, ...).
//   - Run each candidate model zero-shot and with typed HIL tools.
//   - Decision rule: >=55% fix-rate AND <10% register/pin hallucination
//     => build local. ~30% => ship against a cloud model instead.
//
// This package defines the case format, the runner, and the scorer. The actual
// ~40 fixtures live under eval/cases/ as JSON (added incrementally).
package eval

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/nitishagar/piforge/internal/agent"
	"github.com/nitishagar/piforge/internal/hil"
	"github.com/nitishagar/piforge/internal/provider"
)

// Case is a single eval fixture.
type Case struct {
	ID          string   `json:"id"`
	Archetype   string   `json:"archetype"`    // e.g. "wrong-i2c-address"
	Symptom     string   `json:"symptom"`      // user prompt (the symptom, not the answer)
	Setup       Setup    `json:"setup"`        // initial board state (simulated or real)
	Gold        Gold     `json:"gold"`         // the correct fix + diagnosis
	Hallucinated []string `json:"hallucinated"` // patterns that count as hallucination (e.g. ["0x4a","0x4b"] for a wrong addr)
}

// Setup describes the initial hardware/code state. For the MVP these are
// descriptions the harness loads into a simulated provider (see Simulator);
// a later phase wires real Pi fixtures.
type Setup struct {
	Board        string            `json:"board"`
	I2CDevices   map[int]string    `json:"i2c_devices"`   // addr -> chip id
	GPIOPins     map[int]string    `json:"gpio_pins"`     // pin -> mode/value
	Files        map[string]string `json:"files"`         // path -> broken code content
	DmesgTail    []string          `json:"dmesg_tail"`
	Throttled    string            `json:"throttled"`     // e.g. "0x0"
}

// Gold is the correct answer for scoring.
type Gold struct {
	Diagnosis       string   `json:"diagnosis"`
	FixApplies      string   `json:"fix_applies"`      // path of the file the fix touches
	FixMustContain  []string `json:"fix_must_contain"` // substrings the corrected code must contain
	FixMustNotHave  []string `json:"fix_must_not_have"`// substrings the corrected code must NOT contain
	IsHardwareFault bool     `json:"is_hardware_fault"`// true => correct answer is STOP/triage, not edit
}

// Verdict is the per-case score.
type Verdict struct {
	CaseID         string
	Pass           bool
	Partial        bool
	Hallucination  bool
	DurationSec    float64
	Turns          int
	CacheHitRate   float64
	Notes          string
}

// Runner runs a set of cases against a configured model+tools.
type Runner struct {
	client   *provider.Client
	tools    []hil.Tool
	maxTurns int
}

// NewRunner builds a Runner.
func NewRunner(client *provider.Client, tools []hil.Tool, maxTurns int) *Runner {
	return &Runner{client: client, tools: tools, maxTurns: maxTurns}
}

// RunAll runs all *.json cases in a directory and returns per-case verdicts +
// aggregate metrics.
func (r *Runner) RunAll(ctx context.Context, casesDir string, onProgress func(Verdict)) ([]Verdict, error) {
	entries, err := os.ReadDir(casesDir)
	if err != nil {
		return nil, fmt.Errorf("read cases dir: %w", err)
	}
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

// runFile loads + runs a single case file.
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

// runCase runs one case against the configured model+tools and scores it.
func (r *Runner) runCase(ctx context.Context, c Case) (Verdict, error) {
	v := Verdict{CaseID: c.ID}
	start := time.Now()

	// TODO(phase-1): wire the simulated provider that serves c.Setup to the
	// HIL tools (so i2c reads return the fixture's device values, etc.).
	// For now the runner exercises the live loop against the real provider;
	// scoring uses Gold.FixMustContain on the edited file content.
	a := agent.New(r.client, r.tools, r.maxTurns)
	res, err := a.Run(ctx, c.Symptom, nil)
	v.DurationSec = time.Since(start).Seconds()
	if err != nil {
		v.Notes = "error: " + err.Error()
		return v, nil
	}
	v.Turns = res.Turns
	v.CacheHitRate = res.CacheHitRate()
	v.Notes = truncate(res.FinalText, 200)

	// Score: did the fix land the expected content?
	if c.Gold.FixApplies != "" && len(c.Gold.FixMustContain) > 0 {
		got, rerr := os.ReadFile(c.Gold.FixApplies)
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
	if c.Gold.IsHardwareFault {
		// Correct answer is to STOP/triage — partial credit for a correct
		// diagnosis without an edit.
		for _, kw := range []string{"hardware", "wiring", "power", "brownout", "under-voltage", "short", "stop"} {
			if strings.Contains(strings.ToLower(res.FinalText), kw) {
				v.Partial = true
				break
			}
		}
	}

	// Hallucination detection: scan final text for known-wrong patterns.
	lower := strings.ToLower(res.FinalText)
	for _, h := range c.Hallucinated {
		if strings.Contains(lower, strings.ToLower(h)) {
			v.Hallucination = true
			break
		}
	}
	return v, nil
}

// Summary aggregates per-case verdicts into pass/hallucination rates.
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
//   passRate >= threshold AND halluc < threshold => "build local"
//   passRate < ~0.30 => "pivot to cloud model"
func Decide(s Summary, passThreshold, hallucThreshold float64) string {
	if s.PassRate >= passThreshold && s.HallucinationRate <= hallucThreshold {
		return "BUILD_LOCAL"
	}
	if s.PassRate < 0.30 {
		return "PIVOT_TO_CLOUD"
	}
	return "INCONCLUSIVE — tune prompt/tools or add cases"
}

func median(xs []int) int {
	if len(xs) == 0 {
		return 0
	}
	// simple sort + pick middle (n small)
	for i := 0; i < len(xs); i++ {
		for j := i + 1; j < len(xs); j++ {
			if xs[j] < xs[i] {
				xs[i], xs[j] = xs[j], xs[i]
			}
		}
	}
	return xs[len(xs)/2]
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
