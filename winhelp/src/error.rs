/// Errors produced by the winhelp parser.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid HLP magic: expected 0x00035F3F, got 0x{0:08X}")]
    BadMagic(u32),

    #[error("invalid internal file header in '{name}': {detail}")]
    BadInternalFile { name: String, detail: String },

    #[error("internal file not found: '{0}'")]
    FileNotFound(String),

    #[error("decompression failed: {0}")]
    Decompression(String),

    #[error("parse error at offset {offset:#x}: {detail}")]
    Parse { offset: u64, detail: String },

    #[error("unresolved context hash: 0x{0:08X}")]
    UnresolvedHash(u32),
}
