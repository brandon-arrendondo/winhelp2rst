# winhelp — Plans & Roadmap

Last Updated: 2026-04-14

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

# Phase 3b — Opcode Parser Fixes

# Task ID: 25
# Title: Fix opcode parser text extraction quality
# Status: pending
# Dependencies: 8, 14
# Priority: P1
# Description: Fix multiple opcode parser issues causing garbled text output
#   when processing real HLP files (clib.hlp produces readable but malformed RST).
# Details:
Validated against clib.hlp Win16 (709 topics extracted). The opcode parser
(task 8) was built against synthetic test data and has several gaps when
processing real-world opcode streams. Issues observed:

1. Missing word/line separation: consecutive hyperlink texts and paragraph
   items run together without spaces. Example: "CLibraryOverview" should
   be "C Library Overview". Likely cause: link end opcodes (0x89) or
   paragraph formatting opcodes not emitting whitespace between items.

2. Missing newlines in code blocks: #include statements and code examples
   concatenate onto one line. Example: "#include <stdio.h>#include <stdlib.h>"
   The OP_LINE_BREAK (0x81) handler emits a space instead of a newline.
   Code blocks may need detection (e.g., monospace font via OP_FONT_CHANGE)
   to switch to literal-block RST output.

3. Empty link display text: many :ref: directives have no visible text,
   rendering as `:ref:\` <ctx_xxx>\``. The link text bytes between the
   link start opcode (0xE3/0xE6/0xC8/0xCC) and link end opcode (0x89)
   are not being captured — likely because 0x89 doubles as OP_ITALIC_OFF
   and the parser consumes it before the link handler sees it.

4. Opcode byte leakage: fragments like "!iA", "a9", "TH!iAa9" appear in
   output text. These are raw opcode/parameter bytes being misinterpreted
   as literal ASCII. Likely cause: unrecognized opcodes with variable-length
   payloads — the parser skips only the opcode byte but not its parameters,
   so parameter bytes leak into the text buffer.

5. No paragraph breaks between sections: Synopsis, Description, Returns,
   Example, etc. run together. The paragraph info header in LinkData1
   (which we now correctly skip) may contain section-break indicators
   that should emit blank lines.

Implementation: winhelp/src/opcode.rs (major rework)
  - Fix 0x89 ambiguity: link-end takes priority over italic-off when in_link
  - Add variable-length payload tables for all known opcodes
  - Emit newline (not space) for OP_LINE_BREAK
  - Detect monospace font runs → RST literal blocks or ``inline code``
  - Parse LinkData1 paragraph info for section/spacing hints

Validation approach:
  - Compare extracted text against helpdeco RTF output for clib.hlp
  - Spot-check 10+ topics: printf, malloc, fopen, exit, sprintf, etc.
  - Verify all 709 topics produce parseable RST (no docutils errors)

---

# Phase 6 — Advanced Format Handling

# Task ID: 18
# Title: SHG (Segmented Hypergraphics) handling
# Status: pending
# Dependencies: 17
# Priority: P2
# Description: Parse SHG format images, flatten hotspot data, and extract
#   the base bitmap for PNG conversion.
# Details:
SHG files are bitmaps with embedded clickable regions (hotspots). Each
hotspot has a bounding rectangle and a macro/link action. Since RST has
no concept of image maps, we flatten: extract the bitmap, discard hotspot
data, and optionally emit hotspot info as RST comments.

SHG format:
  - SHG header with hotspot count
  - Array of hotspot records (rect, action type, action data)
  - Bitmap data (standard BMP or compressed)

Implementation: winhelp/src/bitmap.rs (extend)
  - `parse_shg(data: &[u8]) -> Result<(Vec<u8>, Vec<Hotspot>)>`
  - `Hotspot { rect, action, target }`

Tests:
  - If fixture files contain SHG images, verify extraction
  - Verify flattened bitmap is valid BMP/PNG
  - Verify hotspot data is captured (even if not used in RST)

---

# Task ID: 19
# Title: WMF (Windows Metafile) handling
# Status: pending
# Dependencies: 17
# Priority: P3
# Description: Handle WMF vector graphics — convert to SVG or rasterize to PNG.
# Details:
WMF is a vector graphics format. Options:
  1. Skip with warning (simplest, acceptable for MVP)
  2. Use external tool (wmf2svg/wmf2png) as optional dependency
  3. Implement basic WMF→SVG conversion (high effort, low priority)

For MVP: extract raw WMF bytes, save as .wmf, emit RST comment noting
unconverted format. Users can post-process.

Implementation: winhelp/src/bitmap.rs (extend)
  - Detect WMF magic bytes
  - Extract and save raw WMF
  - RST writer emits: `.. image:: _images/{filename}.wmf` with comment

---

# Phase 7 — WinHelp 4.0 (Win95) Support

# Task ID: 20
# Title: WinHelp 4.0 format differences
# Status: pending
# Dependencies: 14
# Priority: P2
# Description: Handle WinHelp 4.0 (Win95) format differences for broader
#   HLP file compatibility.
# Details:
WinHelp 4.0 differences from 3.1:
  - Different magic number in file header
  - |PhrIndex for compressed phrase offsets (Hall compression)
  - LZ77 variant may use different window sizes
  - Additional |SYSTEM record types (window definitions, macros)
  - |VIOLA (full-text search index)
  - CNT (contents) file support (separate .cnt file)

Implementation approach: version-detect in container.rs, then branch
parsing logic where formats differ. Most of the document model is shared.

Priority: handle after WinHelp 3.1 is working end-to-end. Many real-world
HLP files are WinHelp 4.0 (Win95/98/XP era), so this is important for
broad utility. The Win16 variant of clib.hlp validates 3.1; the Win32
variant validates 4.0 — same content, different format encoding.

---

# Phase 8 — CLI Polish and Distribution

# Task ID: 21
# Title: CLI error reporting and progress output
# Status: pending
# Dependencies: 14, 15, 16, 17
# Priority: P2
# Description: Polish the hlp2rst CLI with miette error reporting, progress
#   indicators, and useful diagnostic output.
# Details:
  - Use miette for rich error context (file offset, internal file name,
    topic ID where parsing failed)
  - Progress output: "Parsing N topics... [####------] 42%"
  - Summary output: "Wrote N .rst files, M images to output/"
  - --verbose flag for debug-level output (raw opcode dumps, etc.)
  - --dry-run flag: parse and validate without writing output
  - --format-version flag: force WinHelp 3.1 or 4.0 parsing

---

# Task ID: 22
# Title: End-to-end Sphinx round-trip test
# Status: pending
# Dependencies: 16
# Priority: P2
# Description: Verify the full round-trip: HLP → RST → Sphinx HTML build
#   completes without errors or warnings.
# Details:
  - Convert clib.hlp (Win16 variant) to RST
  - Run `sphinx-build -b html output/ output/_build/html`
  - Verify: zero warnings, all cross-references resolve, all images load
  - Run `sphinx-build -b htmlhelp output/ output/_build/htmlhelp`
  - Verify: produces valid HTML Help output (.hhp, .hhc, .hhk)

This is the ultimate validation that the RST output is correct and complete.

---

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

# Test Data

# Task ID: 24
# Title: Obtain test fixtures and helpdeco ground truth
# Status: pending
# Dependencies: none
# Priority: P0
# Description: Obtain primary test fixture (OpenWatcom clib.hlp) and generate
#   helpdeco ground truth for validation.
# Details:
Primary fixture: OpenWatcom C Library Reference (clib.hlp)
  Source: github.com/open-watcom/open-watcom-1.9/releases (tag w11.0c-zips)
    - c_hlp_win.zip → Win16 (WinHelp 3.1) variant
    - c_hlp_nt.zip  → Win32 (WinHelp 4.0) variant
  License: Sybase Open Watcom Public License (open source, redistributable)
  Content: C standard library function reference — hundreds of topics covering
    printf, malloc, fopen, string functions, math functions, etc.

Ground truth generation:
  1. Run `helpdeco clib.hlp` to extract .rtf + .hpj + images
  2. Capture: all context strings, titles, and browse sequences
  3. Capture: all hyperlink source→target pairs
  4. Capture: all image filenames and dimensions
  5. Store in tests/fixtures/clib_hlp/ as structured reference data

Additional fixtures (lower priority):
  - OpenWatcom clr.hlp (C Language Reference) — same source
  - OpenWatcom wccerrs.hlp (C Diagnostic Messages) — small, smoke test
  - Borland Turbo C++ TCWHELP.HLP (4.7 MB) — WinHelp 3.1 stress test

This is a blocking dependency for meaningful testing in phases 1-5.
Unit tests can use synthetic HLP fragments, but integration validation
requires real files.
