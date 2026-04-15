# winhelp — Plans & Roadmap

Last Updated: 2026-04-15 (Task 22 + Task 24 → COMPLETED.md)

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

# Task ID: 26
# Title: Emit image block references from opcode parser
# Status: pending
# Dependencies: 8, 17
# Priority: P2
# Description: The opcode parser does not currently recognize image-reference
#   opcodes ({bmc}, {bml}, {bmr}), so no Block::Image variants are ever
#   produced. As a result, clib.hlp's embedded bitmaps (|bm0..|bmN) are
#   silently dropped. Discovered during Task 22 round-trip validation.
# Details:
Image opcodes per PROPOSAL.md and WinHelp format notes:
  - 0xE3/0xE6 in the LD1 command stream with an embedded image index /
    filename reference.
  - Placement variants: {bmc} = inline, {bml} = left-aligned, {bmr} =
    right-aligned.

Required work:
  1. Identify exact LD1 opcode bytes and payload layout for image refs.
  2. Resolve payload index to `|bmN` internal-file name.
  3. Emit `Block::Image { filename, placement }` from parse_text_record.
  4. Verify end-to-end: clib.hlp produces >0 PNG files under _images/,
     Sphinx htmlhelp build still clean.

Reference: helpdeco's image/bitmap opcode handling in helpdeco.c.
