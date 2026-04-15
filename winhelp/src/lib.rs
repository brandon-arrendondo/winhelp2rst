//! Pure-Rust parser for Windows WinHelp (.hlp) files.
//!
//! Parses the binary HLP format directly — no dependency on `helpdeco` or any
//! external tool. Produces a structured [`HelpFile`] document model suitable
//! for conversion to reStructuredText, HTML, or other formats.

use std::collections::HashMap;
use std::path::Path;

mod bitmap;
mod container;
mod context;
mod decompress;
mod error;
mod font;
mod keyword;
mod opcode;
mod system;
mod topic;

pub use bitmap::{ensure_bmp_header, extract_bitmap};
pub use container::{HlpContainer, InternalFile};
pub use context::{context_hash, ContextMap};
pub use decompress::{lz77_decompress, PhraseTable};
pub use error::Error;
pub use font::{FontDescriptor, FontTable, TitleIndex};
pub use keyword::{build_keyword_index, KeywordIndex, RawKeywordEntry};
pub use opcode::parse_text_record;
pub use system::SystemInfo;
pub use topic::{
    extract_records, flatten_topic_stream, parse_topic_metadata, read_topic_blocks, RawTopicRecord,
    TopicBlock, TopicMetadata, RECORD_TYPE_TABLE, RECORD_TYPE_TEXT, RECORD_TYPE_TOPIC,
    TOPIC_BLOCK_SIZE_31,
};

pub type Result<T> = std::result::Result<T, Error>;

/// Top-level document model for a parsed WinHelp file.
#[derive(Debug, Clone)]
pub struct HelpFile {
    /// Help file title (from |SYSTEM record type 1).
    pub title: String,
    /// Copyright string (from |SYSTEM record type 2).
    pub copyright: Option<String>,
    /// Context string of the root/default topic.
    pub root_topic: String,
    /// All topics in browse-sequence order.
    pub topics: Vec<Topic>,
    /// Keyword index entries.
    pub keyword_index: Vec<KeywordEntry>,
    /// Raw image data by filename (BMP with valid BITMAPFILEHEADER).
    pub images: HashMap<String, Vec<u8>>,
}

/// A single help topic.
#[derive(Debug, Clone)]
pub struct Topic {
    /// Context string — the stable topic identifier (from `#` footnote).
    pub id: String,
    /// Display title (from `$` footnote).
    pub title: String,
    /// Keyword index entries for this topic (from `K` footnotes).
    pub keywords: Vec<String>,
    /// Browse sequence identifier (from `+` footnote).
    pub browse_seq: Option<String>,
    /// Content blocks.
    pub body: Vec<Block>,
}

/// A block-level content element.
#[derive(Debug, Clone)]
pub enum Block {
    /// A paragraph of inline content.
    Paragraph(Vec<Inline>),
    /// A table (rows of cells, each cell containing blocks).
    Table(Vec<Vec<Block>>),
    /// An image reference.
    Image(ImageRef),
}

/// An inline content element within a paragraph.
#[derive(Debug, Clone)]
pub enum Inline {
    /// Plain text.
    Text(String),
    /// Bold text.
    Bold(Vec<Inline>),
    /// Italic text.
    Italic(Vec<Inline>),
    /// A hyperlink.
    Link {
        text: Vec<Inline>,
        target: String,
        kind: LinkKind,
    },
}

/// Hyperlink type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Jump link — navigates to the target topic.
    Jump,
    /// Popup link — shows target topic in a popup window.
    Popup,
}

/// A reference to an embedded image.
#[derive(Debug, Clone)]
pub struct ImageRef {
    /// Filename of the image within the HLP file.
    pub filename: String,
    /// Placement directive.
    pub placement: ImagePlacement,
}

/// Image placement within the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePlacement {
    /// `{bmc}` — inline (character position).
    Inline,
    /// `{bml}` — left-aligned.
    Left,
    /// `{bmr}` — right-aligned.
    Right,
}

/// A keyword index entry mapping a keyword to one or more topics.
#[derive(Debug, Clone)]
pub struct KeywordEntry {
    /// The keyword string.
    pub keyword: String,
    /// Context strings of topics associated with this keyword.
    pub topic_ids: Vec<String>,
}

impl HelpFile {
    /// Parse a WinHelp `.hlp` file from the given path.
    ///
    /// This is the main entry point. It reads the file, parses all internal
    /// structures, and returns a fully resolved document model.
    pub fn from_path(path: &Path) -> Result<Self> {
        let container = HlpContainer::open(path)?;
        Self::from_container(&container)
    }

    /// Parse from an already-opened container.
    pub fn from_container(container: &HlpContainer) -> Result<Self> {
        // 1. Parse |SYSTEM for metadata.
        let system_data = container.read_file("|SYSTEM")?;
        let system = SystemInfo::from_bytes(&system_data)?;

        let title = system.title.clone().unwrap_or_else(|| "(untitled)".into());
        let copyright = system.copyright.clone();

        // 2. Load phrase table (if present).
        let phrases_compressed = system.phrases_compressed();
        let phrases = match container.read_file("|Phrases") {
            Ok(phrases_data) => {
                let phr_index = container.read_file("|PhrIndex").ok();
                PhraseTable::from_bytes(&phrases_data, phr_index.as_deref(), phrases_compressed)?
            }
            Err(Error::FileNotFound(_)) => PhraseTable::empty(),
            Err(e) => return Err(e),
        };

        // 3. Load font table (if present).
        let fonts = match container.read_file("|FONT") {
            Ok(font_data) => FontTable::from_bytes(&font_data)?,
            Err(Error::FileNotFound(_)) => FontTable::empty(),
            Err(e) => return Err(e),
        };

        // 4. Read and decompress |TOPIC blocks (LZ77 only, no phrase expansion yet).
        let topic_data = container.read_file("|TOPIC")?;
        let block_size = system.topic_block_size();
        let blocks = read_topic_blocks(
            &topic_data,
            block_size,
            system.uses_lz77(),
            &phrases,
        )?;
        let stream = flatten_topic_stream(&blocks, system.decompress_size());
        let before_31 = system.minor_version <= 16;
        let mut records = extract_records(&stream, before_31)?;

        // 4b. Capture raw (pre-phrase-expansion) LinkData2 lengths for
        // TOPICOFFSET calculation, then phrase-expand.
        //
        // TOPICOFFSET character counts use the on-disk (phrase-compressed)
        // LinkData2 lengths, because the help compiler computes TOPICOFFSETs
        // from the compressed byte stream it writes.
        let raw_ld2_lens: Vec<usize> = records.iter().map(|r| r.link_data2.len()).collect();

        if !phrases.is_empty() {
            for record in records.iter_mut() {
                if record.data_len2 > record.link_data2.len() {
                    record.link_data2 = phrases.expand(&record.link_data2)?;
                }
            }
        }

        // 5. Load context map: hash → TOPICOFFSET.
        let context_map = match container.read_file("|CONTEXT") {
            Ok(ctx_data) => ContextMap::from_bytes(&ctx_data)?,
            Err(Error::FileNotFound(_)) => ContextMap::empty(),
            Err(e) => return Err(e),
        };

        // 6. Load title index (if present).
        let _title_index = match container.read_file("|TTLBTREE") {
            Ok(ttl_data) => TitleIndex::from_bytes(&ttl_data)?,
            Err(Error::FileNotFound(_)) => TitleIndex::empty(),
            Err(e) => return Err(e),
        };

        // 7. First pass: collect topic metadata and assign context IDs via TOPICOFFSET matching.
        //
        // |CONTEXT stores (HashValue, TopicOffset) where TopicOffset is a logical
        // character count, not a byte offset:
        //   TopicOffset = block_num × 32768 + char_count_within_block
        // The char count increments by the raw (pre-phrase-expansion) LinkData2
        // length for each TEXT/TABLE record. On a decompressed-block boundary,
        // the counter resets to next_block_num × 32768.
        //
        // Context IDs use "ctx_{hash:08x}" as the stable identifier since the
        // hash function is not reversible.

        // Build hash → "ctx_{hash:08x}" name map for ALL context entries (used
        // both for topic ID assignment here and for link resolution in pass 8).
        let mut hash_targets: HashMap<u32, String> = HashMap::new();
        for (hash, _) in context_map.entries() {
            hash_targets.insert(hash, format!("ctx_{hash:08x}"));
        }

        // Sort context entries by TOPICOFFSET for sequential matching.
        let mut context_sorted: Vec<(u32, u32)> = context_map
            .entries()
            .map(|(hash, topicoff)| (topicoff, hash))
            .collect();
        context_sorted.sort_by_key(|&(t, _)| t);

        let mut running_topicoffset: u32 = 0;
        let mut ctx_idx: usize = 0;
        let mut all_meta: Vec<(usize, TopicMetadata)> = Vec::new();

        for (i, record) in records.iter().enumerate() {
            match record.record_type {
                RECORD_TYPE_TOPIC => {
                    let mut meta =
                        parse_topic_metadata(&record.link_data1, &record.link_data2);
                    // Consume any context entries whose TOPICOFFSET ≤ current running value.
                    // (Handles entries for topics at the very start of a block.)
                    while ctx_idx < context_sorted.len()
                        && context_sorted[ctx_idx].0 <= running_topicoffset
                    {
                        let (_, hash) = context_sorted[ctx_idx];
                        if meta.context_id.is_none() {
                            meta.context_id = Some(hash_targets[&hash].clone());
                        }
                        ctx_idx += 1;
                    }
                    all_meta.push((i, meta));
                }
                RECORD_TYPE_TEXT | RECORD_TYPE_TABLE => {
                    // Increment running TOPICOFFSET by the raw (on-disk, pre-phrase-
                    // expansion) LinkData2 length. This matches how the help compiler
                    // computes TOPICOFFSETs from the compressed byte stream it writes.
                    let char_inc = raw_ld2_lens[i] as u32;
                    running_topicoffset = running_topicoffset.saturating_add(char_inc);
                    // After accumulating, assign any newly-in-range context entries
                    // to the most recent TOPICHDR topic.
                    if let Some(last) = all_meta.last_mut() {
                        while ctx_idx < context_sorted.len()
                            && context_sorted[ctx_idx].0 <= running_topicoffset
                        {
                            let (_, hash) = context_sorted[ctx_idx];
                            if last.1.context_id.is_none() {
                                last.1.context_id = Some(hash_targets[&hash].clone());
                            }
                            ctx_idx += 1;
                        }
                    }
                }
                _ => {}
            }
            // Block-boundary transition (WinHelp 3.1+ absolute TOPICPOS):
            // when the next record crosses into a different 16 384-byte virtual block,
            // reset running_topicoffset to next_block_num × 32 768.
            // Guard: next_block must be a valid positive TOPICPOS (≥ 12).  Values
            // with bit 31 set are end-of-chain sentinels (negative as i32) and must
            // not be treated as addresses.
            if !before_31 && (record.next_block as i32) > 0 && record.next_block >= 12 {
                let curr_block = record.stream_offset / 16384;
                let next_stream_off = record.next_block as usize - 12;
                let next_block_num = next_stream_off / 16384;
                if next_block_num != curr_block {
                    running_topicoffset = (next_block_num as u32) * 32768;
                }
            }
        }

        // 8. Second pass: group records into topics with resolved links.
        let mut topics = Vec::new();
        let mut meta_iter = all_meta.into_iter().peekable();
        while let Some((start_idx, meta)) = meta_iter.next() {
            let end_idx = meta_iter.peek().map(|(i, _)| *i).unwrap_or(records.len());

            let mut body: Vec<Block> = Vec::new();
            for record in &records[start_idx + 1..end_idx] {
                if record.record_type == RECORD_TYPE_TEXT || record.record_type == RECORD_TYPE_TABLE
                {
                    // LinkData1 is paragraph formatting (tab stops, margins, etc.)
                    // — not parseable as text opcodes.
                    // LinkData2 is the text content with embedded opcodes
                    // (already phrase-expanded above).
                    if let Ok(parsed_blocks) =
                        parse_text_record(&record.link_data2, &fonts, &hash_targets)
                    {
                        body.extend(parsed_blocks);
                    }
                }
            }

            topics.push(build_topic(meta, body));
        }

        // 9. Build keyword index from topic metadata.
        let keyword_index = build_keyword_index(&topics);

        // 10. Extract referenced images from the container.
        let mut images = HashMap::new();
        for topic in &topics {
            for block in &topic.body {
                if let Block::Image(img) = block {
                    if !images.contains_key(&img.filename) {
                        if let Ok(Some(data)) = extract_bitmap(container, &img.filename) {
                            images.insert(img.filename.clone(), data);
                        }
                    }
                }
            }
        }

        // 11. Determine root topic.
        let root_topic = system
            .starting_topic
            .or_else(|| topics.first().map(|t| t.id.clone()))
            .unwrap_or_default();

        Ok(HelpFile {
            title,
            copyright,
            root_topic,
            topics,
            keyword_index,
            images,
        })
    }
}

/// Build a Topic from parsed metadata and body blocks.
fn build_topic(meta: TopicMetadata, body: Vec<Block>) -> Topic {
    Topic {
        id: meta.context_id.unwrap_or_default(),
        title: meta.title.unwrap_or_else(|| "(untitled)".into()),
        keywords: meta.keywords,
        browse_seq: meta.browse_seq,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_model_construction() {
        let topic = Topic {
            id: "printf".to_string(),
            title: "printf".to_string(),
            keywords: vec!["printf".to_string(), "formatted output".to_string()],
            browse_seq: Some("lib_p".to_string()),
            body: vec![
                Block::Paragraph(vec![
                    Inline::Text("The ".to_string()),
                    Inline::Bold(vec![Inline::Text("printf".to_string())]),
                    Inline::Text(" function writes formatted output.".to_string()),
                ]),
                Block::Paragraph(vec![Inline::Link {
                    text: vec![Inline::Text("See also: fprintf".to_string())],
                    target: "fprintf".to_string(),
                    kind: LinkKind::Jump,
                }]),
            ],
        };

        let helpfile = HelpFile {
            title: "C Library Reference".to_string(),
            copyright: Some("Open Watcom".to_string()),
            root_topic: "contents".to_string(),
            topics: vec![topic],
            keyword_index: vec![KeywordEntry {
                keyword: "printf".to_string(),
                topic_ids: vec!["printf".to_string()],
            }],
            images: HashMap::new(),
        };

        assert_eq!(helpfile.title, "C Library Reference");
        assert_eq!(helpfile.topics.len(), 1);
        assert_eq!(helpfile.topics[0].id, "printf");
        assert_eq!(helpfile.topics[0].keywords.len(), 2);
        assert_eq!(helpfile.keyword_index.len(), 1);
    }

    #[test]
    fn link_kind_equality() {
        assert_eq!(LinkKind::Jump, LinkKind::Jump);
        assert_ne!(LinkKind::Jump, LinkKind::Popup);
    }

    #[test]
    fn image_placement_variants() {
        let img = ImageRef {
            filename: "setup.bmp".to_string(),
            placement: ImagePlacement::Left,
        };
        assert_eq!(img.placement, ImagePlacement::Left);
        assert_ne!(img.placement, ImagePlacement::Inline);
    }
}
