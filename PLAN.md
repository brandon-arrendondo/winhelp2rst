# winhelp — Plans & Roadmap

Last Updated: 2026-04-17 (Task 23 prep: metadata, README, doc warnings)

Goal: Pure-Rust library crate (`winhelp`) + CLI (`hlp2rst`) that parses Windows
WinHelp `.hlp` files and converts them to Sphinx-compatible reStructuredText.
No dependency on `helpdeco` or any external tool.

Completed tasks have been moved to COMPLETED.md.

## Test Fixtures

Primary test fixture: **OpenWatcom C Library Reference** (`clib.hlp`).
  - Source: open-watcom-1.9 release (Sybase Open Watcom Public License)
  - Win16 variant (WinHelp 3.1): c_hlp_win.zip
  - Win32 variant (WinHelp 4.0): c_hlp_nt.zip
  - Content: standard C library function reference (printf, malloc, etc.)
  - Validation baseline: `helpdeco` decompiled output (RTF + HPJ)

Additional test files (descending priority):
  - OpenWatcom `clr.hlp` (C Language Reference) — same license, language spec
  - OpenWatcom `wccerrs.hlp` (C Diagnostic Messages) — smaller, good smoke test
  - Borland Turbo C++ `TCWHELP.HLP` (4.7 MB) — WinHelp 3.1, stress test
  - `win32.hlp` (24 MB, OllyDbg archive) — WinHelp 4.0, size stress test

The parser should work against any well-formed HLP file. Additional real-world
files (game help, enterprise docs) can be added to the fixture set as needed.

## Test Strategy

Default test strategy for all tasks: pre-commit hooks (cargo test + cargo fmt +
cargo clippy + cargo-llvm-cov at 75% line coverage gate), then validation
against `helpdeco` ground truth where applicable.

For the proposal and format research, see PROPOSAL.md.

---

# Phase 8 — CLI Polish and Distribution

# Task ID: 23
# Title: crates.io publication
# Status: pending (prep done — publish step deferred)
# Dependencies: 22
# Priority: P3
# Description: Actually publish both crates to crates.io.  All prep work
#   is done: Cargo.toml metadata (keywords/categories/readme), repo-root
#   README.md with quick-start + format notes + Sphinx round-trip, and
#   a clean `cargo doc --workspace --no-deps` (zero warnings).
# Details:
  - Remaining step: `cargo publish -p winhelp` then `cargo publish -p hlp2rst`
  - Revisit once the author decides to publish — not ready yet.

