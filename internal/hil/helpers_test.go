package hil

import (
	"context"
	"encoding/json"
	"testing"
)

// mustJSON marshals v, failing the test on error (test helper).
func mustJSON(t *testing.T, v any) []byte {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return b
}

// cancelContext returns a context that is canceled when the test finishes.
func cancelContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	return ctx
}
