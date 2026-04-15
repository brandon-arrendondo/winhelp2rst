//! Decompression routines for WinHelp topic data.
//!
//! Two-pass decompression:
//! 1. **LZ77 variant** — sliding-window decompression (4096-byte window,
//!    initialized to spaces).
//! 2. **Phrase substitution** — 2-byte tokens with high bit set expand to
//!    entries from the phrase dictionary.

use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Phrase decompression
// ---------------------------------------------------------------------------

/// Phrase dictionary for first-pass decompression.
///
/// Two phrase-encoding variants exist:
///
/// **WinHelp 3.1 (non-Hall)** — used when only `|Phrases` is present:
///   - Bytes 1–15 in LinkData2 are 2-byte phrase tokens:
///     `phrase_val = 256 * (byte1 - 1) + byte2`
///     `phrase_idx = phrase_val / 2`
///     trailing space if `phrase_val & 1 != 0`
///   - All other bytes (0 or 16–255) are literals.
///
/// **WinHelp 4.0 (Hall)** — used when `|PhrIndex` is present:
///   - `(byte & 1) == 0`: single-byte token, phrase 0–127, `idx = byte / 2`
///   - `(byte & 3) == 1`: two-byte token, phrase 128–16511,
///     `idx = 128 + (byte / 4) * 256 + next_byte`
///   - `(byte & 7) == 3`: literal run, `n = (byte - 3) / 8` literal chars
///   - `(byte & 0x0F) == 7`: space run, `n = (byte - 7) / 16` spaces
///   - `(byte & 0x0F) == 0x0F`: NUL run, `n = (byte - 0x0F) / 16` NULs
#[derive(Debug, Clone)]
pub struct PhraseTable {
    phrases: Vec<Vec<u8>>,
    /// True when loaded from `|PhrIndex` (WinHelp 4.0 Hall encoding).
    hall: bool,
}

impl PhraseTable {
    /// Parse the phrase table from the raw `|Phrases` file bytes.
    ///
    /// There are two main layouts:
    ///
    /// **With |PhrIndex** (WinHelp 4.0): offsets come from `phr_index`;
    /// `|Phrases` has a 2-byte count + phrase data.
    ///
    /// **Without |PhrIndex**: `|Phrases` has all data. Sub-layouts depend
    /// on whether phrase data is LZ77-compressed:
    ///   - Uncompressed: `u16 num_phrases` + `u16[num+1] offsets` + data
    ///   - Compressed: `u16 num_phrases` + `u16 0x0100` + `u32 uncompressed_size`
    ///     + `u16[num+1] offsets` + LZ77-compressed data
    ///
    /// Set `compressed` = true when `|SYSTEM` flags indicate LZ77.
    pub fn from_bytes(
        phrases_data: &[u8],
        phr_index: Option<&[u8]>,
        compressed: bool,
    ) -> Result<Self> {
        if phrases_data.len() < 2 {
            return Err(Error::BadInternalFile {
                name: "|Phrases".into(),
                detail: "too small for phrase count".into(),
            });
        }

        let num_phrases = u16::from_le_bytes([phrases_data[0], phrases_data[1]]) as usize;

        if let Some(index_data) = phr_index {
            // WinHelp 4.0: offsets are in |PhrIndex, phrase data is the
            // remainder of |Phrases after the 2-byte count.
            Self::parse_with_index(phrases_data, index_data, num_phrases)
        } else if compressed && phrases_data.len() >= 8 {
            // Compressed inline phrases: 8-byte header + offsets + LZ77 data.
            Self::parse_compressed_inline(phrases_data, num_phrases)
        } else {
            // Uncompressed inline: 2-byte header + offsets + raw data.
            Self::parse_inline(phrases_data, num_phrases)
        }
    }

    /// Build an empty phrase table (for files without phrase compression).
    pub fn empty() -> Self {
        Self {
            phrases: Vec::new(),
            hall: false,
        }
    }

    /// Number of phrases in the table.
    pub fn len(&self) -> usize {
        self.phrases.len()
    }

    /// Returns true if the phrase table is empty.
    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }

    /// Expand phrase tokens in the given data.
    ///
    /// Uses the WinHelp 3.1 non-Hall algorithm by default, or the Hall
    /// (WinHelp 4.0) algorithm when the table was loaded from `|PhrIndex`.
    pub fn expand(&self, data: &[u8]) -> Result<Vec<u8>> {
        if self.hall {
            self.expand_hall(data)
        } else {
            self.expand_oldstyle(data)
        }
    }

    /// WinHelp 3.1 (non-Hall) phrase expansion.
    ///
    /// Bytes 1–15 introduce a 2-byte phrase token; all other bytes are literal.
    fn expand_oldstyle(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 2);
        let mut i = 0;

        while i < data.len() {
            let cur = data[i];
            i += 1;

            if cur > 0 && cur < 16 {
                // Two-byte phrase token.
                if i >= data.len() {
                    // Incomplete token at end of stream — emit marker byte.
                    out.push(cur);
                    break;
                }
                let next = data[i];
                i += 1;

                let phrase_val = 256 * (cur as usize - 1) + next as usize;
                let phrase_idx = phrase_val / 2;
                let trailing_space = phrase_val & 1 != 0;

                if phrase_idx < self.phrases.len() {
                    out.extend_from_slice(&self.phrases[phrase_idx]);
                    if trailing_space {
                        out.push(b' ');
                    }
                } else {
                    return Err(Error::Decompression(format!(
                        "phrase index {phrase_idx} out of range (table has {} entries)",
                        self.phrases.len()
                    )));
                }
            } else {
                out.push(cur);
            }
        }

        Ok(out)
    }

    /// WinHelp 4.0 (Hall) phrase expansion.
    fn expand_hall(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 2);
        let mut i = 0;

        while i < data.len() {
            let mut cur = data[i] as usize;
            i += 1;

            if cur & 1 == 0 {
                // Single-byte: phrase 0..127
                let idx = cur / 2;
                if idx < self.phrases.len() {
                    out.extend_from_slice(&self.phrases[idx]);
                } else {
                    return Err(Error::Decompression(format!(
                        "phrase index {idx} out of range (table has {} entries)",
                        self.phrases.len()
                    )));
                }
            } else if cur & 3 == 1 {
                // Two-byte: phrase 128..16511
                if i >= data.len() {
                    out.push(cur as u8);
                    break;
                }
                let next = data[i] as usize;
                i += 1;
                let idx = 128 + (cur / 4) * 256 + next;
                if idx < self.phrases.len() {
                    out.extend_from_slice(&self.phrases[idx]);
                } else {
                    return Err(Error::Decompression(format!(
                        "phrase index {idx} out of range (table has {} entries)",
                        self.phrases.len()
                    )));
                }
            } else if cur & 7 == 3 {
                // Literal run: copy (cur - 3) / 8 bytes
                let n = cur.saturating_sub(3) / 8;
                let end = (i + n).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
                cur = cur.saturating_sub(8);
                let _ = cur; // consumed
            } else if cur & 0x0F == 0x07 {
                // Space run: (cur - 7) / 16 spaces
                let n = cur.saturating_sub(7) / 16;
                out.extend(std::iter::repeat_n(b' ', n));
            } else {
                // NUL run: (cur - 0x0F) / 16 NULs
                let n = cur.saturating_sub(0x0F) / 16;
                out.extend(std::iter::repeat_n(0u8, n));
            }
        }

        Ok(out)
    }

    /// WinHelp 3.1: offsets and phrase data are both in `|Phrases`.
    fn parse_inline(data: &[u8], num_phrases: usize) -> Result<Self> {
        // After the u16 count: (num_phrases + 1) u16 offsets, then phrase data.
        let offsets_size = (num_phrases + 1) * 2;
        let offsets_start = 2;
        let phrase_data_start = offsets_start + offsets_size;

        if data.len() < phrase_data_start {
            return Err(Error::BadInternalFile {
                name: "|Phrases".into(),
                detail: "not enough data for phrase offsets".into(),
            });
        }

        let mut phrases = Vec::with_capacity(num_phrases);
        for i in 0..num_phrases {
            let off_pos = offsets_start + i * 2;
            let off = u16::from_le_bytes([data[off_pos], data[off_pos + 1]]) as usize;
            let next_off_pos = offsets_start + (i + 1) * 2;
            let next_off =
                u16::from_le_bytes([data[next_off_pos], data[next_off_pos + 1]]) as usize;

            let start = phrase_data_start + off;
            let end = phrase_data_start + next_off;

            if end > data.len() {
                return Err(Error::BadInternalFile {
                    name: "|Phrases".into(),
                    detail: format!("phrase {i} extends past end of data"),
                });
            }

            phrases.push(data[start..end].to_vec());
        }

        Ok(Self { phrases, hall: false })
    }

    /// Compressed inline phrases (no |PhrIndex, LZ77-compressed).
    ///
    /// Layout:
    ///   u16 num_phrases
    ///   u16 constant (0x0100)
    ///   u32 uncompressed_phrase_data_size
    ///   u16[num_phrases + 1] offsets
    ///   LZ77-compressed phrase data
    ///
    /// Offsets are relative to the start of the offset array. The offset
    /// table itself is `(num_phrases + 1) * 2` bytes. Offset values start
    /// at that size (pointing past the offset table into phrase data).
    fn parse_compressed_inline(data: &[u8], num_phrases: usize) -> Result<Self> {
        let header_size = 8;
        let offsets_count = num_phrases + 1;
        let offsets_bytes = offsets_count * 2;
        let offsets_start = header_size;
        let compressed_start = offsets_start + offsets_bytes;

        if data.len() < compressed_start {
            return Err(Error::BadInternalFile {
                name: "|Phrases".into(),
                detail: "not enough data for phrase offsets (compressed)".into(),
            });
        }

        // Read offsets.
        let mut offsets = Vec::with_capacity(offsets_count);
        for i in 0..offsets_count {
            let pos = offsets_start + i * 2;
            let off = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            offsets.push(off);
        }

        // LZ77-decompress the phrase data section.
        let compressed_data = &data[compressed_start..];
        let decompressed = lz77_decompress(compressed_data)?;

        // Extract phrases. Offsets are relative to the offset array start,
        // so phrase data begins at offset = offsets_bytes within that space.
        let mut phrases = Vec::with_capacity(num_phrases);
        for i in 0..num_phrases {
            let start = offsets[i].saturating_sub(offsets_bytes);
            let end = offsets[i + 1].saturating_sub(offsets_bytes);

            if end > decompressed.len() {
                return Err(Error::BadInternalFile {
                    name: "|Phrases".into(),
                    detail: format!(
                        "phrase {i} extends past decompressed data \
                         (end={end}, decompressed={})",
                        decompressed.len()
                    ),
                });
            }

            phrases.push(decompressed[start..end].to_vec());
        }

        Ok(Self { phrases, hall: false })
    }

    /// WinHelp 4.0: offsets are in `|PhrIndex`, phrase data is in `|Phrases`
    /// after the 2-byte count.
    fn parse_with_index(
        phrases_data: &[u8],
        index_data: &[u8],
        num_phrases: usize,
    ) -> Result<Self> {
        let phrase_data_start = 2; // skip the u16 count
        let offsets_size = (num_phrases + 1) * 2;

        if index_data.len() < offsets_size {
            return Err(Error::BadInternalFile {
                name: "|PhrIndex".into(),
                detail: "not enough data for phrase offsets".into(),
            });
        }

        let mut phrases = Vec::with_capacity(num_phrases);
        for i in 0..num_phrases {
            let off_pos = i * 2;
            let off = u16::from_le_bytes([index_data[off_pos], index_data[off_pos + 1]]) as usize;
            let next_off_pos = (i + 1) * 2;
            let next_off =
                u16::from_le_bytes([index_data[next_off_pos], index_data[next_off_pos + 1]])
                    as usize;

            let start = phrase_data_start + off;
            let end = phrase_data_start + next_off;

            if end > phrases_data.len() {
                return Err(Error::BadInternalFile {
                    name: "|Phrases".into(),
                    detail: format!("phrase {i} extends past end of data (4.0 format)"),
                });
            }

            phrases.push(phrases_data[start..end].to_vec());
        }

        Ok(Self { phrases, hall: true })
    }
}

// ---------------------------------------------------------------------------
// LZ77 decompression
// ---------------------------------------------------------------------------

/// Decompress data using the WinHelp LZ77 variant.
///
/// Algorithm (from HELPFILE.TXT, Pete Davis / Manfred Winterhoff):
/// - Read a control byte; each bit (LSB first) selects the operation.
/// - Bit **clear (0)**: copy next byte literally to output.
/// - Bit **set (1)**: read a 16-bit little-endian word:
///   - lower 12 bits = `pos_field` (distance back from output end, minus 1)
///   - upper  4 bits = length field (actual copy length = value + 3)
///   - Copy `length` bytes starting at `output[current_end − pos_field − 1]`.
///
/// There is no pre-initialized window; back-references before any output
/// produce zero bytes.
pub fn lz77_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4);
    let mut pos: usize = 0;

    while pos < data.len() {
        let control = data[pos];
        pos += 1;

        for bit in 0..8 {
            if pos >= data.len() {
                break;
            }

            if control & (1 << bit) == 0 {
                // Bit clear → literal byte.
                out.push(data[pos]);
                pos += 1;
            } else {
                // Bit set → back-reference: read 2-byte little-endian word.
                if pos + 1 >= data.len() {
                    pos = data.len();
                    break;
                }

                let word = (data[pos] as usize) | ((data[pos + 1] as usize) << 8);
                pos += 2;

                // Lower 12 bits: distance back from current output end, minus 1.
                // Upper  4 bits: copy length minus 3.
                let pos_field = word & 0x0FFF;
                let length = ((word >> 12) & 0xF) + 3;
                let back = pos_field + 1;
                let base = out.len();

                for j in 0..length {
                    let src = base + j;
                    // src - back may underflow if we haven't written enough yet.
                    let byte = if src >= back { out[src - back] } else { 0 };
                    out.push(byte);
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Phrase table tests --

    #[test]
    fn phrase_table_inline() {
        // Build a |Phrases file with 2 phrases: "hello" and "world"
        let mut data = Vec::new();
        // num_phrases = 2
        data.extend_from_slice(&2u16.to_le_bytes());
        // 3 offsets: [0, 5, 10]
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&5u16.to_le_bytes());
        data.extend_from_slice(&10u16.to_le_bytes());
        // phrase data
        data.extend_from_slice(b"helloworld");

        let table = PhraseTable::from_bytes(&data, None, false).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(&table.phrases[0], b"hello");
        assert_eq!(&table.phrases[1], b"world");
    }

    #[test]
    fn phrase_table_with_index() {
        // |Phrases: num_phrases + phrase data (no inline offsets)
        let mut phrases_data = Vec::new();
        phrases_data.extend_from_slice(&2u16.to_le_bytes());
        phrases_data.extend_from_slice(b"foobar");

        // |PhrIndex: offsets [0, 3, 6]
        let mut index_data = Vec::new();
        index_data.extend_from_slice(&0u16.to_le_bytes());
        index_data.extend_from_slice(&3u16.to_le_bytes());
        index_data.extend_from_slice(&6u16.to_le_bytes());

        let table = PhraseTable::from_bytes(&phrases_data, Some(&index_data), false).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(&table.phrases[0], b"foo");
        assert_eq!(&table.phrases[1], b"bar");
    }

    #[test]
    fn phrase_expand_no_tokens() {
        let table = PhraseTable::empty();
        let input = b"plain text";
        let out = table.expand(input).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn phrase_expand_with_tokens() {
        // Build table with phrases: [0] = "hello", [1] = " world"
        let mut data = Vec::new();
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&5u16.to_le_bytes());
        data.extend_from_slice(&11u16.to_le_bytes());
        data.extend_from_slice(b"hello world");

        let table = PhraseTable::from_bytes(&data, None, false).unwrap();

        // Non-Hall (WinHelp 3.1) encoding:
        //   phrase_val = 256 * (byte1 - 1) + byte2
        //   phrase_idx = phrase_val / 2
        //
        // Token for index 0 (no trailing space): phrase_val = 0
        //   byte1 = 1 (0x01), byte2 = 0 (0x00)
        // Token for index 1 (no trailing space): phrase_val = 2
        //   byte1 = 1 (0x01), byte2 = 2 (0x02)
        let input = vec![0x01, 0x00, 0x01, 0x02];
        let out = table.expand(&input).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn phrase_expand_with_trailing_space() {
        // Build table with phrase [0] = "hello"
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes()); // 1 phrase
        data.extend_from_slice(&0u16.to_le_bytes()); // offset[0] = 0
        data.extend_from_slice(&5u16.to_le_bytes()); // offset[1] = 5
        data.extend_from_slice(b"hello");
        let table = PhraseTable::from_bytes(&data, None, false).unwrap();

        // phrase_val = 1 (odd) → phrase_idx = 0, trailing space
        // byte1 = 1, byte2 = 1: phrase_val = 256*0 + 1 = 1
        let input = vec![0x01, 0x01, b'!'];
        let out = table.expand(&input).unwrap();
        assert_eq!(out, b"hello !");
    }

    #[test]
    fn phrase_expand_mixed() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(b"the");

        let table = PhraseTable::from_bytes(&data, None, false).unwrap();

        // "Say " + phrase[0] (no trailing space) + " word"
        // Token for index 0: byte1=0x01, byte2=0x00
        let mut input = Vec::new();
        input.extend_from_slice(b"Say ");
        input.push(0x01); // phrase token marker
        input.push(0x00); // byte2 → phrase_val=0, idx=0, no trailing space
        input.extend_from_slice(b" word");
        let out = table.expand(&input).unwrap();
        assert_eq!(out, b"Say the word");
    }

    #[test]
    fn phrase_expand_literal_high_bytes() {
        // Bytes >= 16 (0x10..0xFF) are always literal in non-Hall mode.
        let table = PhraseTable::empty();
        let input = vec![0x10, 0x80, 0xFF, 0x20];
        let out = table.expand(&input).unwrap();
        assert_eq!(out, input.as_slice());
    }

    #[test]
    fn phrase_expand_out_of_range() {
        let table = PhraseTable::empty();
        // Token marker 0x01 + 0x00 → phrase_val=0, phrase_idx=0, table is empty → error.
        let input = vec![0x01, 0x00];
        let err = table.expand(&input).unwrap_err();
        assert!(matches!(err, Error::Decompression(_)));
    }

    #[test]
    fn phrase_empty_table() {
        let table = PhraseTable::empty();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    // -- LZ77 tests --
    //
    // Encoding rules (correct algorithm):
    //   control bit CLEAR (0) → next byte is a literal
    //   control bit SET   (1) → next 2 bytes are a back-reference:
    //       word = b0 | (b1 << 8)
    //       pos_field = word & 0x0FFF  (distance back from end, minus 1)
    //       length    = (word >> 12) + 3

    #[test]
    fn lz77_empty_input() {
        let out = lz77_decompress(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn lz77_all_literals() {
        // Control byte 0x00 = all 8 bits clear → 8 literal bytes follow.
        let mut input = vec![0x00u8];
        input.extend_from_slice(b"ABCDEFGH");
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDEFGH");
    }

    #[test]
    fn lz77_all_literals_multiple_blocks() {
        // Two control bytes 0x00, each followed by 8 literal bytes.
        let mut input = vec![0x00u8];
        input.extend_from_slice(b"ABCDEFGH");
        input.push(0x00);
        input.extend_from_slice(b"IJKLMNOP");
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDEFGHIJKLMNOP");
    }

    #[test]
    fn lz77_back_reference() {
        // Emit "ABC" as literals (bits 0,1,2 = 0), then a back-ref (bit 3 = 1)
        // that copies the last 3 bytes → "ABCABC".
        //
        // Control byte: bits 0,1,2 clear (literal), bit 3 set (back-ref) → 0x08
        // Back-ref word: pos_field = 2 (distance 3 back), length = 3 (upper nibble 0)
        //   word = 0x0002 → b0 = 0x02, b1 = 0x00
        let input = vec![0x08u8, b'A', b'B', b'C', 0x02, 0x00];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCABC");
    }

    #[test]
    fn lz77_longer_back_reference() {
        // Emit "ABCD" as literals (bits 0–3 clear), then back-ref (bit 4 set)
        // that copies 4 bytes → "ABCDABCD".
        //
        // Control byte: bits 0-3 clear, bit 4 set → 0x10
        // Back-ref: pos_field = 3 (distance 4 back), length = 4 (upper nibble = 1)
        //   word = (1 << 12) | 3 = 0x1003 → b0 = 0x03, b1 = 0x10
        let input = vec![0x10u8, b'A', b'B', b'C', b'D', 0x03, 0x10];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDABCD");
    }

    #[test]
    fn lz77_back_reference_from_empty_output_gives_zeros() {
        // A back-reference before any output has been emitted copies zero bytes
        // (no pre-initialized window).
        //
        // Control byte 0x01: bit 0 set → back-ref immediately.
        // Back-ref: pos_field=2, length=3 → word=0x0002 → b0=0x02, b1=0x00
        // Output is empty so all 3 bytes are zero.
        let input = vec![0x01u8, 0x02, 0x00];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"\x00\x00\x00");
    }

    #[test]
    fn lz77_repeating_run() {
        // "AAAA" via literal 'A' then back-ref of length 3 copying it 3 times.
        //
        // Control byte: bit 0 clear (literal), bit 1 set (back-ref) → 0x02
        // Back-ref: pos_field=0 (distance 1 back), length=3 → word=0x0000
        let input = vec![0x02u8, b'A', 0x00, 0x00];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"AAAA");
    }
}
