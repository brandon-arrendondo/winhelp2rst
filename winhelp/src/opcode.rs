//! Topic text opcode parser.
//!
//! Converts the binary opcode stream within topic text records (types 0x20
//! and 0x23) into the `Block`/`Inline` document model.
//!
//! The opcode byte range 0x00–0x7F is literal text. Bytes 0x80 and above are
//! formatting opcodes (font change, bold, italic, links, images, etc.).
//!
//! Reference: helpdeco source, "The WinHelp File Format" (Pete Davis, 1993).

use std::collections::HashMap;

use crate::font::FontTable;
use crate::{Block, ImagePlacement, ImageRef, Inline, LinkKind, Result};

// ---------------------------------------------------------------------------
// Opcode constants
//
// These values are the commonly documented WinHelp opcodes. Real files may
// use slightly different encodings depending on version; we handle the
// standard set here and treat unknown opcodes as record terminators.
// ---------------------------------------------------------------------------

const OP_FONT_CHANGE: u8 = 0x80;
const OP_LINE_BREAK: u8 = 0x81;
const OP_END_PARAGRAPH: u8 = 0x82;
const OP_TAB: u8 = 0x83;
const OP_BOLD_ON: u8 = 0x86;
const OP_BOLD_OFF: u8 = 0x87;
const OP_ITALIC_ON: u8 = 0x88;
const OP_ITALIC_OFF: u8 = 0x89;
const OP_UNDERLINE_ON: u8 = 0x8B;
const OP_UNDERLINE_OFF: u8 = 0x8C;
const OP_END_OF_TEXT: u8 = 0xFF;

// Link opcodes (WinHelp 3.1 common values — may vary by sub-version).
const OP_JUMP_LINK_HASH: u8 = 0xE3;
const OP_POPUP_LINK_HASH: u8 = 0xE6;
const OP_JUMP_LINK_HASH_ALT: u8 = 0xC8;
const OP_POPUP_LINK_HASH_ALT: u8 = 0xCC;
const OP_LINK_END: u8 = 0x89;

// Image opcodes.
const OP_IMAGE_INLINE: u8 = 0xE0;
const OP_IMAGE_LEFT: u8 = 0xE1;
const OP_IMAGE_RIGHT: u8 = 0xE2;

/// Parse a topic text record into document model blocks.
///
/// The `data` should be the raw bytes of a text record (type 0x20/0x23),
/// starting after the 9-byte record header.
///
/// `hash_targets` maps context string hashes (u32) to context ID strings,
/// enabling hyperlink resolution. Pass an empty map to keep raw hex targets.
///
/// The text record has a paragraph info header of variable length, followed
/// by the opcode stream. The paragraph info header starts with:
///   u16 data_len_or_magic
///   u8  paragraph_flags
///   variable-length tab/indent data
///
/// For simplicity, we skip the paragraph info header and find the text data.
pub fn parse_text_record(
    data: &[u8],
    _fonts: &FontTable,
    hash_targets: &HashMap<u32, String>,
) -> Result<Vec<Block>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    // The text record data layout varies. For WinHelp 3.1:
    //   - The first few bytes are paragraph formatting info.
    //   - We scan for the start of readable text/opcodes.
    //
    // A simple heuristic: if the data starts with paragraph info, skip it.
    // The paragraph info typically ends before the first printable ASCII or
    // known opcode byte.
    //
    // For robustness, we try to parse from the beginning and handle
    // non-text bytes gracefully.

    parse_opcode_stream(data, hash_targets)
}

/// Parse an opcode stream into blocks.
fn parse_opcode_stream(data: &[u8], hash_targets: &HashMap<u32, String>) -> Result<Vec<Block>> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current_inlines: Vec<Inline> = Vec::new();
    let mut text_buf = String::new();

    // Formatting state.
    let mut bold = false;
    let mut italic = false;

    // Link state.
    let mut in_link = false;
    let mut link_kind = LinkKind::Jump;
    let mut link_target_hash: u32 = 0;
    let mut link_text: Vec<Inline> = Vec::new();

    let mut pos = 0;

    while pos < data.len() {
        let byte = data[pos];

        if byte < 0x80 {
            // Literal text byte.
            if byte == 0 {
                // Null byte — skip (padding or record separator).
                pos += 1;
                continue;
            }
            text_buf.push(byte as char);
            pos += 1;
            continue;
        }

        // Flush accumulated text before processing opcode.
        if !text_buf.is_empty() {
            let text_inline = Inline::Text(std::mem::take(&mut text_buf));
            if in_link {
                link_text.push(text_inline);
            } else {
                push_formatted(&mut current_inlines, text_inline, bold, italic);
            }
        }

        match byte {
            OP_FONT_CHANGE => {
                // Skip u16 font index.
                pos += 1;
                if pos + 2 <= data.len() {
                    pos += 2;
                }
            }

            OP_LINE_BREAK => {
                // Soft line break — treat as space.
                let inline = Inline::Text(" ".into());
                if in_link {
                    link_text.push(inline);
                } else {
                    current_inlines.push(inline);
                }
                pos += 1;
            }

            OP_END_PARAGRAPH => {
                // End current paragraph, start a new one.
                if !current_inlines.is_empty() {
                    blocks.push(Block::Paragraph(std::mem::take(&mut current_inlines)));
                }
                bold = false;
                italic = false;
                pos += 1;
            }

            OP_TAB => {
                let inline = Inline::Text("\t".into());
                if in_link {
                    link_text.push(inline);
                } else {
                    current_inlines.push(inline);
                }
                pos += 1;
            }

            OP_BOLD_ON => {
                bold = true;
                pos += 1;
            }

            OP_BOLD_OFF => {
                bold = false;
                pos += 1;
            }

            OP_ITALIC_ON => {
                italic = true;
                pos += 1;
            }

            OP_ITALIC_OFF if !in_link => {
                italic = false;
                pos += 1;
            }

            OP_UNDERLINE_ON => {
                // Treat underline as italic for RST purposes.
                italic = true;
                pos += 1;
            }

            OP_UNDERLINE_OFF => {
                italic = false;
                pos += 1;
            }

            OP_JUMP_LINK_HASH | OP_JUMP_LINK_HASH_ALT => {
                // Hyperlink start (jump): followed by u32 context hash.
                pos += 1;
                if pos + 4 <= data.len() {
                    link_target_hash = u32::from_le_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                }
                in_link = true;
                link_kind = LinkKind::Jump;
                link_text.clear();
            }

            OP_POPUP_LINK_HASH | OP_POPUP_LINK_HASH_ALT => {
                // Hyperlink start (popup): followed by u32 context hash.
                pos += 1;
                if pos + 4 <= data.len() {
                    link_target_hash = u32::from_le_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                }
                in_link = true;
                link_kind = LinkKind::Popup;
                link_text.clear();
            }

            OP_LINK_END if in_link => {
                // End of link — resolve hash to context ID, fall back to hex.
                in_link = false;
                let target = resolve_hash_target(link_target_hash, hash_targets);
                current_inlines.push(Inline::Link {
                    text: std::mem::take(&mut link_text),
                    target,
                    kind: link_kind,
                });
                pos += 1;
            }

            OP_IMAGE_INLINE | OP_IMAGE_LEFT | OP_IMAGE_RIGHT => {
                let placement = match byte {
                    OP_IMAGE_INLINE => ImagePlacement::Inline,
                    OP_IMAGE_LEFT => ImagePlacement::Left,
                    OP_IMAGE_RIGHT => ImagePlacement::Right,
                    _ => unreachable!(),
                };
                pos += 1;

                // Image reference: followed by a variable-length record.
                // Common format: u8 type_marker + data. For WinHelp 3.1,
                // embedded by-reference images have a u16 length + filename.
                //
                // We read a null-terminated filename or a fixed-length name.
                let filename = read_image_filename(data, &mut pos);

                if !current_inlines.is_empty() {
                    blocks.push(Block::Paragraph(std::mem::take(&mut current_inlines)));
                }
                blocks.push(Block::Image(ImageRef {
                    filename,
                    placement,
                }));
            }

            OP_END_OF_TEXT => {
                // End of text stream.
                break;
            }

            _ => {
                // Unknown opcode — skip. Some opcodes have variable-length
                // data. We skip the byte and hope for the best.
                pos += 1;
            }
        }
    }

    // Flush remaining text.
    if !text_buf.is_empty() {
        let text_inline = Inline::Text(text_buf);
        if in_link {
            link_text.push(text_inline);
        } else {
            push_formatted(&mut current_inlines, text_inline, bold, italic);
        }
    }

    // Close any open link.
    if in_link && !link_text.is_empty() {
        let target = resolve_hash_target(link_target_hash, hash_targets);
        current_inlines.push(Inline::Link {
            text: link_text,
            target,
            kind: link_kind,
        });
    }

    // Flush remaining paragraph.
    if !current_inlines.is_empty() {
        blocks.push(Block::Paragraph(current_inlines));
    }

    Ok(blocks)
}

/// Wrap a text inline with bold/italic formatting as needed.
fn push_formatted(inlines: &mut Vec<Inline>, text: Inline, bold: bool, italic: bool) {
    let formatted = if bold && italic {
        Inline::Bold(vec![Inline::Italic(vec![text])])
    } else if bold {
        Inline::Bold(vec![text])
    } else if italic {
        Inline::Italic(vec![text])
    } else {
        text
    };
    inlines.push(formatted);
}

/// Resolve a context hash to a context ID string.
///
/// If the hash is found in `hash_targets`, returns the context ID. Otherwise
/// returns a hex-formatted fallback (e.g., "0xDEADBEEF").
fn resolve_hash_target(hash: u32, hash_targets: &HashMap<u32, String>) -> String {
    match hash_targets.get(&hash) {
        Some(context_id) => context_id.clone(),
        None => format!("0x{hash:08X}"),
    }
}

/// Read an image filename from the opcode stream.
///
/// The image reference format varies. We try:
/// 1. Read a null-terminated string directly.
/// 2. If the next byte looks like a type marker (0x03 = by-reference),
///    skip it and read the filename.
fn read_image_filename(data: &[u8], pos: &mut usize) -> String {
    if *pos >= data.len() {
        return "unknown.bmp".into();
    }

    // Check for type marker byte.
    let start = if data[*pos] < 0x20 {
        *pos += 1; // skip type marker
        *pos
    } else {
        *pos
    };

    // Read null-terminated or until non-filename byte.
    let mut end = start;
    while end < data.len() && data[end] != 0 && data[end] >= 0x20 {
        end += 1;
    }

    let filename = if end > start {
        String::from_utf8_lossy(&data[start..end]).into_owned()
    } else {
        "unknown.bmp".into()
    };

    *pos = if end < data.len() && data[end] == 0 {
        end + 1
    } else {
        end
    };

    filename
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FontTable;

    fn fonts() -> FontTable {
        FontTable::empty()
    }

    fn no_targets() -> HashMap<u32, String> {
        HashMap::new()
    }

    #[test]
    fn plain_text_single_paragraph() {
        let data = b"Hello, world!";
        let blocks = parse_text_record(data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                match &inlines[0] {
                    Inline::Text(t) => assert_eq!(t, "Hello, world!"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn two_paragraphs() {
        let mut data = Vec::new();
        data.extend_from_slice(b"First");
        data.push(OP_END_PARAGRAPH);
        data.extend_from_slice(b"Second");

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn bold_text() {
        let mut data = Vec::new();
        data.push(OP_BOLD_ON);
        data.extend_from_slice(b"bold");
        data.push(OP_BOLD_OFF);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                assert!(matches!(&inlines[0], Inline::Bold(_)));
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn italic_text() {
        let mut data = Vec::new();
        data.push(OP_ITALIC_ON);
        data.extend_from_slice(b"italic");
        data.push(OP_ITALIC_OFF);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                assert!(matches!(&inlines[0], Inline::Italic(_)));
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn bold_italic_text() {
        let mut data = Vec::new();
        data.push(OP_BOLD_ON);
        data.push(OP_ITALIC_ON);
        data.extend_from_slice(b"both");
        data.push(OP_ITALIC_OFF);
        data.push(OP_BOLD_OFF);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                // Bold wrapping Italic wrapping Text.
                match &inlines[0] {
                    Inline::Bold(inner) => {
                        assert!(matches!(&inner[0], Inline::Italic(_)));
                    }
                    other => panic!("expected Bold, got {other:?}"),
                }
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn jump_link() {
        let mut data = Vec::new();
        data.push(OP_JUMP_LINK_HASH);
        data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        data.extend_from_slice(b"click here");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                match &inlines[0] {
                    Inline::Link { text, target, kind } => {
                        assert_eq!(*kind, LinkKind::Jump);
                        assert_eq!(target, "0xDEADBEEF");
                        assert_eq!(text.len(), 1);
                    }
                    other => panic!("expected Link, got {other:?}"),
                }
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn popup_link() {
        let mut data = Vec::new();
        data.push(OP_POPUP_LINK_HASH);
        data.extend_from_slice(&0x12345678u32.to_le_bytes());
        data.extend_from_slice(b"popup text");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        match &blocks[0] {
            Block::Paragraph(inlines) => match &inlines[0] {
                Inline::Link { kind, .. } => assert_eq!(*kind, LinkKind::Popup),
                other => panic!("expected Link, got {other:?}"),
            },
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn image_reference() {
        let mut data = Vec::new();
        data.push(OP_IMAGE_LEFT);
        data.extend_from_slice(b"setup.bmp\0");

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Image(img) => {
                assert_eq!(img.filename, "setup.bmp");
                assert_eq!(img.placement, ImagePlacement::Left);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn mixed_content() {
        let mut data = Vec::new();
        data.extend_from_slice(b"The ");
        data.push(OP_BOLD_ON);
        data.extend_from_slice(b"printf");
        data.push(OP_BOLD_OFF);
        data.extend_from_slice(b" function.");
        data.push(OP_END_PARAGRAPH);
        data.extend_from_slice(b"See also: ");
        data.push(OP_JUMP_LINK_HASH);
        data.extend_from_slice(&0x1000u32.to_le_bytes());
        data.extend_from_slice(b"fprintf");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);

        // First paragraph: "The " + bold("printf") + " function."
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 3);
                assert!(matches!(&inlines[0], Inline::Text(_)));
                assert!(matches!(&inlines[1], Inline::Bold(_)));
                assert!(matches!(&inlines[2], Inline::Text(_)));
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }

        // Second paragraph: "See also: " + link("fprintf")
        match &blocks[1] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 2);
                assert!(matches!(&inlines[0], Inline::Text(_)));
                assert!(matches!(&inlines[1], Inline::Link { .. }));
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn empty_data() {
        let blocks = parse_text_record(&[], &fonts(), &no_targets()).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn end_of_text_terminates() {
        let mut data = Vec::new();
        data.extend_from_slice(b"visible");
        data.push(OP_END_OF_TEXT);
        data.extend_from_slice(b"invisible");

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => match &inlines[0] {
                Inline::Text(t) => assert_eq!(t, "visible"),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn font_change_skipped() {
        let mut data = Vec::new();
        data.extend_from_slice(b"before");
        data.push(OP_FONT_CHANGE);
        data.push(0x02); // font index low
        data.push(0x00); // font index high
        data.extend_from_slice(b"after");

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 2);
                match &inlines[0] {
                    Inline::Text(t) => assert_eq!(t, "before"),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &inlines[1] {
                    Inline::Text(t) => assert_eq!(t, "after"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn line_break_becomes_space() {
        let mut data = Vec::new();
        data.extend_from_slice(b"line1");
        data.push(OP_LINE_BREAK);
        data.extend_from_slice(b"line2");

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                // "line1" + " " + "line2"
                assert_eq!(inlines.len(), 3);
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn jump_link_resolved_via_hash_map() {
        let hash = crate::context_hash("fprintf");
        let mut targets = HashMap::new();
        targets.insert(hash, "fprintf".to_string());

        let mut data = Vec::new();
        data.push(OP_JUMP_LINK_HASH);
        data.extend_from_slice(&hash.to_le_bytes());
        data.extend_from_slice(b"fprintf");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &targets).unwrap();
        match &blocks[0] {
            Block::Paragraph(inlines) => match &inlines[0] {
                Inline::Link { target, kind, .. } => {
                    assert_eq!(target, "fprintf");
                    assert_eq!(*kind, LinkKind::Jump);
                }
                other => panic!("expected Link, got {other:?}"),
            },
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn popup_link_resolved_via_hash_map() {
        let hash = crate::context_hash("malloc");
        let mut targets = HashMap::new();
        targets.insert(hash, "malloc".to_string());

        let mut data = Vec::new();
        data.push(OP_POPUP_LINK_HASH);
        data.extend_from_slice(&hash.to_le_bytes());
        data.extend_from_slice(b"dynamic allocation");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &targets).unwrap();
        match &blocks[0] {
            Block::Paragraph(inlines) => match &inlines[0] {
                Inline::Link { target, kind, .. } => {
                    assert_eq!(target, "malloc");
                    assert_eq!(*kind, LinkKind::Popup);
                }
                other => panic!("expected Link, got {other:?}"),
            },
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_hash_falls_back_to_hex() {
        let mut data = Vec::new();
        data.push(OP_JUMP_LINK_HASH);
        data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        data.extend_from_slice(b"mystery");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &no_targets()).unwrap();
        match &blocks[0] {
            Block::Paragraph(inlines) => match &inlines[0] {
                Inline::Link { target, .. } => {
                    assert_eq!(target, "0xDEADBEEF");
                }
                other => panic!("expected Link, got {other:?}"),
            },
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn mixed_resolved_and_unresolved_links() {
        let hash_printf = crate::context_hash("printf");
        let mut targets = HashMap::new();
        targets.insert(hash_printf, "printf".to_string());

        let mut data = Vec::new();
        // Resolved link.
        data.push(OP_JUMP_LINK_HASH);
        data.extend_from_slice(&hash_printf.to_le_bytes());
        data.extend_from_slice(b"printf");
        data.push(OP_LINK_END);
        // Unresolved link.
        data.push(OP_POPUP_LINK_HASH);
        data.extend_from_slice(&0x11223344u32.to_le_bytes());
        data.extend_from_slice(b"unknown");
        data.push(OP_LINK_END);

        let blocks = parse_text_record(&data, &fonts(), &targets).unwrap();
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 2);
                match &inlines[0] {
                    Inline::Link { target, .. } => assert_eq!(target, "printf"),
                    other => panic!("expected Link, got {other:?}"),
                }
                match &inlines[1] {
                    Inline::Link { target, .. } => assert_eq!(target, "0x11223344"),
                    other => panic!("expected Link, got {other:?}"),
                }
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }
}
