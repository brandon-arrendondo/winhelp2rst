//! HLP file container: header, internal directory B-tree, and file extraction.
//!
//! The HLP file is a virtual filesystem. A B-tree near the start of the file
//! indexes named internal files (e.g. `|SYSTEM`, `|TOPIC`, `|Phrases`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::{Error, Result};

/// HLP file magic number: 0x00035F3F (little-endian).
const HLP_MAGIC: u32 = 0x00035F3F;

/// Parsed HLP file header.
#[derive(Debug, Clone)]
struct HlpHeader {
    /// Byte offset of the internal directory B-tree.
    directory_start: u32,
}

/// An entry in the internal file directory.
#[derive(Debug, Clone)]
pub struct InternalFile {
    /// Internal file name (e.g. `|SYSTEM`, `|TOPIC`).
    pub name: String,
    /// Byte offset within the HLP file.
    pub offset: u64,
}

/// Reader for the HLP container (virtual filesystem layer).
///
/// Provides access to the internal files stored within the HLP archive.
#[derive(Debug)]
pub struct HlpContainer {
    /// Raw bytes of the entire HLP file.
    data: Vec<u8>,
    /// Internal directory entries (name → offset).
    directory: HashMap<String, u64>,
    /// Ordered list of internal file entries.
    files: Vec<InternalFile>,
}

impl HlpContainer {
    /// Open and parse an HLP file from disk.
    pub fn open(path: &Path) -> Result<Self> {
        let data = fs::read(path)?;
        Self::from_bytes(data)
    }

    /// Parse an HLP container from raw bytes.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let header = parse_header(&data)?;
        let (directory, files) = parse_directory(&data, header.directory_start as usize)?;
        Ok(Self {
            data,
            directory,
            files,
        })
    }

    /// List all internal files in directory order.
    pub fn list_files(&self) -> &[InternalFile] {
        &self.files
    }

    /// Read the raw bytes of an internal file by name.
    ///
    /// Internal file data starts with a header: the first 9 bytes contain
    /// file-size and compression information. This method returns the data
    /// portion after that header.
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>> {
        let offset = self
            .directory
            .get(name)
            .copied()
            .ok_or_else(|| Error::FileNotFound(name.to_string()))?;

        read_internal_file_data(&self.data, name, offset as usize)
    }

    /// Read the raw bytes of an internal file including its header.
    ///
    /// Returns everything from the internal file's offset, up to the
    /// file size declared in its header.
    pub fn read_file_raw(&self, name: &str) -> Result<Vec<u8>> {
        let offset = self
            .directory
            .get(name)
            .copied()
            .ok_or_else(|| Error::FileNotFound(name.to_string()))?;

        read_internal_file_raw(&self.data, name, offset as usize)
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Read a little-endian u16 from `data` at `offset`.
fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes: [u8; 2] = data
        .get(offset..offset + 2)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| Error::Parse {
            offset: offset as u64,
            detail: "unexpected EOF reading u16".into(),
        })?;
    Ok(u16::from_le_bytes(bytes))
}

/// Read a little-endian u32 from `data` at `offset`.
fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| Error::Parse {
            offset: offset as u64,
            detail: "unexpected EOF reading u32".into(),
        })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Read a null-terminated string from `data` starting at `offset`.
/// Returns the string and the number of bytes consumed (including the null).
fn read_cstring(data: &[u8], offset: usize) -> Result<(String, usize)> {
    let start = offset;
    let remaining = data.get(offset..).ok_or_else(|| Error::Parse {
        offset: offset as u64,
        detail: "unexpected EOF reading string".into(),
    })?;
    let null_pos = remaining
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Parse {
            offset: offset as u64,
            detail: "unterminated string".into(),
        })?;
    let s = String::from_utf8_lossy(&remaining[..null_pos]).into_owned();
    Ok((s, null_pos + 1 + start - start)) // null_pos + 1 bytes consumed
}

/// Parse the 8-byte HLP file header.
fn parse_header(data: &[u8]) -> Result<HlpHeader> {
    if data.len() < 8 {
        return Err(Error::Parse {
            offset: 0,
            detail: "file too small for HLP header".into(),
        });
    }
    let magic = read_u32(data, 0)?;
    if magic != HLP_MAGIC {
        return Err(Error::BadMagic(magic));
    }
    let directory_start = read_u32(data, 4)?;
    Ok(HlpHeader { directory_start })
}

/// Internal file header format:
///   offset 0x00: u32 reserved_space  — total reserved bytes for this file
///   offset 0x04: u32 used_space      — bytes actually used
///   offset 0x08: u8  file_flags      — compression flags
///
/// The data follows immediately after these 9 bytes.
const INTERNAL_FILE_HEADER_SIZE: usize = 9;

/// Parse the internal directory B-tree.
///
/// The directory sits at `dir_offset` and has this layout:
///   offset +0x00: u16  magic (0x293B)
///   offset +0x02: u16  flags (bit 0x0002 = directory, bit 0x0400 = has counters)
///   offset +0x04: u16  page_size (typically 1024 or 2048)
///   offset +0x06: u16  structure_info (describes key/value layout)
///   offset +0x08: u16  must_be_zero
///   offset +0x0A: u16  num_pages (total pages in B-tree)
///   offset +0x0C: u16  root_page_index
///   offset +0x0E: u16  unused
///   offset +0x10: u16  num_levels
///   offset +0x12: u32  total_entries
///   offset +0x16: (page data follows)
fn parse_directory(
    data: &[u8],
    dir_offset: usize,
) -> Result<(HashMap<String, u64>, Vec<InternalFile>)> {
    // The directory itself starts with an internal-file header (9 bytes)
    // before the B-tree header.
    let btree_offset = dir_offset + INTERNAL_FILE_HEADER_SIZE;

    if data.len() < btree_offset + 22 {
        return Err(Error::Parse {
            offset: dir_offset as u64,
            detail: "directory too small for B-tree header".into(),
        });
    }

    let btree_magic = read_u16(data, btree_offset)?;
    if btree_magic != 0x293B {
        return Err(Error::BadInternalFile {
            name: "(directory)".into(),
            detail: format!("bad B-tree magic: expected 0x293B, got 0x{btree_magic:04X}"),
        });
    }

    let flags = read_u16(data, btree_offset + 2)?;
    let page_size = read_u16(data, btree_offset + 4)? as usize;
    let _structure = read_u16(data, btree_offset + 6)?;
    let _must_be_zero = read_u16(data, btree_offset + 8)?;
    let num_pages = read_u16(data, btree_offset + 10)? as usize;
    let root_page = read_u16(data, btree_offset + 12)? as usize;
    let _unused = read_u16(data, btree_offset + 14)?;
    let num_levels = read_u16(data, btree_offset + 16)? as usize;
    let _total_entries = read_u32(data, btree_offset + 18)?;

    // Pages begin right after the 22-byte B-tree header.
    let pages_start = btree_offset + 22;

    let has_counters = flags & 0x0400 != 0;

    let mut directory = HashMap::new();
    let mut files = Vec::new();

    if num_levels == 0 || num_pages == 0 {
        return Ok((directory, files));
    }

    let btree = BTreeParams {
        data,
        pages_start,
        page_size,
        has_counters,
    };

    // Walk the B-tree from the root to collect all leaf entries.
    collect_leaf_entries(&btree, root_page, num_levels, &mut directory, &mut files)?;

    Ok((directory, files))
}

/// Shared parameters for B-tree traversal.
struct BTreeParams<'a> {
    data: &'a [u8],
    pages_start: usize,
    page_size: usize,
    has_counters: bool,
}

/// Recursively traverse the B-tree to collect entries from leaf pages.
fn collect_leaf_entries(
    btree: &BTreeParams<'_>,
    page_index: usize,
    levels_remaining: usize,
    directory: &mut HashMap<String, u64>,
    files: &mut Vec<InternalFile>,
) -> Result<()> {
    let page_offset = btree.pages_start + page_index * btree.page_size;

    if levels_remaining == 1 {
        // Leaf page
        parse_leaf_page(btree.data, page_offset, btree.page_size, directory, files)?;
    } else {
        // Index (non-leaf) page
        let child_pages =
            parse_index_page(btree.data, page_offset, btree.page_size, btree.has_counters)?;

        for child_index in child_pages {
            collect_leaf_entries(
                btree,
                child_index,
                levels_remaining - 1,
                directory,
                files,
            )?;
        }
    }

    Ok(())
}

/// Parse a leaf page of the directory B-tree.
///
/// Leaf page layout:
///   u16  unused (previous page pointer, not needed for forward scan)
///   u16  num_entries
///   entries[]: each is a null-terminated name + u32 offset
fn parse_leaf_page(
    data: &[u8],
    page_offset: usize,
    page_size: usize,
    directory: &mut HashMap<String, u64>,
    files: &mut Vec<InternalFile>,
) -> Result<()> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "leaf page extends past end of file".into(),
        });
    }

    let _prev_page = read_u16(data, page_offset)?;
    let num_entries = read_u16(data, page_offset + 2)? as usize;

    let mut pos = page_offset + 4;

    for _ in 0..num_entries {
        if pos >= page_end {
            break;
        }

        let (name, consumed) = read_cstring(data, pos)?;
        pos += consumed;

        let offset = read_u32(data, pos)? as u64;
        pos += 4;

        directory.insert(name.clone(), offset);
        files.push(InternalFile {
            name,
            offset,
        });
    }

    Ok(())
}

/// Parse an index (non-leaf) page and return child page indices.
///
/// Index page layout:
///   u16  unused
///   u16  num_entries
///   u16  first_child_page (the child page to the left of the first key)
///   entries[]: each is a null-terminated name + u16 child_page_index
///
/// If the tree has counters (flag 0x0400), each entry also has a u16 count
/// before the child page index.
fn parse_index_page(
    data: &[u8],
    page_offset: usize,
    page_size: usize,
    has_counters: bool,
) -> Result<Vec<usize>> {
    let page_end = page_offset + page_size;
    if data.len() < page_end {
        return Err(Error::Parse {
            offset: page_offset as u64,
            detail: "index page extends past end of file".into(),
        });
    }

    let _unused = read_u16(data, page_offset)?;
    let num_entries = read_u16(data, page_offset + 2)? as usize;

    let mut pos = page_offset + 4;

    // First child page (before first key).
    let first_child = read_u16(data, pos)? as usize;
    pos += 2;

    let mut children = Vec::with_capacity(num_entries + 1);
    children.push(first_child);

    for _ in 0..num_entries {
        if pos >= page_end {
            break;
        }

        // Skip the key name.
        let (_name, consumed) = read_cstring(data, pos)?;
        pos += consumed;

        // Optional counter.
        if has_counters {
            let _count = read_u16(data, pos)?;
            pos += 2;
        }

        // Child page index.
        let child = read_u16(data, pos)? as usize;
        pos += 2;

        children.push(child);
    }

    Ok(children)
}

/// Read the data portion of an internal file (after the 9-byte header).
fn read_internal_file_data(data: &[u8], name: &str, offset: usize) -> Result<Vec<u8>> {
    if data.len() < offset + INTERNAL_FILE_HEADER_SIZE {
        return Err(Error::BadInternalFile {
            name: name.to_string(),
            detail: "offset extends past end of file".into(),
        });
    }

    let _reserved = read_u32(data, offset)?;
    let used_space = read_u32(data, offset + 4)? as usize;
    let _flags = data[offset + 8];

    let data_start = offset + INTERNAL_FILE_HEADER_SIZE;
    let data_end = data_start + used_space;

    if data_end > data.len() {
        return Err(Error::BadInternalFile {
            name: name.to_string(),
            detail: format!(
                "internal file data ({used_space} bytes at 0x{data_start:X}) extends past EOF"
            ),
        });
    }

    Ok(data[data_start..data_end].to_vec())
}

/// Read the raw bytes of an internal file including its header.
fn read_internal_file_raw(data: &[u8], name: &str, offset: usize) -> Result<Vec<u8>> {
    if data.len() < offset + INTERNAL_FILE_HEADER_SIZE {
        return Err(Error::BadInternalFile {
            name: name.to_string(),
            detail: "offset extends past end of file".into(),
        });
    }

    let _reserved = read_u32(data, offset)?;
    let used_space = read_u32(data, offset + 4)? as usize;

    let total_size = INTERNAL_FILE_HEADER_SIZE + used_space;
    let end = offset + total_size;

    if end > data.len() {
        return Err(Error::BadInternalFile {
            name: name.to_string(),
            detail: format!(
                "internal file ({total_size} bytes at 0x{offset:X}) extends past EOF"
            ),
        });
    }

    Ok(data[offset..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid HLP file with a single-leaf-page B-tree directory.
    fn build_test_hlp(entries: &[(&str, u32)]) -> Vec<u8> {
        // We'll lay out:
        //   [0..8)    HLP header: magic + directory_start
        //   [8..17)   Internal file header for directory (9 bytes)
        //   [17..39)  B-tree header (22 bytes)
        //   [39..)    Single leaf page

        let dir_offset: u32 = 8;

        // Build the leaf page.
        let mut page = Vec::new();
        // u16 previous_page (unused for single page)
        page.extend_from_slice(&0u16.to_le_bytes());
        // u16 num_entries
        page.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (name, offset) in entries {
            // null-terminated name
            page.extend_from_slice(name.as_bytes());
            page.push(0);
            // u32 offset
            page.extend_from_slice(&offset.to_le_bytes());
        }

        // Page size must be at least as large as the page content.
        let page_size = page.len().max(64);
        // Pad page to page_size.
        page.resize(page_size, 0);

        let total_btree_data = 22 + page.len();

        let mut buf = Vec::new();

        // HLP header.
        buf.extend_from_slice(&HLP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&dir_offset.to_le_bytes());

        // Internal file header for the directory.
        let reserved_space = total_btree_data as u32;
        let used_space = total_btree_data as u32;
        buf.extend_from_slice(&reserved_space.to_le_bytes());
        buf.extend_from_slice(&used_space.to_le_bytes());
        buf.push(0); // flags

        // B-tree header (22 bytes).
        buf.extend_from_slice(&0x293Bu16.to_le_bytes()); // magic
        buf.extend_from_slice(&0x0002u16.to_le_bytes()); // flags (directory)
        buf.extend_from_slice(&(page_size as u16).to_le_bytes()); // page_size
        buf.extend_from_slice(&0u16.to_le_bytes()); // structure
        buf.extend_from_slice(&0u16.to_le_bytes()); // must_be_zero
        buf.extend_from_slice(&1u16.to_le_bytes()); // num_pages
        buf.extend_from_slice(&0u16.to_le_bytes()); // root_page (page 0)
        buf.extend_from_slice(&0u16.to_le_bytes()); // unused
        buf.extend_from_slice(&1u16.to_le_bytes()); // num_levels (1 = leaf only)
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes()); // total_entries

        // Leaf page.
        buf.extend_from_slice(&page);

        buf
    }

    /// Build a fake internal file at a given buffer position with known content.
    fn build_internal_file(content: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let reserved = (content.len()) as u32;
        let used = (content.len()) as u32;
        buf.extend_from_slice(&reserved.to_le_bytes());
        buf.extend_from_slice(&used.to_le_bytes());
        buf.push(0); // flags
        buf.extend_from_slice(content);
        buf
    }

    #[test]
    fn parse_header_valid() {
        let data = build_test_hlp(&[]);
        let header = parse_header(&data).unwrap();
        assert_eq!(header.directory_start, 8);
    }

    #[test]
    fn parse_header_bad_magic() {
        let mut data = build_test_hlp(&[]);
        // Corrupt magic.
        data[0] = 0xFF;
        let err = parse_header(&data).unwrap_err();
        assert!(matches!(err, Error::BadMagic(_)));
    }

    #[test]
    fn parse_header_too_small() {
        let data = vec![0; 4];
        let err = parse_header(&data).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn empty_directory() {
        let data = build_test_hlp(&[]);
        let container = HlpContainer::from_bytes(data).unwrap();
        assert!(container.list_files().is_empty());
    }

    #[test]
    fn single_entry_directory() {
        let data = build_test_hlp(&[("|SYSTEM", 100)]);
        let container = HlpContainer::from_bytes(data).unwrap();
        let files = container.list_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "|SYSTEM");
        assert_eq!(files[0].offset, 100);
    }

    #[test]
    fn multiple_entries_directory() {
        let data = build_test_hlp(&[
            ("|CONTEXT", 200),
            ("|Phrases", 300),
            ("|SYSTEM", 100),
            ("|TOPIC", 400),
        ]);
        let container = HlpContainer::from_bytes(data).unwrap();
        let files = container.list_files();
        assert_eq!(files.len(), 4);

        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"|SYSTEM"));
        assert!(names.contains(&"|TOPIC"));
        assert!(names.contains(&"|CONTEXT"));
        assert!(names.contains(&"|Phrases"));
    }

    #[test]
    fn read_file_not_found() {
        let data = build_test_hlp(&[("|SYSTEM", 100)]);
        let container = HlpContainer::from_bytes(data).unwrap();
        let err = container.read_file("|NONEXISTENT").unwrap_err();
        assert!(matches!(err, Error::FileNotFound(_)));
    }

    #[test]
    fn read_internal_file_content() {
        // Build an HLP with a directory entry pointing to an internal file
        // that contains known content.
        let content = b"hello, winhelp!";
        let internal = build_internal_file(content);

        // Build HLP, figure out where the internal file will land, then
        // rebuild the directory pointing to the correct offset.
        let placeholder = build_test_hlp(&[("|TEST", 0)]);
        let actual_offset = placeholder.len() as u32;

        let mut full = build_test_hlp(&[("|TEST", actual_offset)]);
        if full.len() < actual_offset as usize {
            full.resize(actual_offset as usize, 0);
        }
        full.extend_from_slice(&internal);

        let container = HlpContainer::from_bytes(full).unwrap();
        let data = container.read_file("|TEST").unwrap();
        assert_eq!(data, content);
    }

    #[test]
    fn read_cstring_basic() {
        let data = b"hello\x00world";
        let (s, consumed) = read_cstring(data, 0).unwrap();
        assert_eq!(s, "hello");
        assert_eq!(consumed, 6); // 5 chars + null
    }

    #[test]
    fn read_u16_basic() {
        let data = [0x34, 0x12];
        assert_eq!(read_u16(&data, 0).unwrap(), 0x1234);
    }

    #[test]
    fn read_u32_basic() {
        let data = [0x78, 0x56, 0x34, 0x12];
        assert_eq!(read_u32(&data, 0).unwrap(), 0x12345678);
    }
}
