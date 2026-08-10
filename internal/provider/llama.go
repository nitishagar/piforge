// Package provider wraps the local llama-server (OpenAI-compatible) client.
// It exposes a chat-completion call that supports tools/tool_choice and
// reports token usage including cache stats — the telemetry the design relies
// on to verify prefix-cache hits across turns.
package provider

import (
	"context"
	"errors"
	"fmt"
	"io"

	"github.com/sashabaranov/go-openai"

	"github.com/nitishagar/piforge/internal/config"
)

// Client is a thin wrapper over go-openai's client pointed at a local
// llama-server. It records usage telemetry useful for the eval harness and
// for verifying prefix-cache reuse.
type Client struct {
	cli      *openai.Client
	cfg      config.ServerConfig
	// Telemetry accumulated across calls in a session.
	metrics Metrics
}

// Metrics holds token + cache telemetry across calls.
type Metrics struct {
	PromptTokens     int
	CompletionTokens int
	// CachedTokens is the count of prompt tokens served from the KV cache
	// (llama-server reports this via usage.prompt_tokens_details.cached_tokens).
	// High values across turns => prefix is byte-stable.
	CachedTokens int
	// Calls is the number of completion calls made.
	Calls int
}

// New returns a Client pointed at the configured llama-server.
func New(cfg config.ServerConfig) *Client {
	ocfg := openai.DefaultConfig(cfg.APIKey)
	ocfg.BaseURL = cfg.BaseURL
	return &Client{cli: openai.NewClientWithConfig(ocfg), cfg: cfg}
}

// ChatRequest is the input to a chat completion.
type ChatRequest struct {
	Messages   []openai.ChatCompletionMessage
	Tools      []openai.Tool
	ToolChoice any // "auto" | "none" | {"type":"function","function":{"name":x}}
	// MaxTokens overrides cfg.MaxTokens when non-zero.
	MaxTokens int
}

// ChatResponse is the output, with telemetry.
type ChatResponse struct {
	Content      string
	ToolCalls    []openai.ToolCall
	FinishReason string
	PromptTokens int
	Completion   int
	Cached       int
}

// Chat performs a non-streaming chat completion. For the MVP this is simpler
// than streaming and avoids the tool-call-argument accumulation gotcha across
// SSE deltas. At ~5 tok/s the latency cost is acceptable for a tool-using
// loop where each turn ends in a tool_call decision.
func (c *Client) Chat(ctx context.Context, req ChatRequest) (*ChatResponse, error) {
	maxTokens := req.MaxTokens
	if maxTokens == 0 {
		maxTokens = c.cfg.MaxTokens
	}

	ccReq := openai.ChatCompletionRequest{
		Model:       openaiChatModel, // llama-server ignores this; uses its loaded model
		Messages:    req.Messages,
		MaxTokens:   maxTokens,
		Temperature: c.cfg.Temperature,
	}
	if len(req.Tools) > 0 {
		ccReq.Tools = req.Tools
	}
	if req.ToolChoice != nil {
		ccReq.ToolChoice = req.ToolChoice
	}

	resp, err := c.cli.CreateChatCompletion(ctx, ccReq)
	if err != nil {
		return nil, fmt.Errorf("chat completion: %w", err)
	}
	if len(resp.Choices) == 0 {
		return nil, errors.New("empty choices in response")
	}

	choice := resp.Choices[0]
	out := &ChatResponse{
		FinishReason: string(choice.FinishReason),
	}
	if choice.Message.Content != "" {
		out.Content = choice.Message.Content
	}
	if len(choice.Message.ToolCalls) > 0 {
		out.ToolCalls = choice.Message.ToolCalls
	}

	// Token accounting. llama-server fills usage including cached_tokens.
	out.PromptTokens = resp.Usage.PromptTokens
	out.Completion = resp.Usage.CompletionTokens
	if resp.Usage.PromptTokensDetails.CachedTokens > 0 {
		out.Cached = resp.Usage.PromptTokensDetails.CachedTokens
	}

	c.metrics.PromptTokens += out.PromptTokens
	c.metrics.CompletionTokens += out.Completion
	c.metrics.CachedTokens += out.Cached
	c.metrics.Calls++
	return out, nil
}

// HealthCheck pings the server's /health endpoint via a models list call.
func (c *Client) HealthCheck(ctx context.Context) error {
	if _, err := c.cli.ListModels(ctx); err != nil {
		return fmt.Errorf("server health check failed (is llama-server running at %s?): %w",
			c.cfg.BaseURL, err)
	}
	return nil
}

// Metrics returns accumulated telemetry.
func (c *Client) Metrics() Metrics { return c.metrics }

// StreamDeltas is a minimal streaming helper for user-facing turns that produce
// text rather than tool calls. It writes deltas to w and returns total usage.
// NOTE: streamed tool calls are NOT accumulated here — use Chat for turns that
// may emit tool_calls.
func (c *Client) StreamDeltas(ctx context.Context, req ChatRequest, w io.Writer) (*ChatResponse, error) {
	maxTokens := req.MaxTokens
	if maxTokens == 0 {
		maxTokens = c.cfg.MaxTokens
	}
	ccReq := openai.ChatCompletionRequest{
		Model:       openaiChatModel,
		Messages:    req.Messages,
		MaxTokens:   maxTokens,
		Temperature: c.cfg.Temperature,
		Stream:      true,
		StreamOptions: &openai.StreamOptions{
			IncludeUsage: true,
		},
	}
	if len(req.Tools) > 0 {
		ccReq.Tools = req.Tools
	}

	stream, err := c.cli.CreateChatCompletionStream(ctx, ccReq)
	if err != nil {
		return nil, fmt.Errorf("create stream: %w", err)
	}
	defer stream.Close()

	out := &ChatResponse{}
	for {
		chunk, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("stream recv: %w", err)
		}
		if len(chunk.Choices) > 0 {
			delta := chunk.Choices[0].Delta
			if delta.Content != "" {
				if _, werr := io.WriteString(w, delta.Content); werr != nil {
					return nil, werr
				}
			}
			if len(chunk.Choices) > 0 {
				out.FinishReason = string(chunk.Choices[0].FinishReason)
			}
		}
		if chunk.Usage != nil {
			out.PromptTokens = chunk.Usage.PromptTokens
			out.Completion = chunk.Usage.CompletionTokens
			if chunk.Usage.PromptTokensDetails.CachedTokens > 0 {
				out.Cached = chunk.Usage.PromptTokensDetails.CachedTokens
			}
		}
	}
	c.metrics.PromptTokens += out.PromptTokens
	c.metrics.CompletionTokens += out.Completion
	c.metrics.CachedTokens += out.Cached
	c.metrics.Calls++
	return out, nil
}

// openaiChatModel is a placeholder model name. llama-server ignores the model
// field and uses whatever GGUF it loaded at startup; go-openai requires a
// non-empty string.
const openaiChatModel = openai.GPT4oMini
