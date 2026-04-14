# winhelp — Plans & Roadmap

Last Updated: 2026-04-14

Goal: Pure-Rust library crate (`winhelp`) + CLI (`hlp2rst`) that parses Windows
WinHelp `.hlp` files and converts them to Sphinx-compatible reStructuredText.
No dependency on `helpdeco` or any external tool.

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

# Phase 0 — Project Scaffolding

# Task ID: 1
# Title: Workspace and crate scaffolding
# Status: pending
# Dependencies: none
# Priority: P0
# Description: Create Cargo workspace with `winhelp` library crate and `hlp2rst`
#   binary crate. Set up pre-commit hooks, CI-ready coverage gate, and initial
#   test infrastructure.
# Details:
Workspace layout:

  winhelp2rst/
    Cargo.toml              ← workspace root
    .pre-commit-config.yaml
    scripts/coverage-gate.sh
    winhelp/                ← library crate
      Cargo.toml
      src/lib.rs
    hlp2rst/                ← binary crate
      Cargo.toml
      src/main.rs
    tests/                  ← integration test fixtures
      fixtures/

  Workspace Cargo.toml: members = ["winhelp", "hlp2rst"]

  winhelp dependencies: winnow, thiserror
  hlp2rst dependencies: winhelp (path), clap 4, miette, image

  Pre-commit hooks (adapted from tools_sqc):
    1. no-commit-to-branch (master/main)
    2. cargo fmt --all (auto-fix)
    3. cargo clippy -- -D warnings
    4. cargo-llvm-cov with 75% coverage gate

  Initial lib.rs: re-export submodules, define top-level HelpFile struct.
  Initial main.rs: clap CLI skeleton (hlp2rst <input.hlp> <output_dir/>).

---

# Phase 1 — Container Reader (the filesystem layer)

# Task ID: 2
# Title: HLP file header and internal directory parsing
# Status: pending
# Dependencies: 1
# Priority: P0
# Description: Parse the HLP file header, locate the internal directory B-tree,
#   and enumerate all internal file entries by name and offset.
# Details:
The HLP file starts with a file header:

  offset 0x00: u32 magic (0x00035F3F for WinHelp 3.1)
  offset 0x04: u32 directory_start — byte offset of the internal directory

The directory is a B-tree of (name, offset) pairs pointing to internal files.
Each B-tree page has:
  - u16 num_entries, u16 previous_page (for leaf pages)
  - Array of null-terminated name strings + u32 offsets

Implementation in winhelp/src/container.rs:
  - `HlpHeader` struct: magic, directory_start
  - `InternalFile` struct: name (String), offset (u64), size (u64)
  - `HlpContainer::open(path) -> Result<Self>`: parse header, read B-tree
  - `HlpContainer::list_files() -> &[InternalFile]`
  - `HlpContainer::read_file(name) -> Result<Vec<u8>>`

B-tree traversal: read the root page at directory_start, follow page pointers
for non-leaf nodes, collect leaf entries. WinHelp B-trees are straightforward
(no balancing needed for read-only access — just walk the pages).

Tests:
  - Parse clib.hlp header, verify magic
  - Enumerate internal files, verify |SYSTEM, |TOPIC, |CONTEXT, |Phrases,
    |TTLBTREE, |FONT present
  - Read |SYSTEM raw bytes, verify non-empty

---

# Task ID: 3
# Title: |SYSTEM internal file parser
# Status: pending
# Dependencies: 2
# Priority: P0
# Description: Parse the |SYSTEM internal file to extract help file title,
#   copyright string, and root topic context ID.
# Details:
|SYSTEM layout:
  offset 0x00: u16 magic (0x036C)
  offset 0x02: u16 minor_version
  offset 0x04: u16 flags
  Followed by variable-length records, each:
    u16 record_type, u16 record_length, [data]

Record types:
  1 = title (null-terminated string)
  2 = copyright (null-terminated string)
  3 = root_topic_offset (u32)
  4 = starting_topic (null-terminated context string)
  6 = window definitions

Implementation: winhelp/src/system.rs
  - `SystemInfo` struct: title, copyright, root_topic, flags, version
  - Parse from raw bytes of |SYSTEM internal file

Tests:
  - Parse clib.hlp |SYSTEM, verify title and version fields
  - Cross-check extracted title against helpdeco ground truth

---

# Phase 2 — Decompression

# Task ID: 4
# Title: Phrase decompression (|Phrases / |PhrIndex)
# Status: pending
# Dependencies: 2
# Priority: P0
# Description: Parse the phrase dictionary and implement phrase substitution
#   (first-pass decompression for topic text).
# Details:
|Phrases contains a table of up to 2048 compressed phrases:
  offset 0x00: u16 num_phrases
  offset 0x02: u16[num_phrases+1] offsets (into phrase data)
  After offsets: raw phrase string data

|PhrIndex (optional, WinHelp 4.0): compressed phrase offsets. If absent,
offsets are inline in |Phrases.

Phrase substitution: in decompressed topic text, a byte pair where the high
bit of the first byte is set encodes a phrase index:
  index = ((byte1 & 0x7F) << 8) | byte2
  Replace the 2-byte token with phrases[index].

Implementation: winhelp/src/decompress.rs
  - `PhraseTable::from_bytes(phrases: &[u8], phr_index: Option<&[u8]>)
     -> Result<Self>`
  - `PhraseTable::expand(data: &[u8]) -> Result<Vec<u8>>`

Tests:
  - Load phrase table from clib.hlp, verify phrase count > 0
  - Expand a known topic block, verify output contains expected English text
    (e.g., C library function names like "printf", "malloc")
  - Round-trip: verify no phrase tokens remain in expanded output

---

# Task ID: 5
# Title: LZ77 variant decompression
# Status: pending
# Dependencies: 2
# Priority: P0
# Description: Implement the WinHelp LZ77 sliding-window decompression used
#   for |TOPIC blocks.
# Details:
WinHelp uses a modified LZ77 with a 4096-byte sliding window (initialized to
spaces). Each |TOPIC block that is compressed has a header indicating
compressed vs. uncompressed.

The compression format:
  - Read a control byte; each bit (LSB first) indicates whether the next
    element is a literal (1) or a back-reference (0)
  - Literal: copy next byte to output and sliding window
  - Back-reference: read 2 bytes as (offset:12, length:4); copy `length + 3`
    bytes from `sliding_window[offset]` to output

Implementation: winhelp/src/decompress.rs (extend from task 4)
  - `lz77_decompress(data: &[u8]) -> Result<Vec<u8>>`
  - Careful with window wrapping (circular buffer at 4096)

Tests:
  - Decompress a known |TOPIC block, verify output length matches expected
  - Verify decompressed output can be phrase-expanded to readable text
  - Edge cases: empty input, block with only literals, block with max-length
    back-references

---

# Task ID: 6
# Title: |TOPIC block reader (decompression integration)
# Status: pending
# Dependencies: 4, 5
# Priority: P0
# Description: Read the |TOPIC internal file as a sequence of blocks, apply
#   LZ77 decompression and phrase substitution, and produce raw topic records.
# Details:
|TOPIC is divided into fixed-size blocks (typically 4096 or 2048 bytes, set
in |SYSTEM flags). Each block has a header:
  u32 last_topic_link — offset of last topic header that starts in this block
  u32 first_topic_link — offset of first topic header in this block (or -1)
  u32 last_topic_header — offset of last topic header ending in this block

After the block header: compressed data (LZ77 or uncompressed, per block).

Decompression pipeline per block:
  1. Read block header
  2. LZ77 decompress the data portion (if compressed flag set)
  3. Phrase-expand the decompressed data
  4. Concatenate blocks to form the complete topic stream

Topic records within the stream are linked: each topic header has a
next_topic pointer forming a linked list.

Implementation: winhelp/src/topic.rs (block reader portion)
  - `TopicBlock` struct: header fields + decompressed data
  - `read_topic_blocks(container: &HlpContainer) -> Result<Vec<TopicBlock>>`
  - `flatten_topic_stream(blocks: &[TopicBlock]) -> Result<Vec<u8>>`

Tests:
  - Read all topic blocks from clib.hlp, verify block count > 0
  - Flatten stream, verify topic linked-list is traversable
  - Verify decompressed stream contains readable text fragments

---

# Phase 3 — Topic Opcode Parser (the hard core)

# Task ID: 7
# Title: Topic record header and footnote parsing
# Status: pending
# Dependencies: 6
# Priority: P0
# Description: Parse topic record headers (type, size, linked-list pointers)
#   and extract footnote metadata (# context, $ title, K keywords, + browse).
# Details:
Each topic record in the flattened stream starts with:
  u32 block_size — size of this record
  u32 data_size — size of decompressed data (may differ)
  u8  topic_type — 0x02 = topic header, 0x20/0x23 = text record

For topic_type 0x02 (topic header):
  u32 next_topic — link to next topic record
  Followed by footnote records

Footnotes are embedded as special character markers:
  '#' (0x23) — context string (the topic's stable ID)
  '$' (0x24) — display title
  'K' (0x4B) — keyword index entry
  '+' (0x2B) — browse sequence ID

These appear as: marker byte + null-terminated string.

Implementation: winhelp/src/topic.rs (record parser portion)
  - `RawTopicRecord` struct: topic_type, data, next_offset
  - `TopicMetadata` struct: context_id, title, keywords, browse_seq
  - Parse footnotes from topic header records

Tests:
  - Parse clib.hlp, extract all topic context IDs
  - Verify topic count and titles match helpdeco ground truth
  - Verify keyword entries are present for topics that have them

---

# Task ID: 8
# Title: Paragraph and text opcode parser
# Status: pending
# Dependencies: 7
# Priority: P0
# Description: Parse the binary opcode stream within topic text records into
#   the document model (paragraphs, formatted text, links, images).
# Details:
Text records (topic_type 0x20 or 0x23) contain:
  Paragraph info header:
    u16 data_size
    u8  paragraph_type (spacing, alignment, etc.)
    Variable-length tab stop / indent data

  Followed by text data with embedded opcodes:
    Literal text bytes (displayed as-is)
    0x80-0xFF range: attribute change opcodes
      0x80 = font change (followed by u16 font_index)
      0x81 = end of line
      0x82 = end of paragraph
      0x83 = tab
      0x86 = bold on
      0x87 = bold off
      0x88 = italic on
      0x89 = italic off
      0x8B = underline on
      0x8C = underline off
      0xC8 = hyperlink start (followed by context string)
      0xCC = hyperlink end
      0xE0-0xE7 = image references ({bmc}, {bml}, {bmr})

Note: exact opcode values need cross-referencing with helpdeco source.
Different sub-versions may shift values.

Implementation: winhelp/src/topic.rs (opcode parser)
  - Parse opcodes into `Block` and `Inline` enum variants from PROPOSAL.md
  - `parse_text_record(data: &[u8], phrases: &PhraseTable,
     fonts: &FontTable) -> Result<Vec<Block>>`

Tests:
  - Parse a known simple topic (plain text only), verify output
  - Parse a topic with bold/italic, verify inline formatting
  - Parse a topic with hyperlinks, verify link targets match ground truth
  - Parse a topic with images, verify image references extracted

---

# Task ID: 9
# Title: Hyperlink and popup opcode handling
# Status: pending
# Dependencies: 8
# Priority: P1
# Description: Fully resolve hyperlink and popup opcodes to target context
#   strings, distinguishing jump links from popup links.
# Details:
Hyperlinks encode as:
  Link start opcode (0xE3 for jump, 0xE6 for popup — verify against helpdeco)
  Link text (inline text with possible formatting)
  Hidden target: context string hash (u32) or inline context string
  Link end opcode

The context string hash maps to an actual context string via the |CONTEXT
B-tree. For correct round-trip, we need to resolve all links to their
context string form (not numeric hash).

Jump links → RST `:ref:` cross-references
Popup links → RST `.. note::` or inline annotation

Implementation: extend topic.rs opcode parser
  - Track link state (in_link, link_kind, accumulated link text)
  - Resolve hash-based targets via |CONTEXT lookup (task 10)

Tests:
  - Verify all hyperlink targets resolve to valid context IDs
  - Verify no orphan links (target context string exists as a topic ID)
  - Count jump vs popup links, compare against helpdeco ground truth

---

# Phase 4 — Index Files

# Task ID: 10
# Title: |CONTEXT B-tree reader
# Status: pending
# Dependencies: 2
# Priority: P1
# Description: Parse the |CONTEXT B-tree to build the mapping from context
#   string hash → topic byte offset.
# Details:
|CONTEXT is a B-tree mapping u32 hash values to u32 topic offsets.
The hash function is documented (case-insensitive hash of the context string).

B-tree format is the same as the internal directory (task 2), but with
different key/value types.

Implementation: winhelp/src/context.rs
  - `ContextMap::from_bytes(data: &[u8]) -> Result<Self>`
  - `ContextMap::resolve_hash(hash: u32) -> Option<u32>` (hash → offset)
  - `context_hash(s: &str) -> u32` — the WinHelp hash function

The reverse mapping (offset → context string) comes from parsing topic
footnotes (task 7). Combined, these allow full bidirectional resolution.

Tests:
  - Load |CONTEXT from clib.hlp, verify entry count
  - Hash a known context string, verify it resolves to expected offset
  - Verify all extracted context strings hash-resolve correctly

---

# Task ID: 11
# Title: |TTLBTREE reader (topic titles)
# Status: pending
# Dependencies: 2
# Priority: P1
# Description: Parse |TTLBTREE to get the ordered topic offset → title mapping.
# Details:
|TTLBTREE is a B-tree mapping u32 topic offsets to null-terminated title
strings. This provides the canonical topic ordering for building index.rst.

Implementation: winhelp/src/context.rs (or titles.rs)
  - `TitleIndex::from_bytes(data: &[u8]) -> Result<Self>`
  - `TitleIndex::titles_in_order() -> Vec<(u32, String)>`

Tests:
  - Load |TTLBTREE from clib.hlp, verify entry count matches topic count
  - Verify title strings match footnote-extracted titles from task 7
  - Verify ordering matches browse sequence

---

# Task ID: 12
# Title: |KWBTREE / |KWDATA keyword index reader
# Status: pending
# Dependencies: 2
# Priority: P2
# Description: Parse keyword index B-trees to extract keyword → topic mappings
#   for RST index directives.
# Details:
|KWBTREE maps keyword strings to offsets into |KWDATA.
|KWDATA contains arrays of topic offsets for each keyword.

Implementation: winhelp/src/keyword.rs
  - `KeywordIndex::from_bytes(kwbtree: &[u8], kwdata: &[u8]) -> Result<Self>`
  - `KeywordIndex::keywords() -> &[KeywordEntry]`
  - `KeywordEntry { keyword: String, topic_offsets: Vec<u32> }`

Tests:
  - Load keyword index from clib.hlp
  - Verify keyword count and a few known keyword-to-topic mappings
    (e.g., "printf" keyword → printf topic)

---

# Task ID: 13
# Title: |FONT table reader
# Status: pending
# Dependencies: 2
# Priority: P2
# Description: Parse the |FONT internal file to extract font definitions used
#   by the opcode parser for semantic formatting decisions.
# Details:
|FONT contains an array of font descriptors:
  u8 attributes (bold, italic, underline flags)
  u8 half_points (font size × 2)
  u8 font_family
  Followed by font name string

The opcode parser (task 8) references fonts by index. Semantic information
(e.g., "this is a monospace font" → RST ``literal``) requires reading the
font table.

Implementation: winhelp/src/font.rs
  - `FontTable::from_bytes(data: &[u8]) -> Result<Self>`
  - `FontDescriptor { name, size, bold, italic, underline, family }`

Tests:
  - Load font table from clib.hlp, verify font count > 0
  - Verify known font entries (e.g., monospace font for code examples)

---

# Phase 5 — RST Writer

# Task ID: 14
# Title: Document model assembly (HelpFile construction)
# Status: pending
# Dependencies: 7, 8, 9, 10, 11
# Priority: P1
# Description: Wire all parsed components together into the top-level HelpFile
#   document model defined in PROPOSAL.md.
# Details:
This is the integration task that connects:
  - Container reader (task 2) → raw internal files
  - System info (task 3) → title, root topic
  - Decompression (task 6) → raw topic stream
  - Topic parser (tasks 7-9) → Topic structs with Block/Inline content
  - Context map (task 10) → link target resolution
  - Title index (task 11) → topic ordering
  - Keywords (task 12) → keyword annotations
  - Fonts (task 13) → semantic formatting

Implementation: winhelp/src/lib.rs
  - `HelpFile::from_path(path: &Path) -> Result<Self>`
  - Orchestrates all parsing steps, resolves cross-references
  - Validates: all links resolve, no orphan topics, title/context consistency

Tests:
  - Load clib.hlp into HelpFile, verify topic count matches helpdeco
  - Verify all inter-topic links resolve to valid target topics
  - Verify root topic is set correctly

---

# Task ID: 15
# Title: Per-topic RST file generation
# Status: pending
# Dependencies: 14
# Priority: P1
# Description: Convert each Topic in the document model to a standalone .rst
#   file following the output conventions in PROPOSAL.md.
# Details:
RST output conventions:
  - Filename: `{context_id}.rst`
  - First line: `.. _{context_id}:` (RST label for cross-referencing)
  - Second line: `.. index:: keyword1, keyword2` (if keywords present)
  - Title: RST heading with `=` underline
  - Body: Block/Inline → RST syntax:
    * Paragraph → blank-line-separated text
    * Bold → `**text**`
    * Italic → `*text*`
    * Link (jump) → `:ref:\`target_id\``
    * Link (popup) → `:ref:\`text <target_id>\`` with annotation
    * Image → `.. image:: _images/{filename}.png`
    * Table → RST grid table or list-table directive

Popup handling: emit as a separate labeled block at the end of the file
with `.. note::` directive, linked from the inline reference.

Implementation: hlp2rst/src/rst.rs
  - `write_topic(topic: &Topic, output_dir: &Path) -> Result<()>`
  - Handle RST escaping (backslash-escape *, \`, |, etc. in text)

Tests:
  - Convert a simple topic, verify valid RST (parseable by docutils)
  - Convert a topic with links, verify :ref: syntax
  - Convert a topic with images, verify .. image:: directive
  - Convert a topic with bold/italic, verify inline markup

---

# Task ID: 16
# Title: index.rst and toctree generation
# Status: pending
# Dependencies: 15
# Priority: P1
# Description: Generate the top-level index.rst with Sphinx toctree containing
#   all topics in browse-sequence order.
# Details:
index.rst structure:

  {title}
  ========

  .. toctree::
     :maxdepth: 2
     :caption: Contents

     topic1
     topic2
     ...

Topic ordering: use browse sequence from |TTLBTREE (task 11). Topics without
browse sequence go at the end, alphabetically by title.

Also generate conf.py for Sphinx:
  - project = title from |SYSTEM
  - copyright = copyright from |SYSTEM
  - extensions = [] (no extensions needed for basic RST)

Implementation: hlp2rst/src/rst.rs (extend)
  - `write_index(helpfile: &HelpFile, output_dir: &Path) -> Result<()>`
  - `write_conf_py(helpfile: &HelpFile, output_dir: &Path) -> Result<()>`

Tests:
  - Generate index.rst from clib.hlp, verify all topics in toctree
  - Verify toctree order matches browse sequence
  - Verify conf.py is valid Python (importable)

---

# Task ID: 17
# Title: Image extraction and BMP → PNG conversion
# Status: pending
# Dependencies: 14
# Priority: P1
# Description: Extract embedded bitmap images from the HLP file and convert
#   BMP format to PNG for RST image directives.
# Details:
Images in HLP files are stored as internal files (named by their original
filename, e.g., "setup.bmp"). The opcode parser (task 8) records image
references as `ImageRef { filename, placement }`.

Most images are Windows BMP format. RST/Sphinx works best with PNG.

Implementation: hlp2rst/src/main.rs or hlp2rst/src/images.rs
  - Extract image internal files via container reader
  - Use `image` crate to decode BMP and encode as PNG
  - Write to `{output_dir}/_images/{filename}.png`
  - Create `_images/` directory automatically

winhelp/src/bitmap.rs (library side):
  - `extract_bitmap(container: &HlpContainer, name: &str) -> Result<Vec<u8>>`
  - Handle BMP with missing BITMAPFILEHEADER (HLP BMPs sometimes omit it)

Tests:
  - Extract a known image from fixture file, verify valid PNG output
  - Verify all image references in topics have corresponding extracted files
  - Verify BMP → PNG conversion produces correct dimensions

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
