package hil

import (
	"os"
	"path/filepath"
	"testing"
)

// TestSafeRejectsTraversal covers the path-traversal protections in edit_file.
// Regression test for the red-team symlink-escape finding.
func TestSafeRejectsTraversal(t *testing.T) {
	tmp := t.TempDir()
	tool := NewCodeEditTool(tmp)

	// Benign paths inside the root are allowed.
	for _, p := range []string{"ok.txt", "sub/dir/ok.txt", "a/../ok.txt"} {
		if _, err := tool.safe(p); err != nil {
			t.Errorf("safe(%q) unexpected error: %v", p, err)
		}
	}

	// Escapes are rejected.
	for _, p := range []string{
		"../escape.txt",
		"../../escape.txt",
		"sub/../../../escape.txt",
		"/etc/passwd", // absolute
	} {
		if _, err := tool.safe(p); err == nil {
			t.Errorf("safe(%q) should have been rejected (escapes root)", p)
		}
	}
}

// TestSafeRejectsSymlinkEscape is the critical regression: a symlink committed
// under the workspace that points OUTSIDE must not let edits escape. Before the
// fix, within() compared lexical paths and the symlink target was followed by
// os.WriteFile, escaping the root.
func TestSafeRejectsSymlinkEscape(t *testing.T) {
	tmp := t.TempDir()
	outside := t.TempDir() // a dir definitively outside the workspace
	link := filepath.Join(tmp, "escape")
	if err := os.Symlink(outside, link); err != nil {
		// Some platforms restrict symlink creation; skip if unsupported.
		t.Skipf("cannot create symlink: %v", err)
	}
	tool := NewCodeEditTool(tmp)
	if _, err := tool.safe("escape/evil.txt"); err == nil {
		t.Fatal("safe() must reject a path that escapes via a symlink under the root")
	}
}

// TestSafeAllowsDotDotSibling: "..foo" (a sibling file whose name starts with
// two dots) must NOT be rejected. The old startsWith(rel, "..") wrongly
// rejected it; the fix uses a path-separator check.
func TestSafeAllowsDotDotSibling(t *testing.T) {
	tmp := t.TempDir()
	tool := NewCodeEditTool(tmp)
	if _, err := tool.safe("..foo"); err != nil {
		t.Errorf("safe(\"..foo\") should be allowed (sibling, not escape): %v", err)
	}
}

// TestEditsCounter: the CodeEditTool records successful edits; the eval scorer
// uses Edits() to verify hardware-fault cases refrained from editing.
func TestEditsCounter(t *testing.T) {
	tmp := t.TempDir()
	tool := NewCodeEditTool(tmp)
	if n := tool.Edits(); n != 0 {
		t.Fatalf("initial Edits() = %d, want 0", n)
	}
	ctx := cancelContext(t)
	if _, err := tool.Execute(ctx, mustJSON(t, map[string]any{"path": "a.txt", "content": "hi"})); err != nil {
		t.Fatal(err)
	}
	if n := tool.Edits(); n != 1 {
		t.Fatalf("after one edit, Edits() = %d, want 1", n)
	}
}
