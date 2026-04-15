//! `|CONTEXT` B-tree reader and context hash function.
//!
//! The `|CONTEXT` internal file maps context string hashes (u32) to topic
//! byte offsets (u32). The hash function is the standard WinHelp
//! case-insensitive hash.

use std::collections::HashMap;

use crate::{Error, Result};

/// Mapping from context string hash to topic byte offset.
#[derive(Debug, Clone)]
pub struct ContextMap {
    /// hash → topic offset
    entries: HashMap<u32, u32>,
}

/// B-tree header size: 38 bytes (same structure as the directory B-tree).
/// See container.rs BTREE_HEADER_SIZE for field layout.
const BTREE_HEADER_SIZE: usize = 0x26; // 38 bytes

impl ContextMap {
    /// Parse from the raw bytes of the `|CONTEXT` internal file.
    ///
    /// The file is a B-tree with u32 keys (hashes) and u32 values (offsets).
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < BTREE_HEADER_SIZE {
            return Err(Error::BadInternalFile {
                name: "|CONTEXT".into(),
                detail: "too small for B-tree header".into(),
            });
        }

        let magic = u16::from_le_bytes([data[0], data[1]]);
        if magic != 0x293B {
            return Err(Error::BadInternalFile {
                name: "|CONTEXT".into(),
                detail: format!("bad B-tree magic: 0x{magic:04X}"),
            });
        }

        let flags = u16::from_le_bytes([data[2], data[3]]);
        let page_size = u16::from_le_bytes([data[4], data[5]]) as usize;
        // +0x06: char[16] structure — skip
        // +0x16: u16 must_be_zero — skip
        // +0x18: u16 page_splits — skip
        let root_page = u16::from_le_bytes([data[0x1A], data[0x1B]]) as usize;
        // +0x1C: i16 must_be_neg_one — skip
        let _num_pages = u16::from_le_bytes([data[0x1E], data[0x1F]]) as usize;
        let num_levels = u16::from_le_bytes([data[0x20], data[0x21]]) as usize;
        let _total_entries =
            u32::from_le_bytes([data[0x22], data[0x23], data[0x24], data[0x25]]);

        let has_counters = flags & 0x0400 != 0;
        let pages_start = BTREE_HEADER_SIZE;

        let mut entries = HashMap::new();

        if num_levels > 0 {
            collect_context_entries(
                data,
                pages_start,
                page_size,
                root_page,
                num_levels,
                has_counters,
                &mut entries,
            )?;
        }

        Ok(Self { entries })
    }

    /// Build an empty context map.
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Look up a topic offset by context string hash.
    pub fn resolve_hash(&self, hash: u32) -> Option<u32> {
        self.entries.get(&hash).copied()
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries as (hash, offset) pairs.
    pub fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.entries.iter().map(|(&h, &o)| (h, o))
    }
}

/// WinHelp context string hash function.
///
/// This is the standard case-insensitive hash used by WinHelp to map context
/// strings to numeric hash values stored in `|CONTEXT`.
pub fn context_hash(s: &str) -> u32 {
    let mut hash: u32 = 0;
    for &b in s.as_bytes() {
        let ch = if b.is_ascii_uppercase() {
            b.to_ascii_lowercase()
        } else {
            b
        };
        hash = hash.wrapping_mul(43).wrapping_add(ch as u32);
    }
    hash
}

// ---------------------------------------------------------------------------
// B-tree traversal (adapted for u32→u32 key-value pairs)
// ---------------------------------------------------------------------------

fn collect_context_entries(
    data: &[u8],
    pages_start: usize,
    page_size: usize,
    page_index: usize,
    levels_remaining: usize,
    has_counters: bool,
    entries: &mut HashMap<u32, u32>,
) -> Result<()> {
    let page_offset = pages_start + page_index * page_size;

    if levels_remaining == 1 {
        parse_context_leaf(data, page_offset, page_size, entries)?;
    } else {
        let children = parse_context_index(data, page_offset, page_size, has_counters)?;
        for child in children {
            collect_context_entries(
                data,
                pages_start,
                page_size,
                child,
                levels_remaining - 1,
                has_counters,
                entries,
            )?;
        }
    }

    Ok(())
}

/// Parse a leaf page with u32 hash → u32 offset entries.
fn parse_context_leaf(
    data: &[u8],
    page_offset: usize,
    page_size: usize,
    entries: &mut HashMap<u32, u32>,
) -> Result<()> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "context leaf page past EOF".into(),
        });
    }

    // BTREENODEHEADER: Unknown(2) + NEntries(2) + PreviousPage(2) + NextPage(2) = 8 bytes
    let _unknown = u16::from_le_bytes([data[page_offset], data[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([data[page_offset + 2], data[page_offset + 3]]) as usize;
    // skip PreviousPage (bytes 4-5) and NextPage (bytes 6-7)

    let mut pos = page_offset + 8;
    for _ in 0..num_entries {
        if pos + 8 > page_end {
            break;
        }
        let hash = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let offset =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        pos += 8;
        entries.insert(hash, offset);
    }

    Ok(())
}

/// Parse an index page for the |CONTEXT B-tree.
/// Keys are u32 hashes; child pointers are u16 page indices.
///
/// Index pages have a 4-byte header (Unknown + NEntries only).
/// PreviousPage and NextPage are only present on leaf pages.
fn parse_context_index(
    data: &[u8],
    page_offset: usize,
    page_size: usize,
    has_counters: bool,
) -> Result<Vec<usize>> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "context index page past EOF".into(),
        });
    }

    let _unknown = u16::from_le_bytes([data[page_offset], data[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([data[page_offset + 2], data[page_offset + 3]]) as usize;

    let mut pos = page_offset + 4;

    // First child.
    let first_child = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;

    let mut children = Vec::with_capacity(num_entries + 1);
    children.push(first_child);

    for _ in 0..num_entries {
        if pos + 4 > page_end {
            break;
        }
        // Skip u32 key.
        pos += 4;
        if has_counters {
            pos += 2;
        }
        if pos + 2 > page_end {
            break;
        }
        let child = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        children.push(child);
    }

    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal |CONTEXT B-tree with a single leaf page.
    fn build_context_btree(entries: &[(u32, u32)]) -> Vec<u8> {
        // Leaf page: BTREENODEHEADER(8) + entries (8 bytes each)
        let mut page = Vec::new();
        page.extend_from_slice(&0u16.to_le_bytes()); // unknown
        page.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // num_entries
        page.extend_from_slice(&0u16.to_le_bytes()); // previous_page
        page.extend_from_slice(&0u16.to_le_bytes()); // next_page
        for (hash, offset) in entries {
            page.extend_from_slice(&hash.to_le_bytes());
            page.extend_from_slice(&offset.to_le_bytes());
        }
        let page_size = page.len().max(32);
        page.resize(page_size, 0);

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

    #[test]
    fn context_hash_basic() {
        // Verify the hash is deterministic and case-insensitive.
        let h1 = context_hash("printf");
        let h2 = context_hash("Printf");
        let h3 = context_hash("PRINTF");
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    #[test]
    fn context_hash_different_strings() {
        assert_ne!(context_hash("printf"), context_hash("malloc"));
    }

    #[test]
    fn context_map_empty() {
        let data = build_context_btree(&[]);
        let map = ContextMap::from_bytes(&data).unwrap();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn context_map_single_entry() {
        let hash = context_hash("printf");
        let data = build_context_btree(&[(hash, 0x1000)]);
        let map = ContextMap::from_bytes(&data).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.resolve_hash(hash), Some(0x1000));
    }

    #[test]
    fn context_map_multiple_entries() {
        let entries = vec![
            (context_hash("printf"), 0x1000),
            (context_hash("malloc"), 0x2000),
            (context_hash("fopen"), 0x3000),
        ];
        let data = build_context_btree(&entries);
        let map = ContextMap::from_bytes(&data).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.resolve_hash(context_hash("printf")), Some(0x1000));
        assert_eq!(map.resolve_hash(context_hash("malloc")), Some(0x2000));
        assert_eq!(map.resolve_hash(context_hash("fopen")), Some(0x3000));
    }

    #[test]
    fn context_map_missing_hash() {
        let data = build_context_btree(&[(context_hash("printf"), 0x1000)]);
        let map = ContextMap::from_bytes(&data).unwrap();
        assert_eq!(map.resolve_hash(context_hash("nonexistent")), None);
    }

    #[test]
    fn context_map_bad_magic() {
        let mut data = build_context_btree(&[]);
        data[0] = 0xFF;
        let err = ContextMap::from_bytes(&data).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn context_hash_empty_string() {
        assert_eq!(context_hash(""), 0);
    }

    #[test]
    fn context_map_roundtrip_entries() {
        let entries = vec![(0xAAAA_BBBB, 100), (0xCCCC_DDDD, 200)];
        let data = build_context_btree(&entries);
        let map = ContextMap::from_bytes(&data).unwrap();
        let collected: HashMap<u32, u32> = map.entries().collect();
        assert_eq!(collected[&0xAAAA_BBBB], 100);
        assert_eq!(collected[&0xCCCC_DDDD], 200);
    }
}
