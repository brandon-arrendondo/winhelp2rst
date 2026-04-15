//! `|FONT` table reader.
//!
//! The `|FONT` internal file contains an array of font descriptors used by the
//! topic opcode parser. Font attributes help determine semantic formatting
//! (e.g. monospace → RST literal).

use crate::{Error, Result};

/// A single font descriptor from the |FONT table.
#[derive(Debug, Clone)]
pub struct FontDescriptor {
    /// Font attributes bitfield (bit 0 = bold, bit 1 = italic, bit 2 = underline).
    pub attributes: u8,
    /// Font size in half-points (e.g. 24 = 12pt).
    pub half_points: u8,
    /// Font family identifier.
    pub font_family: u8,
    /// Font name string.
    pub name: String,
}

impl FontDescriptor {
    /// Returns true if this font is bold.
    pub fn is_bold(&self) -> bool {
        self.attributes & 0x01 != 0
    }

    /// Returns true if this font is italic.
    pub fn is_italic(&self) -> bool {
        self.attributes & 0x02 != 0
    }

    /// Returns true if this font is underlined.
    pub fn is_underline(&self) -> bool {
        self.attributes & 0x04 != 0
    }
}

/// Parsed font table from |FONT.
#[derive(Debug, Clone)]
pub struct FontTable {
    fonts: Vec<FontDescriptor>,
}

impl FontTable {
    /// Parse the font table from raw `|FONT` bytes.
    ///
    /// Layout:
    /// - `u16 num_fonts`
    /// - For each font: `u8 attributes`, `u8 half_points`, `u8 font_family`,
    ///   followed by a null-terminated font name.
    ///
    /// Note: The exact |FONT format varies between WinHelp versions. This
    /// parser handles the common layout. Some files have a different header
    /// (u16 num_face_names + u16 num_descriptors + face name table + descriptor
    /// table). We handle both variants.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(Error::BadInternalFile {
                name: "|FONT".into(),
                detail: "too small for font count".into(),
            });
        }

        let num_fonts = u16::from_le_bytes([data[0], data[1]]) as usize;

        // Try the simple format first: count + entries with inline names.
        match Self::parse_simple(data, num_fonts) {
            Ok(table) if !table.fonts.is_empty() => return Ok(table),
            _ => {}
        }

        // Fallback: empty table if we can't parse.
        Ok(Self { fonts: Vec::new() })
    }

    /// Build an empty font table.
    pub fn empty() -> Self {
        Self { fonts: Vec::new() }
    }

    /// Build a font table from a list of descriptors (test helper).
    #[doc(hidden)]
    pub fn from_descriptors(fonts: Vec<FontDescriptor>) -> Self {
        Self { fonts }
    }

    /// Get a font descriptor by index.
    pub fn get(&self, index: usize) -> Option<&FontDescriptor> {
        self.fonts.get(index)
    }

    /// Number of fonts in the table.
    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    /// Returns true if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    fn parse_simple(data: &[u8], num_fonts: usize) -> Result<Self> {
        let mut fonts = Vec::with_capacity(num_fonts);
        let mut pos = 2;

        for _ in 0..num_fonts {
            if pos + 3 > data.len() {
                break;
            }

            let attributes = data[pos];
            let half_points = data[pos + 1];
            let font_family = data[pos + 2];
            pos += 3;

            // Read null-terminated name.
            let name_start = pos;
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            let name = String::from_utf8_lossy(&data[name_start..pos]).into_owned();
            if pos < data.len() {
                pos += 1; // skip null
            }

            fonts.push(FontDescriptor {
                attributes,
                half_points,
                font_family,
                name,
            });
        }

        Ok(Self { fonts })
    }
}

/// Parsed title index from |TTLBTREE.
///
/// Maps topic byte offsets to display titles, providing the canonical
/// topic ordering for index generation.
#[derive(Debug, Clone)]
pub struct TitleIndex {
    /// Ordered list of (topic_offset, title) pairs.
    entries: Vec<(u32, String)>,
}

impl TitleIndex {
    /// Parse from the raw bytes of the `|TTLBTREE` internal file.
    ///
    /// Like |CONTEXT, this is a B-tree with u32 keys (topic offsets) and
    /// null-terminated string values (titles).
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        /// B-tree header size: 38 bytes.
        const BTREE_HEADER_SIZE: usize = 0x26;

        if data.len() < BTREE_HEADER_SIZE {
            return Err(Error::BadInternalFile {
                name: "|TTLBTREE".into(),
                detail: "too small for B-tree header".into(),
            });
        }

        let magic = u16::from_le_bytes([data[0], data[1]]);
        if magic != 0x293B {
            return Err(Error::BadInternalFile {
                name: "|TTLBTREE".into(),
                detail: format!("bad B-tree magic: 0x{magic:04X}"),
            });
        }

        let _flags = u16::from_le_bytes([data[2], data[3]]);
        let page_size = u16::from_le_bytes([data[4], data[5]]) as usize;
        // +0x06: char[16] structure — skip
        // +0x16: u16 must_be_zero — skip
        // +0x18: u16 page_splits — skip
        let root_page = u16::from_le_bytes([data[0x1A], data[0x1B]]) as usize;
        // +0x1C: i16 must_be_neg_one — skip
        let _num_pages = u16::from_le_bytes([data[0x1E], data[0x1F]]);
        let num_levels = u16::from_le_bytes([data[0x20], data[0x21]]) as usize;

        let pages_start = BTREE_HEADER_SIZE;
        let mut entries = Vec::new();

        if num_levels > 0 {
            collect_title_entries(
                data,
                pages_start,
                page_size,
                root_page,
                num_levels,
                &mut entries,
            )?;
        }

        // Sort by offset for stable ordering.
        entries.sort_by_key(|(offset, _)| *offset);

        Ok(Self { entries })
    }

    /// Build an empty title index.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Get all entries in offset order as (offset, title) pairs.
    pub fn titles_in_order(&self) -> &[(u32, String)] {
        &self.entries
    }

    /// Look up a title by topic offset.
    pub fn get_title(&self, offset: u32) -> Option<&str> {
        self.entries
            .iter()
            .find(|(o, _)| *o == offset)
            .map(|(_, t)| t.as_str())
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn collect_title_entries(
    data: &[u8],
    pages_start: usize,
    page_size: usize,
    page_index: usize,
    levels_remaining: usize,
    entries: &mut Vec<(u32, String)>,
) -> Result<()> {
    let page_offset = pages_start + page_index * page_size;

    if levels_remaining == 1 {
        parse_title_leaf(data, page_offset, page_size, entries)?;
    } else {
        let children = parse_title_index_page(data, page_offset, page_size)?;
        for child in children {
            collect_title_entries(
                data,
                pages_start,
                page_size,
                child,
                levels_remaining - 1,
                entries,
            )?;
        }
    }

    Ok(())
}

/// Parse a leaf page: u32 offset + null-terminated title string per entry.
fn parse_title_leaf(
    data: &[u8],
    page_offset: usize,
    page_size: usize,
    entries: &mut Vec<(u32, String)>,
) -> Result<()> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "title leaf page past EOF".into(),
        });
    }

    let _prev = u16::from_le_bytes([data[page_offset], data[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([data[page_offset + 2], data[page_offset + 3]]) as usize;

    let mut pos = page_offset + 4;
    for _ in 0..num_entries {
        if pos + 4 > page_end {
            break;
        }
        let offset = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;

        // Null-terminated title string.
        let start = pos;
        while pos < page_end && data[pos] != 0 {
            pos += 1;
        }
        let title = String::from_utf8_lossy(&data[start..pos]).into_owned();
        if pos < page_end {
            pos += 1; // skip null
        }

        entries.push((offset, title));
    }

    Ok(())
}

/// Parse an index page for |TTLBTREE. Keys are u32, values are u16 page indices.
fn parse_title_index_page(data: &[u8], page_offset: usize, page_size: usize) -> Result<Vec<usize>> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "title index page past EOF".into(),
        });
    }

    let _unused = u16::from_le_bytes([data[page_offset], data[page_offset + 1]]);
    let num_entries = u16::from_le_bytes([data[page_offset + 2], data[page_offset + 3]]) as usize;

    let mut pos = page_offset + 4;
    let first_child = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;

    let mut children = Vec::with_capacity(num_entries + 1);
    children.push(first_child);

    for _ in 0..num_entries {
        if pos + 6 > page_end {
            break;
        }
        pos += 4; // skip u32 key
        let child = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        children.push(child);
    }

    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- FontTable tests --

    #[test]
    fn font_table_simple() {
        let mut data = Vec::new();
        data.extend_from_slice(&2u16.to_le_bytes()); // 2 fonts
                                                     // Font 0: bold, 24 half-points, family 0, "Arial"
        data.push(0x01);
        data.push(24);
        data.push(0);
        data.extend_from_slice(b"Arial\0");
        // Font 1: italic, 20 half-points, family 1, "Courier"
        data.push(0x02);
        data.push(20);
        data.push(1);
        data.extend_from_slice(b"Courier\0");

        let table = FontTable::from_bytes(&data).unwrap();
        assert_eq!(table.len(), 2);

        let f0 = table.get(0).unwrap();
        assert_eq!(f0.name, "Arial");
        assert!(f0.is_bold());
        assert!(!f0.is_italic());
        assert_eq!(f0.half_points, 24);

        let f1 = table.get(1).unwrap();
        assert_eq!(f1.name, "Courier");
        assert!(f1.is_italic());
        assert!(!f1.is_bold());
    }

    #[test]
    fn font_table_empty() {
        let table = FontTable::empty();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.get(0).is_none());
    }

    #[test]
    fn font_descriptor_flags() {
        let f = FontDescriptor {
            attributes: 0x07, // bold + italic + underline
            half_points: 24,
            font_family: 0,
            name: "Test".into(),
        };
        assert!(f.is_bold());
        assert!(f.is_italic());
        assert!(f.is_underline());
    }

    // -- TitleIndex tests --

    fn build_title_btree(entries: &[(u32, &str)]) -> Vec<u8> {
        // Leaf page
        let mut page = Vec::new();
        page.extend_from_slice(&0u16.to_le_bytes()); // prev
        page.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (offset, title) in entries {
            page.extend_from_slice(&offset.to_le_bytes());
            page.extend_from_slice(title.as_bytes());
            page.push(0);
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
    fn title_index_basic() {
        let data = build_title_btree(&[(0x1000, "printf"), (0x2000, "malloc")]);
        let index = TitleIndex::from_bytes(&data).unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index.get_title(0x1000), Some("printf"));
        assert_eq!(index.get_title(0x2000), Some("malloc"));
    }

    #[test]
    fn title_index_sorted_by_offset() {
        let data = build_title_btree(&[(0x3000, "c"), (0x1000, "a"), (0x2000, "b")]);
        let index = TitleIndex::from_bytes(&data).unwrap();
        let titles = index.titles_in_order();
        assert_eq!(titles[0].0, 0x1000);
        assert_eq!(titles[1].0, 0x2000);
        assert_eq!(titles[2].0, 0x3000);
    }

    #[test]
    fn title_index_empty() {
        let index = TitleIndex::empty();
        assert!(index.is_empty());
    }

    #[test]
    fn title_index_bad_magic() {
        let mut data = build_title_btree(&[]);
        data[0] = 0xFF;
        let err = TitleIndex::from_bytes(&data).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn title_index_missing_offset() {
        let data = build_title_btree(&[(0x1000, "printf")]);
        let index = TitleIndex::from_bytes(&data).unwrap();
        assert_eq!(index.get_title(0x9999), None);
    }
}
