//! Topic text opcode parser.
//!
//! Converts TOPICLINK text records (types 0x20 and 0x23) into the
//! [`Block`]/[`Inline`] document model.
//!
//! # Format
//!
//! Each text record contributes two parallel byte streams:
//!
//! * **LinkData1** — the command/opcode stream. Starts with a variable-length
//!   paragraph info header (paragraph size, alignment, tab stops, etc.), then
//!   a sequence of single-byte opcodes with parameters. Each opcode consumes
//!   zero or more text segments from LinkData2 and/or parameter bytes from
//!   LinkData1 itself.
//! * **LinkData2** — displayable text as a sequence of NUL-terminated strings
//!   ("segments"). Every opcode that emits or wraps visible text pops the
//!   next segment from this stream in order.
//!
//! This split is essential: LinkData2 contains no opcodes — it is plain
//! printable bytes between NULs. All formatting, paragraph structure, and
//! link markup lives in LinkData1.
//!
//! # Opcodes
//!
//! In the command stream, the opcodes most commonly observed in WinHelp 3.1
//! content are:
//!
//! | Byte | Name | Params | Segments consumed | Meaning |
//! |------|------|--------|-------------------|---------|
//! | `0x80 XX YY` | font change | u16 LE font index | 1 | emit segment in current font, then switch to font XX |
//! | `0x81` | line break | — | 1 | emit segment, then hard newline inside the paragraph |
//! | `0x82` | end paragraph | — | 1 | emit segment, then terminate paragraph |
//! | `0x83` | tab | — | 1 | emit segment, then insert a tab character |
//! | `0x86` / `0x87` | bold on / off | — | 0 | toggle bold state |
//! | `0x88` / `0x89` | italic on / link-end | — | `0x89`: 1 (as link text) if in link | `0x88` starts italic; `0x89` ends italic OR ends a link |
//! | `0x8B` / `0x8C` | underline on / off | — | 0 | toggle underline |
//! | `0xE3 HH HH HH HH` | jump link | u32 LE hash | 1 | start jump link; consume leading empty segment |
//! | `0xE6 HH HH HH HH` | popup link | u32 LE hash | 1 | start popup link |
//! | `0xC8 HH HH HH HH` | popup link (alt) | u32 LE hash | 1 | start popup link |
//! | `0xCC HH HH HH HH` | jump link (alt) | u32 LE hash | 1 | start jump link |
//! | `0xFF` | end of record | — | 0 | end of command stream |
//!
//! `0x89` is overloaded: it ends italic AND ends a link. When we are inside a
//! link, it takes priority as link-end and consumes the next segment as the
//! link's display text.
//!
//! # Paragraph info header
//!
//! The preamble before the command stream begins encodes paragraph-level
//! formatting (tab stops, margins, alignment). We only need to skip past it.
//! The heuristic:
//!
//! 1. If the byte pair `0x9E 0x48` appears in the first 32 bytes, that marks
//!    the tab-stop array. Skip past the marker, then consume little-endian
//!    u16 values as tab positions until we reach a byte that starts a known
//!    opcode.
//! 2. Otherwise, skip a minimal fixed-length header (two compressed ints plus
//!    four bytes of flags/spacing) and then scan forward for the first opcode
//!    byte.
//!
//! This is pragmatic rather than exact — it recovers text and links
//! correctly even when we don't perfectly model the paragraph info header.
//!
//! Reference: helpdeco source and Pete Davis / Mike Wallace, "The WinHelp
//! File Format" (1993).

use std::collections::HashMap;

use crate::font::FontTable;
use crate::{Block, Inline, LinkKind, Result};

// Opcode constants.
const OP_FONT_CHANGE: u8 = 0x80;
const OP_LINE_BREAK: u8 = 0x81;
const OP_END_PARAGRAPH: u8 = 0x82;
const OP_TAB: u8 = 0x83;
const OP_BOLD_ON: u8 = 0x86;
const OP_BOLD_OFF: u8 = 0x87;
const OP_ITALIC_ON: u8 = 0x88;
/// Ends italic; also ends a hyperlink when one is open.
const OP_LINK_OR_ITALIC_END: u8 = 0x89;
const OP_UNDERLINE_ON: u8 = 0x8B;
const OP_UNDERLINE_OFF: u8 = 0x8C;
const OP_POPUP_LINK_HASH_ALT: u8 = 0xC8;
const OP_JUMP_LINK_HASH_ALT: u8 = 0xCC;
const OP_JUMP_LINK_HASH: u8 = 0xE3;
const OP_POPUP_LINK_HASH: u8 = 0xE6;
const OP_END_OF_RECORD: u8 = 0xFF;

/// Parse a topic text record into document-model blocks.
///
/// `link_data1` is the command/opcode stream; `link_data2` is the NUL-delimited
/// text segments. `fonts` is the parsed |FONT table (for future semantic
/// styling — e.g. monospace → inline code). `hash_targets` maps context hash
/// → context-id string; unresolved hashes fall back to hex form.
pub fn parse_text_record(
    link_data1: &[u8],
    link_data2: &[u8],
    _fonts: &FontTable,
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

    Ok(parse_command_stream(cmd, &segments, hash_targets))
}

/// Split LinkData2 into text segments at NUL byte boundaries.
///
/// A NUL byte ends the preceding segment; two NULs in a row yield an empty
/// segment between them. Segments past the final NUL are dropped (they are
/// always empty in well-formed records).
fn split_segments(link_data2: &[u8]) -> Vec<String> {
    link_data2
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Pull the next segment, returning empty string if exhausted.
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

    /// Emit a text segment in the current formatting state into the current
    /// paragraph.
    fn emit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Links accumulate their own display text separately (handled in
        // `end_link` — the link's text segment is consumed by `0x89`).
        let inline = wrap_inline(Inline::Text(text.to_string()), self.bold, self.italic);
        push_or_merge(&mut self.current, inline);
        // Underline is not distinct in the RST output model; it's treated as
        // italic by RST writers anyway. Ignored for now.
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
            // Use the target as a fallback so the RST reference has a label.
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
}

/// Parse the command stream, consuming text segments from LinkData2.
fn parse_command_stream(
    cmd: &[u8],
    segments: &[String],
    hash_targets: &HashMap<u32, String>,
) -> Vec<Block> {
    let mut state = ParseState::new();
    let mut cur = SegCursor::new(segments);
    let mut pos = 0;

    while pos < cmd.len() {
        let byte = cmd[pos];
        match byte {
            OP_FONT_CHANGE => {
                // Font change: consume 2 param bytes, emit next segment in
                // current style. Font-based semantic styling (e.g. code) is
                // deferred to a later pass — see doc-comment on parse_text_record.
                pos += 1;
                if pos + 2 <= cmd.len() {
                    pos += 2;
                }
                let seg = cur.next();
                state.emit_text(seg);
            }
            OP_LINE_BREAK => {
                pos += 1;
                let seg = cur.next();
                state.emit_text(seg);
                // Render as a space — RST wraps paragraphs by whitespace
                // anyway. A dedicated newline would break RST paragraph rules.
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
            OP_BOLD_ON => {
                pos += 1;
                state.bold = true;
            }
            OP_BOLD_OFF => {
                pos += 1;
                state.bold = false;
            }
            OP_ITALIC_ON => {
                pos += 1;
                state.italic = true;
            }
            OP_LINK_OR_ITALIC_END => {
                pos += 1;
                if state.in_link {
                    // Link end: the NEXT segment is the link's display text.
                    let seg = cur.next();
                    state.end_link(seg, hash_targets);
                } else {
                    state.italic = false;
                }
            }
            OP_UNDERLINE_ON => {
                pos += 1;
                state.underline = true;
            }
            OP_UNDERLINE_OFF => {
                pos += 1;
                state.underline = false;
            }
            OP_JUMP_LINK_HASH | OP_JUMP_LINK_HASH_ALT => {
                pos += 1;
                let hash = read_u32_le(cmd, &mut pos);
                // Consume the empty "pre-link" segment.
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
                // Unknown opcode: skip one byte and continue. We do not emit
                // or consume a segment — the cost is potentially missing a
                // character, but the alternative (advancing the segment
                // cursor) risks losing whole strings of real content.
                pos += 1;
            }
        }
    }

    // Flush any residual inline content as a final paragraph.
    state.end_paragraph();
    state.blocks
}

/// Read a little-endian u32, advancing `pos`. If the buffer is too short,
/// returns 0 and does not advance past the end.
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

/// Wrap a text inline with the current bold/italic state.
fn wrap_inline(inner: Inline, bold: bool, italic: bool) -> Inline {
    match (bold, italic) {
        (true, true) => Inline::Bold(vec![Inline::Italic(vec![inner])]),
        (true, false) => Inline::Bold(vec![inner]),
        (false, true) => Inline::Italic(vec![inner]),
        (false, false) => inner,
    }
}

/// Append an inline, merging adjacent plain-text runs to keep the model
/// compact.
fn push_or_merge(out: &mut Vec<Inline>, inline: Inline) {
    if let (Some(Inline::Text(prev)), Inline::Text(new)) = (out.last_mut(), &inline) {
        prev.push_str(new);
        return;
    }
    out.push(inline);
}

/// Resolve a context hash to a context ID string.
///
/// If the hash is in `hash_targets`, returns the context ID; otherwise falls
/// back to "0xHHHHHHHH" hex form so the link is still distinguishable.
fn resolve_hash_target(hash: u32, hash_targets: &HashMap<u32, String>) -> String {
    match hash_targets.get(&hash) {
        Some(context_id) => context_id.clone(),
        None => format!("0x{hash:08X}"),
    }
}

/// Locate the start of the command stream within LinkData1.
///
/// See the module-level docs for the heuristic. The strategy is to find the
/// `9E 48` tab-array marker (if any), skip u16 tab positions after it, and
/// stop as soon as we hit what looks like an opcode byte (≥ 0x80 in the low
/// byte of a u16). If the marker is absent, skip a minimal fixed preamble
/// (two compressed ints + flag bytes) then scan forward for the first byte
/// that looks like a valid opcode.
fn find_command_stream_start(ld1: &[u8]) -> usize {
    // Search the first 48 bytes for the tab-array marker.
    let search_limit = ld1.len().min(48);
    if let Some(mpos) = ld1[..search_limit]
        .windows(2)
        .position(|w| w == [0x9E, 0x48])
    {
        // Skip past the 2-byte marker, then read u16 pairs until we hit a
        // byte that could start an opcode.
        let mut p = mpos + 2;
        while p + 1 < ld1.len() {
            // If the low byte of the pair is ≥ 0x80, treat it as an opcode
            // byte — command stream starts here.
            if ld1[p] >= 0x80 {
                return p;
            }
            p += 2;
        }
        return p;
    }

    // No tab-array marker. Walk a minimal preamble.
    //
    // The preamble layout that consistently works for WinHelp 3.1 records
    // without tab stops:
    //   compressed_word topicsize    (1 or 2 bytes; LSB-encoded)
    //   compressed_word topiclength  (1 or 2 bytes)
    //   byte unknown
    //   byte id
    //   u16 bitflags                 (LE)
    //   optional u16 per set bit     (spacing, alignment, etc.)
    //
    // After the fixed part, scan for the first byte that could start a
    // known opcode. This recovers correctly for headerless records too.
    let mut p = 0;

    // Compressed-word topicsize.
    p += compressed_word_size(ld1.get(p).copied());
    // Compressed-word topiclength.
    if p < ld1.len() {
        p += compressed_word_size(ld1.get(p).copied());
    }
    // byte + byte + u16 — 4 fixed bytes.
    p += 4;
    if p >= ld1.len() {
        return ld1.len();
    }

    // Scan forward to the first plausible opcode start.
    while p < ld1.len() {
        if is_opcode_start(ld1[p]) {
            return p;
        }
        p += 1;
    }
    ld1.len()
}

/// Byte size of a compressed-word field (1 byte if LSB=0, else 2 bytes).
fn compressed_word_size(first: Option<u8>) -> usize {
    match first {
        Some(b) if (b & 0x01) != 0 => 2,
        Some(_) => 1,
        None => 0,
    }
}

/// Does this byte begin a known opcode in the command stream?
fn is_opcode_start(b: u8) -> bool {
    matches!(
        b,
        OP_FONT_CHANGE
            | OP_LINE_BREAK
            | OP_END_PARAGRAPH
            | OP_TAB
            | OP_BOLD_ON
            | OP_BOLD_OFF
            | OP_ITALIC_ON
            | OP_LINK_OR_ITALIC_END
            | OP_UNDERLINE_ON
            | OP_UNDERLINE_OFF
            | OP_POPUP_LINK_HASH_ALT
            | OP_JUMP_LINK_HASH_ALT
            | OP_JUMP_LINK_HASH
            | OP_POPUP_LINK_HASH
            | OP_END_OF_RECORD
    )
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

    /// Build a minimal LinkData1 that has no paragraph info header — just
    /// an empty preamble so the command stream starts at byte 0. This keeps
    /// tests focused on command-stream behaviour rather than preamble parsing.
    fn no_preamble() -> Vec<u8> {
        // compressed_word topicsize = 0 (LSB=0, 1 byte)
        // compressed_word topiclength = 0 (LSB=0, 1 byte)
        // byte unknown + byte id + u16 bitflags = 0
        vec![0, 0, 0, 0, 0, 0]
    }

    /// Wrap a command byte sequence into a full LinkData1 buffer.
    fn ld1(cmd: &[u8]) -> Vec<u8> {
        let mut v = no_preamble();
        v.extend_from_slice(cmd);
        v
    }

    #[test]
    fn simple_paragraph_emits_single_block() {
        // One font command to emit the "hello" segment, then end paragraph.
        let cmd = [0x80, 0x00, 0x00, 0x82];
        let ld1 = ld1(&cmd);
        let ld2 = b"hello\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();

        // Expect a single paragraph containing "hello". The final `82`
        // consumes an empty segment.
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected Paragraph, got {:?}", blocks[0]);
        };
        assert_eq!(inlines.len(), 1);
        assert!(matches!(&inlines[0], Inline::Text(t) if t == "hello"));
    }

    #[test]
    fn two_paragraphs_from_two_end_para_opcodes() {
        // Pattern: `80 00 00` (emit seg in font 0), `82` (emit seg, end para),
        // repeated twice.
        let cmd = [0x80, 0x00, 0x00, 0x82, 0x80, 0x00, 0x00, 0x82];
        let ld1 = ld1(&cmd);
        // Segments: "first", "", "", "second", "", ""
        // Pattern trace:
        //   80 00 00 → emit "first"
        //   82       → emit "",    end para 1 (has "first")
        //   80 00 00 → emit ""
        //   82       → emit "second", end para 2 (has "second")
        let ld2 = b"first\0\0\0second\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn jump_link_with_text_from_segment() {
        // Commands:
        //   80 04 00              (emit empty seg → font 4)
        //   E3 EF BE AD DE        (start jump link, hash 0xDEADBEEF)
        //   89                    (link-end → next seg = link text)
        //   80 00 00              (emit trailing empty → font 0)
        //   82                    (emit empty → end paragraph)
        let cmd = [
            0x80, 0x04, 0x00, 0xE3, 0xEF, 0xBE, 0xAD, 0xDE, 0x89, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        // Segments: "", "", "click here", "", ""
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

    #[test]
    fn bold_toggle_wraps_text() {
        let cmd = [
            0x86, // bold on
            0x80, 0x00, 0x00, // emit "printf" in font 0 (bold is on)
            0x87, // bold off
            0x80, 0x00, 0x00, // emit " function" (no style)
            0x82, // end paragraph
        ];
        let ld1 = ld1(&cmd);
        let ld2 = b"printf\0 function\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        assert_eq!(inlines.len(), 2);
        assert!(matches!(&inlines[0], Inline::Bold(_)));
        assert!(matches!(&inlines[1], Inline::Text(t) if t == " function"));
    }

    #[test]
    fn end_of_record_stops_parsing() {
        let cmd = [
            0x80, 0x00, 0x00, // emit "visible"
            0xFF, // end of record
            0x80, 0x00, 0x00, // not reached
        ];
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

    #[test]
    fn tab_array_preamble_is_skipped() {
        // Build LinkData1 containing a 9E 48 tab marker followed by three
        // tab u16 values, then a normal command stream.
        let mut ld1 = Vec::new();
        // Minimal pre-tab preamble (arbitrary bytes; just must not contain 9E 48).
        ld1.extend_from_slice(&[0x10, 0x80, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x02]);
        // Tab marker + 3 u16 values (low byte < 0x80 so they're not taken as opcodes).
        ld1.extend_from_slice(&[0x9E, 0x48]);
        ld1.extend_from_slice(&[0x10, 0x00, 0x20, 0x00, 0x30, 0x00]);
        // Command stream: font-change + end-paragraph.
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
        // Pattern like clib.hlp "See Also" lists:
        //   80 04 00  e3 <hash1> 89  80 00 00
        //   80 04 00  e3 <hash2> 89  80 00 00
        //   82
        // Segments expected per link: "" (before 80 04 00), "" (before e3),
        //   "<linktext>" (consumed by 89), "" (after 89, consumed by 80 00 00),
        //   "<separator>" (consumed by next 80 04 00), ...
        let cmd = [
            0x80, 0x04, 0x00, 0xE3, 0xAA, 0xAA, 0xAA, 0xAA, 0x89, 0x80, 0x00, 0x00, 0x80, 0x04,
            0x00, 0xE3, 0xBB, 0xBB, 0xBB, 0xBB, 0x89, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        // Build LD2 matching the segment pattern.
        let mut ld2: Vec<u8> = Vec::new();
        ld2.extend_from_slice(b"\0"); // seg 0: "" for 1st 80 04 00
        ld2.extend_from_slice(b"\0"); // seg 1: "" for 1st e3
        ld2.extend_from_slice(b"atexit\0"); // seg 2: "atexit" for 1st 89
        ld2.extend_from_slice(b"\0"); // seg 3: "" for 1st trailing 80 00 00
        ld2.extend_from_slice(b", \0"); // seg 4: ", " for 2nd 80 04 00
        ld2.extend_from_slice(b"\0"); // seg 5: "" for 2nd e3
        ld2.extend_from_slice(b"exit\0"); // seg 6: "exit" for 2nd 89
        ld2.extend_from_slice(b"\0"); // seg 7: "" for 2nd trailing 80 00 00
        ld2.extend_from_slice(b"\0"); // seg 8: "" for final 82

        let blocks = parse_text_record(&ld1, &ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!();
        };
        // Expect: Link("atexit"), Text(", "), Link("exit").
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
        // Regression for clib.hlp #include blocks that used to collapse into
        // one line. Two 80/80/82 sequences → two paragraphs.
        let cmd = [
            0x80, 0x09, 0x00, 0x80, 0x00, 0x00, 0x82, 0x80, 0x09, 0x00, 0x80, 0x00, 0x00, 0x82,
        ];
        let ld1 = ld1(&cmd);
        let ld2 = b"\0#include <stdlib.h>\0\0\0void abort( void );\0\0\0";
        let blocks = parse_text_record(&ld1, ld2, &fonts(), &no_targets()).unwrap();
        assert_eq!(blocks.len(), 2);
    }
}
