//! |TOPIC block reader and topic record parser.
//!
//! The `|TOPIC` internal file is divided into fixed-size blocks. Each block
//! has a 12-byte header followed by (possibly compressed) data. The blocks
//! are decompressed (LZ77 then phrase-expanded) and concatenated to form a
//! flat topic stream. Within that stream, topic records are linked together.

use crate::decompress::{lz77_decompress, PhraseTable};
use crate::Result;

/// Default on-disk topic block size for WinHelp 3.1 (4 KiB).
pub const TOPIC_BLOCK_SIZE_31: usize = 4096;

/// Size of the topic block header (3 × u32 = 12 bytes).
const TOPIC_BLOCK_HEADER_SIZE: usize = 12;

/// Size of the TOPICLINK record header (bytes 0–20 inclusive).
///
/// Layout (all little-endian):
///   u32 BlockSize   — total bytes: TOPICLINK header + LinkData1 + LinkData2
///   u32 DataLen2    — decompressed length of LinkData2
///   u32 PrevBlock   — TOPICPOS of previous TOPICLINK (HC31: absolute)
///   u32 NextBlock   — TOPICPOS of next     TOPICLINK (HC31: absolute)
///   u32 DataLen1    — bytes consumed by TOPICLINK header + LinkData1
///   u8  RecordType  — 0x02 = TOPICHDR, 0x20 = text, 0x23 = table
pub const TOPICLINK_HEADER_SIZE: usize = 21;

/// A single topic block (after decompression).
#[derive(Debug, Clone)]
pub struct TopicBlock {
    /// Offset of the last topic link starting in this block.
    pub last_topic_link: u32,
    /// Offset of the first topic link in this block (0xFFFFFFFF if none).
    pub first_topic_link: u32,
    /// Offset of the last topic header ending in this block.
    pub last_topic_header: u32,
    /// Decompressed block data (after LZ77 + phrase expansion).
    pub data: Vec<u8>,
}

/// Topic record type: topic header (contains footnotes and metadata).
pub const RECORD_TYPE_TOPIC: u8 = 0x02;

/// Topic record type: displayable text (with opcodes).
pub const RECORD_TYPE_TEXT: u8 = 0x20;

/// Topic record type: displayable text (table variant).
pub const RECORD_TYPE_TABLE: u8 = 0x23;

/// A raw topic record extracted from the flattened topic stream.
#[derive(Debug, Clone)]
pub struct RawTopicRecord {
    /// Record type (0x02, 0x20, or 0x23).
    pub record_type: u8,
    /// LinkData1: record-type-specific structured data (not phrase-compressed).
    pub link_data1: Vec<u8>,
    /// LinkData2: displayable/title data (may be phrase-compressed).
    pub link_data2: Vec<u8>,
    /// Decompressed length of LinkData2 (used to detect phrase compression).
    pub data_len2: usize,
    /// TOPICPOS of the next TOPICLINK (0 or negative → end of chain).
    pub next_block: u32,
    /// Byte offset of this record within the virtual topic stream.
    pub stream_offset: usize,
}

/// Parsed topic metadata from a TOPICHDR record and related index files.
#[derive(Debug, Clone, Default)]
pub struct TopicMetadata {
    /// Context string (stable topic ID) — populated from the |CONTEXT B+ tree
    /// by the caller; not present in the TOPICHDR record itself.
    pub context_id: Option<String>,
    /// Display title — from the NUL-terminated string in TOPICHDR LinkData2.
    pub title: Option<String>,
    /// Keyword index entries — populated from |KWBTREE by the caller.
    pub keywords: Vec<String>,
    /// Browse sequence identifier — populated from |BGLOSS by the caller.
    pub browse_seq: Option<String>,
}

/// Read `|TOPIC` data as a sequence of decompressed blocks.
///
/// `block_size` is typically 4096 (WinHelp 3.1) or 2048 (WinHelp 4.0).
/// `use_lz77` indicates whether the blocks are LZ77-compressed.
///
/// Phrase expansion is NOT applied here — it must be applied to
/// individual record data payloads after record extraction, since
/// record headers contain binary values that would be misinterpreted
/// as phrase tokens.
pub fn read_topic_blocks(
    topic_data: &[u8],
    block_size: usize,
    use_lz77: bool,
    _phrases: &PhraseTable,
) -> Result<Vec<TopicBlock>> {
    let mut blocks = Vec::new();
    let mut pos = 0;

    while pos + TOPIC_BLOCK_HEADER_SIZE <= topic_data.len() {
        let remaining = topic_data.len() - pos;
        let this_block_size = remaining.min(block_size);

        if this_block_size < TOPIC_BLOCK_HEADER_SIZE {
            break;
        }

        let last_topic_link = u32::from_le_bytes([
            topic_data[pos],
            topic_data[pos + 1],
            topic_data[pos + 2],
            topic_data[pos + 3],
        ]);
        let first_topic_link = u32::from_le_bytes([
            topic_data[pos + 4],
            topic_data[pos + 5],
            topic_data[pos + 6],
            topic_data[pos + 7],
        ]);
        let last_topic_header = u32::from_le_bytes([
            topic_data[pos + 8],
            topic_data[pos + 9],
            topic_data[pos + 10],
            topic_data[pos + 11],
        ]);

        let compressed = &topic_data[pos + TOPIC_BLOCK_HEADER_SIZE..pos + this_block_size];

        // LZ77 decompress if enabled. Phrase expansion is deferred to
        // record-level processing (extract_records / parse_text_record).
        let decompressed = if use_lz77 {
            lz77_decompress(compressed)?
        } else {
            compressed.to_vec()
        };

        blocks.push(TopicBlock {
            last_topic_link,
            first_topic_link,
            last_topic_header,
            data: decompressed,
        });

        pos += block_size;
    }

    Ok(blocks)
}

/// Flatten topic blocks into a virtual topic stream.
///
/// Each decompressed block is padded with zeros to `decompress_size` bytes so
/// that TOPICPOS arithmetic works correctly:
///
///   virtual_offset = topicpos - 12
///   block_num      = virtual_offset / decompress_size
///   block_off      = virtual_offset % decompress_size
///
/// Use `SystemInfo::decompress_size()` for the correct value (16 384 for
/// WinHelp 3.1+, 2 048 for WinHelp 3.0).
pub fn flatten_topic_stream(blocks: &[TopicBlock], decompress_size: usize) -> Vec<u8> {
    let mut stream = Vec::with_capacity(blocks.len() * decompress_size);
    for block in blocks {
        stream.extend_from_slice(&block.data);
        // Pad to the virtual block boundary.
        if block.data.len() < decompress_size {
            let pad = decompress_size - block.data.len();
            stream.extend(std::iter::repeat_n(0u8, pad));
        }
    }
    stream
}

/// Extract raw topic records from the virtual topic stream.
///
/// Uses TOPICPOS navigation to follow the linked list of records rather than
/// linear scanning. This avoids misinterpreting zero-padding between virtual
/// blocks as record data.
///
/// TOPICLINK header layout (21 bytes):
///   Offset  Size  Field
///    0       4    BlockSize  — total bytes: header + LinkData1 + LinkData2
///    4       4    DataLen2   — decompressed length of LinkData2
///    8       4    PrevBlock  — TOPICPOS of previous record (ignored)
///   12       4    NextBlock  — TOPICPOS of next record (0/negative → end)
///   16       4    DataLen1   — bytes of header (21) + LinkData1
///   20       1    RecordType — 0x02=TOPICHDR, 0x20=text, 0x23=table
///
/// `before_31`: true for WinHelp 3.0 (minor ≤ 16) where NextBlock is
/// relative; false for WinHelp 3.1+ (minor > 16) where NextBlock is
/// absolute TOPICPOS.
///
/// The first record is always at TOPICPOS 12 = virtual offset 0.
pub fn extract_records(stream: &[u8], before_31: bool) -> Result<Vec<RawTopicRecord>> {
    let mut records = Vec::new();

    // TOPICPOS 12 → virtual offset 0 (the first block's data begins here).
    let mut virtual_offset: usize = 0;

    loop {
        let pos = virtual_offset;
        if pos + TOPICLINK_HEADER_SIZE > stream.len() {
            break;
        }

        let block_size = u32::from_le_bytes([
            stream[pos],
            stream[pos + 1],
            stream[pos + 2],
            stream[pos + 3],
        ]) as usize;

        if block_size < TOPICLINK_HEADER_SIZE {
            break;
        }

        let data_len2 = u32::from_le_bytes([
            stream[pos + 4],
            stream[pos + 5],
            stream[pos + 6],
            stream[pos + 7],
        ]) as usize;

        let next_block = u32::from_le_bytes([
            stream[pos + 12],
            stream[pos + 13],
            stream[pos + 14],
            stream[pos + 15],
        ]);

        let data_len1 = u32::from_le_bytes([
            stream[pos + 16],
            stream[pos + 17],
            stream[pos + 18],
            stream[pos + 19],
        ]) as usize;

        let record_type = stream[pos + 20];

        // LinkData1: bytes after the 21-byte header up to DataLen1.
        let ld1_len = data_len1.saturating_sub(TOPICLINK_HEADER_SIZE);
        let ld1_end = (pos + TOPICLINK_HEADER_SIZE + ld1_len).min(stream.len());
        let link_data1 = stream[pos + TOPICLINK_HEADER_SIZE..ld1_end].to_vec();

        // LinkData2: remainder of the record up to BlockSize.
        let ld2_start = pos + data_len1.max(TOPICLINK_HEADER_SIZE);
        let ld2_end = (pos + block_size).min(stream.len());
        let link_data2 = if ld2_start < ld2_end {
            stream[ld2_start..ld2_end].to_vec()
        } else {
            Vec::new()
        };

        records.push(RawTopicRecord {
            record_type,
            link_data1,
            link_data2,
            data_len2,
            next_block,
            stream_offset: pos,
        });

        // Navigate to the next record using the NextBlock TOPICPOS.
        if before_31 {
            // WinHelp 3.0: NextBlock is a relative offset added to current TOPICPOS.
            // Current TOPICPOS = virtual_offset + 12.
            let current_topicpos = virtual_offset + 12;
            let next_topicpos = current_topicpos + next_block as usize;
            if next_topicpos < 12 || next_topicpos - 12 >= stream.len() {
                break;
            }
            virtual_offset = next_topicpos - 12;
        } else {
            // WinHelp 3.1+: NextBlock is an absolute TOPICPOS.
            // A value ≤ 0 (treating as signed i32) signals end of chain.
            if (next_block as i32) <= 0 {
                break;
            }
            let next_topicpos = next_block as usize;
            if next_topicpos < 12 {
                break;
            }
            virtual_offset = next_topicpos - 12;
        }
    }

    Ok(records)
}

/// Parse metadata from a TOPICHDR record (type 0x02).
///
/// LinkData1 is the TOPICHEADER struct (28 bytes):
///   u32 BlockSize  — always 28 for TOPICHDR
///   u32 Browse1    — previous browse-sequence topic offset
///   u32 Browse2    — next browse-sequence topic offset
///   u16 HasTitle   — non-zero when a title string follows in LinkData2
///   u32 StartPos   — first text record offset for this topic
///   u32 Entries    — number of entries (non-scrolling region records)
///   u32 TopicNum   — ordinal topic number
///   u32 NonScroll  — TOPICPOS of last non-scroll record (0 if none)
///
/// LinkData2 contains the display title as a NUL-terminated string
/// (only present when HasTitle != 0). Context strings (# footnotes) are
/// stored in the |CONTEXT B+ tree, not in the topic data itself.
pub fn parse_topic_metadata(link_data1: &[u8], link_data2: &[u8]) -> TopicMetadata {
    let mut meta = TopicMetadata::default();

    // Extract title from LinkData2: first NUL-terminated string.
    if !link_data2.is_empty() {
        let end = link_data2
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(link_data2.len());
        let title = String::from_utf8_lossy(&link_data2[..end]).into_owned();
        if !title.is_empty() {
            meta.title = Some(title);
        }
    }

    // LinkData1 has a 28-byte TOPICHEADER struct; nothing else needed here.
    // Context IDs come from the |CONTEXT B+ tree (populated by the caller).
    let _ = link_data1;

    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic topic block (uncompressed, no phrases).
    fn make_block_data(last_link: u32, first_link: u32, last_header: u32, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&last_link.to_le_bytes());
        buf.extend_from_slice(&first_link.to_le_bytes());
        buf.extend_from_slice(&last_header.to_le_bytes());
        buf.extend_from_slice(body);
        buf
    }

    /// Build a TOPICLINK record with the correct 21-byte header.
    ///
    /// `link_data1` goes after the header; `link_data2` follows link_data1.
    /// `next_block` is stored at header offset 12.
    fn make_topiclink(
        record_type: u8,
        link_data1: &[u8],
        link_data2: &[u8],
        next_block: u32,
    ) -> Vec<u8> {
        let data_len1 = TOPICLINK_HEADER_SIZE + link_data1.len();
        let block_size = data_len1 + link_data2.len();
        let data_len2 = link_data2.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(&(block_size as u32).to_le_bytes()); // BlockSize
        buf.extend_from_slice(&(data_len2 as u32).to_le_bytes()); // DataLen2
        buf.extend_from_slice(&0u32.to_le_bytes()); // PrevBlock
        buf.extend_from_slice(&next_block.to_le_bytes()); // NextBlock
        buf.extend_from_slice(&(data_len1 as u32).to_le_bytes()); // DataLen1
        buf.push(record_type); // RecordType
        buf.extend_from_slice(link_data1);
        buf.extend_from_slice(link_data2);
        buf
    }

    #[test]
    fn read_single_uncompressed_block() {
        let body = b"hello world";
        let topic_data = make_block_data(0, 0, 0, body);
        let block_size = topic_data.len();
        let phrases = PhraseTable::empty();

        let blocks = read_topic_blocks(&topic_data, block_size, false, &phrases).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data, body);
    }

    #[test]
    fn read_multiple_blocks() {
        let block_size = 32;
        let body1 = b"block one data!!"; // 16 bytes
        let body2 = b"block two data!!"; // 16 bytes

        let mut topic_data = make_block_data(0, 0, 0, body1);
        topic_data.resize(block_size, 0); // pad to block_size
        let block2 = make_block_data(0, 0xFFFFFFFF, 0, body2);
        topic_data.extend_from_slice(&block2);
        topic_data.resize(block_size * 2, 0);

        let phrases = PhraseTable::empty();
        let blocks = read_topic_blocks(&topic_data, block_size, false, &phrases).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(&blocks[0].data[..body1.len()], &body1[..]);
        assert_eq!(&blocks[1].data[..body2.len()], &body2[..]);
        assert_eq!(blocks[1].first_topic_link, 0xFFFFFFFF);
    }

    #[test]
    fn flatten_pads_blocks_to_decompress_size() {
        let decompress_size = 16;
        let blocks = vec![
            TopicBlock {
                last_topic_link: 0,
                first_topic_link: 0,
                last_topic_header: 0,
                data: b"AAA".to_vec(), // 3 bytes → padded to 16
            },
            TopicBlock {
                last_topic_link: 0,
                first_topic_link: 0,
                last_topic_header: 0,
                data: b"BBB".to_vec(), // 3 bytes → padded to 16
            },
        ];

        let stream = flatten_topic_stream(&blocks, decompress_size);
        assert_eq!(stream.len(), 32);
        assert_eq!(&stream[..3], b"AAA");
        assert_eq!(&stream[3..16], &[0u8; 13]);
        assert_eq!(&stream[16..19], b"BBB");
        assert_eq!(&stream[19..32], &[0u8; 13]);
    }

    #[test]
    fn flatten_no_padding_when_exact() {
        let decompress_size = 3;
        let blocks = vec![
            TopicBlock {
                last_topic_link: 0,
                first_topic_link: 0,
                last_topic_header: 0,
                data: b"AAA".to_vec(),
            },
            TopicBlock {
                last_topic_link: 0,
                first_topic_link: 0,
                last_topic_header: 0,
                data: b"BBB".to_vec(),
            },
        ];

        let stream = flatten_topic_stream(&blocks, decompress_size);
        assert_eq!(stream, b"AAABBB");
    }

    #[test]
    fn extract_records_basic() {
        // One TOPICHDR record: next_block = 0 → chain ends after this record.
        let ld1 = b"";
        let ld2 = b"hello\0";
        // next_block = 0 (treated as i32 <= 0 → end of chain for WinHelp 3.1+)
        let stream = make_topiclink(RECORD_TYPE_TOPIC, ld1, ld2, 0);

        let records = extract_records(&stream, false).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RECORD_TYPE_TOPIC);
        assert_eq!(records[0].link_data1, b"");
        assert_eq!(records[0].link_data2, b"hello\0");
        assert_eq!(records[0].stream_offset, 0);
        assert_eq!(records[0].next_block, 0);
        assert_eq!(records[0].data_len2, ld2.len());
    }

    #[test]
    fn extract_records_multiple() {
        // Build two records linked by TOPICPOS.
        // Record 1 starts at virtual offset 0 (TOPICPOS = 12).
        // Record 2 starts at virtual offset = r1.len() (TOPICPOS = r1.len() + 12).
        // Record 1's next_block = r1.len() + 12.

        let ld1a = b"ld1_data";
        let ld2a = b"Topic Title\0";
        // next_block will be patched after we know r1 size
        let r1_placeholder = make_topiclink(RECORD_TYPE_TOPIC, ld1a, ld2a, 0);
        let r1_len = r1_placeholder.len();

        // Build r1 with correct next_block = r1_len + 12 (TOPICPOS of r2)
        let r1 = make_topiclink(RECORD_TYPE_TOPIC, ld1a, ld2a, (r1_len + 12) as u32);

        let ld1b = b"text_data_here";
        let ld2b = b"";
        // Record 2 ends the chain (next_block = 0)
        let r2 = make_topiclink(RECORD_TYPE_TEXT, ld1b, ld2b, 0);

        let r2_offset = r1.len();
        let mut stream = r1;
        stream.extend_from_slice(&r2);

        let records = extract_records(&stream, false).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, RECORD_TYPE_TOPIC);
        assert_eq!(records[0].link_data1, b"ld1_data");
        assert_eq!(records[0].link_data2, b"Topic Title\0");
        assert_eq!(records[0].next_block as usize, r1_len + 12);
        assert_eq!(records[1].record_type, RECORD_TYPE_TEXT);
        assert_eq!(records[1].link_data1, b"text_data_here");
        assert_eq!(records[1].link_data2, b"");
        assert_eq!(records[1].stream_offset, r2_offset);
    }

    #[test]
    fn extract_records_ignores_zero_padding_after_chain_end() {
        // TOPICPOS navigation stops at next_block=0, so zero-padding
        // after the last record is never visited.
        let record = make_topiclink(RECORD_TYPE_TEXT, b"data", b"", 0);
        let mut stream = record;
        stream.extend(std::iter::repeat_n(0u8, 32)); // padding after chain end

        let records = extract_records(&stream, false).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stream_offset, 0);
    }

    #[test]
    fn parse_metadata_with_title() {
        let ld2 = b"printf Function\0";
        let meta = parse_topic_metadata(b"", ld2);
        assert_eq!(meta.title.as_deref(), Some("printf Function"));
        assert!(meta.context_id.is_none());
        assert!(meta.keywords.is_empty());
        assert!(meta.browse_seq.is_none());
    }

    #[test]
    fn parse_metadata_empty_ld2() {
        let meta = parse_topic_metadata(b"", b"");
        assert!(meta.title.is_none());
        assert!(meta.context_id.is_none());
    }

    #[test]
    fn parse_metadata_nul_only_ld2() {
        // A NUL byte with no string content → no title.
        let meta = parse_topic_metadata(b"", b"\0");
        assert!(meta.title.is_none());
    }

    #[test]
    fn parse_metadata_title_without_nul() {
        // Title not NUL-terminated — read to end of slice.
        let meta = parse_topic_metadata(b"", b"unterminated");
        assert_eq!(meta.title.as_deref(), Some("unterminated"));
    }

    #[test]
    fn parse_metadata_context_id_from_caller() {
        // context_id is NOT set by parse_topic_metadata; caller must supply it.
        let meta = parse_topic_metadata(b"", b"Some Title\0");
        assert!(meta.context_id.is_none());
    }
}
