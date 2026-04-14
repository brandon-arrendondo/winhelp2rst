# Proposal: `winhelp` — Rust WinHelp HLP Parser and RST Converter

## Summary

Build a pure-Rust library crate (`winhelp`) that parses Windows WinHelp `.hlp` files directly
from binary with no external dependencies, plus a companion CLI (`hlp2rst`) that converts them
to reStructuredText. No dependency on `helpdeco` or any other tool.

The output RST is Sphinx-compatible, which means the round-trip
**WinHelp → RST → Sphinx → CHM** is available out of the box (Sphinx ships a `htmlhelp` builder
that produces `.chm`-ready output). This makes the tool useful for modernizing legacy Windows
documentation workflows as well as archival/research purposes.

---

## Why This Is Worth Building

- **Frozen format.** WinHelp 3.1 (1992) has no new versions. The implementation effort is fully
  bounded.
- **No good Rust (or Python) library exists.** The `winhelp` crate would be first-of-kind.
  Existing tools are either old C binaries (`helpdeco`) or lossy one-offs.
- **Broad applicability.** Countless Windows 3.x/9x/XP-era applications — games, utilities,
  enterprise software — ship `.hlp` files. Anyone archiving or modernizing that documentation
  hits this problem.
- **Sphinx round-trip.** RST → Sphinx → `htmlhelp` builder produces CHM. This is the only path
  from an old `.hlp` to a modern, searchable, maintainable documentation source that can also
  be compiled *back* to Windows Help format.
- **Library, not just CLI.** Exposing a clean `winhelp` crate lets others build readers,
  viewers, migrators, or other output formats on top.

---

## Format Overview (WinHelp 3.1)

The HLP file is a **virtual filesystem**: a B-tree index near the end of the file maps named
internal files. Key internal files:

| Internal file | Purpose |
|---|---|
| `\|SYSTEM` | Title, copyright, root topic context ID |
| `\|CONTEXT` | Context string → topic byte offset (B-tree) |
| `\|TTLBTREE` | Topic offset → title (B-tree) |
| `\|TOPIC` | Compressed topic data (the core payload) |
| `\|Phrases` | Phrase dictionary for first-pass decompression |
| `\|PhrIndex` | Phrase index |
| `\|FONT` | Font table |
| `\|KWBTREE` / `\|KWDATA` | Keyword index |
| Bitmap files | BMP, WMF, SHG (segmented hypergraphics) |

**Topic text is not RTF internally.** It is a proprietary binary opcode format: paragraph
records, text records with embedded attribute changes, link records. Decompression is two-pass:

1. **Phrase substitution** — dictionary of up to 2048 phrases; 2-byte tokens expand to strings.
2. **LZ77 variant** — sliding-window compression with WinHelp-specific quirks.

Each topic carries metadata in footnote-style records:
- `#` record — context string (stable topic ID, e.g. `Setup_and_Hosting`)
- `$` record — display title
- `K` record — keyword index entries
- `+` record — browse sequence

Hyperlinks encode as: link-text opcode + hidden destination context string.
Images encode as: `{bmc}` / `{bml}` / `{bmr}` macros (center / left / right).

---

## Architecture

Two crates in one workspace:

```
winhelp/          ← library crate
  src/
    lib.rs
    container.rs      B-tree filesystem: enumerate and read internal files
    decompress.rs     phrase substitution + LZ77 variant
    topic.rs          opcode parser → document model
    context.rs        |CONTEXT B-tree reader (context string ↔ offset)
    font.rs           |FONT table
    bitmap.rs         BMP / WMF / SHG extraction (SHG: flatten to image, drop hotspots)

hlp2rst/          ← binary crate (thin wrapper)
  src/
    main.rs           clap 4 CLI: hlp2rst <input.hlp> <output_dir/>
    rst.rs            document model → per-topic .rst files + index.rst
```

### Document model (sketch)

```rust
pub struct HelpFile {
    pub title: String,
    pub root_topic: String,
    pub topics: Vec<Topic>,
    pub keyword_index: Vec<KeywordEntry>,
}

pub struct Topic {
    pub id: String,              // context string — becomes RST label
    pub title: String,
    pub keywords: Vec<String>,
    pub browse_seq: Option<String>,
    pub body: Vec<Block>,
}

pub enum Block {
    Paragraph(Vec<Inline>),
    Table(Vec<Vec<Block>>),      // rows × cells
    Image(ImageRef),
}

pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Link { text: Vec<Inline>, target: String, kind: LinkKind },
}

pub enum LinkKind { Jump, Popup }
```

### RST output conventions

- One `.rst` file per topic, filename = `{context_id}.rst`
- Topic context string becomes a RST label: `.. _{context_id}:`
- Inter-topic links become `` :ref:`context_id` ``
- Images become `.. image:: _images/{filename}.png` (BMP converted to PNG)
- Keywords become `.. index:: keyword1, keyword2`
- Popups: emit as RST `.. note::` block or `.. sidebar::`, clearly annotated
- `index.rst` built from `|TTLBTREE` browse sequence order

---

## Implementation Phases

| Phase | Scope | Notes |
|---|---|---|
| 1 | Container reader | Parse file header, B-tree index, enumerate/read internal files |
| 2 | Decompression | Phrase substitution + LZ77; validate against known-good extracted text |
| 3 | Topic opcode parser | Paragraph/text/link/image records → document model |
| 4 | Context + keyword indexes | `|CONTEXT` and `|KWBTREE` readers |
| 5 | RST writer | Document model → `.rst` files; image export |
| 6 | SHG / WMF handling | Segmented hypergraphics (flatten); WMF (convert or skip) |
| 7 | WinHelp 4.0 (Win95) | Format differences for broader coverage |

Phases 1–3 are the hard 70%. Phases 4–5 are straightforward once the model is clean.

---

## Test Oracle

Primary test fixture: **OpenWatcom C Library Reference** (`clib.hlp`), available under the
Sybase Open Watcom Public License in both WinHelp 3.1 (Win16) and WinHelp 4.0 (Win32) variants.
The same content in two format versions provides a natural A/B correctness check.

Any HLP file decompiled by `helpdeco` into source RTF + HPJ gives exact ground truth:

- All topic IDs, titles, and browse sequences are known
- All hyperlink destinations are known (context strings)
- All image filenames are known
- The HPJ `[MAP]` section provides context string → integer ID mapping

Any correct parser must reproduce these exactly. The `helpdeco` output is the validation
baseline, not the implementation path. See RESEARCH.md for the full test fixture evaluation.

---

## Crate Dependencies

| Crate | Purpose |
|---|---|
| `winnow` | Binary parser combinators (prefer over `nom` for new projects) |
| `thiserror` | Structured error types |
| `miette` | Rich error reporting for CLI |
| `clap` 4 | CLI argument parsing |
| `image` | BMP → PNG conversion for RST image directives |

---

## Reference Material

- **helpdeco source (GPL)** — Use as a format specification when the docs are ambiguous.
  Not to copy; to cross-reference. Covers the LZ variant and opcode tables in detail.
- **"The Windows 3.1 Help File Format"** — Pete Davis & Mike Wallace, 1993. Covers the
  container structure, B-tree format, and basics of topic records.
- **WinHelp 4.0 additions** — Documented in various MSDN archives; needed for Win95-era files.

---

## Deliverables

1. `winhelp` crate published to crates.io — parses HLP into the document model above
2. `hlp2rst` binary published to crates.io — CLI converter with `--version` WinHelp format flag
3. Test suite using OpenWatcom `clib.hlp` as primary fixture with `helpdeco` ground-truth validation
4. README with quick-start, format notes, and Sphinx round-trip example
