//! RST writer — converts the winhelp document model to Sphinx-compatible
//! reStructuredText files.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use image::ImageFormat;
use winhelp::{is_wmf, Block, HelpFile, ImagePlacement, Inline, LinkKind, Topic};

/// Output format we emit for an embedded image.
///
/// WinHelp bitmaps decoded by `winhelp::extract_bitmap` arrive as either
/// rasterised BMP bytes (re-encoded to PNG for Sphinx) or as Windows
/// Metafile (`.wmf`) bytes that we save verbatim and reference with a
/// caveat comment — Sphinx HTML can't render WMF directly, so users
/// post-process if they need a raster image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageOutFormat {
    /// Persist as `.png` after BMP→PNG conversion.
    Png,
    /// Persist as `.wmf` verbatim — vector format, not auto-converted.
    Wmf,
}

impl ImageOutFormat {
    fn extension(self) -> &'static str {
        match self {
            ImageOutFormat::Png => "png",
            ImageOutFormat::Wmf => "wmf",
        }
    }
}

/// Counts of artifacts persisted by [`write_all`].  Surfaced to the CLI
/// for the trailing summary line ("Wrote N .rst files, M images …").
#[derive(Debug, Default, Clone, Copy)]
pub struct WriteSummary {
    /// Number of primary topic `.rst` files written (excludes alias stubs).
    pub topics_written: usize,
    /// Number of alias stub `.rst` files written.
    pub aliases_written: usize,
    /// Number of image files written under `_images/`.
    pub images_written: usize,
}

/// Optional per-topic progress hook.  Invoked with `(index, total, topic_id)`
/// for each primary topic as it's written.  Used by the CLI's `--verbose`
/// mode to emit a line per topic; default callers pass `None`.
pub type TopicProgress<'a> = Option<&'a mut dyn FnMut(usize, usize, &str)>;

/// Write a complete RST output, invoking `progress` once per primary topic.
///
/// Produces per-topic `.rst` files, alias stubs, an `index.rst` with a
/// toctree, a minimal `conf.py`, and any embedded images under `_images/`.
pub fn write_all_with_progress(
    helpfile: &HelpFile,
    output_dir: &Path,
    mut progress: TopicProgress<'_>,
) -> miette::Result<WriteSummary> {
    fs::create_dir_all(output_dir)
        .map_err(|e| miette::miette!("failed to create output directory: {e}"))?;

    // Create _images directory.
    let images_dir = output_dir.join("_images");
    fs::create_dir_all(&images_dir)
        .map_err(|e| miette::miette!("failed to create _images directory: {e}"))?;

    let mut summary = WriteSummary::default();

    // Extract and convert images.  Capture the output format per filename
    // (PNG vs WMF) so the per-topic writer knows which extension to embed
    // in the `.. image::` directive without needing access to the bytes.
    let mut image_formats: HashMap<String, ImageOutFormat> = HashMap::new();
    for (filename, image_data) in &helpfile.images {
        let format = write_image(&images_dir, filename, image_data)?;
        image_formats.insert(filename.clone(), format);
        summary.images_written += 1;
    }

    // Primary-topic count drives the "i of N" progress display; alias stubs
    // are cheap and don't need their own counter.
    let total_primary = helpfile.topics.iter().filter(|t| !t.id.is_empty()).count();

    // Write per-topic .rst files plus one stub per alias.
    let mut primary_idx = 0usize;
    for topic in &helpfile.topics {
        if topic.id.is_empty() {
            continue;
        }
        primary_idx += 1;
        if let Some(ref mut cb) = progress {
            cb(primary_idx, total_primary, &topic.id);
        }
        write_topic(topic, output_dir, &image_formats)?;
        summary.topics_written += 1;
        for alias in &topic.aliases {
            if alias.is_empty() || alias == &topic.id {
                continue;
            }
            write_alias_stub(alias, &topic.id, output_dir)?;
            summary.aliases_written += 1;
        }
    }

    // Write index.rst.
    write_index(helpfile, output_dir)?;

    // Write conf.py.
    write_conf_py(helpfile, output_dir)?;

    Ok(summary)
}

/// Write a single topic as a .rst file.
fn write_topic(
    topic: &Topic,
    output_dir: &Path,
    image_formats: &HashMap<String, ImageOutFormat>,
) -> miette::Result<()> {
    let mut rst = String::new();

    // RST label for cross-referencing.  Aliases live in their own stub
    // files (see `write_alias_stub`) to avoid duplicate-label warnings
    // under Sphinx when both the primary topic and its alias stub declare
    // the same label.
    writeln!(rst, ".. _{}:", sanitize_label(&topic.id)).unwrap();
    writeln!(rst).unwrap();

    // Keyword index directive.
    if !topic.keywords.is_empty() {
        let kw_list = topic.keywords.join(", ");
        writeln!(rst, ".. index:: {kw_list}").unwrap();
        writeln!(rst).unwrap();
    }

    // Title with underline.
    let title = if topic.title.is_empty() {
        &topic.id
    } else {
        &topic.title
    };
    writeln!(rst, "{title}").unwrap();
    writeln!(rst, "{}", "=".repeat(title.len().max(1))).unwrap();
    writeln!(rst).unwrap();

    // Body blocks.
    for block in &topic.body {
        write_block(&mut rst, block, image_formats);
        writeln!(rst).unwrap();
    }

    let filename = format!("{}.rst", sanitize_filename(&topic.id));
    let path = output_dir.join(&filename);
    fs::write(&path, &rst)
        .map_err(|e| miette::miette!("failed to write {}: {e}", path.display()))?;

    Ok(())
}

/// Write an alias stub file: a minimal `.rst` that holds only the alias
/// label and a one-line redirect note.  Aliases are not added to the
/// toctree, but having the file ensures `:ref:` links using the alias
/// resolve to a real document under Sphinx.
fn write_alias_stub(alias: &str, primary: &str, output_dir: &Path) -> miette::Result<()> {
    let alias_file = sanitize_filename(alias);
    let primary_ref = sanitize_label(primary);
    let primary_title = sanitize_label(primary);
    let mut rst = String::new();
    writeln!(rst, ":orphan:").unwrap();
    writeln!(rst).unwrap();
    writeln!(rst, ".. _{}:", sanitize_label(alias)).unwrap();
    writeln!(rst).unwrap();
    writeln!(rst, "{primary_title}").unwrap();
    writeln!(rst, "{}", "=".repeat(primary_title.len().max(1))).unwrap();
    writeln!(rst).unwrap();
    writeln!(rst, "See :ref:`{primary_ref}`.").unwrap();
    let path = output_dir.join(format!("{alias_file}.rst"));
    fs::write(&path, &rst)
        .map_err(|e| miette::miette!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

/// Write index.rst with toctree.
fn write_index(helpfile: &HelpFile, output_dir: &Path) -> miette::Result<()> {
    let mut rst = String::new();

    writeln!(rst, "{}", helpfile.title).unwrap();
    writeln!(rst, "{}", "=".repeat(helpfile.title.len().max(1))).unwrap();
    writeln!(rst).unwrap();

    if let Some(ref copyright) = helpfile.copyright {
        writeln!(rst, "{copyright}").unwrap();
        writeln!(rst).unwrap();
    }

    writeln!(rst, ".. toctree::").unwrap();
    writeln!(rst, "   :maxdepth: 2").unwrap();
    writeln!(rst, "   :caption: Contents").unwrap();
    writeln!(rst).unwrap();

    for topic in &helpfile.topics {
        if topic.id.is_empty() {
            continue;
        }
        writeln!(rst, "   {}", sanitize_filename(&topic.id)).unwrap();
    }
    writeln!(rst).unwrap();

    let path = output_dir.join("index.rst");
    fs::write(&path, &rst).map_err(|e| miette::miette!("failed to write index.rst: {e}"))?;

    Ok(())
}

/// Write Sphinx conf.py.
fn write_conf_py(helpfile: &HelpFile, output_dir: &Path) -> miette::Result<()> {
    let mut py = String::new();

    writeln!(
        py,
        "# Configuration file for the Sphinx documentation builder."
    )
    .unwrap();
    writeln!(py).unwrap();
    writeln!(py, "project = {}", py_string(&helpfile.title)).unwrap();

    if let Some(ref copyright) = helpfile.copyright {
        writeln!(py, "copyright = {}", py_string(copyright)).unwrap();
    }

    writeln!(py, "extensions = []").unwrap();
    writeln!(py, "exclude_patterns = ['_build']").unwrap();
    writeln!(py).unwrap();

    let path = output_dir.join("conf.py");
    fs::write(&path, &py).map_err(|e| miette::miette!("failed to write conf.py: {e}"))?;

    Ok(())
}

/// Write a single image to disk and report which format we persisted.
///
/// WMF (Aldus Placeable Metafile) bytes are saved verbatim with a `.wmf`
/// extension — we can't rasterise vector data without an external tool,
/// per Task 19's MVP scope.  Everything else is treated as BMP and
/// re-encoded to PNG; if the BMP decoder rejects the bytes, we fall back
/// to a raw write under a sanitised stem so the data is at least preserved
/// for inspection.
fn write_image(
    images_dir: &Path,
    filename: &str,
    image_data: &[u8],
) -> miette::Result<ImageOutFormat> {
    if is_wmf(image_data) {
        let wmf_name = image_output_name_with(filename, ImageOutFormat::Wmf);
        let wmf_path = images_dir.join(&wmf_name);
        fs::write(&wmf_path, image_data)
            .map_err(|e| miette::miette!("failed to write {wmf_name}: {e}"))?;
        return Ok(ImageOutFormat::Wmf);
    }

    let png_name = image_output_name_with(filename, ImageOutFormat::Png);
    let png_path = images_dir.join(&png_name);

    // Try to decode as BMP and re-encode as PNG.
    match image::load_from_memory_with_format(image_data, ImageFormat::Bmp) {
        Ok(img) => {
            img.save_with_format(&png_path, ImageFormat::Png)
                .map_err(|e| miette::miette!("failed to save {png_name}: {e}"))?;
        }
        Err(_) => {
            // Decoding failed — save the raw bytes under a sanitised name so
            // we leave a breadcrumb without colliding with valid PNG outputs.
            let raw_path = images_dir.join(sanitize_image_stem(filename));
            fs::write(&raw_path, image_data)
                .map_err(|e| miette::miette!("failed to write {filename}: {e}"))?;
        }
    }

    Ok(ImageOutFormat::Png)
}

/// Compute the on-disk output name for an embedded image, given the
/// format we'll persist it as.
///
/// WinHelp internal filenames begin with `|` (e.g. `|bm0`) and have no
/// extension. We strip the `|`, replace any remaining filesystem-hostile
/// characters, and tack on the format extension (`.png` for raster output,
/// `.wmf` for vector output saved verbatim).
fn image_output_name_with(filename: &str, format: ImageOutFormat) -> String {
    let stem = sanitize_image_stem(filename);
    let ext = format.extension();
    match stem.rsplit_once('.') {
        Some((s, _)) => format!("{s}.{ext}"),
        None => format!("{stem}.{ext}"),
    }
}

/// Backwards-compatible accessor that assumes PNG output — retained for
/// callers (and tests) that don't yet know the runtime format.
#[cfg(test)]
fn image_output_name(filename: &str) -> String {
    image_output_name_with(filename, ImageOutFormat::Png)
}

/// Strip the leading `|` from a WinHelp internal-file image name and replace
/// any other filesystem-illegal characters with `_`.
fn sanitize_image_stem(filename: &str) -> String {
    let trimmed = filename.strip_prefix('|').unwrap_or(filename);
    trimmed
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
fn swap_extension(filename: &str, new_ext: &str) -> String {
    match filename.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.{new_ext}"),
        None => format!("{filename}.{new_ext}"),
    }
}

// ---------------------------------------------------------------------------
// Block / Inline → RST rendering
// ---------------------------------------------------------------------------

fn write_block(out: &mut String, block: &Block, image_formats: &HashMap<String, ImageOutFormat>) {
    match block {
        Block::Paragraph(inlines) => {
            let mut buf = String::new();
            for inline in inlines {
                write_inline(&mut buf, inline);
            }
            // Strip leading whitespace. WinHelp paragraphs that followed a
            // left/right-aligned image often retained a space that originally
            // separated the bitmap from its caption (e.g. `{bml bm1.bmp} Popup
            // Help`). Left in place, that single leading space makes docutils
            // treat the paragraph as continued directive content of the
            // preceding `.. figure::` and swallows the `:align:` option into
            // the image URI (producing `bm1.png:align:left`). RST paragraphs
            // can't start with whitespace anyway.
            let buf = buf.trim_start_matches([' ', '\t']);
            // Preserve the original newline structure while neutralizing any
            // line that docutils would parse as a section-title underline or
            // transition (ASCII-art dividers in the source text).
            for line in buf.split('\n') {
                if let Some(escaped) = neutralize_transition_line(line) {
                    writeln!(out, "{escaped}").unwrap();
                } else {
                    writeln!(out, "{line}").unwrap();
                }
            }
        }
        Block::Table(rows) => {
            // Simple list-table rendering.
            writeln!(out, ".. list-table::").unwrap();
            writeln!(out, "   :widths: auto").unwrap();
            writeln!(out).unwrap();
            for row in rows {
                for (i, cell) in row.iter().enumerate() {
                    if i == 0 {
                        write!(out, "   * - ").unwrap();
                    } else {
                        write!(out, "     - ").unwrap();
                    }
                    write_block_inline(out, cell);
                    writeln!(out).unwrap();
                }
            }
        }
        Block::Image(img) => {
            // Default to PNG for images we never wrote (e.g. external
            // baggage that the parser referenced but the container didn't
            // contain) — that path was already best-effort before WMF
            // support.
            let format = image_formats
                .get(&img.filename)
                .copied()
                .unwrap_or(ImageOutFormat::Png);
            let directive = match img.placement {
                ImagePlacement::Inline => "image",
                ImagePlacement::Left | ImagePlacement::Right => "figure",
            };
            let out_name = image_output_name_with(&img.filename, format);
            if format == ImageOutFormat::Wmf {
                // RST comment leads with `..` followed by text on the same
                // line.  Sphinx HTML can't render WMF directly, so flag the
                // unconverted format for any human reader of the source.
                writeln!(
                    out,
                    ".. WMF (Windows Metafile) — vector image saved unconverted; render to PNG/SVG to display in Sphinx HTML."
                )
                .unwrap();
            }
            writeln!(out, ".. {directive}:: _images/{out_name}").unwrap();
            if img.placement == ImagePlacement::Right {
                writeln!(out, "   :align: right").unwrap();
            } else if img.placement == ImagePlacement::Left {
                writeln!(out, "   :align: left").unwrap();
            }
            writeln!(out).unwrap();
        }
    }
}

/// Write a block as inline text (for table cells).
fn write_block_inline(out: &mut String, block: &Block) {
    match block {
        Block::Paragraph(inlines) => {
            for inline in inlines {
                write_inline(out, inline);
            }
        }
        Block::Image(img) => {
            write!(out, "|{}", img.filename).unwrap();
        }
        Block::Table(_) => {
            write!(out, "(nested table)").unwrap();
        }
    }
}

fn write_inline(out: &mut String, inline: &Inline) {
    match inline {
        Inline::Text(text) => {
            write!(out, "{}", escape_rst(text)).unwrap();
        }
        Inline::Bold(children) => {
            write!(out, "**").unwrap();
            for child in children {
                write_inline(out, child);
            }
            write!(out, "**").unwrap();
        }
        Inline::Italic(children) => {
            write!(out, "*").unwrap();
            for child in children {
                write_inline(out, child);
            }
            write!(out, "*").unwrap();
        }
        Inline::Link { text, target, kind } => {
            match kind {
                LinkKind::Jump => {
                    write!(out, ":ref:`").unwrap();
                    for child in text {
                        write_inline(out, child);
                    }
                    write!(out, " <{}>", sanitize_label(target)).unwrap();
                    write!(out, "`").unwrap();
                }
                LinkKind::Popup => {
                    // Popup links rendered as ref with note annotation.
                    write!(out, ":ref:`").unwrap();
                    for child in text {
                        write_inline(out, child);
                    }
                    write!(out, " <{}>", sanitize_label(target)).unwrap();
                    write!(out, "`").unwrap();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// If a line would be parsed by docutils as a transition or section-title
/// underline (4+ uniform punctuation chars, ignoring leading whitespace),
/// return a version with a backslash inserted before the first punctuation
/// char to break the uniform run. Returns `None` for lines that don't look
/// like a marker.
///
/// Example: `"                   -------------------------"`
///       → `"                   \\-------------------------"`
///
/// The backslash renders as a visible character in the final HTML, which is
/// acceptable since these are ASCII-art diagrams where exact alignment is
/// already lost by HTML's whitespace collapsing. The alternative — a
/// `\ ` null escape — would defeat the transition parser too but the
/// backslash form is clearer to anyone reading the raw RST.
fn neutralize_transition_line(line: &str) -> Option<String> {
    let lead_end = line.len() - line.trim_start().len();
    let body = line[lead_end..].trim_end();
    if body.len() < 4 {
        return None;
    }
    let first = body.chars().next()?;
    // Docutils' transition/underline char set is broad, but the real offenders
    // in WinHelp content are ASCII dividers. Keep the list narrow to avoid
    // needlessly escaping prose that happens to start with punctuation.
    // Docutils treats any of these as a valid title/transition underline,
    // so any 4+ uniform run of one of them must be neutralized.  The full
    // list is taken from docutils/parsers/rst/states.py.
    if !matches!(
        first,
        '!' | '"'
            | '#'
            | '$'
            | '%'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | '-'
            | '.'
            | '/'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '['
            | '\\'
            | ']'
            | '^'
            | '_'
            | '`'
            | '{'
            | '|'
            | '}'
            | '~'
    ) {
        return None;
    }
    if !body.chars().all(|c| c == first) {
        return None;
    }
    let mut escaped = String::with_capacity(line.len() + 1);
    escaped.push_str(&line[..lead_end]);
    escaped.push('\\');
    escaped.push_str(&line[lead_end..]);
    Some(escaped)
}

/// Escape RST special characters in text.
fn escape_rst(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('`', "\\`")
        .replace('|', "\\|")
        .replace('_', "\\_")
}

/// Sanitize a context string for use as an RST label.
fn sanitize_label(s: &str) -> String {
    s.replace([' ', '\\', '/'], "_")
}

/// Sanitize a context string for use as a filename (without extension).
fn sanitize_filename(s: &str) -> String {
    s.replace([' ', '\\', '/', ':', '<', '>', '"', '|', '?', '*'], "_")
}

/// Format a string as a Python string literal.  Escapes backslashes, single
/// quotes, and line breaks so that multi-line metadata (e.g. copyright
/// strings with embedded newlines) round-trip through `conf.py`.
fn py_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use winhelp::{ImageRef, KeywordEntry};

    /// Test helper: write without a progress callback.
    fn write_all(helpfile: &HelpFile, output_dir: &Path) -> miette::Result<WriteSummary> {
        write_all_with_progress(helpfile, output_dir, None)
    }

    fn sample_helpfile() -> HelpFile {
        HelpFile {
            title: "Test Help".into(),
            copyright: Some("(c) Test".into()),
            root_topic: "intro".into(),
            topics: vec![
                Topic {
                    id: "intro".into(),
                    aliases: Vec::new(),
                    title: "Introduction".into(),
                    keywords: vec!["intro".into()],
                    browse_seq: None,
                    body: vec![Block::Paragraph(vec![
                        Inline::Text("Welcome to ".into()),
                        Inline::Bold(vec![Inline::Text("Test Help".into())]),
                        Inline::Text(".".into()),
                    ])],
                },
                Topic {
                    id: "chapter1".into(),
                    aliases: Vec::new(),
                    title: "Chapter 1".into(),
                    keywords: vec![],
                    browse_seq: Some("ch".into()),
                    body: vec![
                        Block::Paragraph(vec![
                            Inline::Text("See ".into()),
                            Inline::Link {
                                text: vec![Inline::Text("Introduction".into())],
                                target: "intro".into(),
                                kind: LinkKind::Jump,
                            },
                        ]),
                        Block::Image(ImageRef {
                            filename: "diagram.bmp".into(),
                            placement: ImagePlacement::Left,
                        }),
                    ],
                },
            ],
            keyword_index: vec![KeywordEntry {
                keyword: "intro".into(),
                topic_ids: vec!["intro".into()],
            }],
            images: HashMap::new(),
        }
    }

    #[test]
    fn write_all_creates_files() {
        let dir = std::env::temp_dir().join("hlp2rst_test_write_all");
        let _ = fs::remove_dir_all(&dir);

        let summary = write_all(&sample_helpfile(), &dir).unwrap();

        assert!(dir.join("index.rst").exists());
        assert!(dir.join("conf.py").exists());
        assert!(dir.join("intro.rst").exists());
        assert!(dir.join("chapter1.rst").exists());
        assert!(dir.join("_images").is_dir());
        assert_eq!(summary.topics_written, 2);
        assert_eq!(summary.aliases_written, 0);
        assert_eq!(summary.images_written, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_all_with_progress_invokes_callback_per_primary_topic() {
        let dir = std::env::temp_dir().join("hlp2rst_test_progress_cb");
        let _ = fs::remove_dir_all(&dir);

        let mut seen: Vec<(usize, usize, String)> = Vec::new();
        let mut cb = |i: usize, n: usize, id: &str| {
            seen.push((i, n, id.to_string()));
        };
        let summary = write_all_with_progress(&sample_helpfile(), &dir, Some(&mut cb)).unwrap();

        assert_eq!(summary.topics_written, 2);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], (1, 2, "intro".to_string()));
        assert_eq!(seen[1], (2, 2, "chapter1".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_all_counts_aliases_in_summary() {
        use winhelp::{HelpFile, Topic};

        let dir = std::env::temp_dir().join("hlp2rst_test_alias_summary");
        let _ = fs::remove_dir_all(&dir);

        let helpfile = HelpFile {
            title: "T".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: vec!["intro_alias".into(), "another".into()],
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![Block::Paragraph(vec![Inline::Text("hi".into())])],
            }],
            keyword_index: vec![],
            images: HashMap::new(),
        };

        let summary = write_all(&helpfile, &dir).unwrap();
        assert_eq!(summary.topics_written, 1);
        assert_eq!(summary.aliases_written, 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_rst_contains_toctree() {
        let dir = std::env::temp_dir().join("hlp2rst_test_index");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("index.rst")).unwrap();
        assert!(content.contains(".. toctree::"));
        assert!(content.contains("intro"));
        assert!(content.contains("chapter1"));
        assert!(content.contains("Test Help"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topic_rst_has_label_and_title() {
        let dir = std::env::temp_dir().join("hlp2rst_test_topic");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(content.contains(".. _intro:"));
        assert!(content.contains("Introduction"));
        assert!(content.contains("========"));
        assert!(content.contains(".. index:: intro"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topic_rst_has_bold() {
        let dir = std::env::temp_dir().join("hlp2rst_test_bold");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(content.contains("**Test Help**"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topic_rst_has_link() {
        let dir = std::env::temp_dir().join("hlp2rst_test_link");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("chapter1.rst")).unwrap();
        assert!(content.contains(":ref:`Introduction <intro>`"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topic_rst_has_image() {
        let dir = std::env::temp_dir().join("hlp2rst_test_image");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("chapter1.rst")).unwrap();
        assert!(content.contains(".. figure:: _images/diagram.png"));
        assert!(content.contains(":align: left"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conf_py_valid() {
        let dir = std::env::temp_dir().join("hlp2rst_test_confpy");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        let content = fs::read_to_string(dir.join("conf.py")).unwrap();
        assert!(content.contains("project = 'Test Help'"));
        assert!(content.contains("copyright = '(c) Test'"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn escape_rst_special_chars() {
        assert_eq!(escape_rst("a*b"), "a\\*b");
        assert_eq!(escape_rst("a`b"), "a\\`b");
        assert_eq!(escape_rst("a|b"), "a\\|b");
    }

    #[test]
    fn sanitize_label_spaces() {
        assert_eq!(sanitize_label("my topic"), "my_topic");
    }

    #[test]
    fn py_string_escaping() {
        assert_eq!(py_string("it's"), "'it\\'s'");
    }

    #[test]
    fn neutralize_transition_detects_pure_dashes() {
        assert_eq!(
            neutralize_transition_line("     -------------------------").as_deref(),
            Some("     \\-------------------------"),
        );
    }

    #[test]
    fn neutralize_transition_detects_equals_and_tildes() {
        assert!(neutralize_transition_line("====").is_some());
        assert!(neutralize_transition_line("~~~~~").is_some());
    }

    #[test]
    fn neutralize_transition_ignores_mixed_and_short_lines() {
        assert!(neutralize_transition_line("---").is_none()); // too short
        assert!(neutralize_transition_line("--=--").is_none()); // mixed
        assert!(neutralize_transition_line("   Prose text.").is_none());
        assert!(neutralize_transition_line("").is_none());
    }

    #[test]
    fn paragraph_with_dashes_gets_escaped_on_write() {
        use winhelp::{HelpFile, Topic};

        let dir = std::env::temp_dir().join("hlp2rst_test_transition_escape");
        let _ = fs::remove_dir_all(&dir);

        let helpfile = HelpFile {
            title: "T".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: Vec::new(),
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![
                    Block::Paragraph(vec![Inline::Text("Header".into())]),
                    Block::Paragraph(vec![Inline::Text("     -------------------------".into())]),
                    Block::Paragraph(vec![Inline::Text("Next".into())]),
                ],
            }],
            keyword_index: vec![],
            images: HashMap::new(),
        };

        write_all(&helpfile, &dir).unwrap();

        let content = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(
            content.contains("\\-------------------------"),
            "dashes should be escaped:\n{content}",
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A Stars!-style topic: left-aligned image immediately followed by a
    /// paragraph whose text, in the original WinHelp source, sat right after
    /// the bitmap (so it starts with a space). Regression guard: the leading
    /// space must not survive into the RST output, otherwise docutils treats
    /// the paragraph as continuation of the `.. figure::` directive and
    /// concatenates `:align: left` into the image URI.
    #[test]
    fn paragraph_after_figure_has_no_leading_whitespace() {
        use winhelp::{HelpFile, ImageRef, Topic};

        let dir = std::env::temp_dir().join("hlp2rst_test_no_leading_ws_after_fig");
        let _ = fs::remove_dir_all(&dir);

        let helpfile = HelpFile {
            title: "T".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: Vec::new(),
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![
                    Block::Image(ImageRef {
                        filename: "|bm1".into(),
                        placement: ImagePlacement::Left,
                    }),
                    Block::Paragraph(vec![Inline::Text(" Popup Help".into())]),
                ],
            }],
            keyword_index: vec![],
            images: HashMap::new(),
        };

        write_all(&helpfile, &dir).unwrap();

        let content = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(
            content.contains(".. figure:: _images/bm1.png"),
            "figure directive missing:\n{content}",
        );
        assert!(
            content.contains("   :align: left"),
            "align option missing:\n{content}",
        );
        assert!(
            !content.contains(" Popup Help"),
            "paragraph kept its leading space, which confuses docutils:\n{content}",
        );
        assert!(
            content.contains("Popup Help"),
            "paragraph text missing entirely:\n{content}",
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_extension_bmp_to_png() {
        assert_eq!(swap_extension("diagram.bmp", "png"), "diagram.png");
        assert_eq!(swap_extension("setup.BMP", "png"), "setup.png");
        assert_eq!(swap_extension("noext", "png"), "noext.png");
        assert_eq!(swap_extension("multi.dots.bmp", "png"), "multi.dots.png");
    }

    #[test]
    fn image_output_name_strips_pipe_and_adds_png() {
        // WinHelp embedded bitmaps use |bmN names — strip the `|` and add .png.
        assert_eq!(image_output_name("|bm0"), "bm0.png");
        assert_eq!(image_output_name("|bm20"), "bm20.png");
        // Already-reasonable names just get the extension swapped.
        assert_eq!(image_output_name("diagram.bmp"), "diagram.png");
        // Chars Windows disallows in paths are replaced with underscore.
        assert_eq!(image_output_name("a/b:c"), "a_b_c.png");
    }

    #[test]
    fn bmp_to_png_conversion() {
        // Build a minimal valid BMP: 2x2, 24-bit.
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&70u32.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());

        bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
        bmp.extend_from_slice(&2i32.to_le_bytes()); // width
        bmp.extend_from_slice(&2i32.to_le_bytes()); // height
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bpp
        bmp.extend_from_slice(&0u32.to_le_bytes()); // compression
        bmp.extend_from_slice(&16u32.to_le_bytes()); // image size
        bmp.extend_from_slice(&0i32.to_le_bytes()); // x ppm
        bmp.extend_from_slice(&0i32.to_le_bytes()); // y ppm
        bmp.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bmp.extend_from_slice(&0u32.to_le_bytes()); // important colors

        // 2x2 pixels (each row padded to 4 bytes): 8 bytes per row.
        bmp.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        bmp.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        let dir = std::env::temp_dir().join("hlp2rst_test_bmp2png");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_image(&dir, "test.bmp", &bmp).unwrap();

        // Should have created test.png.
        let png_path = dir.join("test.png");
        assert!(png_path.exists(), "PNG file should be created");

        // Verify it's a valid PNG (starts with PNG magic).
        let png_data = fs::read(&png_path).unwrap();
        assert_eq!(&png_data[0..4], &[0x89, 0x50, 0x4E, 0x47]); // PNG magic

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_image_with_embedded_bmp_data() {
        // Same BMP as above, but included in a full HelpFile.
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&70u32.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&2i32.to_le_bytes());
        bmp.extend_from_slice(&2i32.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&16u32.to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        bmp.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        let mut images = HashMap::new();
        images.insert("diagram.bmp".to_string(), bmp);

        let helpfile = HelpFile {
            title: "Image Test".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: Vec::new(),
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![Block::Image(ImageRef {
                    filename: "diagram.bmp".into(),
                    placement: ImagePlacement::Inline,
                })],
            }],
            keyword_index: vec![],
            images,
        };

        let dir = std::env::temp_dir().join("hlp2rst_test_image_write");
        let _ = fs::remove_dir_all(&dir);

        write_all(&helpfile, &dir).unwrap();

        // Verify PNG file exists.
        assert!(dir.join("_images/diagram.png").exists());

        // Verify RST references .png.
        let rst = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(rst.contains("_images/diagram.png"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Build a minimal byte stream that satisfies `winhelp::is_wmf` —
    /// just the Aldus Placeable Metafile magic followed by enough trailing
    /// zeros to look like a header.  We don't need a renderable WMF; the
    /// writer treats the bytes opaquely.
    fn make_fake_wmf() -> Vec<u8> {
        let mut wmf = Vec::with_capacity(32);
        wmf.extend_from_slice(&winhelp::APM_MAGIC.to_le_bytes());
        wmf.extend_from_slice(&[0u8; 28]);
        wmf
    }

    #[test]
    fn write_image_with_wmf_data_emits_dot_wmf_and_returns_wmf_format() {
        let dir = std::env::temp_dir().join("hlp2rst_test_wmf_write_image");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let wmf = make_fake_wmf();
        let format = write_image(&dir, "|bm5", &wmf).unwrap();

        assert_eq!(format, ImageOutFormat::Wmf);
        let saved = dir.join("bm5.wmf");
        assert!(saved.exists(), "expected {} to exist", saved.display());
        // Bytes must be saved verbatim — we don't transform vector data.
        let on_disk = fs::read(&saved).unwrap();
        assert_eq!(on_disk, wmf);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topic_rst_emits_wmf_directive_with_caveat_comment() {
        use winhelp::{HelpFile, ImageRef, Topic};

        let dir = std::env::temp_dir().join("hlp2rst_test_wmf_topic_emission");
        let _ = fs::remove_dir_all(&dir);

        let mut images = HashMap::new();
        images.insert("|bm0".to_string(), make_fake_wmf());

        let helpfile = HelpFile {
            title: "WMF Test".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: Vec::new(),
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![Block::Image(ImageRef {
                    filename: "|bm0".into(),
                    placement: ImagePlacement::Inline,
                })],
            }],
            keyword_index: vec![],
            images,
        };

        write_all(&helpfile, &dir).unwrap();

        // WMF file is on disk under _images/.
        assert!(dir.join("_images/bm0.wmf").exists());
        // No phantom .png is generated for the WMF image.
        assert!(!dir.join("_images/bm0.png").exists());

        let rst = fs::read_to_string(dir.join("intro.rst")).unwrap();
        // Image directive references the .wmf path with the right
        // extension.
        assert!(
            rst.contains(".. image:: _images/bm0.wmf"),
            "directive missing or wrong extension:\n{rst}",
        );
        // Caveat comment flags the format as unconverted.
        assert!(
            rst.contains(".. WMF (Windows Metafile)"),
            "WMF caveat comment missing:\n{rst}",
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_all_persists_mixed_bmp_and_wmf_with_correct_extensions() {
        // Two embedded images in one HelpFile — one BMP, one WMF — both
        // referenced from the same topic.  Ensures the per-filename format
        // map dispatches the right extension to each image directive.
        use winhelp::{HelpFile, ImageRef, Topic};

        // Minimal 2x2 24-bit BMP (matches the existing bmp_to_png test).
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&70u32.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&2i32.to_le_bytes());
        bmp.extend_from_slice(&2i32.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&16u32.to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        bmp.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        let mut images = HashMap::new();
        images.insert("|bm0".to_string(), bmp);
        images.insert("|bm1".to_string(), make_fake_wmf());

        let helpfile = HelpFile {
            title: "Mixed".into(),
            copyright: None,
            root_topic: "intro".into(),
            topics: vec![Topic {
                id: "intro".into(),
                aliases: Vec::new(),
                title: "Intro".into(),
                keywords: vec![],
                browse_seq: None,
                body: vec![
                    Block::Image(ImageRef {
                        filename: "|bm0".into(),
                        placement: ImagePlacement::Inline,
                    }),
                    Block::Image(ImageRef {
                        filename: "|bm1".into(),
                        placement: ImagePlacement::Left,
                    }),
                ],
            }],
            keyword_index: vec![],
            images,
        };

        let dir = std::env::temp_dir().join("hlp2rst_test_mixed_bmp_wmf");
        let _ = fs::remove_dir_all(&dir);

        write_all(&helpfile, &dir).unwrap();

        assert!(dir.join("_images/bm0.png").exists());
        assert!(dir.join("_images/bm1.wmf").exists());

        let rst = fs::read_to_string(dir.join("intro.rst")).unwrap();
        assert!(rst.contains(".. image:: _images/bm0.png"));
        assert!(rst.contains(".. figure:: _images/bm1.wmf"));
        assert!(rst.contains("   :align: left"));

        let _ = fs::remove_dir_all(&dir);
    }
}
