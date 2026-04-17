# winhelp / hlp2rst

Pure-Rust toolkit for reading Windows WinHelp (`.hlp`) files and converting
them to Sphinx-compatible reStructuredText. Two crates in one workspace:

- **`winhelp`** — library. Parses an `.hlp` file into a structured document
  model (topics, inline formatting, links, tables, images, keyword index).
- **`hlp2rst`** — CLI. Walks the document model and writes one `.rst` per
  topic plus an `index.rst`, a minimal `conf.py`, and extracted images
  under `_images/`.

No dependency on `helpdeco` or any external decompiler — the binary format
is parsed directly.

## Quick start

```bash
# Build from source
cargo build --release

# Convert an .hlp to a directory of .rst
./target/release/hlp2rst path/to/clib.hlp ./out/

# Useful flags
hlp2rst input.hlp out/ --verbose          # one line per topic written
hlp2rst input.hlp out/ --dry-run          # parse only, skip writing
hlp2rst input.hlp out/ --format-version 3.1   # force format if header lies
```

### Library usage

```rust
use winhelp::HelpFile;

let help = HelpFile::from_path("clib.hlp".as_ref())?;
println!("{}: {} topics", help.title, help.topics.len());
for topic in &help.topics {
    println!("  {} — {}", topic.id, topic.title);
}
```

See [`winhelp/src/lib.rs`](winhelp/src/lib.rs) for the full document model
(`HelpFile`, `Topic`, `Block`, `Inline`, `LinkKind`, `ImageRef`).

## Format notes

WinHelp is a virtual filesystem built around a B-tree index, with per-topic
content stored as a stream of binary opcode records — not RTF.

Supported:

- WinHelp 3.0, 3.1 (Win16) and 4.0 (Win32/Win95-era HCW 4.00) containers
- LZ77 decompression and phrase substitution (both `|Phrases` and the
  4.0 Hall variant: `|PhrIndex` + `|PhrImage`)
- Context IDs, keyword index (`|KWBTREE`/`|KWDATA`), title index, font
  table, system metadata
- Bold / italic inline styling from `|FONT` descriptors
- Jump and popup links (resolved to context-hash-derived stable IDs)
- Tables via the dedicated `TL_TABLE` record parser
- Bitmap extraction: `|bmXX` internal files, DIB and DDB (MRB type 5)
  variants; Windows Metafiles passed through verbatim
- Segmented Hypergraphics (SHG) hotspot parsing
- `--format-version` override for files whose `|SYSTEM` header misreports
  their version

Not supported (out of scope for this project):

- Macros (`|ROSE` / embedded `!` links) — recognised but not executed
- Secondary windows and window-placement directives
- Compiling `.hlp` files (this is a reader, not a writer)

## Sphinx round-trip: `.hlp` → `.rst` → HTML / CHM

The RST emitted by `hlp2rst` is designed to be a valid Sphinx project out
of the box. A minimal `conf.py` and `index.rst` with a toctree are written
alongside the per-topic files.

```bash
# 1. Convert
hlp2rst clib.hlp clib-rst/

# 2. Install Sphinx once
pip install sphinx

# 3. Build HTML
sphinx-build -b html clib-rst/ clib-html/

# 4. (Optional) Build a modern .chm via Sphinx's htmlhelp builder
sphinx-build -b htmlhelp clib-rst/ clib-chm/
#   Then run `hhc clib-chm/*.hhp` with Microsoft HTML Help Workshop
#   to produce the final .chm.
```

This round-trip is the primary motivating use case: modernising legacy
Windows documentation into a maintainable RST source that can still be
compiled back to a Windows Help format if needed.

## Validation

The primary test fixture is OpenWatcom's `clib.hlp` (C Library Reference),
available in both WinHelp 3.1 (Win16) and 4.0 (Win32) variants under the
Sybase Open Watcom Public License. Additional stress fixtures include
OpenWatcom `clr.hlp` / `wccerrs.hlp`, Borland Turbo C++ `TCWHELP.HLP`, and
`win32.hlp`. `helpdeco` decompilation output (RTF + HPJ) serves as the
ground-truth oracle for topic IDs, link targets, and image filenames.

## License

MIT. See [LICENSE](LICENSE).
