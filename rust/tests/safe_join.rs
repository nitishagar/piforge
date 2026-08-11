//! Path-traversal + symlink-escape regression tests for the code-edit tool.
//! Ports of the Go codeedit_test.go. The key regression: a symlink under the
//! workspace that points outside must NOT let edits escape.
use piforge::hil::Tool; // bring execute() into scope
use piforge::sim::{CodeEditTool, Setup};
use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
async fn rejects_traversal_and_absolute() {
    let tool = CodeEditTool::new(std::env::temp_dir().to_string_lossy().to_string());
    for bad in ["../escape.txt", "../../escape.txt", "sub/../../../escape.txt", "/etc/passwd"] {
        let r = tool.execute(&json!({"path":bad,"content":"x"})).await;
        assert!(!r.ok, "should reject {bad}");
    }
}

#[tokio::test]
async fn allows_benign() {
    let tmp = tempfile_dir();
    let tool = CodeEditTool::new(tmp.to_string_lossy().to_string());
    for ok in ["ok.txt", "sub/dir/ok.txt"] {
        let r = tool.execute(&json!({"path":ok,"content":"x"})).await;
        assert!(r.ok, "should allow {ok}: {:?}", r.error);
    }
}

#[tokio::test]
async fn rejects_symlink_escape() {
    let tmp = tempfile_dir();
    let outside = tempfile_dir(); // definitively outside the workspace
    let link = tmp.join("escape");
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(&outside, &link).is_err() {
            return; // platform restricts symlinks; skip
        }
    }
    let tool = CodeEditTool::new(tmp.to_string_lossy().to_string());
    let r = tool.execute(&json!({"path":"escape/evil.txt","content":"x"})).await;
    assert!(!r.ok, "symlink-escape must be rejected: {:?}", r.error);
}

fn tempfile_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("piforge-test-{}-{}", std::process::id(), rand_u32()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rand_u32() -> u32 {
    use std::time::SystemTime;
    let d = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.subsec_nanos()) | 1
}
