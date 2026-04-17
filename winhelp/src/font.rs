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
    /// Real layout (helpdeco `FONTHEADER`, confirmed against clib.hlp):
    ///
    /// ```text
    /// u16 num_facenames
    /// u16 num_descriptors
    /// u16 facenames_offset       (8 for WinHelp 3.1 OLDFONT; 16 for NEWFONT)
    /// u16 descriptors_offset
    /// [optional 8 bytes for NEWFONT: num_formats, formats_offset,
    ///                               num_charmap_tables, charmap_tables_offset]
    /// [facename table: num_facenames × fixed-size null-terminated strings]
    /// [descriptor table: num_descriptors × {u8 attr, u8 half_pts, u8 family,
    ///                                       u16 face_idx, u8[3] fg, u8[3] bg}
    ///                                       for OLDFONT (11 bytes each)]
    /// ```
    ///
    /// Face-name and descriptor sizes are derived from the offsets rather
    /// than hard-coded: this makes the parser tolerate both OLDFONT (11-byte
    /// descriptor, 20- or 32-byte face cell) and longer NEWFONT variants
    /// without version-sniffing.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(Error::BadInternalFile {
                name: "|FONT".into(),
                detail: "too small for FONTHEADER".into(),
            });
        }

        let num_facenames = u16::from_le_bytes([data[0], data[1]]) as usize;
        let num_descriptors = u16::from_le_bytes([data[2], data[3]]) as usize;
        let facenames_off = u16::from_le_bytes([data[4], data[5]]) as usize;
        let descriptors_off = u16::from_le_bytes([data[6], data[7]]) as usize;

        if num_facenames == 0
            || num_descriptors == 0
            || facenames_off < 8
            || descriptors_off <= facenames_off
            || descriptors_off > data.len()
        {
            return Ok(Self { fonts: Vec::new() });
        }

        let facename_region = descriptors_off - facenames_off;
        if !facename_region.is_multiple_of(num_facenames) {
            return Ok(Self { fonts: Vec::new() });
        }
        let facename_len = facename_region / num_facenames;

        let mut names = Vec::with_capacity(num_facenames);
        for i in 0..num_facenames {
            let start = facenames_off + i * facename_len;
            let end = start + facename_len;
            let slice = &data[start..end];
            let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
            names.push(String::from_utf8_lossy(&slice[..nul]).into_owned());
        }

        let desc_region = data.len() - descriptors_off;
        if desc_region < num_descriptors {
            return Ok(Self { fonts: Vec::new() });
        }
        let desc_size = desc_region / num_descriptors;
        if desc_size < 5 {
            return Ok(Self { fonts: Vec::new() });
        }

        let mut fonts = Vec::with_capacity(num_descriptors);
        for i in 0..num_descriptors {
            let p = descriptors_off + i * desc_size;
            let rec = &data[p..p + desc_size];
            let attributes = rec[0];
            let half_points = rec[1];
            let font_family = rec[2];
            let face_idx = u16::from_le_bytes([rec[3], rec[4]]) as usize;
            let name = names.get(face_idx).cloned().unwrap_or_default();
            fonts.push(FontDescriptor {
                attributes,
                half_points,
                font_family,
                name,
            });
        }

        Ok(Self { fonts })
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

    /// Build a synthetic OLDFONT |FONT blob with `faces` and `descs`
    /// (attributes, half_points, family, face_idx) tuples.
    fn build_oldfont(faces: &[&str], descs: &[(u8, u8, u8, u16)]) -> Vec<u8> {
        let facename_len = 20usize; // matches clib.hlp win16
        let header_size = 8usize;
        let facenames_off = header_size;
        let descriptors_off = facenames_off + faces.len() * facename_len;
        let desc_size = 11usize; // OLDFONT

        let mut buf = Vec::new();
        buf.extend_from_slice(&(faces.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(descs.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(facenames_off as u16).to_le_bytes());
        buf.extend_from_slice(&(descriptors_off as u16).to_le_bytes());

        for name in faces {
            let mut cell = vec![0u8; facename_len];
            let bytes = name.as_bytes();
            cell[..bytes.len().min(facename_len - 1)]
                .copy_from_slice(&bytes[..bytes.len().min(facename_len - 1)]);
            buf.extend_from_slice(&cell);
        }

        for &(attr, half_pts, family, face_idx) in descs {
            let mut rec = vec![0u8; desc_size];
            rec[0] = attr;
            rec[1] = half_pts;
            rec[2] = family;
            rec[3..5].copy_from_slice(&face_idx.to_le_bytes());
            buf.extend_from_slice(&rec);
        }

        buf
    }

    #[test]
    fn font_table_oldfont_parses_facenames_and_attributes() {
        let data = build_oldfont(
            &["Helv", "Courier"],
            &[
                (0x00, 20, 3, 0), // plain Helv
                (0x01, 20, 3, 0), // bold Helv
                (0x02, 20, 3, 0), // italic Helv
                (0x03, 20, 3, 0), // bold+italic Helv
                (0x00, 20, 1, 1), // plain Courier
            ],
        );
        let table = FontTable::from_bytes(&data).unwrap();
        assert_eq!(table.len(), 5);

        let f0 = table.get(0).unwrap();
        assert_eq!(f0.name, "Helv");
        assert!(!f0.is_bold() && !f0.is_italic() && !f0.is_underline());
        assert_eq!(f0.half_points, 20);

        assert!(table.get(1).unwrap().is_bold());
        assert!(table.get(2).unwrap().is_italic());
        let f3 = table.get(3).unwrap();
        assert!(f3.is_bold() && f3.is_italic());

        assert_eq!(table.get(4).unwrap().name, "Courier");
    }

    #[test]
    fn font_table_rejects_short_header() {
        let err = FontTable::from_bytes(&[0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn font_table_returns_empty_on_inconsistent_offsets() {
        // facenames_off > descriptors_off → nonsensical, return empty.
        let mut data = vec![0u8; 32];
        data[0..2].copy_from_slice(&1u16.to_le_bytes()); // 1 facename
        data[2..4].copy_from_slice(&1u16.to_le_bytes()); // 1 descriptor
        data[4..6].copy_from_slice(&20u16.to_le_bytes()); // facenames_off
        data[6..8].copy_from_slice(&8u16.to_le_bytes()); // descriptors_off (< facenames)
        let table = FontTable::from_bytes(&data).unwrap();
        assert!(table.is_empty());
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
