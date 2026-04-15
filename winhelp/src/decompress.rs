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
/// The `|Phrases` internal file contains up to 2048 short strings. In topic
/// text, a two-byte token where the first byte has its high bit set encodes
/// a phrase index: `index = ((byte1 & 0x7F) << 8) | byte2`.
#[derive(Debug, Clone)]
pub struct PhraseTable {
    phrases: Vec<Vec<u8>>,
}

impl PhraseTable {
    /// Parse the phrase table from the raw `|Phrases` file bytes.
    ///
    /// Layout:
    /// - `u16 num_phrases`
    /// - `u16[num_phrases + 1]` offsets into phrase data
    /// - phrase data bytes
    ///
    /// If `phr_index` is provided (WinHelp 4.0 `|PhrIndex`), offsets come
    /// from there instead (the `|Phrases` file contains only the phrase data,
    /// and the first two bytes are still `num_phrases`).
    pub fn from_bytes(phrases_data: &[u8], phr_index: Option<&[u8]>) -> Result<Self> {
        if phrases_data.len() < 2 {
            return Err(Error::BadInternalFile {
                name: "|Phrases".into(),
                detail: "too small for phrase count".into(),
            });
        }

        let num_phrases =
            u16::from_le_bytes([phrases_data[0], phrases_data[1]]) as usize;

        if let Some(index_data) = phr_index {
            // WinHelp 4.0: offsets are in |PhrIndex, phrase data is the
            // remainder of |Phrases after the 2-byte count.
            Self::parse_with_index(phrases_data, index_data, num_phrases)
        } else {
            // WinHelp 3.1: offsets are inline in |Phrases.
            Self::parse_inline(phrases_data, num_phrases)
        }
    }

    /// Build an empty phrase table (for files without phrase compression).
    pub fn empty() -> Self {
        Self {
            phrases: Vec::new(),
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
    /// Scans for 2-byte tokens where the first byte has its high bit set.
    /// Replaces them with the corresponding phrase. All other bytes pass
    /// through unchanged.
    pub fn expand(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 2);
        let mut i = 0;

        while i < data.len() {
            if data[i] & 0x80 != 0 {
                // Phrase token: need two bytes.
                if i + 1 >= data.len() {
                    // Trailing high-bit byte with no second byte — emit as-is.
                    out.push(data[i]);
                    i += 1;
                    continue;
                }

                let index = ((data[i] as usize & 0x7F) << 8) | data[i + 1] as usize;
                i += 2;

                if index < self.phrases.len() {
                    out.extend_from_slice(&self.phrases[index]);
                } else {
                    return Err(Error::Decompression(format!(
                        "phrase index {index} out of range (table has {} entries)",
                        self.phrases.len()
                    )));
                }
            } else {
                out.push(data[i]);
                i += 1;
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

        Ok(Self { phrases })
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

        Ok(Self { phrases })
    }
}

// ---------------------------------------------------------------------------
// LZ77 decompression
// ---------------------------------------------------------------------------

/// Window size for WinHelp LZ77 decompression.
const LZ77_WINDOW_SIZE: usize = 4096;

/// Decompress data using the WinHelp LZ77 variant.
///
/// The algorithm uses a 4096-byte sliding window initialized to spaces (0x20).
/// Input is processed as:
/// - Read a control byte; each bit (LSB first) indicates literal (1) or
///   back-reference (0).
/// - Literal: copy next byte to output and window.
/// - Back-reference: read 2 bytes as `(offset:12, length:4)`; copy
///   `length + 3` bytes from `window[offset]`.
pub fn lz77_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut window = vec![0x20u8; LZ77_WINDOW_SIZE];
    let mut win_pos: usize = 0;
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut pos: usize = 0;

    while pos < data.len() {
        let control = data[pos];
        pos += 1;

        for bit in 0..8 {
            if pos >= data.len() {
                break;
            }

            if control & (1 << bit) != 0 {
                // Literal byte.
                let byte = data[pos];
                pos += 1;

                out.push(byte);
                window[win_pos] = byte;
                win_pos = (win_pos + 1) % LZ77_WINDOW_SIZE;
            } else {
                // Back-reference: 2 bytes.
                if pos + 1 >= data.len() {
                    // Not enough data for a back-reference — stop.
                    pos = data.len();
                    break;
                }

                let b0 = data[pos] as usize;
                let b1 = data[pos + 1] as usize;
                pos += 2;

                // Encoding: low 8 bits of b0 + low 4 bits of b1 = 12-bit offset
                //           high 4 bits of b1 = 4-bit length (actual length = value + 3)
                let offset = b0 | ((b1 & 0x0F) << 8);
                let length = (b1 >> 4) + 3;

                for j in 0..length {
                    let byte = window[(offset + j) % LZ77_WINDOW_SIZE];
                    out.push(byte);
                    window[win_pos] = byte;
                    win_pos = (win_pos + 1) % LZ77_WINDOW_SIZE;
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

        let table = PhraseTable::from_bytes(&data, None).unwrap();
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

        let table = PhraseTable::from_bytes(&phrases_data, Some(&index_data)).unwrap();
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

        let table = PhraseTable::from_bytes(&data, None).unwrap();

        // Token for index 0: first byte = 0x80 | (0 >> 8) = 0x80, second = 0x00
        // Token for index 1: first byte = 0x80 | (1 >> 8) = 0x80, second = 0x01
        let input = vec![0x80, 0x00, 0x80, 0x01];
        let out = table.expand(&input).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn phrase_expand_mixed() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(b"the");

        let table = PhraseTable::from_bytes(&data, None).unwrap();

        // "Say " + phrase[0] + " word"
        let mut input = Vec::new();
        input.extend_from_slice(b"Say ");
        input.push(0x80);
        input.push(0x00);
        input.extend_from_slice(b" word");
        let out = table.expand(&input).unwrap();
        assert_eq!(out, b"Say the word");
    }

    #[test]
    fn phrase_expand_out_of_range() {
        let table = PhraseTable::empty();
        let input = vec![0x80, 0x00]; // index 0, but table is empty
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

    #[test]
    fn lz77_empty_input() {
        let out = lz77_decompress(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn lz77_all_literals() {
        // Control byte 0xFF = all 8 bits literal.
        // Followed by 8 literal bytes.
        let mut input = vec![0xFF];
        input.extend_from_slice(b"ABCDEFGH");
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDEFGH");
    }

    #[test]
    fn lz77_all_literals_multiple_blocks() {
        // Two control bytes, each all-literal.
        let mut input = vec![0xFF];
        input.extend_from_slice(b"ABCDEFGH");
        input.push(0xFF);
        input.extend_from_slice(b"IJKLMNOP");
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDEFGHIJKLMNOP");
    }

    #[test]
    fn lz77_back_reference() {
        // Write "ABCABC" by emitting ABC as literals then back-referencing.
        // Control byte: bits 0,1,2 = literal (1), bits 3 = back-ref (0) → 0x07
        // But we need 3 literals + 1 back-ref (3 bytes).
        // Actually the back-ref copies 3+ bytes. Let's do:
        // - Emit 'A','B','C' as 3 literals
        // - Back-reference to copy them again (offset=win_pos-3, length=3→encoded 0)
        //
        // Control byte: bits 0,1,2 = 1 (literal), bit 3 = 0 (back-ref)
        // bits 4-7 unused → 0x07
        //
        // After emitting A,B,C the window has them at positions 0,1,2.
        // win_pos = 3. Back-ref: offset=0, length=3 (encoded as 0).
        // b0 = offset & 0xFF = 0, b1 = ((offset >> 8) & 0x0F) | ((length-3) << 4)
        // = 0 | (0 << 4) = 0
        let input = vec![0x07, b'A', b'B', b'C', 0x00, 0x00];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCABC");
    }

    #[test]
    fn lz77_longer_back_reference() {
        // Emit "ABCD" as literals, then back-reference with length 4 (encoded 1).
        // Control byte: bits 0-3 = literal (1), bit 4 = back-ref (0) → 0x0F
        // Back-ref: offset=0, length=4 → b0=0, b1=((4-3)<<4)|0 = 0x10
        let input = vec![0x0F, b'A', b'B', b'C', b'D', 0x00, 0x10];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"ABCDABCD");
    }

    #[test]
    fn lz77_window_initialized_to_spaces() {
        // A back-reference to the initial window should produce spaces.
        // Control byte: bit 0 = 0 (back-ref)
        // Back-ref: offset=0, length=3 (encoded 0)
        let input = vec![0x00, 0x00, 0x00];
        let out = lz77_decompress(&input).unwrap();
        assert_eq!(out, b"   "); // 3 spaces
    }
}
