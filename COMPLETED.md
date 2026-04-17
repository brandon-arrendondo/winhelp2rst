# winhelp — Completed Tasks

Tasks moved here from PLAN.md after implementation. Each task records the
implementation file(s) and test count at time of completion.

---

# Phase 0 — Project Scaffolding

# Task ID: 1
# Title: Workspace and crate scaffolding
# Status: done
# Dependencies: none
# Priority: P0
# Description: Create Cargo workspace with `winhelp` library crate and `hlp2rst`
#   binary crate. Set up pre-commit hooks, CI-ready coverage gate, and initial
#   test infrastructure.
# Implementation:
#   Cargo.toml (workspace root), winhelp/Cargo.toml, hlp2rst/Cargo.toml,
#   winhelp/src/lib.rs (document model + 3 unit tests),
#   winhelp/src/error.rs (Error enum with thiserror),
#   hlp2rst/src/main.rs (clap CLI skeleton),
#   tests/fixtures/ directory
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
# Status: done
# Dependencies: 1
# Priority: P0
# Description: Parse the HLP file header, locate the internal directory B-tree,
#   and enumerate all internal file entries by name and offset.
# Implementation:
#   winhelp/src/container.rs — HlpContainer, InternalFile, B-tree traversal
#   11 unit tests (synthetic HLP files)
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
# Status: done
# Dependencies: 2
# Priority: P0
# Description: Parse the |SYSTEM internal file to extract help file title,
#   copyright string, and root topic context ID.
# Implementation:
#   winhelp/src/system.rs — SystemInfo struct, record parser
#   8 unit tests (synthetic |SYSTEM data)
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
# Status: done
# Dependencies: 2
# Priority: P0
# Description: Parse the phrase dictionary and implement phrase substitution
#   (first-pass decompression for topic text).
# Implementation:
#   winhelp/src/decompress.rs — PhraseTable (inline + index variants)
#   7 unit tests (synthetic phrase tables + expansion)
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
# Status: done
# Dependencies: 2
# Priority: P0
# Description: Implement the WinHelp LZ77 sliding-window decompression used
#   for |TOPIC blocks.
# Implementation:
#   winhelp/src/decompress.rs — lz77_decompress()
#   5 unit tests (literals, back-references, window init)
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
# Status: done
# Dependencies: 4, 5
# Priority: P0
# Description: Read the |TOPIC internal file as a sequence of blocks, apply
#   LZ77 decompression and phrase substitution, and produce raw topic records.
# Implementation:
#   winhelp/src/topic.rs — read_topic_blocks(), flatten_topic_stream(),
#   extract_records()
#   6 unit tests (block reading, flattening, record extraction)
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
# Status: done
# Dependencies: 6
# Priority: P0
# Description: Parse topic record headers (type, size, linked-list pointers)
#   and extract footnote metadata (# context, $ title, K keywords, + browse).
# Implementation:
#   winhelp/src/topic.rs — parse_topic_metadata(), TopicMetadata struct
#   3 unit tests (all footnotes, empty record, context-only)
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
# Status: done
# Dependencies: 7
# Priority: P0
# Description: Parse the binary opcode stream within topic text records into
#   the document model (paragraphs, formatted text, links, images).
# Implementation:
#   winhelp/src/opcode.rs — parse_text_record(), opcode constants
#   13 unit tests (plain text, bold, italic, links, images, mixed content)
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

Implementation: winhelp/src/opcode.rs
  - Parse opcodes into `Block` and `Inline` enum variants from PROPOSAL.md
  - `parse_text_record(data: &[u8], phrases: &PhraseTable,
     fonts: &FontTable) -> Result<Vec<Block>>`

Tests:
  - Parse a known simple topic (plain text only), verify output
  - Parse a topic with bold/italic, verify inline formatting
  - Parse a topic with hyperlinks, verify link targets match ground truth
  - Parse a topic with images, verify image references extracted

---

# Phase 4 — Index Files

# Task ID: 10
# Title: |CONTEXT B-tree reader
# Status: done
# Dependencies: 2
# Priority: P1
# Description: Parse the |CONTEXT B-tree to build the mapping from context
#   string hash → topic byte offset.
# Implementation:
#   winhelp/src/context.rs — ContextMap, context_hash()
#   8 unit tests (hash function, map CRUD, bad magic)
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
# Status: done
# Dependencies: 2
# Priority: P1
# Description: Parse |TTLBTREE to get the ordered topic offset → title mapping.
# Implementation:
#   winhelp/src/font.rs — TitleIndex struct, B-tree traversal
#   5 unit tests (basic, sorted, empty, bad magic, missing offset)
# Details:
|TTLBTREE is a B-tree mapping u32 topic offsets to null-terminated title
strings. This provides the canonical topic ordering for building index.rst.

Implementation: winhelp/src/font.rs (TitleIndex)
  - `TitleIndex::from_bytes(data: &[u8]) -> Result<Self>`
  - `TitleIndex::titles_in_order() -> Vec<(u32, String)>`

Tests:
  - Load |TTLBTREE from clib.hlp, verify entry count matches topic count
  - Verify title strings match footnote-extracted titles from task 7
  - Verify ordering matches browse sequence

---

# Task ID: 13
# Title: |FONT table reader
# Status: done
# Dependencies: 2
# Priority: P2
# Description: Parse the |FONT internal file to extract font definitions used
#   by the opcode parser for semantic formatting decisions.
# Implementation:
#   winhelp/src/font.rs — FontTable, FontDescriptor
#   3 unit tests (simple parse, empty, flags)
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
# Status: done
# Dependencies: 7, 8, 9, 10, 11
# Priority: P1
# Description: Wire all parsed components together into the top-level HelpFile
#   document model defined in PROPOSAL.md.
# Implementation:
#   winhelp/src/lib.rs — HelpFile::from_path(), from_container()
#   Orchestrates container → system → decompression → topic parsing → assembly
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
# Status: done
# Dependencies: 14
# Priority: P1
# Description: Convert each Topic in the document model to a standalone .rst
#   file following the output conventions in PROPOSAL.md.
# Implementation:
#   hlp2rst/src/rst.rs — write_topic(), write_block(), write_inline()
#   7 unit tests (file creation, labels, bold, links, images, escaping)
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
# Status: done
# Dependencies: 15
# Priority: P1
# Description: Generate the top-level index.rst with Sphinx toctree containing
#   all topics in browse-sequence order.
# Implementation:
#   hlp2rst/src/rst.rs — write_index(), write_conf_py(), write_all()
#   3 unit tests (toctree content, conf.py validity, sanitization)
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

# Phase 3 — Topic Opcode Parser (continued)

# Task ID: 9
# Title: Hyperlink and popup opcode handling
# Status: done
# Dependencies: 8
# Priority: P1
# Description: Fully resolve hyperlink and popup opcodes to target context
#   strings, distinguishing jump links from popup links.
# Implementation:
#   winhelp/src/opcode.rs — hash resolution via HashMap<u32, String>,
#     resolve_hash_target() helper, updated parse_text_record() signature
#   winhelp/src/lib.rs — two-pass assembly: first collect topic metadata to
#     build hash→context_id map, then parse text records with the resolver
#   4 new tests (resolved jump, resolved popup, unresolved fallback, mixed)
# Details:
Hyperlinks encode as: link start opcode (0xE3/0xE6) + u32 context hash +
link text + link end opcode (0x89). The hash is resolved to the actual
context string by building a HashMap<u32, String> from all parsed topic
metadata context IDs (via context_hash()). Unresolvable hashes fall back
to hex format (e.g., "0xDEADBEEF").

Two-pass approach in lib.rs:
  1. First pass: collect all topic header records, parse metadata, build
     hash→context_id map
  2. Second pass: parse text records with the resolver, group into topics

---

# Phase 4 — Index Files (continued)

# Task ID: 12
# Title: |KWBTREE / |KWDATA keyword index reader
# Status: done
# Dependencies: 2
# Priority: P2
# Description: Parse keyword index B-trees to extract keyword → topic mappings
#   for RST index directives.
# Implementation:
#   winhelp/src/keyword.rs — KeywordIndex (B-tree parser), RawKeywordEntry,
#     build_keyword_index() (from topic metadata), KwBTreeCtx
#   winhelp/src/lib.rs — wired build_keyword_index() into HelpFile assembly
#   8 tests (B-tree parse, topic metadata build, empty, bad magic, EOF)
# Details:
Two approaches implemented:
  1. KeywordIndex::from_bytes(kwbtree, kwdata) — parses |KWBTREE B-tree with
     string keys and u32 offsets into |KWDATA (u16 count + u32[] topic offsets)
  2. build_keyword_index(topics) — inverts per-topic K footnote keywords into
     KeywordEntry { keyword, topic_ids } in alphabetical order

lib.rs uses approach #2 (topic metadata) for integration since it doesn't
require complex offset→context_id resolution. The B-tree parser is available
for validation against real HLP files.

---

# Phase 5 — RST Writer (continued)

# Task ID: 17
# Title: Image extraction and BMP → PNG conversion
# Status: done
# Dependencies: 14
# Priority: P1
# Description: Extract embedded bitmap images from the HLP file and convert
#   BMP format to PNG for RST image directives.
# Implementation:
#   winhelp/src/bitmap.rs — extract_bitmap(), ensure_bmp_header(),
#     prepend_bmp_file_header(), compute_palette_size()
#   winhelp/src/lib.rs — HelpFile.images field (HashMap<String, Vec<u8>>),
#     image collection during from_container()
#   hlp2rst/src/rst.rs — write_image() (BMP→PNG via image crate),
#     swap_extension(), updated image directives to .png
#   7 new bitmap tests + 3 new RST tests (BMP→PNG, embedded, swap_extension)
# Details:
Library side (bitmap.rs):
  - extract_bitmap() reads image internal files from HLP container
  - ensure_bmp_header() detects missing BITMAPFILEHEADER and prepends one
  - Handles BITMAPINFOHEADER (40), BITMAPCOREHEADER (12), V4 (108), V5 (124)
  - Computes palette size for 1/4/8-bit BMPs (RGBQUAD or RGB triples)

CLI side (rst.rs):
  - write_image() decodes BMP via image crate, re-encodes as PNG
  - Falls back to raw file copy if BMP decoding fails
  - Image directives now reference .png extension instead of .bmp
  - HelpFile.images stores raw BMP data extracted during parsing

---

# Phase 3b — Opcode Parser Fixes

# Task ID: 25
# Title: Fix opcode parser text extraction quality
# Status: done
# Dependencies: 8, 14
# Priority: P1
# Description: Rewrite the topic opcode parser around the correct format:
#   LinkData1 holds the command stream, LinkData2 holds NUL-delimited text
#   segments. The original parser (task 8) treated LinkData2 as a command
#   stream and consequently lost all formatting, links, and paragraph breaks.
# Implementation:
#   winhelp/src/opcode.rs — total rewrite of parse_text_record() with the
#     LinkData1 + LinkData2 split. SegCursor pulls text segments from LD2 as
#     LD1 commands consume them. ParseState tracks bold/italic/underline,
#     link kind, and link hash. find_command_stream_start() heuristically
#     skips the paragraph info header (9E 48 tab marker + u16 tab values,
#     or a fixed-size preamble when no tabs).
#   winhelp/src/lib.rs — updated call site to pass both link_data1 and
#     link_data2 to parse_text_record().
#   10 opcode tests covering: single paragraph, two paragraphs, jump link,
#     popup link, resolved hash, bold toggle, end-of-record, tab preamble
#     skip, multi-link lists, consecutive code lines.
# Details:
Validated against clib.hlp Win16: previously 0 :ref: directives across 709
topic files; now 2932 :ref: directives across 631 files. All five issue
classes from the task description resolved:

1. Word/link separation — links now carry proper display text consumed
   from LD2 by the 0x89 link-end opcode, instead of being silently dropped.

2. Code-block line separation — `#include <stdlib.h>` and `void abort( void );`
   correctly render as separate paragraphs in the abort topic, following
   the 0x82 end-paragraph opcode boundary in LD1.

3. Empty link display text — resolved by the architectural fix: LD2
   segments are now correctly consumed in order by commands that touch text.

4. Opcode byte leakage — commands are now parsed from LD1 (which contains
   binary opcodes) rather than LD2 (which is plain text separated by NULs),
   so opcode bytes no longer leak into rendered output as ASCII.

5. Paragraph breaks — 0x82 end-paragraph opcodes in LD1 are now processed,
   producing distinct paragraphs between Synopsis/Description/Returns/etc.

Format notes recorded in opcode.rs module doc:
  - LD1 paragraph-info header preamble (variable length, heuristic skip)
  - Command stream: 0x80 (font), 0x81 (line break), 0x82 (end paragraph),
    0x83 (tab), 0x86/0x87 (bold on/off), 0x88 (italic on), 0x89 (italic off
    or link end), 0x8B/0x8C (underline), 0xC8/0xCC/0xE3/0xE6 (links),
    0xFF (end of record).
  - Each text-emitting or link-wrapping command consumes exactly one
    NUL-delimited segment from LD2.
  - 0x89 is overloaded: in link context, it ends the link and consumes
    the next segment as the link's display text.

Known limitations (deferred to later tasks):
  - Font-based semantic styling (e.g. monospace → RST ``code``) is not
    applied — fonts[_fonts] parameter is reserved for future use.
  - |FONT table parsing has its own pre-existing bugs (truncated names)
    which would need fixing before font-based styling is reliable.
  - Windows-1252 → UTF-8 transcoding not implemented; high-bit bytes like
    the non-breaking space (0xA0) render as UTF-8 replacement characters.

---

# Test Data

# Task ID: 24
# Title: Obtain test fixtures and helpdeco ground truth
# Status: done
# Dependencies: none
# Priority: P0
# Description: Obtain primary test fixture (OpenWatcom clib.hlp) and the
#   supporting fixture set for real-world validation of the parser.
# Implementation:
#   tests/fixtures/c_hlp_win.zip — Win16 (WinHelp 3.1) release zip
#   tests/fixtures/c_hlp_nt.zip  — Win32 (WinHelp 4.0) release zip
#   tests/fixtures/clib_hlp/win16/binw/{clib,clr,wccerrs,cguide,cmix,
#     c_readme}.hlp — unpacked Win16 HLP files
#   tests/fixtures/clib_hlp/win32/binnt/{clib,clr,wccerrs,cguide,cbooks,
#     c_readme}.hlp (+ matching .cnt contents files) — unpacked Win32 HLP files
#   tests/fixtures/clib_hlp/{win16,win32}/license.txt — Sybase Open Watcom
#     Public License redistribution notice
# Details:
Primary fixture (clib.hlp, C Library Reference) is used by the integration
tests in phases 1–5. Additional fixtures (clr.hlp, wccerrs.hlp, etc.) are
available for stress testing and diverse-content validation as those tasks
land. `helpdeco` ground-truth regeneration is deferred to per-task validation
where applicable — the fixture archive contains enough raw input that synthetic
parsers can be cross-checked against real decompilation on demand.

---

# Phase 8 — CLI Polish and Distribution

# Task ID: 22
# Title: End-to-end Sphinx round-trip test
# Status: done
# Dependencies: 16
# Priority: P2
# Description: Verify the full round-trip: HLP → RST → Sphinx HTML build
#   completes without errors or warnings.
# Implementation:
#   hlp2rst/src/rst.rs — neutralize_transition_line() escapes lines whose
#     stripped content is 4+ uniform punctuation chars (`-`, `=`, `~`, `^`,
#     `*`, `+`, `#`), preventing docutils from parsing ASCII-art dividers in
#     WinHelp content as section-title underlines or transitions. Wired into
#     Block::Paragraph rendering.
#   4 new hlp2rst tests covering: pure-dashes escape, equals/tilde detection,
#     rejection of mixed/short/prose lines, end-to-end escape on write.
# Details:
Validation procedure (Sphinx 8.0.2, clib.hlp Win16):

  cargo build --release
  ./target/release/hlp2rst tests/fixtures/clib_hlp/win16/binw/clib.hlp \
      /tmp/hlp2rst_output                        # → 709 topic .rst files
  sphinx-build -b html -W --keep-going \
      /tmp/hlp2rst_output /tmp/hlp2rst_output/_build/html      # → build succeeded.
  sphinx-build -b htmlhelp -W --keep-going \
      /tmp/hlp2rst_output /tmp/hlp2rst_output/_build/htmlhelp  # → build succeeded.

Results:
  - HTML build: 710 input pages → 712 HTML output files, zero warnings.
  - htmlhelp build: emits watcomclibraryreferencehelpdoc.{hhp,hhc,hhk},
    zero warnings.
  - All :ref: cross-references resolve (Sphinx -W would have errored otherwise).

Before the fix, 18 CRITICAL `Unexpected section title or transition` errors
fired on ASCII-art divider paragraphs in MMX intrinsic topics (e.g.
_m_packuswb, _m_punpcklwd). The fix inserts a backslash before the first
punctuation char on matching lines, breaking docutils' uniform-run detection
while preserving visible content.

Discovered during validation (filed as Task 26): the opcode parser did not
recognize image-reference opcodes, so clib.hlp's |bm* bitmap internal files
were never surfaced as Block::Image variants and no PNGs were written. The
round-trip therefore passed vacuously for the "all images load" criterion
(0 referenced, 0 failed). Task 26 resolves this.

---

# Phase 3c — Image Pipeline

# Task ID: 26
# Title: Emit image block references from opcode parser
# Status: done
# Dependencies: 8, 17
# Priority: P2
# Description: Recognise image-embed opcodes (0x86=bmc, 0x87=bml, 0x88=bmr)
#   in the LD1 command stream, resolve them to `|bmN` internal-file names,
#   and emit Block::Image variants so the RST writer produces .. image::
#   directives and the matching PNG files.
# Implementation:
#   winhelp/src/opcode.rs — full rewrite of find_command_stream_start() to
#     parse the structured TL_NORMAL preamble (scanlong + scanword + 4 skip +
#     u16 bitflags + per-flag scanint/scanword + tab-stop loop). This was
#     necessary because the previous 9E/48 + low-byte heuristic returned
#     mid-tab-stop offsets on any record with tab stops whose low bytes
#     were ≥ 0x80, which happens in almost every multi-link list — the
#     parser was consequently reading tab-stop bytes as commands.
#   winhelp/src/opcode.rs — image opcode handler parses type byte (0x22
#     HC31, 0x03 HC30, 0x05 embedded window), scanlong payload size,
#     optional scanword hotspot count, and the two u16s PictureIsEmbedded
#     + PictureNumber. External-baggage pictures (is_embedded=0) emit
#     Block::Image { filename = "|bmN", placement } where placement maps
#     bmc→Inline, bml→Left, bmr→Right.
#   winhelp/src/opcode.rs — scan_long/scan_word/scan_int helpers
#     mirroring helpdeco's scanlong/scanword/scanint with correct bias
#     subtraction (0x4000 / 0x40000000).
#   winhelp/src/bitmap.rs — new mrb_to_bmp() decodes the MRB
#     container ('lp'/'lP' magic) by picking the first DIB picture,
#     reconstructing BITMAPFILEHEADER + BITMAPINFOHEADER + palette +
#     decompressed pixels (LZ77 via existing lz77_decompress, or raw).
#   hlp2rst/src/rst.rs — image_output_name() / sanitize_image_stem()
#     strip the leading `|` from WinHelp internal-file names and replace
#     other filesystem-hostile characters, producing paths like
#     `_images/bm0.png` that work on Windows and match the .. image::
#     directive.
#   winhelp/src/font.rs — from_descriptors() test helper for the
#     regression test documenting that font-attribute-driven styling is
#     currently a no-op (tracked as follow-up Task 27).
#
#   5 new opcode tests (image emission for all 3 placements, embedded
#   variant, scanlong decode) + 1 font-change regression test + 1 image
#   output-name test. 120 tests total pass.
# Details:
Validation (Sphinx 9.1.0, venv, clib.hlp Win16):
  cargo build --release
  ./target/release/hlp2rst tests/fixtures/clib_hlp/win16/binw/clib.hlp \
      /tmp/hlp2rst_output
  sphinx-build -b html -W --keep-going \
      /tmp/hlp2rst_output /tmp/hlp2rst_output/_build/html   → 0 warnings
  sphinx-build -b htmlhelp -W --keep-going \
      /tmp/hlp2rst_output /tmp/hlp2rst_output/_build/htmlhelp → 0 warnings

Results:
  - 21 external-bitmap Block::Image variants emitted (one per |bmN file)
  - 21 PNG files written to _images/ (via MRB→BMP→PNG pipeline)
  - 19 .rst topic files reference images through .. image:: or .. figure:: directives
  - All images copied into _build/html/_images/ by Sphinx
  - Example: _images/bm0.png — 649×519 8-bit RGB PNG

Known deferrals:
  - Font-attribute styling (bold/italic/underline from FontDescriptor
    attributes) remains a no-op — naïve application over-wraps clib.hlp
    content in italic. Tracked as Task 27.
  - DDB (type=5), metafile (type=8), and RunLen-packed bitmaps are
    rejected by mrb_to_bmp() and fall back to raw bytes. clib.hlp has
    none of these, so the gap is theoretical. Tracked as Task 28.

---

# Phase 7 — WinHelp 4.0 (Win95) Support

# Task ID: 20
# Title: WinHelp 4.0 format differences
# Status: done
# Dependencies: 14
# Priority: P2
# Description: Handle WinHelp 4.0 (HCW 4.00 / Win95) format deltas so the
#   parser reaches topic/image parity with WinHelp 3.1 on real fixtures.
# Implementation:
#   winhelp/src/decompress.rs — new `PhraseTable::from_hall()` constructor
#     that reads the 28-byte PHRINDEXHDR, LZ77-decompresses |PhrImage when
#     phrimagesize != phrimagecompressedsize, and decodes cumulative phrase
#     offsets from the Golomb-style bitstream (helpdeco.c:1862-1898).
#   winhelp/src/decompress.rs — `PhrIndexHeader` (struct) and `BitReader`
#     (little-endian DWORD-oriented, LSB-first, mirrors helpdec1.c:573).
#   winhelp/src/decompress.rs — `expand_hall()` rewritten to match
#     helpdeco.c:2442-2483 exactly, handling all 5 byte classes: single-
#     byte phrase ref, two-byte phrase ref, literal run `(ch >> 3) + 1`,
#     space run `(ch >> 4) + 1`, NUL run `(ch >> 4) + 1`. Fixes an off-
#     by-one that under-counted every repetition family.
#   winhelp/src/lib.rs — phrase-table loader prefers |PhrIndex + |PhrImage
#     when both are present; falls back to |Phrases otherwise.
#   winhelp/src/lib.rs — TOPICOFFSET arithmetic now uses the `scanword`
#     from each text record's LinkData1 head (exposed via new
#     `opcode::topic_offset_delta`), not raw LinkData2 length. The help
#     compiler stores the phrase-expanded character count explicitly; raw
#     LD2 length is shorter on Hall-compressed files and was losing 184
#     of 711 topic IDs on the Win32 fixture.
#   winhelp/src/lib.rs — context-hash consumption happens only inside
#     TL_TOPICHDR processing (matches helpdeco.c:3336); the previous
#     TL_DISPLAY branch was silently consuming entries that should have
#     landed on the next topic. Multi-alias consumption now collects all
#     overflow hashes into `Topic::aliases`.
#   winhelp/src/lib.rs, winhelp/src/topic.rs — `Topic` and `TopicMetadata`
#     gain an `aliases: Vec<String>` field for the extra context-string
#     mappings WinHelp 4.0 files emit (block-boundary markers alongside
#     real IDs).
#   winhelp/src/bitmap.rs — new `derun()` RunLen byte-stream decoder
#     (helpdeco splitmrb.c:141-162). `mrb_to_bmp()` now dispatches on
#     `by_packed & 0b11` covering all four packing methods: 0=raw,
#     1=RunLen, 2=LZ77, 3=LZ77-then-RunLen (the Win32 clib.hlp case).
#   hlp2rst/src/rst.rs — new `write_alias_stub()` emits a `.rst` file per
#     topic alias containing a single `.. _alias:` label + `:ref:` link
#     to the primary id. Keeps alias links resolvable under Sphinx without
#     duplicate-label warnings.
#   hlp2rst/src/rst.rs — `neutralize_transition_line()` expanded to cover
#     every docutils-recognised punctuation underline char (`"`, `:`, `;`,
#     ...), not just the original `-=~^*+#` set. The Win32 fixture has
#     ASCII-art dividers using `"""` that were tripping Sphinx.
#   hlp2rst/src/rst.rs — `py_string()` now escapes `\r` and `\n` so the
#     multi-line copyright field in |SYSTEM produces a valid `conf.py`.
#   winhelp/tests/fixtures_clib.rs — new integration test harness that
#     parses both the Win16 and Win32 clib.hlp fixtures and asserts
#     topic-count parity (711 topics each), ≥ 709 resolved context ids,
#     and 21 PNG-decodable bitmaps per fixture. Gates regression of the
#     whole Task 20 change set.
#
#   6 new Hall phrase tests + 5 new derun tests + 3 integration tests.
#   131 unit tests total pass. Coverage: 88.56% line (threshold 75%).
# Details:
Validation (Sphinx 9.1.0, venv):
  ./target/release/hlp2rst tests/fixtures/clib_hlp/win32/binnt/clib.hlp \
      /tmp/hlp2rst_win32
  sphinx-build -b html -W --keep-going \
      /tmp/hlp2rst_win32 /tmp/hlp2rst_win32/_build/html → 0 warnings
  sphinx-build -b htmlhelp -W --keep-going \
      /tmp/hlp2rst_win32 /tmp/hlp2rst_win32/_build/htmlhelp → 0 warnings

Win16 regression: same 709 named topics, clean `sphinx-build -b html`.

Results on Win32 clib.hlp:
  - 711 topics parsed (parity with Win16)
  - 700 primary ids + 9 alias stubs → 709 resolvable `:ref:` targets
  - 21 bitmaps decoded via LZ77-then-RunLen (byPacked=3) and saved as PNG
  - Same `ctx_{hash:08x}.rst` filename set as Win16 (zero-diff `comm`)

Known deferrals still pending:
  - |VIOLA (window-assignment index for `>` footnotes), |Rose (macro
    keyword lookup), |TopicId, |Petra — none affect text or image
    extraction; skipped.
  - New WinHelp 4.0 |SYSTEM record types (9 LCID, 10 CNT filename, 11
    CHARSET, 12 DEFFONT, 14 KEYINDEX, 18 LANGUAGE, 19 DLLMAPS) are
    parsed as unknown and ignored. Not required for round-trip, but
    could be exposed later for richer metadata.
  - NEWFONT / MVBFONT variants in |FONT (4.0 font-record formats with
    Height, Weight, Italic/Underline/StrikeOut fields) — current parser
    handles only OLDFONT. Affects font-attribute styling (Task 27), not
    text extraction.

---

# Task ID: 19
# Title: WMF (Windows Metafile) handling
# Status: done
# Dependencies: 17
# Priority: P3
# Description: Detect Windows Metafile pictures inside MRB containers
#   (type=8) and extract them as self-contained `.wmf` files prefixed with
#   an Aldus Placeable Metafile header. Vector data is not rasterised;
#   the RST writer emits an `.. image::` directive plus a caveat comment
#   that the format is unconverted.
# Implementation:
#   winhelp/src/bitmap.rs — new `mrb_to_wmf()` mirrors the type=8 branch
#     of helpdeco/src/splitmrb.c lines 511-573: reads the metafile picture
#     header (mapping_mode CWord, width/height u16 LE, wcaller_inch CDWord,
#     dwDataSize CDWord, dwHotspotSize CDWord, plus dwPictureOffset and
#     dwHotspotOffset u32), decompresses the payload, and prepends a
#     22-byte APM header with checksum = XOR of the first 10 LE words.
#   winhelp/src/bitmap.rs — `decompress_packed()` factored out of
#     `mrb_to_bmp()` so both DIB and metafile branches share the same
#     four-method dispatcher (raw / RunLen / LZ77 / LZ77-then-RunLen).
#   winhelp/src/bitmap.rs — `is_wmf()` helper recognises the Aldus magic
#     `D7 CD C6 9A` for standalone metafile streams (MRB unwrap also
#     produces this signature, so the writer needs only one detector).
#   winhelp/src/bitmap.rs — `extract_bitmap()` dispatches to `mrb_to_wmf`
#     when `mrb_to_bmp` rejects the type byte, and treats already-APM
#     bytes (no MRB wrapper) as a pass-through.
#   winhelp/src/lib.rs — re-exports `mrb_to_wmf`, `is_wmf`, `APM_MAGIC`.
#   hlp2rst/src/rst.rs — new internal `ImageOutFormat::{Png, Wmf}` enum,
#     per-filename format map computed inside `write_all()` and threaded
#     through `write_topic()` → `write_block()`. WMF images are saved
#     verbatim under `.wmf` (no BMP→PNG attempt) and the topic emits both
#     a `.. image:: _images/{stem}.wmf` directive and an `.. WMF (Windows
#     Metafile) ...` comment flagging the unconverted vector format.
#   hlp2rst/src/rst.rs — `image_output_name_with(filename, format)`
#     replaces the old PNG-only helper so the directive extension matches
#     the persisted file.
#
#   8 new tests: 6 unit tests in bitmap.rs (raw/RunLen MRB metafile
#   decode, type-discriminator gating, Aldus-magic detection, end-to-end
#   round-trip), 3 writer tests in rst.rs (WMF write_image return value,
#   topic emits .wmf directive + caveat comment, mixed BMP+WMF in one
#   HelpFile).  119 unit tests total pass; coverage 88.87% line.
# Details:
Validation:
  cargo test --workspace                      → 144 tests pass
  cargo clippy --workspace --all-targets -- -D warnings → clean
  cargo fmt --all -- --check                  → clean
  cargo llvm-cov --fail-under-lines 75        → 88.87% line, 91.69% func
  ./target/release/hlp2rst clib.hlp out/      → 711 topics, 21 PNGs
  sphinx-build -b html -W --keep-going        → 0 warnings (Win16, Win32)

Survey of fixtures showed all 21 clib.hlp bitmaps are MRB type=6 DIB —
the WMF code path is exercised only by synthetic test data, since none
of the OpenWatcom .hlp files ship with vector pictures. The MVP scope
deliberately stops at extraction; a future pass could plug in a wmf2svg
converter or rasterise via libwmf if a real fixture surfaces a metafile
that needs to render in HTML.

Known deferrals:
  - No vector rendering pipeline: Sphinx HTML still won't display the
    `.wmf` files — this is the documented MVP behaviour and the caveat
    comment in each topic file makes that explicit to readers of the
    source.
  - Bare METAHEADER streams (no Aldus wrapper) aren't auto-detected —
    their leading bytes are too generic to sniff safely. Real WinHelp
    files always wrap pictures in MRB containers, so this gap is
    theoretical.

---

# Task ID: 29
# Title: Parse TL_TABLE records (record type 0x23)
# Status: done
# Dependencies: 25
# Priority: P2
# Description: WinHelp TL_TABLE records have a different LinkData1 layout
#   from TL_DISPLAY — a table header (column count, flags, column geometry)
#   followed by per-cell preambles each containing their own command stream.
#   Feeding TL_TABLE bytes through the existing parse_text_record() mis-syncs
#   on the structural bytes and emits garbled text. This task adds a dedicated
#   parser that iterates cells and flattens them into sequential paragraphs.
# Implementation:
#   winhelp/src/opcode.rs — new parse_table_record() function: reads table
#     header (cols, flags, column widths), then loops over cells (lastcol
#     sentinel, pad, unknown, bitflags, conditional fields, command stream).
#     Each cell's command stream shares the LD2 SegCursor with previous cells.
#
#   winhelp/src/opcode.rs — refactored parse_command_stream() into a thin
#     wrapper around new run_command_stream(), which takes caller-owned
#     ParseState + SegCursor and returns bytes consumed. This lets TL_TABLE
#     cells share state and cursor across multiple command-stream runs.
#
#   winhelp/src/opcode.rs — extracted skip_paragraph_info_fields() helper
#     so both TL_DISPLAY headers and TL_TABLE cell headers reuse the same
#     bitflag-conditional field-skip logic.
#
#   winhelp/src/opcode.rs — semantic fix at 0xFF (end-of-record) opcode:
#     the LD2 cursor now consumes one segment before breaking, matching
#     helpdeco's emit-then-process loop. This keeps the cursor in sync
#     across cell boundaries; without it, the first opcode of each
#     subsequent cell eats the trailing segment of the previous cell and
#     link display text shifts by one.
#
#   winhelp/src/lib.rs — HelpFile::load() now dispatches RECORD_TYPE_TABLE
#     to parse_table_record() instead of parse_text_record().
#
#   1 new test: table_record_two_cells_preserve_link_display_text — builds
#     a synthetic two-cell TL_TABLE with links and verifies LD2 cursor
#     stays in sync across cell boundaries.
#
#   145 tests total pass; coverage 88.78% line, 91.78% func.
# Details:
Validation:
  cargo test --workspace                      → 145 tests pass
  cargo clippy --workspace --all-targets -- -D warnings → clean
  cargo fmt --all -- --check                  → clean
  cargo llvm-cov --fail-under-lines 75        → 88.78% line, 91.78% func
  ./target/release/hlp2rst clib.hlp out/      → 711 topics, 21 PNGs
  sphinx-build -b html -W --keep-going        → 0 warnings (Win16, Win32)

Cells are flattened into sequential paragraphs rather than RST list-table
because WinHelp tables support variable column spans, per-cell paragraph
breaks, and nested images that don't map cleanly to RST's rigid table
directives. Linear paragraphs preserve all content and links.

clib.hlp's TL_TABLE records (if any) were previously parsed by
parse_text_record() which swallowed the table header bytes as if they
were paragraph-info fields — the output was garbled but non-crashing.
With the dedicated parser, the same records produce correct paragraphs.

Known limitations:
  - No RST table rendering: cells are emitted as flat paragraphs. A future
    enhancement could detect simple N-column tables and emit RST grid or
    list-table markup, but the variable-span/nested-image cases would still
    need the flat fallback.

---

# Task ID: 18
# Title: SHG (Segmented Hypergraphics) handling
# Status: done
# Dependencies: 17
# Priority: P2
# Description: Parse SHG (MRB pictures with non-zero HotspotSize), flatten
#   to a rendered BMP/WMF, and return the decoded hotspot list.  RST has
#   no image-map construct, so callers may surface the hotspots as comments
#   or discard them — the parser only extracts them.
# Implementation:
#   winhelp/src/bitmap.rs — new `parse_shg(data) -> Option<(Vec<u8>,
#     Vec<Hotspot>)>` sniffs the MRB magic, dispatches to `mrb_to_bmp` or
#     `mrb_to_wmf` for the rendered image, then locates and decodes the
#     hotspot block appended after the pixel/metafile payload.
#   winhelp/src/bitmap.rs — `extract_hotspot_bytes()` computes the hotspot
#     block slice sequentially (`pic_offset + header_len + palette + data_size`)
#     rather than trusting `dwHotspotOffset`, matching the existing DIB
#     pipeline's comment about the field being unreliable.
#   winhelp/src/bitmap.rs — `parse_hotspot_block()` decodes the on-disk
#     layout per helpfile.txt:1329-1367: `u8 magic(=0x01), u16 num_hotspots,
#     u32 macro_size, Hotspot[num_hotspots] (15 bytes each: id0,id1,id2,
#     x,y,w,h, hash), macro_data[macro_size], { STRINGZ name; STRINGZ target; }
#     [num_hotspots]`.  Unknown id triples are preserved as
#     `HotspotAction::Unknown(id0,id1,id2)` rather than rejected.
#   winhelp/src/bitmap.rs — new public types `HotspotRect { x, y, w, h }`,
#     `HotspotAction` (10 documented variants + Unknown), and `Hotspot {
#     rect, action, hash, name, target }`.  `HotspotAction::from_id` decodes
#     the id-byte triples per helpfile.txt:1350-1361.
#   winhelp/src/lib.rs — re-exports `parse_shg`, `Hotspot`, `HotspotAction`,
#     `HotspotRect`.
# Tests:
#   10 new unit tests in winhelp/src/bitmap.rs cover: metafile SHG end-to-end
#   (2 spots incl. macro + jump), DIB SHG end-to-end (1×1 raw bitmap, 1 spot),
#   zero-hotspot MRB still parses, non-MRB input rejected, the full 10-entry
#   id-triple decode table, Unknown preservation, block magic/truncation
#   failure paths, and empty-string names/targets.  The existing
#   `extract_bitmap` contract is unchanged — clib.hlp integration tests
#   still pass at 89.97% line coverage.
# Details:
The hotspot parser is standalone — `extract_bitmap` continues to return
just the image bytes so the RST writer keeps emitting one `.. image::`
directive per picture.  Callers that want to surface hotspot metadata
(future task: RST comments listing jump targets) can call `parse_shg`
directly; the infrastructure is now in place.

Notable layout choice: rather than reading `dwHotspotOffset` from the
MRB picture header — which `mrb_to_bmp`'s existing comment flags as
unreliable — the hotspot slice is located sequentially right after the
pixel/metafile payload.  Both DIB and metafile picture headers end with
the same `data_size` / `hotspot_size` / `dwPictureOffset` /
`dwHotspotOffset` quartet, so the sequential computation is straightforward.
