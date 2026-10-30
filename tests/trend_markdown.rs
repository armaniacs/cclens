//! The trend view's rendered markdown, end to end through the real binary.
//! A unit test cannot assert the rendered output: the test harness captures
//! its threads' `println!` per-thread, so it never reaches stdout — here the
//! binary's own stdout is real, and the store is synthetic.

use saniwalens::core::span::SessionStart;
use saniwalens::store::{IngestBatch, OpenMode, SessionMeta, Store};
use std::path::{Path, PathBuf};

/// A synthetic session stamped at `epoch_secs` (1970) with no subagent use.
fn session(id: &str, epoch_secs: i64) -> SessionMeta {
    SessionMeta {
        id: id.to_string(),
        project: "demo".to_string(),
        slug: "demo".to_string(),
        root: String::new(),
        source_path: format!("/tmp/{id}.jsonl"),
        sub_tokens: 0,
        sub_agent_count: 0,
        start: Some(SessionStart {
            timestamp_ms: epoch_secs * 1000,
            ctx: 9_000,
        }),
    }
}

/// Write a synthetic store: one `file_edit` work event per session at the
/// session's epoch, and no skill spans anywhere.
fn write_store(dir: &Path, sessions: &[(&str, i64)]) -> PathBuf {
    let db = dir.join("store.db");
    {
        let mut store = Store::open(&db, OpenMode::ReadWrite).unwrap();
        for (id, epoch_secs) in sessions {
            let meta = session(id, *epoch_secs);
            store
                .ingest(IngestBatch::new(&meta, &[], &[]).work_events(&[(
                    epoch_secs * 1000,
                    "file_edit",
                    "main.rs",
                    None,
                )]))
                .unwrap();
        }
    }
    db
}

/// Run the binary's `trend` on `db` in markdown, frozen (the store is read
/// as-is; the synthetic home holds nothing to refresh from), and return its
/// stdout.
fn trend_markdown(dir: &Path, db: &Path) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_saniwalens"))
        .args([
            "trend",
            "--since",
            "1970-01-02",
            "--until",
            "1970-01-02",
            "--format",
            "markdown",
            "--frozen",
        ])
        .arg("--db")
        .arg(db)
        .env("HOME", dir)
        .env("CLAUDE_CONFIG_DIR", dir.join(".claude"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .output()
        .expect("the binary runs");
    assert!(
        out.status.success(),
        "trend failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A per-test scratch dir, cleaned up afterwards — synthetic only.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "saniwalens-trend-markdown-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn trend_markdown_omits_an_empty_section_instead_of_bare_headers() {
    // Both windows hold file_edit events but no skill usage anywhere: the
    // Skills section has no rows while the kind and totals sections do. The
    // windows: current 1970-01-02 JST ([56_400, 142_799]) holds the 100_000
    // session; the equal-length window right before it ([−30_000, 56_399])
    // holds the 1_000 one.
    let dir = scratch_dir("empty-skills");
    let db = write_store(&dir, &[("past", 1_000), ("present", 100_000)]);
    let out = trend_markdown(&dir, &db);

    // No bare skill table, and no section label without its table — a section
    // with no rows is omitted, per the usage view's convention.
    let bare =
        "| skill | previous | current | delta | direction |\n| --- | --- | --- | --- | --- |";
    assert!(!out.contains(bare), "a bare skill table rendered: {out:?}");
    assert!(
        !out.contains("Skills (largest movement first):"),
        "the empty section's label still rendered: {out:?}"
    );
    // The sections that do have rows still render.
    assert!(out.contains("| kind |"), "the kind table is gone: {out:?}");
    assert!(
        out.contains("| total |"),
        "the totals table is gone: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn trend_note_names_the_previous_window_that_holds_no_events() {
    // Only the current window holds events: the previous side of every
    // comparison is empty, so the note explains what n/a means and names the
    // previous window with its range.
    let dir = scratch_dir("one-sided");
    let db = write_store(&dir, &[("present", 100_000)]);
    let out = trend_markdown(&dir, &db);

    assert!(
        out.contains("holds no events in this store"),
        "the n/a note is missing: {out:?}"
    );
    assert!(
        out.contains("previous window (since "),
        "the note does not name the previous window: {out:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
