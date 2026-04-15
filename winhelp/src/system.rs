//! Parser for the `|SYSTEM` internal file.
//!
//! The `|SYSTEM` file contains metadata about the help file: title, copyright,
//! root topic context ID, WinHelp version, and window definitions.

use crate::{Error, Result};

/// Expected magic number at the start of |SYSTEM.
const SYSTEM_MAGIC: u16 = 0x036C;

/// Parsed metadata from the `|SYSTEM` internal file.
///
/// The SYSTEMHEADER layout (12 bytes total):
///   Offset 0–1:  Magic (0x036C)
///   Offset 2–3:  Minor version
///   Offset 4–5:  Major version (always 1; NOT the compression flags)
///   Offset 6–9:  GenDate (generation timestamp)
///   Offset 10–11: Flags (compression flags: 0=none, 4=LZ77 4k, 8=LZ77 2k)
///
/// Variable-length records follow the 12-byte header.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    /// WinHelp minor version (e.g. 15 for WinHelp 3.0, 21 for WinHelp 3.1+).
    pub minor_version: u16,
    /// Major version field (offset 4-5, always 1).  Not the compression flags.
    pub flags: u16,
    /// Compression/format flags at offset 10-11 (present when minor >= 10).
    /// 0 = uncompressed, 4 = LZ77 4 KiB blocks, 8 = LZ77 2 KiB blocks.
    pub flags_ex: Option<u16>,
    /// Help file title (record type 1).
    pub title: Option<String>,
    /// Copyright string (record type 2).
    pub copyright: Option<String>,
    /// Root topic byte offset in |TOPIC (record type 3).
    pub root_topic_offset: Option<u32>,
    /// Starting topic context string (record type 4).
    pub starting_topic: Option<String>,
}

impl SystemInfo {
    /// Parse from the raw bytes of the `|SYSTEM` internal file.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 6 {
            return Err(Error::BadInternalFile {
                name: "|SYSTEM".into(),
                detail: "too small for |SYSTEM header".into(),
            });
        }

        let magic = u16::from_le_bytes([data[0], data[1]]);
        if magic != SYSTEM_MAGIC {
            return Err(Error::BadInternalFile {
                name: "|SYSTEM".into(),
                detail: format!("bad magic: expected 0x{SYSTEM_MAGIC:04X}, got 0x{magic:04X}"),
            });
        }

        let minor_version = u16::from_le_bytes([data[2], data[3]]);
        let flags = u16::from_le_bytes([data[4], data[5]]);

        // For minor >= 10, the header has 6 extra bytes:
        //   u32 gen_date (seconds since epoch) at offset 6
        //   u16 flags_ex at offset 10
        // Records start at byte 12 instead of byte 6.
        let (flags_ex, records_start) = if minor_version >= 10 && data.len() >= 12 {
            let fex = u16::from_le_bytes([data[10], data[11]]);
            (Some(fex), 12)
        } else {
            (None, 6)
        };

        let mut info = SystemInfo {
            minor_version,
            flags,
            flags_ex,
            title: None,
            copyright: None,
            root_topic_offset: None,
            starting_topic: None,
        };

        // Parse variable-length records after the header.
        let mut pos = records_start;
        while pos + 4 <= data.len() {
            let record_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
            let record_len = u16::from_le_bytes([data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;

            if pos + record_len > data.len() {
                break;
            }

            let record_data = &data[pos..pos + record_len];

            match record_type {
                1 => {
                    // Title: null-terminated string
                    info.title = Some(read_nul_string(record_data));
                }
                2 => {
                    // Copyright: null-terminated string
                    info.copyright = Some(read_nul_string(record_data));
                }
                3 => {
                    // Root topic offset: u32
                    if record_data.len() >= 4 {
                        info.root_topic_offset = Some(u32::from_le_bytes([
                            record_data[0],
                            record_data[1],
                            record_data[2],
                            record_data[3],
                        ]));
                    }
                }
                4 => {
                    // Starting topic context string
                    info.starting_topic = Some(read_nul_string(record_data));
                }
                _ => {
                    // Skip unknown record types (window defs, macros, etc.)
                }
            }

            pos += record_len;
        }

        Ok(info)
    }

    /// Returns the compression flags for this file.
    ///
    /// For files with minor >= 10, this is the `Flags` field at offset 10-11
    /// (the `flags_ex` field in this struct). For older files, falls back to
    /// the field at offset 4-5 (`flags`), though those files don't use LZ77.
    pub fn compression_flags(&self) -> u16 {
        self.flags_ex.unwrap_or(self.flags)
    }

    /// Returns true if topic blocks are LZ77-compressed.
    ///
    /// From HELPFILE.TXT: Minor > 16 with Flags=4 or Flags=8 → LZ77 compressed.
    /// The Flags field is at |SYSTEM offset 10-11 (not 4-5 which is Major=1).
    pub fn uses_lz77(&self) -> bool {
        let f = self.compression_flags();
        self.minor_version > 16 && (f == 4 || f == 8)
    }

    /// Returns true if the phrase table in |Phrases is LZ77-compressed.
    ///
    /// The |Phrases data block is LZ77-compressed in WinHelp 3.1+ files.
    pub fn phrases_compressed(&self) -> bool {
        self.minor_version > 16
    }

    /// Returns the on-disk topic block size.
    ///
    /// From HELPFILE.TXT:
    ///   Minor ≤ 16 (WinHelp 3.0)             → 2 048 bytes
    ///   Minor > 16, Flags = 8 (LZ77, 2 k)    → 2 048 bytes
    ///   Minor > 16, Flags = 0 or 4 (4 k)     → 4 096 bytes
    pub fn topic_block_size(&self) -> usize {
        let f = self.compression_flags();
        if self.minor_version <= 16 || f == 8 {
            2048
        } else {
            4096
        }
    }

    /// Returns the virtual decompression-buffer size used for TOPICPOS maths.
    ///
    /// From HELPFILE.TXT / helpdeco source:
    ///   WinHelp 3.0 (Minor ≤ 16): DecompressSize = TopicBlockSize = 2 048
    ///   WinHelp 3.1+ (Minor > 16): DecompressSize = 16 384 (0x4000) always
    ///
    /// TOPICPOS arithmetic: `(topicpos − 12) / decompress_size` = block number,
    /// `(topicpos − 12) % decompress_size` = byte offset within that block.
    pub fn decompress_size(&self) -> usize {
        if self.minor_version <= 16 {
            2048
        } else {
            0x4000 // 16 384
        }
    }
}

/// Read a null-terminated string from a byte slice.
fn read_nul_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal |SYSTEM file with given records.
    /// For minor >= 10, includes the extended header (gen_date + flags_ex).
    fn build_system(minor_version: u16, flags: u16, records: &[(u16, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SYSTEM_MAGIC.to_le_bytes());
        buf.extend_from_slice(&minor_version.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        if minor_version >= 10 {
            // Extended header: u32 gen_date + u16 flags_ex
            buf.extend_from_slice(&0u32.to_le_bytes()); // gen_date
            buf.extend_from_slice(&flags.to_le_bytes()); // flags_ex = same as flags for tests
        }
        for (rtype, rdata) in records {
            buf.extend_from_slice(&rtype.to_le_bytes());
            buf.extend_from_slice(&(rdata.len() as u16).to_le_bytes());
            buf.extend_from_slice(rdata);
        }
        buf
    }

    #[test]
    fn parse_minimal_system_old() {
        // minor < 10: standard 6-byte header, no extended flags.
        let data = build_system(9, 0, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.minor_version, 9);
        assert_eq!(info.flags, 0);
        assert!(info.flags_ex.is_none());
        assert!(info.title.is_none());
        assert!(info.copyright.is_none());
    }

    #[test]
    fn parse_minimal_system_extended() {
        // minor >= 10: extended 12-byte header with flags_ex.
        let data = build_system(21, 0x0001, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.minor_version, 21);
        assert_eq!(info.flags, 0x0001);
        assert_eq!(info.flags_ex, Some(0x0001));
    }

    #[test]
    fn parse_title_and_copyright() {
        // minor=15 is WinHelp 3.0: NEVER LZ77-compressed regardless of flags.
        let data = build_system(
            15,
            0x0004,
            &[(1, b"C Library Reference\0"), (2, b"Open Watcom\0")],
        );
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.title.as_deref(), Some("C Library Reference"));
        assert_eq!(info.copyright.as_deref(), Some("Open Watcom"));
        assert!(!info.uses_lz77()); // 3.0 files are never LZ77-compressed
    }

    #[test]
    fn lz77_flags_31() {
        // minor=21, flags=4 → WinHelp 3.1 LZ77 compressed.
        let data = build_system(21, 0x0004, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert!(info.uses_lz77());
        assert_eq!(info.topic_block_size(), 4096);
        assert_eq!(info.decompress_size(), 16384);
    }

    #[test]
    fn lz77_flags_31_2k() {
        // minor=21, flags=8 → WinHelp 3.1 LZ77 with 2k blocks.
        let data = build_system(21, 0x0008, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert!(info.uses_lz77());
        assert_eq!(info.topic_block_size(), 2048);
        assert_eq!(info.decompress_size(), 16384);
    }

    #[test]
    fn uncompressed_31() {
        // minor=21, flags=0 → WinHelp 3.1 uncompressed.
        let data = build_system(21, 0x0000, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert!(!info.uses_lz77());
        assert_eq!(info.topic_block_size(), 4096);
        assert_eq!(info.decompress_size(), 16384);
    }

    #[test]
    fn wh30_sizes() {
        // minor=15 (3.0): block size and decompress size both 2k.
        let data = build_system(15, 0x0000, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert!(!info.uses_lz77());
        assert_eq!(info.topic_block_size(), 2048);
        assert_eq!(info.decompress_size(), 2048);
    }

    #[test]
    fn parse_root_topic_offset() {
        let offset_bytes = 0x1234u32.to_le_bytes();
        let data = build_system(15, 0, &[(3, &offset_bytes)]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.root_topic_offset, Some(0x1234));
    }

    #[test]
    fn parse_starting_topic() {
        let data = build_system(21, 0, &[(4, b"contents\0")]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.starting_topic.as_deref(), Some("contents"));
        assert_eq!(info.minor_version, 21);
    }

    #[test]
    fn bad_magic() {
        let mut data = build_system(15, 0, &[]);
        data[0] = 0xFF;
        let err = SystemInfo::from_bytes(&data).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn too_small() {
        let data = vec![0; 4];
        let err = SystemInfo::from_bytes(&data).unwrap_err();
        assert!(matches!(err, Error::BadInternalFile { .. }));
    }

    #[test]
    fn unknown_records_skipped() {
        let data = build_system(
            15,
            0,
            &[
                (99, b"unknown data"),
                (1, b"My Help File\0"),
                (255, b"more unknown"),
            ],
        );
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.title.as_deref(), Some("My Help File"));
    }

    #[test]
    fn multiple_records_all_types() {
        let offset_bytes = 42u32.to_le_bytes();
        let data = build_system(
            21,
            0x0004,
            &[
                (1, b"Test Title\0"),
                (2, b"(c) 2026\0"),
                (3, &offset_bytes),
                (4, b"main_page\0"),
            ],
        );
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.title.as_deref(), Some("Test Title"));
        assert_eq!(info.copyright.as_deref(), Some("(c) 2026"));
        assert_eq!(info.root_topic_offset, Some(42));
        assert_eq!(info.starting_topic.as_deref(), Some("main_page"));
        assert!(info.uses_lz77());
    }
}
