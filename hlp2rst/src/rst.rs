//! RST writer — converts the winhelp document model to Sphinx-compatible
//! reStructuredText files.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use winhelp::{Block, HelpFile, ImagePlacement, Inline, LinkKind, Topic};

/// Write a complete RST output: per-topic .rst files, index.rst, and conf.py.
pub fn write_all(helpfile: &HelpFile, output_dir: &Path) -> miette::Result<()> {
    fs::create_dir_all(output_dir)
        .map_err(|e| miette::miette!("failed to create output directory: {e}"))?;

    // Create _images directory.
    let images_dir = output_dir.join("_images");
    fs::create_dir_all(&images_dir)
        .map_err(|e| miette::miette!("failed to create _images directory: {e}"))?;

    // Write per-topic .rst files.
    for topic in &helpfile.topics {
        if topic.id.is_empty() {
            continue;
        }
        write_topic(topic, output_dir)?;
    }

    // Write index.rst.
    write_index(helpfile, output_dir)?;

    // Write conf.py.
    write_conf_py(helpfile, output_dir)?;

    Ok(())
}

/// Write a single topic as a .rst file.
fn write_topic(topic: &Topic, output_dir: &Path) -> miette::Result<()> {
    let mut rst = String::new();

    // RST label for cross-referencing.
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
        write_block(&mut rst, block);
        writeln!(rst).unwrap();
    }

    let filename = format!("{}.rst", sanitize_filename(&topic.id));
    let path = output_dir.join(&filename);
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
    fs::write(&path, &rst)
        .map_err(|e| miette::miette!("failed to write index.rst: {e}"))?;

    Ok(())
}

/// Write Sphinx conf.py.
fn write_conf_py(helpfile: &HelpFile, output_dir: &Path) -> miette::Result<()> {
    let mut py = String::new();

    writeln!(py, "# Configuration file for the Sphinx documentation builder.").unwrap();
    writeln!(py).unwrap();
    writeln!(py, "project = {}", py_string(&helpfile.title)).unwrap();

    if let Some(ref copyright) = helpfile.copyright {
        writeln!(py, "copyright = {}", py_string(copyright)).unwrap();
    }

    writeln!(py, "extensions = []").unwrap();
    writeln!(py, "exclude_patterns = ['_build']").unwrap();
    writeln!(py).unwrap();

    let path = output_dir.join("conf.py");
    fs::write(&path, &py)
        .map_err(|e| miette::miette!("failed to write conf.py: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Block / Inline → RST rendering
// ---------------------------------------------------------------------------

fn write_block(out: &mut String, block: &Block) {
    match block {
        Block::Paragraph(inlines) => {
            for inline in inlines {
                write_inline(out, inline);
            }
            writeln!(out).unwrap();
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
            let directive = match img.placement {
                ImagePlacement::Inline => "image",
                ImagePlacement::Left | ImagePlacement::Right => "figure",
            };
            writeln!(out, ".. {directive}:: _images/{}", img.filename).unwrap();
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

/// Format a string as a Python string literal.
fn py_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winhelp::{ImageRef, KeywordEntry};

    fn sample_helpfile() -> HelpFile {
        HelpFile {
            title: "Test Help".into(),
            copyright: Some("(c) Test".into()),
            root_topic: "intro".into(),
            topics: vec![
                Topic {
                    id: "intro".into(),
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
        }
    }

    #[test]
    fn write_all_creates_files() {
        let dir = std::env::temp_dir().join("hlp2rst_test_write_all");
        let _ = fs::remove_dir_all(&dir);

        write_all(&sample_helpfile(), &dir).unwrap();

        assert!(dir.join("index.rst").exists());
        assert!(dir.join("conf.py").exists());
        assert!(dir.join("intro.rst").exists());
        assert!(dir.join("chapter1.rst").exists());
        assert!(dir.join("_images").is_dir());

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
        assert!(content.contains(".. figure:: _images/diagram.bmp"));
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
}
