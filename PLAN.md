# winhelp — Plans & Roadmap

Last Updated: 2026-04-17 (Task 21 → COMPLETED.md)

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
# Status: pending
# Dependencies: 22
# Priority: P3
# Description: Prepare both crates for crates.io publication with proper
#   metadata, README, and documentation.
# Details:
  - Cargo.toml metadata: description, license (MIT/Apache-2.0), repository,
    keywords, categories
  - README.md with quick-start, format notes, and Sphinx round-trip example
  - `cargo doc` generates clean documentation
  - Publish winhelp first (library), then hlp2rst (depends on winhelp)

---

# Task ID: 27
# Title: Font-attribute-driven bold/italic/underline styling
# Status: pending
# Dependencies: 13, 26
# Priority: P3
# Description: The opcode parser currently emits paragraphs in a single
#   neutral font style — bold/italic/underline runs are flattened. This is
#   deliberate: clib.hlp's body font (font index 4) carries attribute flags
#   that, when applied naïvely, wrap most sentences in italic and corrupt
#   the RST output.
# Details:
The infrastructure for font-attribute styling is already in place:
  - `FontDescriptor::is_bold`/`is_italic`/`is_underline` (font.rs)
  - `ParseState::apply_font` in winhelp/src/opcode.rs is a no-op pending
    a reliable mapping strategy.

Required work:
  1. Survey clib.hlp's |FONT table to understand which attributes really
     mean "semibold body", "italic emphasis", etc., and which are noise.
  2. Decide a mapping: perhaps use font_family or name heuristics to
     distinguish "body fonts" (ignore attributes) from "emphasis fonts"
     (apply attributes).
  3. Wire the mapping into `apply_font` and verify the round-trip still
     passes with zero Sphinx warnings.

Regression test: `font_change_does_not_toggle_bold_state` in opcode.rs
documents the current no-op behaviour — update it when the mapping lands.

---

# Task ID: 28
# Title: Render MRB bitmaps for non-DIB variants (DDB, metafile)
# Status: pending
# Dependencies: 26
# Priority: P3
# Description: `mrb_to_bmp()` currently handles only type=6 DIB with
#   byPacked=0 (raw) or byPacked=2 (LZ77). Other variants cause the
#   decoder to return None and the raw MRB bytes are saved verbatim,
#   which Sphinx cannot render.
# Details:
Out-of-scope variants:
  - type=5 DDB (device-dependent bitmap): needs DDB→DIB conversion
    (see helpdeco splitmrb.c lines 468-487) including optional run-length
    decompression via `GetPackedByte`.
  - type=8 metafile (WMF): vector format; would need WMF→SVG or raster
    conversion.
  - byPacked=1 (RunLen-only) and byPacked=3 (RunLen+LZ77): currently
    rejected. RunLen would need the `derun`/`GetPackedByte` state machine
    from helpdeco.

None of clib.hlp's 21 bitmaps trigger these paths, so the miss is
theoretical until another fixture needs them.
