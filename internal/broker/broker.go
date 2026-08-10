// Package broker is the safety gate between the agent and physical hardware.
// It implements the operation classification (R/B/I), risk-tiered arming, and
// the under-voltage stop signal.
//
// Design (from the red-teamed spec):
//   - Class R (read): auto-approve — no gate.
//   - Class B (bounded): log + allow.
//   - Class I (physical/irreversible): per-action human confirm, risk-tiered,
//     scoped. The agent can REQUEST an arm; only a human can GRANT it.
//   - Under-voltage (vcgencmd get_throttled bit0/bit16): refuse all Class I.
//
// This is the ConfirmGate implementation. A future AutoGate (eval/batch only)
// auto-approves, gated behind PIFORGE_ALLOW_AUTO_ARM=1.
package broker

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/nitishagar/piforge/internal/config"
)

// Gate is the interface tools consult before Class I operations.
type Gate interface {
	Allow(physical string, detail map[string]any) error
}

// ConfirmGate asks a human before each Class I op, risk-tiered, scoped.
type ConfirmGate struct {
	cfg     config.SafetyConfig
	tel     ThrottledReader // for the under-voltage stop check
	confirm Confirmer       // how to ask the human
	mu      sync.Mutex
	// autoArmed tracks per-(physical,pin) arms with an expiry, the way sudo
	// time-limits credentials. Never a global permanent toggle.
	armed map[string]time.Time
}

// ThrottledReader reads the under-voltage state. Implemented by the telemetry
// tool; decoupled here so the broker doesn't import hil.
type ThrottledReader interface {
	UnderVoltageActive() (now, sinceBoot bool, err error)
}

// Confirmer asks the human a yes/no question. Returns true if approved.
type Confirmer func(prompt string) bool

// New returns a ConfirmGate. If cfg.ArmMode == "auto", the gate auto-approves
// (requires PIFORGE_ALLOW_AUTO_ARM=1, enforced by config.Validate).
func New(cfg config.SafetyConfig, tel ThrottledReader, confirm Confirmer) *ConfirmGate {
	return &ConfirmGate{
		cfg: cfg, tel: tel, confirm: confirm, armed: map[string]time.Time{},
	}
}

// Allow implements Gate. It is consulted before any Class I op.
func (g *ConfirmGate) Allow(physical string, detail map[string]any) error {
	if g.cfg.ArmMode == "auto" {
		return nil // eval/batch only; config.Validate gates this
	}
	g.mu.Lock()
	defer g.mu.Unlock()

	// 1. Under-voltage stop signal.
	if g.cfg.StopOnUnderVoltage && g.tel != nil {
		now, since, err := g.tel.UnderVoltageActive()
		if err == nil && (now || since) {
			return fmt.Errorf("REFUSED: under-voltage active (now=%v since_boot=%v) — adding load risks brownout/SD corruption; upgrade PSU and clear before retrying", now, since)
		}
	}

	// 2. Per-(physical,key) scoped arm still valid?
	key := armKey(physical, detail)
	if exp, ok := g.armed[key]; ok && time.Now().Before(exp) {
		return nil // previously armed within the window
	}

	// 3. Risk-tier the op for the prompt.
	risk := riskTier(physical, detail)
	prompt := formatPrompt(physical, detail, risk)

	// 4. Ask the human.
	if !g.confirm(prompt) {
		return fmt.Errorf("DENIED by user (op=%s risk=%s)", physical, risk)
	}

	// 5. Grant a scoped, time-limited arm (30s window) — re-approval-free
	// within the window for the same op+pin, but never a global toggle.
	g.armed[key] = time.Now().Add(30 * time.Second)
	return nil
}

// armKey groups repeat approvals: same physical action on the same pin/key.
func armKey(physical string, detail map[string]any) string {
	k := physical
	if pin, ok := detail["pin"]; ok {
		k = fmt.Sprintf("%s:pin=%v", physical, pin)
	} else if addr, ok := detail["address"]; ok {
		k = fmt.Sprintf("%s:addr=%v", physical, addr)
	}
	return k
}

// riskTier labels the prompt so the human sees what kind of op they're approving.
func riskTier(physical string, detail map[string]any) string {
	switch physical {
	case "gpio_set":
		return "I/level1"
	case "pwm_start", "motor_drive", "relay_drive":
		return "I/level2-mechanical"
	case "i2c_write":
		return "I/peripheral"
	case "flash_write", "eeprom_write":
		return "I/irreversible"
	default:
		return "I"
	}
}

func formatPrompt(physical string, detail map[string]any, risk string) string {
	details := ""
	for _, k := range []string{"pin", "value", "address", "register", "frequency"} {
		if v, ok := detail[k]; ok {
			details += fmt.Sprintf(" %s=%v", k, v)
		}
	}
	return fmt.Sprintf("[PiForge %s]%s approve? (y/N): ", risk, details)
}

// ---- Confirmer implementations ----

// StdinConfirmer asks via stdin/stdout. Suitable for an interactive TTY.
func StdinConfirmer(ctx context.Context) Confirmer {
	r := bufio.NewReader(os.Stdin)
	return func(prompt string) bool {
		fmt.Fprint(os.Stderr, prompt)
		line, err := r.ReadString('\n')
		if err != nil {
			return false
		}
		answer := strings.TrimSpace(strings.ToLower(line))
		return answer == "y" || answer == "yes"
	}
}

// AlwaysDenyConfirmer denies everything — the safe default when no TTY/confirm
// channel is wired (e.g. an unattended run).
func AlwaysDenyConfirmer() Confirmer {
	return func(prompt string) bool { return false }
}

// ThrottledFunc adapts a func to ThrottledReader.
type ThrottledFunc func() (now, sinceBoot bool, err error)

func (f ThrottledFunc) UnderVoltageActive() (bool, bool, error) { return f() }
