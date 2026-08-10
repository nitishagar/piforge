package broker

import (
	"testing"

	"github.com/nitishagar/piforge/internal/config"
)

// stubThrottled lets a test control the under-voltage state returned by the gate.
type stubThrottled struct{ now, since bool }

func (s stubThrottled) UnderVoltageActive() (bool, bool, error) { return s.now, s.since, nil }

// TestUnderVoltageBlocksEvenInAutoMode is the regression test for the red-team
// finding: the under-voltage STOP signal must fire even when ArmMode=="auto".
// Previously the auto short-circuit returned before the under-voltage check.
func TestUnderVoltageBlocksEvenInAutoMode(t *testing.T) {
	g := New(config.SafetyConfig{
		ArmMode:            "auto",
		StopOnUnderVoltage: true,
	}, stubThrottled{now: true, since: false}, nil)
	if err := g.Allow("gpio_set", map[string]any{"pin": 17, "value": 1}); err == nil {
		t.Fatal("auto-arm must NOT bypass the under-voltage STOP signal")
	}
	// since-boot only should also block.
	g = New(config.SafetyConfig{ArmMode: "auto", StopOnUnderVoltage: true},
		stubThrottled{now: false, since: true}, nil)
	if err := g.Allow("gpio_set", map[string]any{"pin": 17}); err == nil {
		t.Fatal("under-voltage since-boot must block Class I ops")
	}
}

// TestAutoArmAllowsWhenVoltageOK: in auto mode with healthy power, ops are allowed.
func TestAutoArmAllowsWhenVoltageOK(t *testing.T) {
	g := New(config.SafetyConfig{ArmMode: "auto", StopOnUnderVoltage: true},
		stubThrottled{now: false, since: false}, nil)
	if err := g.Allow("gpio_set", map[string]any{"pin": 17, "value": 1}); err != nil {
		t.Fatalf("auto-arm with healthy power should allow: %v", err)
	}
}

// TestConfirmDeny: in confirm mode, a "no" from the human blocks the op.
func TestConfirmDeny(t *testing.T) {
	g := New(config.SafetyConfig{ArmMode: "confirm", StopOnUnderVoltage: true},
		stubThrottled{}, func(string) bool { return false })
	if err := g.Allow("gpio_set", map[string]any{"pin": 17, "value": 1}); err == nil {
		t.Fatal("denied confirm must block the op")
	}
}

// TestConfirmGrantsScopedArm: a "yes" grants a 30s scoped arm; a second call
// for the same pin is allowed without re-confirm; a different pin re-confirms.
func TestConfirmGrantsScopedArm(t *testing.T) {
	confirmed := &confirmCounter{allow: true}
	g := New(config.SafetyConfig{ArmMode: "confirm", StopOnUnderVoltage: true},
		stubThrottled{}, confirmed.confirm)
	if err := g.Allow("gpio_set", map[string]any{"pin": 17, "value": 1}); err != nil {
		t.Fatalf("first confirm should allow: %v", err)
	}
	if confirmed.calls != 1 {
		t.Fatalf("expected 1 confirm call, got %d", confirmed.calls)
	}
	// Same pin within the window => no re-confirm.
	if err := g.Allow("gpio_set", map[string]any{"pin": 17, "value": 0}); err != nil {
		t.Fatalf("scoped arm should allow repeat: %v", err)
	}
	if confirmed.calls != 1 {
		t.Fatalf("scoped arm should not re-confirm; got %d calls", confirmed.calls)
	}
	// Different pin => re-confirm required.
	if err := g.Allow("gpio_set", map[string]any{"pin": 18, "value": 1}); err != nil {
		t.Fatalf("different pin should allow after re-confirm: %v", err)
	}
	if confirmed.calls != 2 {
		t.Fatalf("different pin should re-confirm; got %d calls", confirmed.calls)
	}
}

// TestRiskTier covers the tier labels used in the human prompt.
func TestRiskTier(t *testing.T) {
	cases := map[string]string{
		"gpio_set":    "I/level1",
		"pwm_start":   "I/level2-mechanical",
		"motor_drive": "I/level2-mechanical",
		"i2c_write":   "I/peripheral",
		"flash_write": "I/irreversible",
		"unknown":     "I",
	}
	for op, want := range cases {
		if got := riskTier(op, nil); got != want {
			t.Errorf("riskTier(%q) = %q, want %q", op, got, want)
		}
	}
}

type confirmCounter struct {
	calls int
	allow bool
}

func (c *confirmCounter) confirm(string) bool {
	c.calls++
	return c.allow
}
