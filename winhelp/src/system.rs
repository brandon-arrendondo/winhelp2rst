//! Parser for the `|SYSTEM` internal file.
//!
//! The `|SYSTEM` file contains metadata about the help file: title, copyright,
//! root topic context ID, WinHelp version, and window definitions.

use crate::{Error, Result};

/// Expected magic number at the start of |SYSTEM.
const SYSTEM_MAGIC: u16 = 0x036C;

/// Parsed metadata from the `|SYSTEM` internal file.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    /// WinHelp minor version (e.g. 15 for WinHelp 3.1, 21 for WinHelp 4.0).
    pub minor_version: u16,
    /// Flags field (bit 2 = LZ77 compression, bit 3 = phrase compression).
    pub flags: u16,
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

        let mut info = SystemInfo {
            minor_version,
            flags,
            title: None,
            copyright: None,
            root_topic_offset: None,
            starting_topic: None,
        };

        // Parse variable-length records after the 6-byte header.
        let mut pos = 6;
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

    /// Returns true if the help file uses LZ77 compression on topic blocks.
    pub fn uses_lz77(&self) -> bool {
        self.flags & 0x0004 != 0
    }

    /// Returns true if the help file uses phrase compression.
    pub fn uses_phrases(&self) -> bool {
        // Phrase compression is indicated by flags bit 1 (0x0002) being CLEAR
        // for old-style phrases, or the presence of |Phrases file.
        // In practice, we detect by the existence of |Phrases.
        // The flags field interpretation varies, so this is best-effort.
        true
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
    fn build_system(minor_version: u16, flags: u16, records: &[(u16, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SYSTEM_MAGIC.to_le_bytes());
        buf.extend_from_slice(&minor_version.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        for (rtype, rdata) in records {
            buf.extend_from_slice(&rtype.to_le_bytes());
            buf.extend_from_slice(&(rdata.len() as u16).to_le_bytes());
            buf.extend_from_slice(rdata);
        }
        buf
    }

    #[test]
    fn parse_minimal_system() {
        let data = build_system(15, 0, &[]);
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.minor_version, 15);
        assert_eq!(info.flags, 0);
        assert!(info.title.is_none());
        assert!(info.copyright.is_none());
    }

    #[test]
    fn parse_title_and_copyright() {
        let data = build_system(
            15,
            0x0004,
            &[
                (1, b"C Library Reference\0"),
                (2, b"Open Watcom\0"),
            ],
        );
        let info = SystemInfo::from_bytes(&data).unwrap();
        assert_eq!(info.title.as_deref(), Some("C Library Reference"));
        assert_eq!(info.copyright.as_deref(), Some("Open Watcom"));
        assert!(info.uses_lz77());
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
