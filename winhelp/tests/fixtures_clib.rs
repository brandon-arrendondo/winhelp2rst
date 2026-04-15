//! End-to-end fixture tests against the OpenWatcom `clib.hlp` reference.
//!
//! We parse both the Win16 (WinHelp 3.1) and Win32 (WinHelp 4.0 / Hall)
//! variants of the same help file and assert coverage parity: same topic
//! count, same number of resolved context IDs, same bitmap count.  These
//! lock in the Task 20 work (Hall phrase decompression, `scanword`-based
//! TOPICOFFSET deltas, byPacked=3 MRB decoding).
//!
//! The fixtures live under `tests/fixtures/clib_hlp/` at the workspace
//! root (not under `winhelp/`), so paths are constructed relative to
//! `CARGO_MANIFEST_DIR`.

use std::path::PathBuf;
use winhelp::HelpFile;

/// Resolve a path relative to the workspace root (one directory up from
/// the `winhelp/` crate manifest).
fn workspace_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push(rel);
    p
}

#[test]
fn clib_win16_parses_with_full_coverage() {
    let path = workspace_path("tests/fixtures/clib_hlp/win16/binw/clib.hlp");
    let hf = HelpFile::from_path(&path).expect("parse Win16 clib.hlp");
    assert_eq!(hf.topics.len(), 711, "Win16 topic count");
    let named = hf.topics.iter().filter(|t| !t.id.is_empty()).count();
    assert!(
        named >= 709,
        "Win16 expected ≥ 709 named topics, got {named}"
    );
    assert_eq!(hf.images.len(), 21, "Win16 image count");
}

#[test]
fn clib_win32_hall_phrases_reach_topic_parity() {
    let path = workspace_path("tests/fixtures/clib_hlp/win32/binnt/clib.hlp");
    let hf = HelpFile::from_path(&path).expect("parse Win32 clib.hlp");
    assert_eq!(hf.topics.len(), 711, "Win32 topic count");

    // 700 distinct primary ids + 9 aliases ⇒ 709 resolvable context
    // strings.  The regression pre-Task-20 was ~184 unresolved ids.
    let named = hf.topics.iter().filter(|t| !t.id.is_empty()).count();
    assert!(
        named >= 700,
        "Win32 expected ≥ 700 named topics (Hall + scanword TOPICOFFSET), got {named}"
    );
    let resolved_total: usize = hf
        .topics
        .iter()
        .map(|t| {
            if t.id.is_empty() {
                0
            } else {
                1 + t.aliases.len()
            }
        })
        .sum();
    assert!(
        resolved_total >= 709,
        "Win32 expected ≥ 709 resolvable context ids (with aliases), got {resolved_total}"
    );
}

#[test]
fn clib_win32_all_bitmaps_decode_to_png() {
    let path = workspace_path("tests/fixtures/clib_hlp/win32/binnt/clib.hlp");
    let hf = HelpFile::from_path(&path).expect("parse Win32 clib.hlp");
    assert_eq!(hf.images.len(), 21, "Win32 image count");
    for (name, bytes) in &hf.images {
        assert!(
            image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp).is_ok(),
            "Win32 bitmap {name} did not decode as BMP ({} bytes)",
            bytes.len()
        );
    }
}
