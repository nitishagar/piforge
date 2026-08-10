package eval

import "testing"

// TestDecideGate verifies the 55%/10% build/pivot/inconclusive decision rule.
func TestDecideGate(t *testing.T) {
	cases := []struct {
		name   string
		s      Summary
		pass   float64
		halluc float64
		want   string
	}{
		{"clear pass", Summary{PassRate: 0.70, HallucinationRate: 0.05}, 0.55, 0.10, "BUILD_LOCAL"},
		{"pass but too much halluc", Summary{PassRate: 0.70, HallucinationRate: 0.20}, 0.55, 0.10, "INCONCLUSIVE"},
		{"clear pivot", Summary{PassRate: 0.20, HallucinationRate: 0.05}, 0.55, 0.10, "PIVOT_TO_CLOUD"},
		{"borderline inconclusive", Summary{PassRate: 0.40, HallucinationRate: 0.05}, 0.55, 0.10, "INCONCLUSIVE"},
		{"exactly at threshold", Summary{PassRate: 0.55, HallucinationRate: 0.10}, 0.55, 0.10, "BUILD_LOCAL"},
	}
	for _, tc := range cases {
		if got := Decide(tc.s, tc.pass, tc.halluc); got != tc.want {
			t.Errorf("%s: Decide() = %q, want %q", tc.name, got, tc.want)
		}
	}
}

// TestContainsDiagnosis verifies the word-phrase (not bare-substring) matching
// the red-team found was overmatching. "default" must NOT trip "fault"; "powered"
// must NOT trip "power".
func TestContainsDiagnosis(t *testing.T) {
	pos := []string{
		"this is a hardware fault, stop coding",
		"under-voltage detected, get a better psu",
		"the wiring is wrong, rewire it",
		"this is a short circuit",
		"add a pull-up resistor",
	}
	for _, s := range pos {
		if !containsDiagnosis(s) {
			t.Errorf("containsDiagnosis(%q) = false, want true", s)
		}
	}
	neg := []string{
		"the default register value is 0x76",
		"the sensor is powered on and reading fine",
		"this is a shorter function than expected",
		"use a stopwatch to time it",
		"i will power through this bug",
	}
	for _, s := range neg {
		if containsDiagnosis(s) {
			t.Errorf("containsDiagnosis(%q) = true, want false (false positive)", s)
		}
	}
}

// TestSummarizeAggregates checks the aggregate metrics over a set of verdicts.
func TestSummarizeAggregates(t *testing.T) {
	vs := []Verdict{
		{Pass: true, Turns: 2, CacheHitRate: 0.8},
		{Pass: true, Turns: 4, CacheHitRate: 0.6},
		{Pass: false, Hallucination: true, Turns: 3, CacheHitRate: 0.5},
	}
	s := Summarize(vs)
	if s.Total != 3 || s.Passed != 2 || s.Hallucinated != 1 {
		t.Errorf("counts: total=%d passed=%d halluc=%d", s.Total, s.Passed, s.Hallucinated)
	}
	if s.PassRate < 0.66 || s.PassRate > 0.67 {
		t.Errorf("PassRate = %.3f, want ~0.667", s.PassRate)
	}
	if s.HallucinationRate < 0.33 || s.HallucinationRate > 0.34 {
		t.Errorf("HallucinationRate = %.3f, want ~0.333", s.HallucinationRate)
	}
	// median of {2,3,4} = 3
	if s.MedianTurns != 3 {
		t.Errorf("MedianTurns = %d, want 3", s.MedianTurns)
	}
}
