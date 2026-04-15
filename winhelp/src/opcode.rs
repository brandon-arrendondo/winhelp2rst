//! Topic text opcode parser.
//!
//! Converts TOPICLINK text records (types 0x20 and 0x23) into the
//! [`Block`]/[`Inline`] document model.
//!
//! # Format
//!
//! Each text record contributes two parallel byte streams:
//!
//! * **LinkData1** — the command/opcode stream. Starts with a structured
//!   paragraph-info header (size, char count, 4 skip bytes, bitflags, and
//!   per-flag fields for alignment/margins/tab stops), then a sequence of
//!   single-byte opcodes with parameters. Each opcode consumes zero or more
//!   text segments from LinkData2 and/or parameter bytes from LinkData1.
//! * **LinkData2** — displayable text as a sequence of NUL-terminated strings
//!   ("segments"). Every opcode that emits or wraps visible text pops the
//!   next segment from this stream in order.
//!
//! This split is essential: LinkData2 contains no opcodes — it is plain
//! printable bytes between NULs. All formatting, paragraph structure, link
//! markup, and image references live in LinkData1.
//!
//! # Paragraph info header (TL_NORMAL 0x20)
//!
//! ```text
//! scanlong    unknown               (2 or 4 bytes)
//! scanword    char count increment  (1 or 2 bytes)  — used for TOPICOFFSET math
//! skip 4      unknown
//! u16         bitflags
//! if bits & 0x0001: scanlong        (absolute topic offset)
//! if bits & 0x0002: scanint         (\sb space-before)
//! if bits & 0x0004: scanint         (\sa space-after)
//! if bits & 0x0008: scanint         (\sl line-spacing)
//! if bits & 0x0010: scanint         (\li left-indent)
//! if bits & 0x0020: scanint         (\ri right-indent)
//! if bits & 0x0040: scanint         (\fi first-indent)
//! if bits & 0x0100: skip 3          (border flags byte + u16 border-space)
//! if bits & 0x0200:                 (tab stops)
//!     scanint y1
//!     repeat y1 times:
//!         x1 = scanword
//!         if x1 & 0x4000: scanword  (tab-alignment code)
//! ```
//!
//! Compressed ints (all little-endian, per helpdeco's scanword/scanint/scanlong):
//!
//! * **scanword**: if low-bit = 1, 2-byte form `value = u16 >> 1`; else
//!   1-byte form `value = u8 >> 1`.
//! * **scanint**: like scanword but signed, with bias: 2-byte form
//!   `value = (u16 >> 1) - 0x4000`; 1-byte form `value = (u8 >> 1) - 0x40`.
//! * **scanlong**: if low-bit = 1, 4-byte form `value = (u32 >> 1) - 0x40000000`;
//!   else 2-byte form `value = (u16 >> 1) - 0x4000`.
//!
//! # Opcodes
//!
//! | Byte | Name | Params | Segments consumed | Meaning |
//! |------|------|--------|-------------------|---------|
//! | `0x80 XX YY` | font change | u16 LE font index | 1 | emit segment, then switch to font XX |
//! | `0x81` | line break | — | 1 | emit segment, then hard newline |
//! | `0x82` | end paragraph | — | 1 | emit segment, then terminate paragraph |
//! | `0x83` | tab | — | 1 | emit segment, then insert a tab |
//! | `0x86` | bmc (inline image) | type + scanlong + payload | 1 | emit empty seg, then emit image block |
//! | `0x87` | bml (left-aligned image) | type + scanlong + payload | 1 | same but left placement |
//! | `0x88` | bmr (right-aligned image) | type + scanlong + payload | 1 | same but right placement |
//! | `0x89` | link end (hotspot) | — | 1 (as link text) | ends a hyperlink, consumes next segment as display text |
//! | `0xFF` | end of record | — | 0 | end of command stream |
//! | `0xE3 HH HH HH HH` | jump link | u32 LE hash | 1 | start jump link |
//! | `0xE6 HH HH HH HH` | popup link | u32 LE hash | 1 | start popup link |
//!
//! # Image opcode payload (0x86/0x87/0x88)
//!
//! After the opcode byte:
//!
//! ```text
//! byte type_code         (0x22 = HC31 picture, 0x03 = HC30 picture, 0x05 = embedded window)
//! scanlong payload_size
//! if type_code == 0x22: scanword hotspot_count
//! u16 PictureIsEmbedded  (0 = external/baggage, 1 = embedded next bitmap)
//! u16 PictureNumber      (index N → resolved to |bmN internal file)
//! [remaining payload bytes...]
//! ```
//!
//! The opcode consumes `2 + scanlong_size + (scanword_size if 0x22) +
//! payload_size` bytes total beyond the opcode byte, where `ptr += payload_size`
//! is measured from the byte after the scanlong value.
//!
//! # Bold / italic / underline
//!
//! WinHelp does not expose these as standalone toggle opcodes. Instead,
//! every `0x80` font-change opcode selects a [`FontDescriptor`] in the
//! |FONT table, and its `attributes` byte encodes bold/italic/underline
//! bits. We derive the active formatting state from the current font on
//! each font change.
//!
//! Reference: helpdeco source (`TopicDump` in src/helpdeco.c, case 0x86+)
//! and Pete Davis / Mike Wallace, "The WinHelp File Format" (1993).

use std::collections::HashMap;

use crate::font::FontTable;
use crate::{Block, ImagePlacement, ImageRef, Inline, LinkKind, Result};

// Opcode constants.
const OP_FONT_CHANGE: u8 = 0x80;
const OP_LINE_BREAK: u8 = 0x81;
const OP_END_PARAGRAPH: u8 = 0x82;
const OP_TAB: u8 = 0x83;
/// bmc — inline/centered bitmap.
const OP_IMAGE_CENTER: u8 = 0x86;
/// bml — left-aligned bitmap.
const OP_IMAGE_LEFT: u8 = 0x87;
/// bmr — right-aligned bitmap.
const OP_IMAGE_RIGHT: u8 = 0x88;
/// End of hotspot (hyperlink). Overloaded with italic-off in older docs;
/// per helpdeco this is always link-end. When we are not currently inside
/// a link, we treat it as a no-op.
const OP_LINK_END: u8 = 0x89;
const OP_POPUP_LINK_HASH_ALT: u8 = 0xC8;
const OP_JUMP_LINK_HASH_ALT: u8 = 0xCC;
const OP_JUMP_LINK_HASH: u8 = 0xE3;
const OP_POPUP_LINK_HASH: u8 = 0xE6;
const OP_END_OF_RECORD: u8 = 0xFF;

// Image opcode type codes (second byte after 0x86/0x87/0x88).
const IMAGE_TYPE_HC31: u8 = 0x22;
const IMAGE_TYPE_HC30: u8 = 0x03;
const IMAGE_TYPE_EMBEDDED_WINDOW: u8 = 0x05;

/// Parse a topic text record into document-model blocks.
///
/// `link_data1` is the command/opcode stream; `link_data2` is the NUL-delimited
/// text segments. `fonts` is the parsed |FONT table (for bold/italic/underline
/// state). `hash_targets` maps context hash → context-id string; unresolved
/// hashes fall back to hex form.
/// Return the TOPICOFFSET character-count delta for a TL_DISPLAY or TL_TABLE
/// record, reading only the two leading compressed fields of `link_data1`.
///
/// The layout at the head of LinkData1 is:
///   - `scanlong`: unused size-ish field
///   - `scanword`: character-count increment that the help compiler used
///     when computing TOPICOFFSETs (phrase-expanded virtual length, not the
///     raw on-disk byte count).
///
/// See helpdeco.c:3356-3362 — this is the value `TopicOffset += x1`.
///
/// Returning 0 when LinkData1 is too short keeps callers from panicking on
/// malformed records; the record is effectively a no-op for offset maths.
pub fn topic_offset_delta(link_data1: &[u8]) -> u16 {
    let mut p = 0usize;
    let _ = scan_long(link_data1, &mut p);
    scan_word(link_data1, &mut p)
}

pub fn parse_text_record(
    link_data1: &[u8],
    link_data2: &[u8],
    fonts: &FontTable,
    hash_targets: &HashMap<u32, String>,
) -> Result<Vec<Block>> {
    if link_data1.is_empty() {
        return Ok(Vec::new());
    }

    let cmd_start = find_command_stream_start(link_data1);
    if cmd_start >= link_data1.len() {
        return Ok(Vec::new());
    }
    let cmd = &link_data1[cmd_start..];
    let segments = split_segments(link_data2);

    Ok(parse_command_stream(cmd, &segments, fonts, hash_targets))
}

/// Split LinkData2 into text segments at NUL byte boundaries.
fn split_segments(link_data2: &[u8]) -> Vec<String> {
    link_data2
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

struct SegCursor<'a> {
    segs: &'a [String],
    idx: usize,
}

impl<'a> SegCursor<'a> {
    fn new(segs: &'a [String]) -> Self {
        Self { segs, idx: 0 }
    }

    fn next(&mut self) -> &'a str {
        if self.idx < self.segs.len() {
            let s = self.segs[self.idx].as_str();
            self.idx += 1;
            s
        } else {
            ""
        }
    }
}

/// State while processing the command stream.
struct ParseState {
    blocks: Vec<Block>,
    current: Vec<Inline>,
    bold: bool,
    italic: bool,
    underline: bool,
    in_link: bool,
    link_kind: LinkKind,
    link_hash: u32,
}

impl ParseState {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            current: Vec::new(),
            bold: false,
            italic: false,
            underline: false,
            in_link: false,
            link_kind: LinkKind::Jump,
            link_hash: 0,
        }
    }

    fn emit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let inline = wrap_inline(Inline::Text(text.to_string()), self.bold, self.italic);
        push_or_merge(&mut self.current, inline);
        let _ = self.underline;
    }

    fn end_paragraph(&mut self) {
        if !self.current.is_empty() {
            let para = std::mem::take(&mut self.current);
            self.blocks.push(Block::Paragraph(para));
        }
    }

    fn start_link(&mut self, kind: LinkKind, hash: u32) {
        self.in_link = true;
        self.link_kind = kind;
        self.link_hash = hash;
    }

    fn end_link(&mut self, text: &str, hash_targets: &HashMap<u32, String>) {
        self.in_link = false;
        let target = resolve_hash_target(self.link_hash, hash_targets);
        let link_text = if text.is_empty() {
            vec![Inline::Text(target.clone())]
        } else {
            vec![Inline::Text(text.to_string())]
        };
        self.current.push(Inline::Link {
            text: link_text,
            target,
            kind: self.link_kind,
        });
    }

    /// Push an image block. If there are pending inlines, flush them first
    /// so the image is its own block rather than getting mixed into a
    /// paragraph accidentally.
    fn push_image(&mut self, filename: String, placement: ImagePlacement) {
        self.end_paragraph();
        self.blocks.push(Block::Image(ImageRef {
            filename,
            placement,
        }));
    }

    /// Update bold/italic/underline state from the font descriptor at the
    /// given index.
    ///
    /// Current behaviour: this is a no-op. Mapping raw font descriptors
    /// directly to bold/italic/underline over-formats real help content —
    /// clib.hlp uses a "semibold" body font (font 4) that, when treated as
    /// italic, wraps every sentence's worth of text in asterisks and
    /// corrupts the RST output. Until we have a reliable mapping (likely
    /// via font-family / face-name heuristics), we leave the style state
    /// untouched on font change. The `FontTable` parameter is kept so the
    /// future implementation fits without a signature break.
    fn apply_font(&mut self, _fonts: &FontTable, _font_index: usize) {
        // Intentionally empty — see doc comment.
    }
}

/// Parse the command stream, consuming text segments from LinkData2.
fn parse_command_stream(
    cmd: &[u8],
    segments: &[String],
    fonts: &FontTable,
    hash_targets: &HashMap<u32, String>,
) -> Vec<Block> {
    let mut state = ParseState::new();
    let mut cur = SegCursor::new(segments);
    let mut pos = 0;

    while pos < cmd.len() {
        let byte = cmd[pos];
        match byte {
            OP_FONT_CHANGE => {
                pos += 1;
                let font_idx = if pos + 2 <= cmd.len() {
                    let idx = u16::from_le_bytes([cmd[pos], cmd[pos + 1]]);
                    pos += 2;
                    idx as usize
                } else {
                    0
                };
                let seg = cur.next();
                state.emit_text(seg);
                state.apply_font(fonts, font_idx);
            }
            OP_LINE_BREAK => {
                pos += 1;
                let seg = cur.next();
                state.emit_text(seg);
                push_or_merge(&mut state.current, Inline::Text(" ".into()));
            }
            OP_END_PARAGRAPH => {
                pos += 1;
                let seg = cur.next();
                state.emit_text(seg);
                state.end_paragraph();
            }
            OP_TAB => {
                pos += 1;
                let seg = cur.next();
                state.emit_text(seg);
                push_or_merge(&mut state.current, Inline::Text("\t".into()));
            }
            OP_IMAGE_CENTER | OP_IMAGE_LEFT | OP_IMAGE_RIGHT => {
                // Consume one LD2 segment (per helpdeco's one-per-command loop).
                let _seg = cur.next();
                let placement = match byte {
                    OP_IMAGE_CENTER => ImagePlacement::Inline,
                    OP_IMAGE_LEFT => ImagePlacement::Left,
                    _ => ImagePlacement::Right,
                };
                if let Some((filename, new_pos)) = parse_image_opcode(cmd, pos, placement) {
                    if let Some(name) = filename {
                        state.push_image(name, placement);
                    }
                    pos = new_pos;
                } else {
                    // Couldn't parse — skip opcode byte only.
                    pos += 1;
                }
            }
            OP_LINK_END => {
                pos += 1;
                if state.in_link {
                    let seg = cur.next();
                    state.end_link(seg, hash_targets);
                }
                // Not in a link: 0x89 is a no-op in our model. Historically
                // some docs call 0x89 "italic off", but helpdeco always
                // treats it as end-of-hotspot.
            }
            OP_JUMP_LINK_HASH | OP_JUMP_LINK_HASH_ALT => {
                pos += 1;
                let hash = read_u32_le(cmd, &mut pos);
                let _ = cur.next();
                state.start_link(LinkKind::Jump, hash);
            }
            OP_POPUP_LINK_HASH | OP_POPUP_LINK_HASH_ALT => {
                pos += 1;
                let hash = read_u32_le(cmd, &mut pos);
                let _ = cur.next();
                state.start_link(LinkKind::Popup, hash);
            }
            OP_END_OF_RECORD => break,
            _ => {
                // Unknown opcode: skip one byte. Do not advance the segment
                // cursor — the risk of dropping real text outweighs the cost
                // of occasionally mis-skipping.
                pos += 1;
            }
        }
    }

    state.end_paragraph();
    state.blocks
}

/// Parse an image opcode starting at `pos` (pointing to the opcode byte).
/// Returns `Some((filename, new_pos))` on success, where `filename` is
/// `Some("|bmN")` if the image is an external baggage reference, or `None`
/// if the image is embedded / a window and we don't materialise a filename.
fn parse_image_opcode(
    cmd: &[u8],
    pos: usize,
    _placement: ImagePlacement,
) -> Option<(Option<String>, usize)> {
    if pos + 2 >= cmd.len() {
        return None;
    }
    let type_byte = cmd[pos + 1];
    let mut p = pos + 2;
    let payload_size = scan_long(cmd, &mut p) as usize;
    let payload_start = p;

    let filename = match type_byte {
        IMAGE_TYPE_HC31 => {
            let _hotspots = scan_word(cmd, &mut p);
            resolve_external_bitmap(cmd, p)
        }
        IMAGE_TYPE_HC30 => resolve_external_bitmap(cmd, p),
        IMAGE_TYPE_EMBEDDED_WINDOW => None,
        _ => None,
    };

    // Advance past the payload (from the position right after scanlong).
    let new_pos = payload_start.saturating_add(payload_size).min(cmd.len());
    Some((filename, new_pos))
}

/// If the two u16 values at `p` encode an external baggage reference
/// (PictureIsEmbedded=0 + PictureNumber=N), return the `|bmN` filename.
/// Embedded references (PictureIsEmbedded=1) don't have a stable external
/// name in our model, so we return None for those.
fn resolve_external_bitmap(cmd: &[u8], p: usize) -> Option<String> {
    if p + 4 > cmd.len() {
        return None;
    }
    let is_embedded = u16::from_le_bytes([cmd[p], cmd[p + 1]]);
    let number = u16::from_le_bytes([cmd[p + 2], cmd[p + 3]]);
    if is_embedded == 0 {
        Some(format!("|bm{number}"))
    } else {
        None
    }
}

/// Read a little-endian u32, advancing `pos`.
fn read_u32_le(buf: &[u8], pos: &mut usize) -> u32 {
    if *pos + 4 > buf.len() {
        let r = *pos;
        *pos = buf.len();
        let mut arr = [0u8; 4];
        let take = buf.len() - r;
        arr[..take].copy_from_slice(&buf[r..]);
        return u32::from_le_bytes(arr);
    }
    let v = u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    v
}

/// Read a compressed long (scanlong): LSB=1 → 4-byte form, LSB=0 → 2-byte form.
fn scan_long(data: &[u8], pos: &mut usize) -> u32 {
    if *pos >= data.len() {
        return 0;
    }
    if (data[*pos] & 1) != 0 {
        if *pos + 4 > data.len() {
            *pos = data.len();
            return 0;
        }
        let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
        *pos += 4;
        (v >> 1).wrapping_sub(0x4000_0000)
    } else {
        if *pos + 2 > data.len() {
            *pos = data.len();
            return 0;
        }
        let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
        *pos += 2;
        ((v >> 1) as u32).wrapping_sub(0x4000)
    }
}

/// Read a compressed unsigned word (scanword): LSB=1 → 2-byte form, LSB=0 → 1-byte form.
fn scan_word(data: &[u8], pos: &mut usize) -> u16 {
    if *pos >= data.len() {
        return 0;
    }
    if (data[*pos] & 1) != 0 {
        if *pos + 2 > data.len() {
            *pos = data.len();
            return 0;
        }
        let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
        *pos += 2;
        v >> 1
    } else {
        let v = data[*pos];
        *pos += 1;
        (v >> 1) as u16
    }
}

/// Read a compressed signed int (scanint): bias-subtracted variant of scanword.
fn scan_int(data: &[u8], pos: &mut usize) -> i16 {
    if *pos >= data.len() {
        return 0;
    }
    if (data[*pos] & 1) != 0 {
        if *pos + 2 > data.len() {
            *pos = data.len();
            return 0;
        }
        let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
        *pos += 2;
        ((v >> 1) as i16).wrapping_sub(0x4000)
    } else {
        let v = data[*pos];
        *pos += 1;
        ((v >> 1) as i16).wrapping_sub(0x40)
    }
}

/// Wrap a text inline with the current bold/italic state.
fn wrap_inline(inner: Inline, bold: bool, italic: bool) -> Inline {
    match (bold, italic) {
        (true, true) => Inline::Bold(vec![Inline::Italic(vec![inner])]),
        (true, false) => Inline::Bold(vec![inner]),
        (false, true) => Inline::Italic(vec![inner]),
        (false, false) => inner,
    }
}

/// Append an inline, merging adjacent plain-text runs.
fn push_or_merge(out: &mut Vec<Inline>, inline: Inline) {
    if let (Some(Inline::Text(prev)), Inline::Text(new)) = (out.last_mut(), &inline) {
        prev.push_str(new);
        return;
    }
    out.push(inline);
}

/// Resolve a context hash to a context ID string.
fn resolve_hash_target(hash: u32, hash_targets: &HashMap<u32, String>) -> String {
    match hash_targets.get(&hash) {
        Some(context_id) => context_id.clone(),
        None => format!("0x{hash:08X}"),
    }
}

/// Locate the start of the command stream within LinkData1 for a TL_NORMAL
/// record, by consuming the structured paragraph info header.
fn find_command_stream_start(ld1: &[u8]) -> usize {
    let mut p = 0usize;

    // scanlong (unused topic-size-ish field).
    let _ = scan_long(ld1, &mut p);
    // scanword (character count increment for TOPICOFFSET math).
    let _ = scan_word(ld1, &mut p);
    // Skip 4 unknown bytes.
    p = p.saturating_add(4);
    if p + 2 > ld1.len() {
        return ld1.len();
    }
    // u16 bitflags.
    let bitflags = u16::from_le_bytes([ld1[p], ld1[p + 1]]);
    p += 2;

    // Conditional fields. Order matches helpdeco exactly.
    if bitflags & 0x0001 != 0 {
        let _ = scan_long(ld1, &mut p);
    }
    for mask in [0x0002, 0x0004, 0x0008, 0x0010, 0x0020, 0x0040] {
        if bitflags & mask != 0 {
            let _ = scan_int(ld1, &mut p);
        }
    }
    if bitflags & 0x0100 != 0 {
        // Border: 1 byte + 2 bytes.
        p = p.saturating_add(3);
    }
    if bitflags & 0x0200 != 0 {
        // Tab stops.
        let y1 = scan_int(ld1, &mut p) as i32;
        for _ in 0..y1.max(0) {
            let x1 = scan_word(ld1, &mut p);
            if x1 & 0x4000 != 0 {
                let _ = scan_word(ld1, &mut p);
            }
        }
    }

    p.min(ld1.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::{FontDescriptor, FontTable};

    fn fonts() -> FontTable {
        FontTable::empty()
    }

    fn no_targets() -> HashMap<u32, String> {
        HashMap::new()
    }

    /// Build a valid empty preamble for a TL_NORMAL record. Layout:
    ///   scanlong = 0 (2 bytes) + scanword = 0 (1 byte) + 4 skip bytes +
    ///   u16 bitflags = 0 (2 bytes) = 9 bytes total.
    fn empty_preamble() -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    }

    /// Wrap a command byte sequence with the empty preamble.
    fn ld1(cmd: &[u8]) -> Vec<u8> {
        let mut v = empty_preamble();
        v.extend_from_slice(cmd);
        v
    }

    #[test]
    fn simple_paragraph_emits_single_block() {
        let cmd = [0x80, 0x00, 0x00, 0x82];
        let ld1 = ld1(&cmd);
        let ld2 = b"hello\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected Paragraph, got {:?}", blocks[0]);
        };
        assert_eq!(inlines.len(), 1);
        assert!(matches!(&inlines[0], Inline::Text(t) if t == "hello"));
    }

    #[test]
    fn two_paragraphs_from_two_end_para_opcodes() {
        let cmd = [0x80, 0x00, 0x00, 0x82, 0x80, 0x00, 0x00, 0x82];
        let ld1 = ld1(&cmd);
        let ld2 = b"first\0\0\0second\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn jump_link_with_text_from_segment() {
        let cmd = [
            0x80, 0x04, 0x00, 0xE3, 0xEF, 0xBE, 0xAD, 0xDE, 0x89, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        let ld2 = b"\0\0click here\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(inlines.len(), 1);
        let Inline::Link { text, target, kind } = &inlines[0] else {
            panic!("expected Link, got {:?}", inlines[0]);
        };
        assert_eq!(*kind, LinkKind::Jump);
        assert_eq!(target, "0xDEADBEEF");
        assert_eq!(text.len(), 1);
        assert!(matches!(&text[0], Inline::Text(t) if t == "click here"));
    }

    #[test]
    fn popup_link_uses_popup_kind() {
        let cmd = [
            0x80, 0x00, 0x00, 0xE6, 0x78, 0x56, 0x34, 0x12, 0x89, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        let ld2 = b"\0\0tooltip\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        let Inline::Link { kind, target, .. } = &inlines[0] else {
            panic!();
        };
        assert_eq!(*kind, LinkKind::Popup);
        assert_eq!(target, "0x12345678");
    }

    #[test]
    fn resolved_hash_uses_context_id() {
        let hash = crate::context_hash("printf");
        let mut targets = HashMap::new();
        targets.insert(hash, "printf".to_string());

        let mut cmd = vec![0x80, 0x00, 0x00, 0xE3];
        cmd.extend_from_slice(&hash.to_le_bytes());
        cmd.extend_from_slice(&[0x89, 0x80, 0x00, 0x00, 0x82]);
        let ld1 = ld1(&cmd);
        let ld2 = b"\0\0printf\0\0\0";

        let blocks = parse_text_record(&ld1, ld2, &fonts(), &targets).unwrap();
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        let Inline::Link { target, .. } = &inlines[0] else {
            panic!();
        };
        assert_eq!(target, "printf");
    }

    /// Font changes currently do not propagate into bold/italic/underline
    /// state — see `ParseState::apply_font` for the rationale. This test
    /// documents the current behaviour so we notice if we re-enable
    /// font-attribute styling later.
    #[test]
    fn font_change_does_not_toggle_bold_state() {
        // Font 1 is bold in this synthetic table; font 0 is regular.
        let fonts_with_bold = FontTable::from_descriptors(vec![
            FontDescriptor {
                attributes: 0x00,
                half_points: 20,
                font_family: 0,
                name: "Regular".into(),
            },
            FontDescriptor {
                attributes: 0x01,
                half_points: 20,
                font_family: 0,
                name: "Bold".into(),
            },
        ]);
        let cmd = [0x80, 0x01, 0x00, 0x80, 0x00, 0x00, 0x82];
        let ld1 = ld1(&cmd);
        let ld2 = b"\0printf\0 function\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts_with_bold, &no_targets()).unwrap();
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        // Everything comes out as plain text (merged by push_or_merge), with
        // no Bold wrapper applied.
        assert_eq!(inlines.len(), 1);
        assert!(matches!(&inlines[0], Inline::Text(t) if t == "printf function"));
    }

    #[test]
    fn end_of_record_stops_parsing() {
        let cmd = [0x80, 0x00, 0x00, 0xFF, 0x80, 0x00, 0x00];
        let ld1 = ld1(&cmd);
        let ld2 = b"visible\0invisible\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        assert!(matches!(&inlines[0], Inline::Text(t) if t == "visible"));
    }

    #[test]
    fn empty_record_returns_no_blocks() {
        let blocks = parse_text_record(&[], b"", &fonts(), &no_targets()).unwrap();
        assert!(blocks.is_empty());
    }

    /// Build an LD1 with a realistic preamble: bitflags 0x0A50 (qc + qr +
    /// li + fi + tab stops). Verifies the tab-stop path of
    /// find_command_stream_start matches helpdeco semantics.
    #[test]
    fn preamble_with_tab_stops_is_skipped() {
        let mut ld1 = Vec::new();
        // scanlong topicsize = 4: 1-byte LSB=0 → 2 bytes value (4<<1)|0x8000 = 0x8008.
        ld1.extend_from_slice(&[0x08, 0x80]);
        // scanword chars = 4: 1-byte LSB=0 → (4<<1) = 0x08.
        ld1.push(0x08);
        // Skip 4 bytes.
        ld1.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // u16 bitflags = 0x0200 (tab stops only).
        ld1.extend_from_slice(&[0x00, 0x02]);
        // scanint y1 = 2: 1-byte LSB=0 → (2 + 0x40) << 1 = 0x84.
        ld1.push(0x84);
        // tab 0: 1-byte scanword LSB=0 → value = 36 → byte 0x48.
        ld1.push(0x48);
        // tab 1: 2-byte scanword LSB=1 → value = 72 → bytes 0x91, 0x00.
        ld1.extend_from_slice(&[0x91, 0x00]);
        // Command stream: font change + end paragraph.
        ld1.extend_from_slice(&[0x80, 0x00, 0x00, 0x82]);
        let ld2 = b"hello\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        assert!(matches!(&inlines[0], Inline::Text(t) if t == "hello"));
    }

    #[test]
    fn multiple_links_in_series_preserve_separators() {
        let cmd = [
            0x80, 0x04, 0x00, 0xE3, 0xAA, 0xAA, 0xAA, 0xAA, 0x89, 0x80, 0x00, 0x00, 0x80, 0x04,
            0x00, 0xE3, 0xBB, 0xBB, 0xBB, 0xBB, 0x89, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        let mut ld2: Vec<u8> = Vec::new();
        ld2.extend_from_slice(b"\0");
        ld2.extend_from_slice(b"\0");
        ld2.extend_from_slice(b"atexit\0");
        ld2.extend_from_slice(b"\0");
        ld2.extend_from_slice(b", \0");
        ld2.extend_from_slice(b"\0");
        ld2.extend_from_slice(b"exit\0");
        ld2.extend_from_slice(b"\0");
        ld2.extend_from_slice(b"\0");

        let blocks = parse_text_record(&ld1, &ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        assert_eq!(inlines.len(), 3);
        let Inline::Link { text, target, .. } = &inlines[0] else {
            panic!("expected link, got {:?}", inlines[0]);
        };
        assert_eq!(target, "0xAAAAAAAA");
        assert!(matches!(&text[0], Inline::Text(t) if t == "atexit"));
        assert!(matches!(&inlines[1], Inline::Text(t) if t == ", "));
        let Inline::Link { text, target, .. } = &inlines[2] else {
            panic!();
        };
        assert_eq!(target, "0xBBBBBBBB");
        assert!(matches!(&text[0], Inline::Text(t) if t == "exit"));
    }

    #[test]
    fn consecutive_code_lines_produce_separate_paragraphs() {
        let cmd = [
            0x80, 0x09, 0x00, 0x80, 0x00, 0x00, 0x82, 0x80, 0x09, 0x00, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        let ld2 = b"\0#include <stdlib.h>\0\0\0void abort( void );\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn image_opcode_emits_block_image() {
        // Real bmc layout from clib.hlp:
        //   86 22 08 80 02 00 00 NN 00
        // → HC31 picture, hotspots=1, external bitmap #NN.
        //
        // Preceding 80 00 00 sets font + consumes an empty segment; the
        // image op consumes another empty segment; trailing 82 ends the
        // paragraph (no paragraph to flush because image is emitted as its
        // own block).
        let cmd = [
            0x80, 0x00, 0x00, // font change, consumes seg 0
            0x86, 0x22, 0x08, 0x80, 0x02, 0x00, 0x00, 0x07, 0x00, // bmc bm7, consumes seg 1
            0x82, // end paragraph, consumes seg 2
            0xFF,
        ];
        let ld1 = ld1(&cmd);
        // 3 empty segments to match consumption count.
        let ld2 = b"\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Image(img) = &blocks[0] else {
            panic!("expected Image, got {:?}", blocks[0]);
        };
        assert_eq!(img.filename, "|bm7");
        assert_eq!(img.placement, ImagePlacement::Inline);
    }

    #[test]
    fn image_opcode_left_and_right_placement() {
        // Left-aligned (bml, 0x87).
        let cmd_l = [
            0x80, 0x00, 0x00, 0x87, 0x22, 0x08, 0x80, 0x02, 0x00, 0x00, 0x03, 0x00, 0x82, 0xFF,
        ];
        let ld1_l = ld1(&cmd_l);
        let blocks_l = parse_text_record(&ld1_l, b"\0\0\0", &fonts(), &no_targets()).unwrap();
        let Block::Image(img) = &blocks_l[0] else {
            panic!();
        };
        assert_eq!(img.filename, "|bm3");
        assert_eq!(img.placement, ImagePlacement::Left);

        // Right-aligned (bmr, 0x88).
        let cmd_r = [
            0x80, 0x00, 0x00, 0x88, 0x22, 0x08, 0x80, 0x02, 0x00, 0x00, 0x05, 0x00, 0x82, 0xFF,
        ];
        let ld1_r = ld1(&cmd_r);
        let blocks_r = parse_text_record(&ld1_r, b"\0\0\0", &fonts(), &no_targets()).unwrap();
        let Block::Image(img) = &blocks_r[0] else {
            panic!();
        };
        assert_eq!(img.filename, "|bm5");
        assert_eq!(img.placement, ImagePlacement::Right);
    }

    #[test]
    fn image_opcode_embedded_variant_emits_no_block() {
        // PictureIsEmbedded = 1 → no external filename; we emit no block.
        let cmd = [
            0x80, 0x00, 0x00, 0x86, 0x22, 0x08, 0x80, 0x02, 0x01, 0x00, 0x00, 0x00, 0x82, 0xFF,
        ];
        let ld1 = ld1(&cmd);
        let blocks = parse_text_record(&ld1, b"\0\0\0", &fonts(), &no_targets()).unwrap();
        // The trailing 0x82 finds no pending paragraph either. Result: 0 blocks.
        assert!(blocks.is_empty(), "expected no blocks, got {blocks:?}");
    }

    #[test]
    fn scan_long_decodes_2byte_and_4byte_forms() {
        // 2-byte form: bytes 0x08 0x80 → value = (0x8008 >> 1) - 0x4000 = 4.
        let mut p = 0;
        assert_eq!(scan_long(&[0x08, 0x80], &mut p), 4);
        assert_eq!(p, 2);

        // 4-byte form: bytes with LSB=1. Encode value 1000:
        //   stored = ((1000 + 0x40000000) << 1) | 1 = 0x80_00_07_D1.
        let v = 1000u32;
        let stored = ((v.wrapping_add(0x4000_0000)) << 1) | 1;
        let bytes = stored.to_le_bytes();
        let mut p = 0;
        assert_eq!(scan_long(&bytes, &mut p), 1000);
        assert_eq!(p, 4);
    }
}
