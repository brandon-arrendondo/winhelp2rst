//! |TOPIC block reader and topic record parser.
//!
//! The `|TOPIC` internal file is divided into fixed-size blocks. Each block
//! has a 12-byte header followed by (possibly compressed) data. The blocks
//! are decompressed (LZ77 then phrase-expanded) and concatenated to form a
//! flat topic stream. Within that stream, topic records are linked together.

use crate::decompress::{lz77_decompress, PhraseTable};
use crate::Result;

/// Default topic block size for WinHelp 3.1 (4 KiB).
pub const TOPIC_BLOCK_SIZE_31: usize = 4096;

/// Size of the topic block header (3 × u32 = 12 bytes).
const TOPIC_BLOCK_HEADER_SIZE: usize = 12;

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
    /// Raw data of this record.
    pub data: Vec<u8>,
    /// Byte offset of this record within the topic stream.
    pub stream_offset: usize,
}

/// Parsed topic metadata from footnote markers in a topic header record.
#[derive(Debug, Clone, Default)]
pub struct TopicMetadata {
    /// Context string — the stable topic ID (from `#` footnote).
    pub context_id: Option<String>,
    /// Display title (from `$` footnote).
    pub title: Option<String>,
    /// Keyword index entries (from `K` footnotes).
    pub keywords: Vec<String>,
    /// Browse sequence identifier (from `+` footnote).
    pub browse_seq: Option<String>,
}

/// Read `|TOPIC` data as a sequence of decompressed blocks.
///
/// `block_size` is typically 4096 (WinHelp 3.1) or 2048 (WinHelp 4.0).
/// `use_lz77` indicates whether the blocks are LZ77-compressed.
pub fn read_topic_blocks(
    topic_data: &[u8],
    block_size: usize,
    use_lz77: bool,
    phrases: &PhraseTable,
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

        // Decompress: LZ77 first (if enabled), then phrase expansion.
        let decompressed = if use_lz77 {
            lz77_decompress(compressed)?
        } else {
            compressed.to_vec()
        };

        let expanded = if !phrases.is_empty() {
            phrases.expand(&decompressed)?
        } else {
            decompressed
        };

        blocks.push(TopicBlock {
            last_topic_link,
            first_topic_link,
            last_topic_header,
            data: expanded,
        });

        pos += block_size;
    }

    Ok(blocks)
}

/// Flatten topic blocks into a single contiguous byte stream.
pub fn flatten_topic_stream(blocks: &[TopicBlock]) -> Vec<u8> {
    let total: usize = blocks.iter().map(|b| b.data.len()).sum();
    let mut stream = Vec::with_capacity(total);
    for block in blocks {
        stream.extend_from_slice(&block.data);
    }
    stream
}

/// Extract raw topic records from the flattened topic stream.
///
/// Each record starts with:
///   u32 block_size — total size of this record
///   u32 data_size  — decompressed data size (may differ from block_size)
///   u8  record_type — 0x02 (topic header), 0x20/0x23 (text)
///
/// Records link to each other; we scan linearly.
pub fn extract_records(stream: &[u8]) -> Result<Vec<RawTopicRecord>> {
    let mut records = Vec::new();
    let mut pos = 0;

    while pos + 9 <= stream.len() {
        let block_size = u32::from_le_bytes([
            stream[pos],
            stream[pos + 1],
            stream[pos + 2],
            stream[pos + 3],
        ]) as usize;

        // block_size includes the header. A size of 0 or less than 9 means
        // we've hit padding or end-of-stream.
        if block_size < 9 {
            break;
        }

        let _data_size = u32::from_le_bytes([
            stream[pos + 4],
            stream[pos + 5],
            stream[pos + 6],
            stream[pos + 7],
        ]);

        let record_type = stream[pos + 8];

        let record_end = (pos + block_size).min(stream.len());
        let data = stream[pos + 9..record_end].to_vec();

        records.push(RawTopicRecord {
            record_type,
            data,
            stream_offset: pos,
        });

        pos += block_size;
    }

    Ok(records)
}

/// Parse footnote metadata from a topic header record (type 0x02).
///
/// In topic header records, the data after the record header contains:
///   u32 next_topic — link offset to next topic
///   Then footnote markers: special byte + null-terminated string
///
/// Footnote marker bytes:
///   0x23 '#' — context string
///   0x24 '$' — display title
///   0x4B 'K' — keyword
///   0x2B '+' — browse sequence
pub fn parse_topic_metadata(record_data: &[u8]) -> TopicMetadata {
    let mut meta = TopicMetadata::default();

    if record_data.len() < 4 {
        return meta;
    }

    // Skip the u32 next_topic link.
    let mut pos = 4;

    while pos < record_data.len() {
        let marker = record_data[pos];
        pos += 1;

        if pos >= record_data.len() {
            break;
        }

        // Read null-terminated string.
        let remaining = &record_data[pos..];
        let null_pos = remaining.iter().position(|&b| b == 0);
        let (s, advance) = match null_pos {
            Some(n) => {
                let s = String::from_utf8_lossy(&remaining[..n]).into_owned();
                (s, n + 1)
            }
            None => {
                let s = String::from_utf8_lossy(remaining).into_owned();
                (s, remaining.len())
            }
        };

        match marker {
            0x23 => meta.context_id = Some(s),  // '#'
            0x24 => meta.title = Some(s),        // '$'
            0x4B => meta.keywords.push(s),       // 'K'
            0x2B => meta.browse_seq = Some(s),   // '+'
            _ => {}
        }

        pos += advance;
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic topic block (uncompressed, no phrases).
    fn make_block_data(
        last_link: u32,
        first_link: u32,
        last_header: u32,
        body: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&last_link.to_le_bytes());
        buf.extend_from_slice(&first_link.to_le_bytes());
        buf.extend_from_slice(&last_header.to_le_bytes());
        buf.extend_from_slice(body);
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
    fn flatten_produces_contiguous_stream() {
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

        let stream = flatten_topic_stream(&blocks);
        assert_eq!(stream, b"AAABBB");
    }

    #[test]
    fn extract_records_basic() {
        // Build a stream with one record:
        // block_size = 15 (9 header + 6 data)
        // data_size = 6
        // record_type = 0x02
        // data = "abcdef"
        let mut stream = Vec::new();
        stream.extend_from_slice(&15u32.to_le_bytes()); // block_size
        stream.extend_from_slice(&6u32.to_le_bytes()); // data_size
        stream.push(RECORD_TYPE_TOPIC); // record_type
        stream.extend_from_slice(b"abcdef");

        let records = extract_records(&stream).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, RECORD_TYPE_TOPIC);
        assert_eq!(records[0].data, b"abcdef");
        assert_eq!(records[0].stream_offset, 0);
    }

    #[test]
    fn extract_records_multiple() {
        let mut stream = Vec::new();

        // Record 1: topic header
        stream.extend_from_slice(&12u32.to_le_bytes());
        stream.extend_from_slice(&3u32.to_le_bytes());
        stream.push(RECORD_TYPE_TOPIC);
        stream.extend_from_slice(b"abc");

        // Record 2: text
        let r2_offset = stream.len();
        stream.extend_from_slice(&11u32.to_le_bytes());
        stream.extend_from_slice(&2u32.to_le_bytes());
        stream.push(RECORD_TYPE_TEXT);
        stream.extend_from_slice(b"xy");

        let records = extract_records(&stream).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, RECORD_TYPE_TOPIC);
        assert_eq!(records[1].record_type, RECORD_TYPE_TEXT);
        assert_eq!(records[1].stream_offset, r2_offset);
    }

    #[test]
    fn extract_records_stops_at_zero_size() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&12u32.to_le_bytes());
        stream.extend_from_slice(&3u32.to_le_bytes());
        stream.push(RECORD_TYPE_TOPIC);
        stream.extend_from_slice(b"abc");
        // Zero-size record → stop
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.push(0);

        let records = extract_records(&stream).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn parse_metadata_with_all_footnotes() {
        // Build: u32 next_topic + footnote markers
        let mut data = Vec::new();
        data.extend_from_slice(&100u32.to_le_bytes()); // next_topic
        data.push(0x23); // '#' context
        data.extend_from_slice(b"printf\0");
        data.push(0x24); // '$' title
        data.extend_from_slice(b"printf Function\0");
        data.push(0x4B); // 'K' keyword
        data.extend_from_slice(b"printf\0");
        data.push(0x4B); // 'K' keyword
        data.extend_from_slice(b"formatted output\0");
        data.push(0x2B); // '+' browse
        data.extend_from_slice(b"lib_p\0");

        let meta = parse_topic_metadata(&data);
        assert_eq!(meta.context_id.as_deref(), Some("printf"));
        assert_eq!(meta.title.as_deref(), Some("printf Function"));
        assert_eq!(meta.keywords.len(), 2);
        assert_eq!(meta.keywords[0], "printf");
        assert_eq!(meta.keywords[1], "formatted output");
        assert_eq!(meta.browse_seq.as_deref(), Some("lib_p"));
    }

    #[test]
    fn parse_metadata_empty_record() {
        let meta = parse_topic_metadata(&[]);
        assert!(meta.context_id.is_none());
        assert!(meta.title.is_none());
        assert!(meta.keywords.is_empty());
        assert!(meta.browse_seq.is_none());
    }

    #[test]
    fn parse_metadata_only_context() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.push(0x23);
        data.extend_from_slice(b"my_topic\0");

        let meta = parse_topic_metadata(&data);
        assert_eq!(meta.context_id.as_deref(), Some("my_topic"));
        assert!(meta.title.is_none());
    }
}
