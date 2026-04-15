//! Pure-Rust parser for Windows WinHelp (.hlp) files.
//!
//! Parses the binary HLP format directly — no dependency on `helpdeco` or any
//! external tool. Produces a structured [`HelpFile`] document model suitable
//! for conversion to reStructuredText, HTML, or other formats.

use std::path::Path;

mod container;
mod context;
mod decompress;
mod error;
mod font;
mod opcode;
mod system;
mod topic;

pub use container::{HlpContainer, InternalFile};
pub use context::{context_hash, ContextMap};
pub use decompress::{lz77_decompress, PhraseTable};
pub use error::Error;
pub use font::{FontDescriptor, FontTable, TitleIndex};
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
        let phrases = match container.read_file("|Phrases") {
            Ok(phrases_data) => {
                let phr_index = container.read_file("|PhrIndex").ok();
                PhraseTable::from_bytes(
                    &phrases_data,
                    phr_index.as_deref(),
                )?
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

        // 4. Read and decompress |TOPIC blocks.
        let topic_data = container.read_file("|TOPIC")?;
        let blocks = read_topic_blocks(
            &topic_data,
            TOPIC_BLOCK_SIZE_31,
            system.uses_lz77(),
            &phrases,
        )?;
        let stream = flatten_topic_stream(&blocks);
        let records = extract_records(&stream)?;

        // 5. Load context map (if present).
        let _context_map = match container.read_file("|CONTEXT") {
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

        // 7. Group records into topics.
        //    A topic header record (0x02) starts a new topic. Subsequent
        //    text records (0x20/0x23) belong to the current topic.
        let mut topics = Vec::new();
        let mut current_meta: Option<TopicMetadata> = None;
        let mut current_body: Vec<Block> = Vec::new();

        for record in &records {
            match record.record_type {
                RECORD_TYPE_TOPIC => {
                    // Flush previous topic.
                    if let Some(meta) = current_meta.take() {
                        topics.push(build_topic(meta, std::mem::take(&mut current_body)));
                    }
                    current_meta = Some(parse_topic_metadata(&record.data));
                    current_body.clear();
                }
                RECORD_TYPE_TEXT | RECORD_TYPE_TABLE => {
                    if current_meta.is_some() {
                        if let Ok(parsed_blocks) = parse_text_record(&record.data, &fonts) {
                            current_body.extend(parsed_blocks);
                        }
                    }
                }
                _ => {}
            }
        }

        // Flush last topic.
        if let Some(meta) = current_meta.take() {
            topics.push(build_topic(meta, current_body));
        }

        // 8. Determine root topic.
        let root_topic = system
            .starting_topic
            .or_else(|| topics.first().map(|t| t.id.clone()))
            .unwrap_or_default();

        Ok(HelpFile {
            title,
            copyright,
            root_topic,
            topics,
            keyword_index: Vec::new(), // TODO: parse |KWBTREE in a later task
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
