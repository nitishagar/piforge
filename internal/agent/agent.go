// Package agent implements the ReAct agent loop: send messages+tools to the
// local model, dispatch tool calls, append tool results, repeat until the
// model stops calling tools or hits the turn budget.
//
// Prefix-cache discipline (the key cost lever on a slow Pi):
//   - The system prompt + tool schemas are a byte-stable prefix.
//   - Message history is append-only — earlier messages are never mutated.
//   - This lets llama-server's automatic prefix cache reuse KV across turns,
//     so each turn pays only for the delta. Verify via Client.Metrics().CachedTokens.
package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/sashabaranov/go-openai"

	"github.com/nitishagar/piforge/internal/hil"
	"github.com/nitishagar/piforge/internal/provider"
)

// SystemPrompt is the frozen instruction prefix. Kept short and stable so the
// model + tool schemas cache cleanly. Pin it; do not mutate per turn.
const SystemPrompt = `You are PiForge, a coding agent running ON a Raspberry Pi 5 with direct access to its hardware (GPIO, I2C, sensors, telemetry). You diagnose and fix code that misbehaves on the actual hardware by reading live state and correlating it with the driver code.

Workflow:
1. Call hardware_inventory to ground yourself.
2. Read telemetry (action=snapshot) BEFORE any physical action — under-voltage is a STOP signal.
3. Read the live sensor/bus state (i2c, gpio, scope) and the driver code (edit_file's sibling read: ask the user or use a shell read).
4. Correlate observed state with the code. Form ONE hypothesis.
5. Edit the code (edit_file) or fix config. Re-read the sensor to confirm.
6. If the symptom is a hardware fault (shorted pins, brownout, missing pull-ups, fried board), STOP editing and tell the user — do not keep coding.

Rules:
- Treat every register address and pin number as a hypothesis to verify on hardware, never as a fact.
- Prefer hardware-backed interfaces (kernel I2C/SPI, hardware PWM) over bit-banged ones in the code you write.
- On the Pi 5, only the lgpio backend works; RPi.GPIO is broken. Use gpiozero/libgpiod conventions.
- GPIO outputs are Class I (physical): each write may require human approval. Reads are always safe.
- Be concise. The hardware is slow (~5 tokens/sec). Do not over-explore.
`

// LLMClient is the minimal interface the agent loop needs from a model
// provider. *provider.Client (real llama-server) and eval.MockProvider both
// satisfy it.
type LLMClient interface {
	// Chat performs one completion. Returns content, tool calls, finish reason,
	// and token telemetry (prompt/completion/cached).
	Chat(ctx context.Context, req provider.ChatRequest) (*provider.ChatResponse, error)
}

// Agent drives the tool-use loop.
type Agent struct {
	client   LLMClient
	tools    map[string]hil.Tool
	maxTurns int
}

// New builds an Agent with the given tools and turn budget.
func New(client LLMClient, tools []hil.Tool, maxTurns int) *Agent {
	m := map[string]hil.Tool{}
	for _, t := range tools {
		m[t.Name()] = t
	}
	if maxTurns <= 0 {
		maxTurns = 12
	}
	return &Agent{client: client, tools: m, maxTurns: maxTurns}
}

// Run executes one task. userMsg is the user's symptom/request. It streams
// final assistant text to onText (may be nil) and returns the final response
// + accumulated metrics.
func (a *Agent) Run(ctx context.Context, userMsg string, onText func(string)) (*RunResult, error) {
	// Frozen system prefix + append-only history.
	msgs := []openai.ChatCompletionMessage{
		{Role: openai.ChatMessageRoleSystem, Content: SystemPrompt},
		{Role: openai.ChatMessageRoleUser, Content: userMsg},
	}

	toolDefs := make([]openai.Tool, 0, len(a.tools))
	for _, t := range a.tools {
		toolDefs = append(toolDefs, t.Schema())
	}

	var accPrompt, accCompletion, accCached int
	for turn := 0; turn < a.maxTurns; turn++ {
		resp, err := a.client.Chat(ctx, provider.ChatRequest{
			Messages:   msgs,
			Tools:      toolDefs,
			ToolChoice: "auto",
		})
		if err != nil {
			return nil, fmt.Errorf("turn %d: %w", turn, err)
		}
		accPrompt += resp.PromptTokens
		accCompletion += resp.Completion
		accCached += resp.Cached

		// No tool calls => terminal turn.
		if len(resp.ToolCalls) == 0 {
			if onText != nil && resp.Content != "" {
				onText(resp.Content)
			}
			return &RunResult{
				FinalText:    resp.Content,
				Turns:        turn + 1,
				PromptTokens: accPrompt,
				Completion:   accCompletion,
				CachedTokens: accCached,
			}, nil
		}

		// Emit any assistant text that accompanied the tool calls.
		if onText != nil && resp.Content != "" {
			onText(resp.Content + "\n")
		}

		// Append the assistant message that CARRIES the tool_calls (the model's
		// tool-call request). This is required for OpenAI-shape message history.
		msgs = append(msgs, openai.ChatCompletionMessage{
			Role:      openai.ChatMessageRoleAssistant,
			Content:   resp.Content,
			ToolCalls: resp.ToolCalls,
		})

		// Dispatch each tool call and append its result as a "tool" message.
		for _, call := range resp.ToolCalls {
			out := a.dispatch(ctx, call)
			msgs = append(msgs, openai.ChatCompletionMessage{
				Role:       openai.ChatMessageRoleTool,
				Content:    out,
				ToolCallID: call.ID,
			})
		}
	}

	return nil, errors.New("turn budget exhausted without a terminal response")
}

// RunResult is the summary of a completed run.
type RunResult struct {
	FinalText    string
	Turns        int
	PromptTokens int
	Completion   int
	CachedTokens int
}

// CacheHitRate is the fraction of prompt tokens served from the KV cache.
// On a byte-stable prefix across turns this climbs toward 1.0.
func (r RunResult) CacheHitRate() float64 {
	if r.PromptTokens == 0 {
		return 0
	}
	return float64(r.CachedTokens) / float64(r.PromptTokens)
}

// dispatch executes one tool call and returns the JSON string the model sees.
func (a *Agent) dispatch(ctx context.Context, call openai.ToolCall) string {
	name := call.Function.Name
	tool, ok := a.tools[name]
	if !ok {
		return hil.Errorf(name, "unknown tool %q", name).String()
	}
	args := json.RawMessage(call.Function.Arguments)
	res, err := tool.Execute(ctx, args)
	if err != nil {
		return hil.Errorf(name, "execute: %v", err).String()
	}
	// Tools return Result values (which carry their own OK/Error). Normalize to JSON.
	if r, ok := res.(hil.Result); ok {
		return r.String()
	}
	b, _ := json.Marshal(res)
	return string(b)
}
