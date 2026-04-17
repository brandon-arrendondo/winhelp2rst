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
    ///
    /// Mirrors helpdeco.c:2442-2483 (`PhraseReplace`, Hall branch).  Each
    /// input byte `ch` is classified by its low bits:
    ///
    /// | low bits | meaning                              | count formula       |
    /// |----------|--------------------------------------|---------------------|
    /// | `xxxxxxx0` (any even) | phrase index = `ch / 2` (0..127) | 1 phrase |
    /// | `xxxxxx01`           | two-byte token; `idx = 128 + (ch/4)*256 + next` | 1 phrase |
    /// | `xxxxx011`           | literal copy: `(ch >> 3) + 1` bytes taken from the stream |
    /// | `xxxx0111`           | space run: `(ch >> 4) + 1` space characters |
    /// | `xxxx1111`           | NUL run: `(ch >> 4) + 1` NUL bytes |
    ///
    /// The `+1` comes from helpdeco's loop shape (`while CurChar > 0`
    /// decrementing by 8 or 16 each iteration), which guarantees at least
    /// one repetition for the smallest legal value in each family.
    fn expand_hall(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 2);
        let mut i = 0;

        while i < data.len() {
            let cur = data[i] as usize;
            i += 1;

            if cur & 1 == 0 {
                // Single-byte phrase reference, index 0..127.
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
                // Two-byte phrase reference, index 128..16511.
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
                // Literal byte run.  `(cur >> 3) + 1` literals follow.
                let n = (cur >> 3) + 1;
                let end = (i + n).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
            } else if cur & 0x0F == 0x07 {
                // Space run.  `(cur >> 4) + 1` spaces.
                let n = (cur >> 4) + 1;
                out.extend(std::iter::repeat_n(b' ', n));
            } else {
                // NUL run.  `(cur >> 4) + 1` NULs.
                let n = (cur >> 4) + 1;
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

        Ok(Self {
            phrases,
            hall: false,
        })
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

        Ok(Self {
            phrases,
            hall: false,
        })
    }

    /// Build the WinHelp 4.0 Hall phrase table from `|PhrIndex` + `|PhrImage`.
    ///
    /// `|PhrIndex` starts with a `PhrIndexHeader` (28 bytes), followed by a
    /// Golomb-style bitstream that encodes cumulative phrase lengths.  See
    /// helpdeco.c:1862-1898 for the reference implementation.
    ///
    /// `|PhrImage` is the concatenated phrase blob.  If
    /// `phrimagesize != phrimagecompressedsize`, it is LZ77-compressed and
    /// we decompress it here before splitting phrases using the decoded
    /// offsets.
    pub fn from_hall(phr_index: &[u8], phr_image: &[u8]) -> Result<Self> {
        let header = PhrIndexHeader::from_bytes(phr_index)?;

        // Decompress the phrase image if the two size fields disagree.
        let image: Vec<u8> = if header.phrimagesize == header.phrimagecompressedsize {
            phr_image.to_vec()
        } else {
            let decompressed = lz77_decompress(phr_image)?;
            if decompressed.len() != header.phrimagesize as usize {
                return Err(Error::BadInternalFile {
                    name: "|PhrImage".into(),
                    detail: format!(
                        "LZ77 output is {} bytes, PHRINDEXHDR says {}",
                        decompressed.len(),
                        header.phrimagesize
                    ),
                });
            }
            decompressed
        };

        // Decode `entries` cumulative offsets from the bitstream starting
        // immediately after the 28-byte header.  The bitstream is read in
        // 32-bit little-endian words, LSB-first, matching helpdec1.c:573
        // (`GetBit`).
        let mut reader = BitReader::new(&phr_index[PhrIndexHeader::SIZE..]);
        let mut offsets: Vec<usize> = Vec::with_capacity(header.entries as usize + 1);
        offsets.push(0);

        let bits = header.bits as u32;
        for _ in 0..header.entries {
            // Unary prefix: each `1` bit adds (1 << bits) to the length,
            // minimum length = 1.
            let mut n: usize = 1;
            while reader.read_bit()? {
                n = n
                    .checked_add(1 << bits)
                    .ok_or_else(|| Error::BadInternalFile {
                        name: "|PhrIndex".into(),
                        detail: "phrase length overflowed usize".into(),
                    })?;
            }
            // Fine-grained suffix.  The first bit is always read (adds 1
            // if set); subsequent bits are guarded by `bits > N` and
            // contribute 2, 4, 8, 16.  Matches helpdeco.c:1890-1894.
            if reader.read_bit()? {
                n += 1;
            }
            let mut add = 2usize;
            for step in 1..5 {
                if (bits as usize) > step {
                    if reader.read_bit()? {
                        n = n.checked_add(add).ok_or_else(|| Error::BadInternalFile {
                            name: "|PhrIndex".into(),
                            detail: "phrase length overflowed usize".into(),
                        })?;
                    }
                    add <<= 1;
                }
            }
            let next = offsets[offsets.len() - 1] + n;
            offsets.push(next);
        }

        // The final cumulative offset must match the decompressed image
        // size, otherwise the bitstream decode is off.
        let last = *offsets.last().unwrap();
        if last != image.len() {
            return Err(Error::BadInternalFile {
                name: "|PhrIndex".into(),
                detail: format!(
                    "decoded phrase offsets cover {last} bytes, |PhrImage has {}",
                    image.len()
                ),
            });
        }

        let mut phrases: Vec<Vec<u8>> = Vec::with_capacity(header.entries as usize);
        for i in 0..header.entries as usize {
            phrases.push(image[offsets[i]..offsets[i + 1]].to_vec());
        }

        Ok(Self {
            phrases,
            hall: true,
        })
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

        Ok(Self {
            phrases,
            hall: true,
        })
    }
}

// ---------------------------------------------------------------------------
// Hall phrase bitstream helpers (WinHelp 4.0 |PhrIndex)
// ---------------------------------------------------------------------------

/// Header at the start of `|PhrIndex` (28 bytes).  See helpdeco.h:220-232.
#[derive(Debug, Clone, Copy)]
struct PhrIndexHeader {
    entries: u32,
    phrimagesize: u32,
    phrimagecompressedsize: u32,
    /// Low 4 bits of the packed `{bits:4, unknown:12}` WORD at offset 24.
    bits: u8,
}

impl PhrIndexHeader {
    const SIZE: usize = 28;

    fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(Error::BadInternalFile {
                name: "|PhrIndex".into(),
                detail: format!(
                    "need {} bytes for PHRINDEXHDR, got {}",
                    Self::SIZE,
                    data.len()
                ),
            });
        }
        let entries = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let phrimagesize = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let phrimagecompressedsize = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let packed = u16::from_le_bytes([data[24], data[25]]);
        let bits = (packed & 0x0F) as u8;
        Ok(Self {
            entries,
            phrimagesize,
            phrimagecompressedsize,
            bits,
        })
    }
}

/// Little-endian DWORD-oriented bit reader.
///
/// Mirrors helpdec1.c:573 `GetBit`: consumes 32-bit words least-significant-
/// bit first, refilling from the backing slice once the current word is
/// exhausted.
struct BitReader<'a> {
    data: &'a [u8],
    /// Byte offset of the next DWORD to load.
    pos: usize,
    /// Current 32-bit word; undefined until `mask` has been shifted in.
    value: u32,
    /// Single-bit mask used to extract the next bit.  `0` means the next
    /// call must refill `value` from `data`.
    mask: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            value: 0,
            mask: 0,
        }
    }

    fn read_bit(&mut self) -> Result<bool> {
        self.mask <<= 1;
        if self.mask == 0 {
            if self.pos + 4 > self.data.len() {
                return Err(Error::BadInternalFile {
                    name: "|PhrIndex".into(),
                    detail: "bitstream exhausted before all phrase lengths were read".into(),
                });
            }
            self.value = u32::from_le_bytes([
                self.data[self.pos],
                self.data[self.pos + 1],
                self.data[self.pos + 2],
                self.data[self.pos + 3],
            ]);
            self.pos += 4;
            self.mask = 1;
        }
        Ok(self.value & self.mask != 0)
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

    // -- Hall phrase (WinHelp 4.0) tests --

    /// Build a minimal Hall |PhrIndex + |PhrImage pair with the given phrase
    /// byte-strings, using `bits=0` so every phrase's length is encoded with
    /// the zero-suffix-bits path (single `0` terminator bit + one body bit).
    fn build_hall(phrases: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let image: Vec<u8> = phrases.iter().flat_map(|p| p.iter().copied()).collect();
        // PHRINDEXHDR (28 bytes).
        let mut idx = Vec::with_capacity(64);
        idx.extend_from_slice(&1u32.to_le_bytes()); // always4A01
        idx.extend_from_slice(&(phrases.len() as u32).to_le_bytes()); // entries
        idx.extend_from_slice(&0u32.to_le_bytes()); // compressedsize (unused)
        idx.extend_from_slice(&(image.len() as u32).to_le_bytes()); // phrimagesize
        idx.extend_from_slice(&(image.len() as u32).to_le_bytes()); // phrimagecompressedsize (== phrimagesize → no LZ77)
        idx.extend_from_slice(&0u32.to_le_bytes()); // always0
        idx.extend_from_slice(&0u16.to_le_bytes()); // bits=0, unknown=0
        idx.extend_from_slice(&0x4A00u16.to_le_bytes()); // always4A00

        // Bitstream: per phrase, one `0` bit (end of Golomb prefix, adds
        // nothing) plus one "fine" bit that adds 1 if set.  With bits=0,
        // no further suffix bits are read.  We need each phrase length = its
        // actual byte count.  Length formula with bits=0:
        //   n = 1 + (suffix1 ? 1 : 0)
        // So only phrases of length 1 or 2 are representable without Golomb
        // iterations.  For longer phrases, the Golomb loop contributes
        // `1 << bits` per set bit — with bits=0 each iteration adds 1, so
        // length N requires (N-1) ones then a zero, then the suffix bit.
        let mut bits = BitWriter::new();
        for p in phrases {
            let n = p.len();
            assert!(n >= 1);
            // Golomb unary: (n - 1) one-bits followed by one zero-bit.
            for _ in 0..(n - 1) {
                bits.push(true);
            }
            bits.push(false);
            // Exactly one suffix bit is read (adds 1 if set) — we want it 0.
            bits.push(false);
        }
        idx.extend_from_slice(&bits.finish());
        (idx, image)
    }

    struct BitWriter {
        words: Vec<u32>,
        cur: u32,
        mask: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                words: Vec::new(),
                cur: 0,
                mask: 0,
            }
        }
        fn push(&mut self, bit: bool) {
            // Match helpdec1.c:573 `GetBit` ordering: mask starts at 0,
            // shifts left on the very first call, so bit 0 of the word is
            // the second bit written.  We mirror that: start mask=1 and
            // double after every write; the first bit written lands in
            // bit 1 (mask=2 after shift).  Actually, GetBit does
            // `mask <<= 1` FIRST and only refills when the shift produces
            // 0, so its first returned bit uses mask=1 (bit 0).  Mirror
            // that by writing bits in the order bit0, bit1, ...
            self.mask = self.mask.wrapping_shl(1);
            if self.mask == 0 {
                if self.cur != 0 || !self.words.is_empty() {
                    self.words.push(self.cur);
                    self.cur = 0;
                }
                self.mask = 1;
            }
            if bit {
                self.cur |= self.mask;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.mask != 0 {
                self.words.push(self.cur);
            }
            let mut out = Vec::with_capacity(self.words.len() * 4);
            for w in self.words {
                out.extend_from_slice(&w.to_le_bytes());
            }
            out
        }
    }

    #[test]
    fn hall_table_decodes_two_single_char_phrases() {
        let (idx, img) = build_hall(&[b"A", b"B"]);
        let table = PhraseTable::from_hall(&idx, &img).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(&table.phrases[0], b"A");
        assert_eq!(&table.phrases[1], b"B");
    }

    #[test]
    fn hall_expand_single_byte_phrase_even_byte() {
        // Hall encoding: even byte N means phrase N/2.
        let (idx, img) = build_hall(&[b"hello", b"world"]);
        let table = PhraseTable::from_hall(&idx, &img).unwrap();
        // Encoded stream: 0x00 (phrase 0), 0x02 (phrase 1)
        let out = table.expand(&[0x00, 0x02]).unwrap();
        assert_eq!(out, b"helloworld");
    }

    #[test]
    fn hall_expand_literal_run_formula() {
        // Control byte 0x0B (bit pattern xxxxx011) → literal run of
        // `(0x0B >> 3) + 1` = 2 literal bytes following.
        let (idx, img) = build_hall(&[b"phrase0"]);
        let table = PhraseTable::from_hall(&idx, &img).unwrap();
        let out = table.expand(&[0x0B, 0x41, 0x42]).unwrap();
        assert_eq!(out, b"AB");
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn hall_expand_space_run_formula() {
        // 0x07 → bits xxxx0111 → space run of (0x07 >> 4) + 1 = 1 space.
        // 0x17 → (1+1) = 2 spaces.
        let (idx, img) = build_hall(&[b"x"]);
        let table = PhraseTable::from_hall(&idx, &img).unwrap();
        let out = table.expand(&[0x07, 0x17]).unwrap();
        assert_eq!(out, b"   "); // 1 + 2
    }

    #[test]
    fn hall_expand_null_run_formula() {
        // 0x0F → bits xxxx1111 → NUL run of (0x0F >> 4) + 1 = 1 NUL.
        let (idx, img) = build_hall(&[b"x"]);
        let table = PhraseTable::from_hall(&idx, &img).unwrap();
        let out = table.expand(&[0x0F, 0x1F]).unwrap();
        assert_eq!(out, b"\x00\x00\x00"); // 1 + 2
    }

    #[test]
    fn hall_rejects_phrindex_offset_mismatch() {
        // Build a valid Hall table, then fib about the `entries` field so
        // the bitstream produces fewer offsets than the image covers.
        // This trips the "decoded phrase offsets cover N bytes, |PhrImage
        // has M" sanity check at the end of from_hall.
        let (mut idx, img) = build_hall(&[b"ABCDE"]);
        // Claim only 1 entry while the image expects length 5 bytes from
        // what would have been a 1-entry decode — but since build_hall
        // wrote a Golomb prefix for a 5-byte phrase, the first decoded
        // length is 5, so offsets = [0, 5] — matches the 5-byte image.
        // To force a mismatch, shrink the declared image size.
        idx[12..16].copy_from_slice(&3u32.to_le_bytes()); // phrimagesize
        idx[16..20].copy_from_slice(&3u32.to_le_bytes()); // phrimagecompressedsize
        let err = PhraseTable::from_hall(&idx, &img[..3]).unwrap_err();
        match err {
            Error::BadInternalFile { ref detail, .. } => {
                assert!(detail.contains("cover"), "got: {detail}");
            }
            _ => panic!("expected BadInternalFile, got {err:?}"),
        }
    }
}
