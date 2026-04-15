//! `|KWBTREE` / `|KWDATA` keyword index reader.
//!
//! The keyword index maps keyword strings to the topics they appear in.
//!
//! **|KWBTREE** is a B-tree with null-terminated keyword string keys and u32
//! values pointing into **|KWDATA**. At each |KWDATA offset: a u16 count
//! followed by `count` u32 topic offsets.
//!
//! Since topic metadata already carries `K` footnotes (keywords), we also
//! provide [`build_keyword_index`] which constructs the same mapping from
//! parsed [`Topic`] data — no |KWBTREE parsing needed.

use std::collections::BTreeMap;

use crate::{Error, KeywordEntry, Result, Topic};

/// Raw keyword entry from |KWBTREE / |KWDATA (unresolved topic offsets).
#[derive(Debug, Clone)]
pub struct RawKeywordEntry {
    /// The keyword string.
    pub keyword: String,
    /// Raw topic offsets from |KWDATA.
    pub topic_offsets: Vec<u32>,
}

/// Parsed keyword index from |KWBTREE + |KWDATA.
#[derive(Debug, Clone)]
pub struct KeywordIndex {
    entries: Vec<RawKeywordEntry>,
}

/// B-tree header size: 38 bytes.
const BTREE_HEADER_SIZE: usize = 0x26; // 38 bytes

impl KeywordIndex {
    /// Parse from the raw bytes of `|KWBTREE` and `|KWDATA`.
    ///
    /// |KWBTREE is a B-tree mapping keyword strings to u32 offsets into |KWDATA.
    /// |KWDATA at each offset contains: u16 count + u32\[count\] topic offsets.
    pub fn from_bytes(kwbtree: &[u8], kwdata: &[u8]) -> Result<Self> {
        if kwbtree.len() < BTREE_HEADER_SIZE {
            return Err(Error::BadInternalFile {
                name: "|KWBTREE".into(),
                detail: "too small for B-tree header".into(),
            });
        }

        let magic = u16::from_le_bytes([kwbtree[0], kwbtree[1]]);
        if magic != 0x293B {
            return Err(Error::BadInternalFile {
                name: "|KWBTREE".into(),
                detail: format!("bad B-tree magic: 0x{magic:04X}"),
            });
        }

        let flags = u16::from_le_bytes([kwbtree[2], kwbtree[3]]);
        let page_size = u16::from_le_bytes([kwbtree[4], kwbtree[5]]) as usize;
        // +0x06: char[16] structure — skip
        // +0x16: u16 must_be_zero — skip
        // +0x18: u16 page_splits — skip
        let root_page = u16::from_le_bytes([kwbtree[0x1A], kwbtree[0x1B]]) as usize;
        // +0x1C: i16 must_be_neg_one — skip
        // +0x1E: u16 total_pages — skip
        let num_levels = u16::from_le_bytes([kwbtree[0x20], kwbtree[0x21]]) as usize;

        let has_counters = flags & 0x0400 != 0;
        let pages_start = BTREE_HEADER_SIZE;

        let ctx = KwBTreeCtx {
            btree: kwbtree,
            kwdata,
            pages_start,
            page_size,
            has_counters,
        };

        let mut entries = Vec::new();

        if num_levels > 0 {
            collect_kw_entries(&ctx, root_page, num_levels, &mut entries)?;
        }

        Ok(Self { entries })
    }

    /// Build an empty keyword index.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Get the raw keyword entries.
    pub fn entries(&self) -> &[RawKeywordEntry] {
        &self.entries
    }

    /// Number of keywords in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build keyword index entries from already-parsed topics.
///
/// Inverts the per-topic `keywords` lists: for each keyword string across all
/// topics, collects the context IDs of topics that use it. Returned in
/// alphabetical keyword order.
pub fn build_keyword_index(topics: &[Topic]) -> Vec<KeywordEntry> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for topic in topics {
        for kw in &topic.keywords {
            map.entry(kw.clone()).or_default().push(topic.id.clone());
        }
    }

    map.into_iter()
        .map(|(keyword, topic_ids)| KeywordEntry { keyword, topic_ids })
        .collect()
}

// ---------------------------------------------------------------------------
// B-tree traversal for |KWBTREE
// ---------------------------------------------------------------------------

/// Shared context for B-tree traversal.
struct KwBTreeCtx<'a> {
    btree: &'a [u8],
    kwdata: &'a [u8],
    pages_start: usize,
    page_size: usize,
    has_counters: bool,
}

fn collect_kw_entries(
    ctx: &KwBTreeCtx<'_>,
    page_index: usize,
    levels_remaining: usize,
    entries: &mut Vec<RawKeywordEntry>,
) -> Result<()> {
    let page_offset = ctx.pages_start + page_index * ctx.page_size;

    if levels_remaining == 1 {
        parse_kw_leaf(ctx.btree, ctx.kwdata, page_offset, ctx.page_size, entries)?;
    } else {
        let children =
            parse_kw_index_page(ctx.btree, page_offset, ctx.page_size, ctx.has_counters)?;
        for child in children {
            collect_kw_entries(ctx, child, levels_remaining - 1, entries)?;
        }
    }

    Ok(())
}

/// Parse a leaf page: keyword string + u32 kwdata_offset per entry.
fn parse_kw_leaf(
    btree: &[u8],
    kwdata: &[u8],
    page_offset: usize,
    page_size: usize,
    entries: &mut Vec<RawKeywordEntry>,
) -> Result<()> {
    let page_end = page_offset + page_size;
    if btree.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "KWBTREE leaf page past EOF".into(),
        });
    }

    let _prev_page = u16::from_le_bytes([btree[page_offset], btree[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([btree[page_offset + 2], btree[page_offset + 3]]) as usize;

    let mut pos = page_offset + 4;

    for _ in 0..num_entries {
        if pos >= page_end {
            break;
        }

        // Read null-terminated keyword string.
        let str_start = pos;
        while pos < page_end && btree[pos] != 0 {
            pos += 1;
        }
        let keyword = String::from_utf8_lossy(&btree[str_start..pos]).into_owned();
        if pos < page_end {
            pos += 1; // skip null terminator
        }

        // Read u32 offset into |KWDATA.
        if pos + 4 > page_end {
            break;
        }
        let kwdata_offset =
            u32::from_le_bytes([btree[pos], btree[pos + 1], btree[pos + 2], btree[pos + 3]])
                as usize;
        pos += 4;

        // Read topic offsets from |KWDATA.
        let topic_offsets = read_kwdata_entry(kwdata, kwdata_offset)?;

        entries.push(RawKeywordEntry {
            keyword,
            topic_offsets,
        });
    }

    Ok(())
}

/// Read a single entry from |KWDATA: u16 count + u32[count] topic offsets.
fn read_kwdata_entry(kwdata: &[u8], offset: usize) -> Result<Vec<u32>> {
    if offset + 2 > kwdata.len() {
        return Err(Error::Parse {
            offset: offset as u64,
            detail: "KWDATA entry past EOF".into(),
        });
    }

    let count = u16::from_le_bytes([kwdata[offset], kwdata[offset + 1]]) as usize;
    let data_start = offset + 2;
    let data_end = data_start + count * 4;

    if data_end > kwdata.len() {
        return Err(Error::Parse {
            offset: offset as u64,
            detail: format!(
                "KWDATA entry needs {} bytes but only {} available",
                count * 4,
                kwdata.len() - data_start
            ),
        });
    }

    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let p = data_start + i * 4;
        let val = u32::from_le_bytes([kwdata[p], kwdata[p + 1], kwdata[p + 2], kwdata[p + 3]]);
        offsets.push(val);
    }

    Ok(offsets)
}

/// Parse an index (non-leaf) page for |KWBTREE.
/// Keys are null-terminated strings; child pointers are u16 page indices.
fn parse_kw_index_page(
    btree: &[u8],
    page_offset: usize,
    page_size: usize,
    has_counters: bool,
) -> Result<Vec<usize>> {
    let page_end = page_offset + page_size;
    if btree.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "KWBTREE index page past EOF".into(),
        });
    }

    let _unused = u16::from_le_bytes([btree[page_offset], btree[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([btree[page_offset + 2], btree[page_offset + 3]]) as usize;

    let mut pos = page_offset + 4;

    // First child page.
    if pos + 2 > page_end {
        return Ok(Vec::new());
    }
    let first_child = u16::from_le_bytes([btree[pos], btree[pos + 1]]) as usize;
    pos += 2;

    let mut children = Vec::with_capacity(num_entries + 1);
    children.push(first_child);

    for _ in 0..num_entries {
        // Skip null-terminated key string.
        while pos < page_end && btree[pos] != 0 {
            pos += 1;
        }
        if pos < page_end {
            pos += 1; // skip null
        }

        if has_counters && pos + 2 <= page_end {
            pos += 2; // skip counter
        }

        if pos + 2 > page_end {
            break;
        }
        let child = u16::from_le_bytes([btree[pos], btree[pos + 1]]) as usize;
        pos += 2;
        children.push(child);
    }

    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Inline};

    /// Build a minimal |KWBTREE single-leaf B-tree.
    ///
    /// Each entry is (keyword_string, kwdata_offset_u32).
    fn build_kwbtree(entries: &[(&str, u32)], page_size: usize) -> Vec<u8> {
        // Leaf page: u16 prev + u16 num_entries + entries
        let mut page = Vec::new();
        page.extend_from_slice(&0u16.to_le_bytes()); // prev
        page.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (kw, offset) in entries {
            page.extend_from_slice(kw.as_bytes());
            page.push(0); // null terminator
            page.extend_from_slice(&offset.to_le_bytes());
        }
        page.resize(page_size, 0); // pad to page_size

        // B-tree header (38 bytes)
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x293Bu16.to_le_bytes()); // +0x00: magic
        buf.extend_from_slice(&0u16.to_le_bytes()); // +0x02: flags
        buf.extend_from_slice(&(page_size as u16).to_le_bytes()); // +0x04: page_size
        buf.extend_from_slice(&[0u8; 16]); // +0x06: structure (char[16])
        buf.extend_from_slice(&0u16.to_le_bytes()); // +0x16: must_be_zero
        buf.extend_from_slice(&0u16.to_le_bytes()); // +0x18: page_splits
        buf.extend_from_slice(&0u16.to_le_bytes()); // +0x1A: root_page
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // +0x1C: must_be_neg_one
        buf.extend_from_slice(&1u16.to_le_bytes()); // +0x1E: total_pages
        buf.extend_from_slice(&1u16.to_le_bytes()); // +0x20: num_levels
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes()); // +0x22: total_entries
        buf.extend_from_slice(&page);
        buf
    }

    /// Build |KWDATA from a list of (offset, topic_offsets) pairs.
    fn build_kwdata(entries: &[(usize, &[u32])]) -> Vec<u8> {
        // Calculate required size.
        let max_end = entries
            .iter()
            .map(|(off, tops)| off + 2 + tops.len() * 4)
            .max()
            .unwrap_or(0);
        let mut data = vec![0u8; max_end];

        for &(offset, ref tops) in entries {
            let count = tops.len() as u16;
            data[offset] = count as u8;
            data[offset + 1] = (count >> 8) as u8;
            for (i, &t) in tops.iter().enumerate() {
                let p = offset + 2 + i * 4;
                let bytes = t.to_le_bytes();
                data[p..p + 4].copy_from_slice(&bytes);
            }
        }

        data
    }

    #[test]
    fn parse_single_keyword() {
        let kwbtree = build_kwbtree(&[("printf", 0)], 64);
        let kwdata = build_kwdata(&[(0, &[0x1000])]);

        let idx = KeywordIndex::from_bytes(&kwbtree, &kwdata).unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.entries()[0].keyword, "printf");
        assert_eq!(idx.entries()[0].topic_offsets, vec![0x1000]);
    }

    #[test]
    fn parse_multiple_keywords() {
        let kwbtree = build_kwbtree(&[("fopen", 0), ("malloc", 6), ("printf", 12)], 128);
        let kwdata = build_kwdata(&[(0, &[0x1000]), (6, &[0x2000]), (12, &[0x3000, 0x3100])]);

        let idx = KeywordIndex::from_bytes(&kwbtree, &kwdata).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.entries()[0].keyword, "fopen");
        assert_eq!(idx.entries()[1].keyword, "malloc");
        assert_eq!(idx.entries()[2].keyword, "printf");
        assert_eq!(idx.entries()[2].topic_offsets, vec![0x3000, 0x3100]);
    }

    #[test]
    fn parse_empty_kwbtree() {
        let kwbtree = build_kwbtree(&[], 32);
        let kwdata: Vec<u8> = Vec::new();

        let idx = KeywordIndex::from_bytes(&kwbtree, &kwdata).unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut kwbtree = build_kwbtree(&[], 32);
        kwbtree[0] = 0xFF;
        let kwdata: Vec<u8> = Vec::new();

        let err = KeywordIndex::from_bytes(&kwbtree, &kwdata).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn kwdata_past_eof_rejected() {
        let kwbtree = build_kwbtree(&[("test", 100)], 64);
        let kwdata = vec![0u8; 10]; // too small for offset 100

        let err = KeywordIndex::from_bytes(&kwbtree, &kwdata).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn build_index_from_topics() {
        let topics = vec![
            Topic {
                id: "printf".into(),
                title: "printf".into(),
                keywords: vec!["printf".into(), "formatted output".into()],
                browse_seq: None,
                body: vec![],
            },
            Topic {
                id: "fprintf".into(),
                title: "fprintf".into(),
                keywords: vec!["fprintf".into(), "formatted output".into()],
                browse_seq: None,
                body: vec![],
            },
        ];

        let index = build_keyword_index(&topics);

        // BTreeMap order: "formatted output", "fprintf", "printf"
        assert_eq!(index.len(), 3);
        assert_eq!(index[0].keyword, "formatted output");
        assert_eq!(index[0].topic_ids, vec!["printf", "fprintf"]);
        assert_eq!(index[1].keyword, "fprintf");
        assert_eq!(index[1].topic_ids, vec!["fprintf"]);
        assert_eq!(index[2].keyword, "printf");
        assert_eq!(index[2].topic_ids, vec!["printf"]);
    }

    #[test]
    fn build_index_no_keywords() {
        let topics = vec![Topic {
            id: "intro".into(),
            title: "Introduction".into(),
            keywords: vec![],
            browse_seq: None,
            body: vec![Block::Paragraph(vec![Inline::Text("Hello".into())])],
        }];

        let index = build_keyword_index(&topics);
        assert!(index.is_empty());
    }

    #[test]
    fn keyword_index_empty() {
        let idx = KeywordIndex::empty();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.entries().is_empty());
    }
}
