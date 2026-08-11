//! Workspace isolation (IMPLICIT_SPEC invariant 11).
//!
//! `temp_workspace` must key on case-id (not PID) so cases in one process get
//! disjoint dirs, and the `WorkspaceGuard` must remove the dir on drop so /tmp
//! doesn't leak one-per-run. Scaling 13 → ~40 makes this load-bearing.
use piforge::eval::temp_workspace;
use std::collections::HashMap;

fn files(k: &str, v: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(k.into(), v.into());
    m
}

#[test]
fn two_cases_with_same_filename_get_disjoint_dirs() {
    // Two cases that both write "main.py" must not collide. Pre-fix, both
    // shared /tmp/piforge-eval-<pid>, so the second overwrote the first.
    let g1 = temp_workspace("case-alpha", &files("main.py", "alpha-content"))
        .expect("create workspace 1");
    let g2 =
        temp_workspace("case-beta", &files("main.py", "beta-content")).expect("create workspace 2");

    // Different dirs.
    assert_ne!(g1.path, g2.path, "two cases shared a temp dir");

    // Each dir contains its OWN content, not the other's.
    let f1 = std::fs::read_to_string(format!("{}/main.py", g1.path)).unwrap();
    let f2 = std::fs::read_to_string(format!("{}/main.py", g2.path)).unwrap();
    assert_eq!(f1, "alpha-content");
    assert_eq!(f2, "beta-content");
    assert_ne!(f1, f2, "cross-contamination between cases");

    // Both exist while guards are held.
    assert!(std::path::Path::new(&g1.path).exists());
    assert!(std::path::Path::new(&g2.path).exists());

    let p1 = g1.path.clone();
    let p2 = g2.path.clone();
    drop(g1);
    drop(g2);

    // Both cleaned up on drop.
    assert!(
        !std::path::Path::new(&p1).exists(),
        "workspace 1 not cleaned up: {p1}"
    );
    assert!(
        !std::path::Path::new(&p2).exists(),
        "workspace 2 not cleaned up: {p2}"
    );
}

#[test]
fn same_case_run_twice_does_not_collide() {
    // Re-running the same case-id must get a fresh dir (random suffix), not
    // reuse the stale one. This defends the isolation invariant under retries.
    let g1 = temp_workspace("repeated-case", &files("a.txt", "first")).expect("first run");
    let p1 = g1.path.clone();
    drop(g1);
    assert!(!std::path::Path::new(&p1).exists(), "first dir leaked");

    let g2 = temp_workspace("repeated-case", &files("a.txt", "second")).expect("second run");
    // New dir, no leftover from the first run, correct content.
    assert_ne!(g2.path, p1, "retry reused the stale dir");
    let body = std::fs::read_to_string(format!("{}/a.txt", g2.path)).unwrap();
    assert_eq!(body, "second");
}

#[test]
fn guard_is_idempotent_on_dirs_that_vanish() {
    // If something removes the dir before the guard drops, the Drop must not
    // panic (a leftover dir is cosmetic; a panic would kill the run).
    let g = temp_workspace("vanishing", &files("x.txt", "y")).expect("workspace");
    std::fs::remove_dir_all(&g.path).expect("external removal");
    drop(g); // must not panic
}

#[test]
fn nested_file_paths_are_created() {
    // Fixtures may reference nested paths (e.g. "src/lib.rs"); temp_workspace
    // must create the parent dirs.
    let mut nested = HashMap::new();
    nested.insert("src/deep/lib.rs".into(), "fn main() {}".into());
    let g = temp_workspace("nested-case", &nested).expect("nested workspace");
    let body = std::fs::read_to_string(format!("{}/src/deep/lib.rs", g.path)).unwrap();
    assert_eq!(body, "fn main() {}");
}

#[test]
fn case_id_with_unsafe_chars_is_sanitized() {
    // A future fixture id with a slash/path-separator must not escape the temp
    // dir root. Sanitization replaces non-alnum/-/_ with '_'.
    let g = temp_workspace("evil/../escape", &HashMap::new()).expect("sanitized");
    let p = std::path::Path::new(&g.path);
    // The parent must be the system temp dir, not something traversed out of.
    assert_eq!(
        p.parent().unwrap(),
        std::env::temp_dir(),
        "unsafe case-id escaped the temp dir"
    );
    // The dir name contains no literal '/' from the id.
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    assert!(!name.contains('/'), "unsanitized slash in dir name: {name}");
}

#[test]
fn fixture_file_key_with_traversal_is_rejected() {
    // Security regression: a fixture's setup.files key like "../../etc/leak"
    // must NOT write outside the workspace root. The seeding loop routes every
    // key through sim::safe_join (the same containment primitive edit_file
    // uses). A traversal key must be rejected with an error, not written.
    let mut files = HashMap::new();
    files.insert("../../etc/piforge-leak-test".into(), "pwned".into());
    files.insert("/etc/absolute-escape".into(), "pwned2".into());
    let result = temp_workspace("traversal-case", &files);
    assert!(
        result.is_err(),
        "a traversal/absolute fixture file key must be rejected, not written outside the workspace"
    );
    // And nothing should have been written to either target.
    assert!(
        !std::path::Path::new("/etc/piforge-leak-test").exists(),
        "traversal wrote outside the workspace!"
    );
    assert!(
        !std::path::Path::new("/etc/absolute-escape").exists(),
        "absolute path wrote outside the workspace!"
    );
}
