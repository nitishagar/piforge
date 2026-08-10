// Mock provider for the eval harness. Plays back a scripted sequence of
// assistant turns per case, so the runner + tool dispatch + scorer can be
// exercised in CI without a real llama-server. NOT a substitute for the real
// model evaluation — that's the whole point of the gate.
package eval

import (
	"context"
	"sync"

	"github.com/sashabaranov/go-openai"

	"github.com/nitishagar/piforge/internal/provider"
)

// MockTurn is one scripted assistant turn. Either ToolCalls or Text is used.
type MockTurn struct {
	ToolCalls []openai.ToolCall
	Text      string
	// telemetry values the mock pretends the model used.
	PromptTokens int
	Completion   int
	CachedTokens int
}

// MockProvider plays back per-case scripts. The script function returns the
// sequence of turns for a given case ID; the provider returns them one at a
// time on each Chat call.
type MockProvider struct {
	Script func(caseID string) []MockTurn

	mu      sync.Mutex
	current string // active case ID
	turns   []MockTurn
	pos     int
}

// setCase loads the script for a case. Called by the runner before each Run.
func (m *MockProvider) setCase(caseID string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.current = caseID
	m.turns = m.Script(caseID)
	m.pos = 0
}

// Chat implements agent.LLMClient.
func (m *MockProvider) Chat(ctx context.Context, req provider.ChatRequest) (*provider.ChatResponse, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	var turn MockTurn
	if m.pos < len(m.turns) {
		turn = m.turns[m.pos]
		m.pos++
	}
	out := &provider.ChatResponse{
		Content:      turn.Text,
		ToolCalls:    turn.ToolCalls,
		FinishReason: "stop",
		PromptTokens: orDefault(turn.PromptTokens, 100),
		Completion:   orDefault(turn.Completion, 50),
		Cached:       turn.CachedTokens,
	}
	// If the scripted turn had tool calls, the loop continues; else it's terminal.
	if len(turn.ToolCalls) > 0 {
		out.FinishReason = "tool_calls"
	}
	return out, nil
}

func orDefault(v, def int) int {
	if v == 0 {
		return def
	}
	return v
}
