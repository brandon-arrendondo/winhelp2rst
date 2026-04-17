//! Bitmap extraction and BMP header fixup.
//!
//! WinHelp stores embedded pictures as internal files (`|bmN`). The on-disk
//! format is usually MRB (multi-resolution bitmap, magic `lp`/`lP`) wrapping
//! one or more pictures — each of which may be a DIB, DDB, or metafile,
//! optionally RunLen- or LZ77-compressed. This module:
//!
//! 1. Detects MRB containers and unwraps the first DIB (`type=6`) or
//!    DDB (`type=5`) picture, producing a standard BMP (`ensure_bmp_header`
//!    handles the fallback case where the raw bytes are already a plain BMP
//!    missing only its file header).  DDB inputs synthesise a 2-entry
//!    monochrome palette and realign the 2-byte-aligned DDB scanlines to
//!    DIB's 4-byte-aligned stride, mirroring helpdeco splitmrb.c:468-487.
//! 2. Detects MRB containers wrapping a Windows Metafile (`type=8`) and
//!    reconstructs an Aldus Placeable Metafile (APM) header so the result is
//!    a self-contained `.wmf` file. Vector data is not rasterised — the
//!    downstream writer saves the bytes verbatim with a `.. image::`
//!    reference and a comment that the format is unconverted.
//! 3. Returns the synthesised bytes so the downstream RST writer can either
//!    re-encode (BMP → PNG via `image`) or persist (`.wmf`) directly.
//!
//! Reference: helpdeco/src/splitmrb.c (`main`, `decompress`, type=8 branch
//! at lines 511-573 for the APM-header reconstruction recipe).
//!
//! # SHG (Segmented Hypergraphics)
//!
//! An SHG file is an MRB picture that carries a non-zero `HotspotSize`.
//! After the pixel/metafile payload, the container appends a hotspot block
//! describing clickable regions (jump, popup, macro) overlaid on the
//! bitmap.  RST has no native image-map construct, so [`parse_shg`]
//! flattens the picture to its rendered bytes (BMP or WMF) and returns the
//! hotspot list separately — callers may surface the hotspot list as RST
//! comments, warnings, or simply discard it.
//!
//! Hotspot layout per `helpfile.txt:1329-1367`:
//!
//! ```text
//! u8  magic = 0x01
//! u16 num_hotspots
//! u32 macro_size
//! Hotspot[num_hotspots]   (15 bytes each: id0,id1,id2, x,y,w,h, hash)
//! u8  macro_data[macro_size]
//! { STRINGZ name; STRINGZ target; }[num_hotspots]
//! ```

use crate::container::HlpContainer;
use crate::decompress::lz77_decompress;
use crate::{Error, Result};

/// BMP file magic bytes ("BM").
const BMP_MAGIC: [u8; 2] = [0x42, 0x4D];

/// Size of the BITMAPFILEHEADER (14 bytes).
const BMP_FILE_HEADER_SIZE: usize = 14;

/// BITMAPINFOHEADER size field value (40 bytes) — the most common variant.
const BITMAPINFOHEADER_SIZE: u32 = 40;

/// MRB picture-type byte for a Device-Dependent Bitmap (DDB) — Windows 3.0
/// pre-DIB format with no in-file palette and 2-byte-aligned scanlines.
const MRB_TYPE_DDB: u8 = 5;
/// MRB picture-type byte for a Device-Independent Bitmap (DIB) — 4-byte-
/// aligned scanlines with an in-file BITMAPINFOHEADER-style palette.
const MRB_TYPE_DIB: u8 = 6;
/// MRB picture-type byte for a Windows Metafile picture.
const MRB_TYPE_METAFILE: u8 = 8;

/// Aldus Placeable Metafile magic, little-endian on disk: `D7 CD C6 9A`.
pub const APM_MAGIC: u32 = 0x9AC6_CDD7;
/// Length of the Aldus Placeable Metafile header (22 bytes).
const APM_HEADER_LEN: usize = 22;

/// Size of a single on-disk HOTSPOT record (helpdeco `splitmrb.c:327`).
const HOTSPOT_RECORD_LEN: usize = 15;

/// Sentinel byte that introduces the hotspot block (helpfile.txt:1329).
const HOTSPOT_BLOCK_MAGIC: u8 = 0x01;

/// Axis-aligned rectangle in pixel coordinates: `(x, y)` top-left, size `(w, h)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotspotRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Action triggered when a hotspot is clicked.
///
/// Decoded from the `(id0, id1, id2)` discriminator triple. Unknown triples
/// are preserved verbatim rather than rejected, so oddball files still
/// round-trip through [`parse_shg`] without losing rectangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotspotAction {
    /// `0xC8 0x00 0x00` — run a macro, visible box.
    MacroVisible,
    /// `0xCC 0x04 0x00` — run a macro, invisible.
    MacroInvisible,
    /// `0xE2 0x00 0x00` — pop up a topic, visible box.
    PopupJumpVisible,
    /// `0xE3 0x00 0x00` — jump to a topic, visible box.
    TopicJumpVisible,
    /// `0xE6 0x04 0x00` — pop up a topic, invisible.
    PopupJumpInvisible,
    /// `0xE7 0x04 0x00` — jump to a topic, invisible.
    TopicJumpInvisible,
    /// `0xEA 0x00 0x00` — pop up a topic in an external file, visible.
    ExternalPopupVisible,
    /// `0xEB 0x00 0x00` — jump to an external file / secondary window, visible.
    ExternalTopicVisible,
    /// `0xEE 0x04 0x00` — pop up a topic in an external file, invisible.
    ExternalPopupInvisible,
    /// `0xEF 0x04 0x00` — jump to an external file / secondary window, invisible.
    ExternalTopicInvisible,
    /// Unrecognised id triple — preserved verbatim for debugging.
    Unknown(u8, u8, u8),
}

impl HotspotAction {
    fn from_id(id0: u8, id1: u8, id2: u8) -> Self {
        match (id0, id1, id2) {
            (0xC8, 0x00, 0x00) => Self::MacroVisible,
            (0xCC, 0x04, 0x00) => Self::MacroInvisible,
            (0xE2, 0x00, 0x00) => Self::PopupJumpVisible,
            (0xE3, 0x00, 0x00) => Self::TopicJumpVisible,
            (0xE6, 0x04, 0x00) => Self::PopupJumpInvisible,
            (0xE7, 0x04, 0x00) => Self::TopicJumpInvisible,
            (0xEA, 0x00, 0x00) => Self::ExternalPopupVisible,
            (0xEB, 0x00, 0x00) => Self::ExternalTopicVisible,
            (0xEE, 0x04, 0x00) => Self::ExternalPopupInvisible,
            (0xEF, 0x04, 0x00) => Self::ExternalTopicInvisible,
            _ => Self::Unknown(id0, id1, id2),
        }
    }
}

/// One clickable region overlaid on an SHG picture.
///
/// Fields mirror the on-disk record plus the decoded name/target strings.
/// `target` is a context name for jumps/popups, a macro body for macro
/// hotspots, or `ContextName>Window@File` for external references — the
/// action variant indicates which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotspot {
    pub rect: HotspotRect,
    pub action: HotspotAction,
    pub hash: u32,
    pub name: String,
    pub target: String,
}

/// WinHelp RunLen byte-stream decoder.
///
/// Mirrors helpdeco splitmrb.c:141-162 (`derun`) driven by GetPackedByte-
/// style state.  The input is a stream of alternating control bytes and
/// data bytes:
///
/// * A control byte interpreted as `signed i8` whose **value is negative**
///   introduces a **literal run** of `|count|` data bytes that each copy
///   straight through.
/// * A control byte whose **value is non-negative** introduces a **packed
///   run** whose single following data byte is emitted `count` times.
///
/// The state machine is driven one *input* byte at a time.  After a run
/// ends, the next byte is a new control byte.
fn derun(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 2);
    let mut count: i8 = 0;
    for &b in input {
        let c = b;
        // Mirrors helpdeco's conditional exactly — keeping the nested
        // structure makes the state transitions obvious.
        if count & 0x7F != 0 {
            if (count as u8) & 0x80 != 0 {
                // Literal run: emit this byte, consume one slot.
                out.push(c);
                count = count.wrapping_add(-1i8);
            } else {
                // Packed run: emit `count` copies of this byte, reset.
                for _ in 0..(count as i32) {
                    out.push(c);
                }
                count = 0;
            }
        } else {
            // New control byte.
            count = c as i8;
        }
    }
    out
}

/// Extract a bitmap from the HLP container by internal filename.
///
/// Returns the image bytes as a self-contained standard BMP with a valid
/// BITMAPFILEHEADER. The source format may be MRB, a headerless DIB, or an
/// already-complete BMP — all three are normalised to a parseable BMP.
///
/// Returns `None` if the file doesn't exist in the container.
pub fn extract_bitmap(container: &HlpContainer, name: &str) -> Result<Option<Vec<u8>>> {
    let raw = match container.read_file(name) {
        Ok(data) => data,
        Err(Error::FileNotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };

    if raw.is_empty() {
        return Ok(Some(raw));
    }

    // Detect MRB (magic 'lp' 0x706C or 'lP' 0x506C — both match mask 0xDFFF == 0x506C).
    if raw.len() >= 2 {
        let sig = u16::from_le_bytes([raw[0], raw[1]]);
        if (sig & 0xDFFF) == 0x506C {
            if let Some(bmp) = mrb_to_bmp(&raw) {
                return Ok(Some(bmp));
            }
            if let Some(wmf) = mrb_to_wmf(&raw) {
                return Ok(Some(wmf));
            }
            // Fall through to raw bytes if MRB decode fails — callers can at
            // least save them verbatim for inspection.
            return Ok(Some(raw));
        }
    }

    // Standalone WMF stored without an MRB wrapper (rare in practice but
    // cheap to detect): both Aldus Placeable headers and bare METAHEADER
    // streams pass through unchanged so the writer can save them as `.wmf`.
    if is_wmf(&raw) {
        return Ok(Some(raw));
    }

    Ok(Some(ensure_bmp_header(&raw)))
}

/// Return `true` if `data` looks like a standalone Windows Metafile.
///
/// Recognises the Aldus Placeable Metafile magic (`D7 CD C6 9A`); bare
/// METAHEADER streams are not auto-detected because their leading bytes
/// (`01 00 09 00` for a memory metafile) are too generic to sniff safely.
pub fn is_wmf(data: &[u8]) -> bool {
    data.len() >= 4 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == APM_MAGIC
}

/// Decode an MRB container's first DIB (`type=6`) or DDB (`type=5`) picture
/// into a standalone BMP.
///
/// DIB pictures are passed through with their in-file palette and 4-byte-
/// aligned scanlines intact.  DDB pictures synthesise a 2-entry black/white
/// palette and realign the DDB's 2-byte scanlines to DIB's 4-byte stride —
/// only 1-bit monochrome DDBs are supported because no palette is recoverable
/// from the file for higher bit depths.
///
/// Returns `None` if the container is malformed, the first picture is an
/// unsupported type (metafile), or the picture uses a packing method the
/// variant doesn't support (DDB only permits `byPacked` 0 or 1).
///
/// Reference: helpdeco splitmrb.c lines 372-492.
pub fn mrb_to_bmp(data: &[u8]) -> Option<Vec<u8>> {
    let mut cur = Cursor::new(data);
    let _sig = cur.take_u16()?;
    let num_pictures = cur.take_u16()?;
    if num_pictures == 0 {
        return None;
    }
    // Pick the first picture. Most HLP content files embed only one DIB per
    // |bmN internal file; the other resolutions are used only when the
    // compiler needs to render multiple DPIs.
    let pic_offset = cur.take_u32()? as usize;
    let mut pic = Cursor::new(data.get(pic_offset..)?);

    let by_type = pic.take_u8()?;
    let by_packed = pic.take_u8()?;
    if by_type != MRB_TYPE_DIB && by_type != MRB_TYPE_DDB {
        // Metafile (8) is handled by `mrb_to_wmf`; anything else we can't
        // interpret, so callers can fall back to raw bytes.
        return None;
    }

    let _x_ppm = pic.take_cdword()?;
    let _y_ppm = pic.take_cdword()?;
    let planes = pic.take_cword()? as u16;
    let bit_count = pic.take_cword()? as u16;
    let width = pic.take_cdword()? as i32;
    let height = pic.take_cdword()? as i32;
    let clr_used = pic.take_cdword()?;
    let _clr_important = pic.take_cdword()?;
    let data_size = pic.take_cdword()? as usize;
    let _hotspot_size = pic.take_cdword()?;
    // `dwPictureOffset` and `dwHotspotOffset` follow. Both are plain u32
    // values. `dwPictureOffset` is unreliable in practice — helpdeco reads
    // and discards it — so we locate pixel data sequentially (palette,
    // then compressed pixels) instead.
    let _picture_offset = pic.take_u32_plain()?;
    let _hotspot_offset = pic.take_u32_plain()?;

    let payload_start = pic_offset + pic.pos();

    let (palette_bytes, pixels, colors) = match by_type {
        MRB_TYPE_DIB => {
            let colors = if bit_count <= 8 {
                let requested = clr_used as usize;
                if requested == 0 {
                    1usize << bit_count
                } else {
                    requested
                }
            } else {
                0
            };
            let palette_len = colors * 4;
            // Palette immediately follows the variable-length header.
            if payload_start + palette_len > data.len() {
                return None;
            }
            let palette = data[payload_start..payload_start + palette_len].to_vec();
            // Compressed pixel data lives immediately after the palette.
            let pixel_data_start = payload_start + palette_len;
            if pixel_data_start + data_size > data.len() {
                return None;
            }
            let compressed_pixels = &data[pixel_data_start..pixel_data_start + data_size];
            let pixels = decompress_packed(compressed_pixels, by_packed)?;
            (palette, pixels, colors)
        }
        MRB_TYPE_DDB => {
            // DDB is Windows 3.0's pre-DIB format: no in-file palette, and
            // scanlines are word-aligned instead of DWORD-aligned.  Only
            // 1-bit monochrome is realistically convertible — higher bit
            // depths have no recoverable color table.
            if bit_count != 1 {
                return None;
            }
            // Per helpdeco splitmrb.c:372, DDB supports raw (0) and RunLen
            // (1) packing only — LZ77 (bit 1) combinations are rejected.
            if by_packed & 0b10 != 0 {
                return None;
            }
            if payload_start + data_size > data.len() {
                return None;
            }
            let compressed_pixels = &data[payload_start..payload_start + data_size];
            let ddb_pixels = match by_packed & 0b1 {
                0 => compressed_pixels.to_vec(),
                1 => derun(compressed_pixels),
                _ => return None,
            };
            let dib_pixels = ddb_to_dib_pixels(&ddb_pixels, width, height, bit_count)?;
            // Synthesise a 2-entry monochrome palette (black, white) in
            // RGBQUAD order per helpdeco:457-465.
            let palette = vec![
                0x00, 0x00, 0x00, 0x00, // black
                0xFF, 0xFF, 0xFF, 0x00, // white
            ];
            (palette, dib_pixels, 2)
        }
        _ => return None,
    };

    // Assemble the output BMP: BITMAPFILEHEADER + BITMAPINFOHEADER + palette + pixels.
    let bmi_header_size = BITMAPINFOHEADER_SIZE as usize;
    let pixel_offset = BMP_FILE_HEADER_SIZE + bmi_header_size + palette_bytes.len();
    let total_size = pixel_offset + pixels.len();

    let mut out = Vec::with_capacity(total_size);
    // BITMAPFILEHEADER.
    out.extend_from_slice(&BMP_MAGIC);
    out.extend_from_slice(&(total_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    // BITMAPINFOHEADER.
    out.extend_from_slice(&BITMAPINFOHEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&planes.to_le_bytes());
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&(colors as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
                                                // Palette + pixels.
    out.extend_from_slice(&palette_bytes);
    out.extend_from_slice(&pixels);

    Some(out)
}

/// Realign a DDB pixel buffer to a DIB's 4-byte row stride.
///
/// DDB scanlines are aligned to 2-byte (WORD) boundaries; DIB scanlines
/// are aligned to 4-byte (DWORD) boundaries.  Each DDB row is copied
/// verbatim and then padded with `0x20` bytes to reach the DIB stride,
/// matching helpdeco's `fwrite("    ", pad, 1, fTarget)` at splitmrb.c:486.
///
/// The pad byte value is unused by any BMP renderer — it sits in the
/// alignment gutter — but matching helpdeco keeps the output bitwise
/// identical for testability.
fn ddb_to_dib_pixels(ddb: &[u8], width: i32, height: i32, bit_count: u16) -> Option<Vec<u8>> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let bits_per_row = (width as u64).checked_mul(bit_count as u64)?;
    let ddb_stride = (bits_per_row.div_ceil(16) * 2) as usize;
    let dib_stride = (bits_per_row.div_ceil(32) * 4) as usize;
    let pad = dib_stride - ddb_stride;
    let rows = height as usize;
    if ddb.len() < ddb_stride.checked_mul(rows)? {
        return None;
    }
    let mut out = Vec::with_capacity(dib_stride * rows);
    for row in 0..rows {
        let start = row * ddb_stride;
        out.extend_from_slice(&ddb[start..start + ddb_stride]);
        out.extend(std::iter::repeat_n(0x20u8, pad));
    }
    Some(out)
}

/// Decode an MRB container's first metafile picture (type=8) into a
/// self-contained `.wmf` file with an Aldus Placeable Metafile header.
///
/// Returns `None` if the container is malformed, the first picture isn't a
/// metafile, or the picture's payload uses a packing method we don't
/// support.  Hotspot data is discarded — RST has no equivalent of WinHelp
/// image maps; see Task 18 (SHG) for the related raster case.
///
/// Reference: helpdeco/src/splitmrb.c lines 511-573.
pub fn mrb_to_wmf(data: &[u8]) -> Option<Vec<u8>> {
    let mut cur = Cursor::new(data);
    let _sig = cur.take_u16()?;
    let num_pictures = cur.take_u16()?;
    if num_pictures == 0 {
        return None;
    }
    let pic_offset = cur.take_u32()? as usize;
    let mut pic = Cursor::new(data.get(pic_offset..)?);

    let by_type = pic.take_u8()?;
    let by_packed = pic.take_u8()?;
    if by_type != MRB_TYPE_METAFILE {
        return None;
    }

    // Metafile picture header (helpdeco splitmrb.c:515-524). The mapping
    // mode and the trailing wInch/dwReserved fields are stored in the
    // source but unused by the standard APM header (we use 2540 twips/inch,
    // matching helpdeco), so read-and-discard.
    let _mapping_mode = pic.take_cword()?;
    let width = pic.take_u16()? as i16; // rcBBox.right
    let height = pic.take_u16()? as i16; // rcBBox.bottom
    let _wcaller_inch = pic.take_cdword()?;
    let data_size = pic.take_cdword()? as usize;
    let _hotspot_size = pic.take_cdword()?;
    let _picture_offset = pic.take_u32_plain()?;
    let _hotspot_offset = pic.take_u32_plain()?;

    let payload_start = pic_offset + pic.pos();
    if payload_start + data_size > data.len() {
        return None;
    }
    let compressed = &data[payload_start..payload_start + data_size];
    let metafile_bytes = decompress_packed(compressed, by_packed)?;

    Some(build_apm_wmf(width, height, &metafile_bytes))
}

/// Parse an SHG / MRB-with-hotspots file into a flattened image plus the
/// decoded hotspot list.
///
/// The image bytes are whatever [`extract_bitmap`] would have produced for
/// the same input — a standalone BMP for DIB pictures, a self-contained
/// WMF (APM-wrapped) for metafile pictures.  Hotspot records are decoded
/// per `helpfile.txt:1329-1367`; unknown id triples are preserved as
/// [`HotspotAction::Unknown`] rather than rejected.
///
/// Returns `None` if the input isn't an MRB container, the first picture
/// is an unsupported variant, or the hotspot block is truncated/malformed.
/// A picture with `HotspotSize == 0` yields `Some((bitmap, vec![]))`.
pub fn parse_shg(data: &[u8]) -> Option<(Vec<u8>, Vec<Hotspot>)> {
    // Sniff MRB magic — mirrors the check in `extract_bitmap`.
    if data.len() < 2 {
        return None;
    }
    let sig = u16::from_le_bytes([data[0], data[1]]);
    if (sig & 0xDFFF) != 0x506C {
        return None;
    }

    let bitmap = mrb_to_bmp(data).or_else(|| mrb_to_wmf(data))?;
    let hotspots = extract_hotspot_bytes(data)
        .map(|bytes| parse_hotspot_block(bytes).unwrap_or_default())
        .unwrap_or_default();
    Some((bitmap, hotspots))
}

/// Locate and return the raw hotspot block bytes for the first picture of
/// an MRB container, if any.  Returns `None` when the file is malformed or
/// has no hotspots.
///
/// Both DIB (type=6) and metafile (type=8) layouts store the hotspot block
/// after the pixel/metafile payload; we compute its position sequentially
/// rather than trusting `dwHotspotOffset` (which we already know from the
/// existing `mrb_to_bmp` comments to be unreliable in practice).
fn extract_hotspot_bytes(data: &[u8]) -> Option<&[u8]> {
    let mut cur = Cursor::new(data);
    let _sig = cur.take_u16()?;
    let num_pictures = cur.take_u16()?;
    if num_pictures == 0 {
        return None;
    }
    let pic_offset = cur.take_u32()? as usize;
    let mut pic = Cursor::new(data.get(pic_offset..)?);

    let by_type = pic.take_u8()?;
    let _by_packed = pic.take_u8()?;

    let (data_size, hotspot_size) = match by_type {
        MRB_TYPE_DIB | MRB_TYPE_DDB => {
            let _x_ppm = pic.take_cdword()?;
            let _y_ppm = pic.take_cdword()?;
            let _planes = pic.take_cword()?;
            let bit_count = pic.take_cword()? as u16;
            let _width = pic.take_cdword()?;
            let _height = pic.take_cdword()?;
            let clr_used = pic.take_cdword()?;
            let _clr_important = pic.take_cdword()?;
            let data_size = pic.take_cdword()? as usize;
            let hotspot_size = pic.take_cdword()? as usize;
            let _picture_offset = pic.take_u32_plain()?;
            let _hotspot_offset = pic.take_u32_plain()?;
            // DDB has no in-file palette; DIB's palette is `colors * 4`
            // bytes between the header and the compressed pixel data.
            let palette_bytes = if by_type == MRB_TYPE_DIB {
                let colors = if bit_count <= 8 {
                    if clr_used == 0 {
                        1usize << bit_count
                    } else {
                        clr_used as usize
                    }
                } else {
                    0
                };
                colors * 4
            } else {
                0
            };
            (data_size + palette_bytes, hotspot_size)
        }
        MRB_TYPE_METAFILE => {
            let _mapping_mode = pic.take_cword()?;
            let _width = pic.take_u16()?;
            let _height = pic.take_u16()?;
            let _wcaller_inch = pic.take_cdword()?;
            let data_size = pic.take_cdword()? as usize;
            let hotspot_size = pic.take_cdword()? as usize;
            let _picture_offset = pic.take_u32_plain()?;
            let _hotspot_offset = pic.take_u32_plain()?;
            (data_size, hotspot_size)
        }
        _ => return None,
    };

    if hotspot_size == 0 {
        return None;
    }
    let hotspot_start = pic_offset + pic.pos() + data_size;
    data.get(hotspot_start..hotspot_start + hotspot_size)
}

/// Decode a hotspot block (the bytes pointed to by `dwHotspotOffset`).
///
/// Returns `None` if the block is truncated, the leading sentinel byte is
/// wrong, or a string is unterminated.  On success the vector contains one
/// [`Hotspot`] per on-disk record in source order.
fn parse_hotspot_block(block: &[u8]) -> Option<Vec<Hotspot>> {
    // Header: u8 magic, u16 num_hotspots, u32 macro_size.
    if block.len() < 7 {
        return None;
    }
    if block[0] != HOTSPOT_BLOCK_MAGIC {
        return None;
    }
    let num = u16::from_le_bytes([block[1], block[2]]) as usize;
    let macro_size = u32::from_le_bytes([block[3], block[4], block[5], block[6]]) as usize;

    let records_start = 7usize;
    let records_end = records_start.checked_add(num.checked_mul(HOTSPOT_RECORD_LEN)?)?;
    if records_end > block.len() {
        return None;
    }
    let strings_start = records_end.checked_add(macro_size)?;
    if strings_start > block.len() {
        return None;
    }

    // Walk the record array once; strings follow the macro block and are
    // consumed sequentially from a shared cursor.
    let mut raw = Vec::with_capacity(num);
    for i in 0..num {
        let r = &block[records_start + i * HOTSPOT_RECORD_LEN..][..HOTSPOT_RECORD_LEN];
        let id0 = r[0];
        let id1 = r[1];
        let id2 = r[2];
        let x = u16::from_le_bytes([r[3], r[4]]);
        let y = u16::from_le_bytes([r[5], r[6]]);
        let w = u16::from_le_bytes([r[7], r[8]]);
        let h = u16::from_le_bytes([r[9], r[10]]);
        let hash = u32::from_le_bytes([r[11], r[12], r[13], r[14]]);
        raw.push((id0, id1, id2, x, y, w, h, hash));
    }

    let mut cur = strings_start;
    let mut out = Vec::with_capacity(num);
    for (id0, id1, id2, x, y, w, h, hash) in raw {
        let name = take_stringz(block, &mut cur)?;
        let target = take_stringz(block, &mut cur)?;
        out.push(Hotspot {
            rect: HotspotRect { x, y, w, h },
            action: HotspotAction::from_id(id0, id1, id2),
            hash,
            name,
            target,
        });
    }
    Some(out)
}

/// Read a null-terminated string from `block` starting at `*cursor`,
/// advancing the cursor past the terminator.  Non-UTF-8 bytes are replaced
/// per [`String::from_utf8_lossy`] so corrupt strings don't poison the
/// rest of the parse.
fn take_stringz(block: &[u8], cursor: &mut usize) -> Option<String> {
    let start = *cursor;
    let rest = block.get(start..)?;
    let nul = rest.iter().position(|&b| b == 0)?;
    let s = String::from_utf8_lossy(&rest[..nul]).into_owned();
    *cursor = start + nul + 1;
    Some(s)
}

/// Decompress an MRB picture payload according to the `byPacked` flags.
///
/// Four packing methods per HELPFILE.TXT:1283-1309:
///   0 = raw, 1 = RunLen, 2 = LZ77, 3 = LZ77-then-RunLen.
///
/// When both flags are set, LZ77 is applied first and its output is then
/// fed through the RunLen decoder — matching helpdeco splitmrb.c:164-218
/// (`decompress` — `method & 2` picks LZ77, `method & 1` picks RunLen, and
/// the two can combine).  Returns `None` for any flag combination outside
/// the documented 2-bit field.
fn decompress_packed(input: &[u8], by_packed: u8) -> Option<Vec<u8>> {
    match by_packed & 0b11 {
        0 => Some(input.to_vec()),
        1 => Some(derun(input)),
        2 => lz77_decompress(input).ok(),
        3 => {
            let lz = lz77_decompress(input).ok()?;
            Some(derun(&lz))
        }
        _ => None,
    }
}

/// Prepend a 22-byte Aldus Placeable Metafile (APM) header to a raw WMF
/// payload so the result is a self-contained `.wmf` file.
///
/// The bounding rectangle is taken from the MRB metafile header; `wInch`
/// is fixed at 2540 twips/inch matching helpdeco's choice (splitmrb.c:547).
/// The 16-bit checksum is the XOR of the first ten little-endian words of
/// the header (the 20 bytes preceding the checksum slot).
fn build_apm_wmf(width: i16, height: i16, metafile: &[u8]) -> Vec<u8> {
    let mut header = [0u8; APM_HEADER_LEN];
    // dwKey
    header[0..4].copy_from_slice(&APM_MAGIC.to_le_bytes());
    // hMF (handle) — zero on disk
    header[4..6].copy_from_slice(&0u16.to_le_bytes());
    // rcBBox: left=0, top=0, right=width, bottom=height (4 i16 LE)
    header[6..8].copy_from_slice(&0i16.to_le_bytes());
    header[8..10].copy_from_slice(&0i16.to_le_bytes());
    header[10..12].copy_from_slice(&width.to_le_bytes());
    header[12..14].copy_from_slice(&height.to_le_bytes());
    // wInch — twips/inch
    header[14..16].copy_from_slice(&2540u16.to_le_bytes());
    // dwReserved — must be zero
    header[16..20].copy_from_slice(&0u32.to_le_bytes());

    // wChecksum — XOR of the first 10 LE words.
    let mut checksum: u16 = 0;
    for i in 0..10 {
        let off = i * 2;
        checksum ^= u16::from_le_bytes([header[off], header[off + 1]]);
    }
    header[20..22].copy_from_slice(&checksum.to_le_bytes());

    let mut out = Vec::with_capacity(APM_HEADER_LEN + metafile.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(metafile);
    out
}

/// Minimal byte cursor that mirrors helpdeco's `GetCWord` / `GetCDWord`
/// variable-length integer decoders.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn take_u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn take_u16(&mut self) -> Option<u16> {
        let lo = *self.data.get(self.pos)?;
        let hi = *self.data.get(self.pos + 1)?;
        self.pos += 2;
        Some(u16::from_le_bytes([lo, hi]))
    }

    fn take_u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes([
            *self.data.get(self.pos)?,
            *self.data.get(self.pos + 1)?,
            *self.data.get(self.pos + 2)?,
            *self.data.get(self.pos + 3)?,
        ]))
        .inspect(|_| self.pos += 4)
    }

    /// Plain u32 read without advancing via inspect-trick.
    fn take_u32_plain(&mut self) -> Option<u32> {
        self.take_u32()
    }

    /// `GetCWord`: 1-byte form `(b >> 1)` when bit 0 = 0, else 2-byte form
    /// `((next_byte << 8 | b) >> 1)`.
    fn take_cword(&mut self) -> Option<u32> {
        let b = self.take_u8()?;
        if b & 1 == 0 {
            Some((b as u32) >> 1)
        } else {
            let n = self.take_u8()?;
            Some((((n as u32) << 8) | (b as u32)) >> 1)
        }
    }

    /// `GetCDWord`: read a u16 first; if bit 0 = 1 read a second u16 and
    /// combine into a u32 before shifting right by 1.
    fn take_cdword(&mut self) -> Option<u32> {
        let w = self.take_u16()?;
        if w & 1 == 0 {
            Some((w as u32) >> 1)
        } else {
            let w2 = self.take_u16()?;
            Some((((w2 as u32) << 16) | (w as u32)) >> 1)
        }
    }
}

/// Ensure the BMP data has a valid BITMAPFILEHEADER.
///
/// If the data already starts with "BM", returns it as-is. Otherwise,
/// checks for a BITMAPINFOHEADER (u32 size == 40) and prepends the
/// missing file header.
pub fn ensure_bmp_header(data: &[u8]) -> Vec<u8> {
    // Already has BM header.
    if data.len() >= 2 && data[0..2] == BMP_MAGIC {
        return data.to_vec();
    }

    // Check if it starts with a BITMAPINFOHEADER (first u32 is the header size).
    if data.len() >= 4 {
        let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if header_size == BITMAPINFOHEADER_SIZE
            || header_size == 12
            || header_size == 108
            || header_size == 124
        {
            return prepend_bmp_file_header(data);
        }
    }

    // Unknown format — return as-is.
    data.to_vec()
}

/// Prepend a BITMAPFILEHEADER to raw BITMAPINFOHEADER + pixel data.
fn prepend_bmp_file_header(data: &[u8]) -> Vec<u8> {
    let total_size = (BMP_FILE_HEADER_SIZE + data.len()) as u32;

    // Calculate pixel data offset: BITMAPFILEHEADER + BITMAPINFOHEADER + palette.
    let info_header_size = if data.len() >= 4 {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        BITMAPINFOHEADER_SIZE
    };

    // For BITMAPINFOHEADER, palette size depends on bits-per-pixel and color count.
    let palette_size = compute_palette_size(data, info_header_size);
    let pixel_offset = (BMP_FILE_HEADER_SIZE as u32) + info_header_size + palette_size;

    let mut bmp = Vec::with_capacity(total_size as usize);

    // BITMAPFILEHEADER (14 bytes).
    bmp.extend_from_slice(&BMP_MAGIC); // bfType = "BM"
    bmp.extend_from_slice(&total_size.to_le_bytes()); // bfSize
    bmp.extend_from_slice(&0u16.to_le_bytes()); // bfReserved1
    bmp.extend_from_slice(&0u16.to_le_bytes()); // bfReserved2
    bmp.extend_from_slice(&pixel_offset.to_le_bytes()); // bfOffBits

    // Append the original data (info header + palette + pixels).
    bmp.extend_from_slice(data);

    bmp
}

/// Compute the palette size in bytes from a BITMAPINFOHEADER.
fn compute_palette_size(data: &[u8], info_header_size: u32) -> u32 {
    if info_header_size == 12 {
        // BITMAPCOREHEADER: bits_per_pixel at offset 10 (u16).
        if data.len() >= 12 {
            let bpp = u16::from_le_bytes([data[10], data[11]]);
            if bpp <= 8 {
                return (1u32 << bpp) * 3; // RGB triples, no padding
            }
        }
        return 0;
    }

    // BITMAPINFOHEADER (40 bytes): biClrUsed at offset 32, biBitCount at offset 14.
    if data.len() >= 36 {
        let bpp = u16::from_le_bytes([data[14], data[15]]);
        let clr_used = u32::from_le_bytes([data[32], data[33], data[34], data[35]]);

        if bpp <= 8 {
            let colors = if clr_used > 0 { clr_used } else { 1u32 << bpp };
            return colors * 4; // RGBQUAD entries
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid BMP with BITMAPFILEHEADER.
    fn make_full_bmp() -> Vec<u8> {
        let mut bmp = Vec::new();
        // BITMAPFILEHEADER (14 bytes).
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&70u32.to_le_bytes()); // file size
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel offset

        // BITMAPINFOHEADER (40 bytes).
        bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
        bmp.extend_from_slice(&2i32.to_le_bytes()); // width
        bmp.extend_from_slice(&2i32.to_le_bytes()); // height
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bpp (24-bit)
        bmp.extend_from_slice(&0u32.to_le_bytes()); // compression
        bmp.extend_from_slice(&16u32.to_le_bytes()); // image size
        bmp.extend_from_slice(&0i32.to_le_bytes()); // x ppm
        bmp.extend_from_slice(&0i32.to_le_bytes()); // y ppm
        bmp.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bmp.extend_from_slice(&0u32.to_le_bytes()); // important colors

        // Pixel data: 2x2 24-bit (each row padded to 4 bytes).
        bmp.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        bmp.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        bmp
    }

    /// Build a BMP missing the BITMAPFILEHEADER (starts with BITMAPINFOHEADER).
    fn make_headerless_bmp() -> Vec<u8> {
        let full = make_full_bmp();
        full[BMP_FILE_HEADER_SIZE..].to_vec()
    }

    #[test]
    fn full_bmp_returned_unchanged() {
        let full = make_full_bmp();
        let result = ensure_bmp_header(&full);
        assert_eq!(result[0..2], BMP_MAGIC);
        assert_eq!(result.len(), full.len());
    }

    #[test]
    fn headerless_bmp_gets_header_prepended() {
        let headerless = make_headerless_bmp();
        let result = ensure_bmp_header(&headerless);

        // Should now start with "BM".
        assert_eq!(result[0..2], BMP_MAGIC);
        // Should be 14 bytes larger.
        assert_eq!(result.len(), headerless.len() + BMP_FILE_HEADER_SIZE);
        // Original data should follow the header.
        assert_eq!(&result[BMP_FILE_HEADER_SIZE..], &headerless[..]);
    }

    #[test]
    fn pixel_offset_correct_for_24bit() {
        let headerless = make_headerless_bmp();
        let result = ensure_bmp_header(&headerless);

        // For 24-bit BMP with no palette: offset = 14 (file header) + 40 (info header) = 54.
        let pixel_offset = u32::from_le_bytes([result[10], result[11], result[12], result[13]]);
        assert_eq!(pixel_offset, 54);
    }

    #[test]
    fn palette_size_for_8bit() {
        // Build a fake 8-bit BITMAPINFOHEADER.
        let mut data = vec![0u8; 40];
        // header size = 40
        data[0..4].copy_from_slice(&40u32.to_le_bytes());
        // biBitCount = 8 at offset 14
        data[14..16].copy_from_slice(&8u16.to_le_bytes());
        // biClrUsed = 0 (means 2^8 = 256 colors)
        data[32..36].copy_from_slice(&0u32.to_le_bytes());

        let palette = compute_palette_size(&data, 40);
        assert_eq!(palette, 256 * 4); // 256 RGBQUAD entries
    }

    #[test]
    fn palette_size_for_24bit() {
        let mut data = vec![0u8; 40];
        data[0..4].copy_from_slice(&40u32.to_le_bytes());
        data[14..16].copy_from_slice(&24u16.to_le_bytes());
        data[32..36].copy_from_slice(&0u32.to_le_bytes());

        let palette = compute_palette_size(&data, 40);
        assert_eq!(palette, 0); // No palette for 24-bit
    }

    #[test]
    fn empty_data_returned_as_is() {
        let result = ensure_bmp_header(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn unknown_format_returned_as_is() {
        let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic
        let result = ensure_bmp_header(&data);
        assert_eq!(result, data);
    }

    // -- RunLen (derun) decoder tests --

    #[test]
    fn derun_packed_run_expands_count_copies() {
        // Control byte 5 (positive) → next byte is data, emitted 5 times.
        let out = derun(&[5, b'A']);
        assert_eq!(out, b"AAAAA");
    }

    #[test]
    fn derun_literal_run_copies_bytes_through() {
        // Control 0x83 as i8 is -125 (0x80 | 3), meaning a literal run of 3.
        let out = derun(&[0x83, b'X', b'Y', b'Z']);
        assert_eq!(out, b"XYZ");
    }

    #[test]
    fn derun_mixed_packed_and_literal_runs() {
        // Packed 3 × 'A', then literal 2 × ['B', 'C'], then packed 2 × 'D'.
        // Encoding:
        //   control=3, data='A'       → "AAA"
        //   control=0x82 (literal 2), 'B', 'C' → "BC"
        //   control=2, data='D'       → "DD"
        let out = derun(&[3, b'A', 0x82, b'B', b'C', 2, b'D']);
        assert_eq!(out, b"AAABCDD");
    }

    #[test]
    fn derun_zero_control_consumes_byte_and_emits_nothing() {
        // Control 0 degenerates to "reset": next byte becomes new control.
        let out = derun(&[0, 0, 3, b'Z']);
        assert_eq!(out, b"ZZZ");
    }

    #[test]
    fn derun_empty_input() {
        assert!(derun(&[]).is_empty());
    }

    // -- WMF / metafile (MRB type=8) tests --

    /// Build a synthetic MRB type=8 metafile with the given packing flag
    /// and pre-encoded payload.  The picture's bbox is fixed at 200×100
    /// to make the APM-header checksum predictable across tests.
    fn make_mrb_metafile(by_packed: u8, payload: &[u8]) -> Vec<u8> {
        // Picture header sizes: by_type+by_packed (2) + cword mapping_mode (1)
        // + width (2) + height (2) + cdword wcaller_inch (2) + cdword
        // data_size (2) + cdword hotspot_size (2) + dwPictureOffset (4) +
        // dwHotspotOffset (4) = 21 bytes.
        let mut mrb = Vec::new();
        // sig 'lP' + 1 picture + offset
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes()); // pic_offset = 8 (right after this)

        // by_type=8 metafile, by_packed=<flag>
        mrb.push(MRB_TYPE_METAFILE);
        mrb.push(by_packed);
        // mapping_mode: cword(0) → single byte 0x00
        mrb.push(0x00);
        // width=200, height=100 (raw u16 LE)
        mrb.extend_from_slice(&200u16.to_le_bytes());
        mrb.extend_from_slice(&100u16.to_le_bytes());
        // wcaller_inch: cdword(0) → 2 bytes
        mrb.extend_from_slice(&0u16.to_le_bytes());
        // data_size: cdword(N) where N = payload.len(); CDWord encodes as
        // value<<1 (LSB=0) in 16 bits when fits.
        let ds_encoded = (payload.len() as u16) << 1;
        mrb.extend_from_slice(&ds_encoded.to_le_bytes());
        // hotspot_size: cdword(0)
        mrb.extend_from_slice(&0u16.to_le_bytes());
        // dwPictureOffset, dwHotspotOffset
        mrb.extend_from_slice(&0u32.to_le_bytes());
        mrb.extend_from_slice(&0u32.to_le_bytes());
        // payload
        mrb.extend_from_slice(payload);
        mrb
    }

    /// Compute the expected APM-header bytes for a 200×100 metafile —
    /// matches the math in `build_apm_wmf` so tests can verify the header
    /// independently of the production formula.
    fn expected_apm_header_200x100() -> [u8; APM_HEADER_LEN] {
        let mut h = [0u8; APM_HEADER_LEN];
        h[0..4].copy_from_slice(&APM_MAGIC.to_le_bytes());
        // hMF, rcBBox.left, rcBBox.top all zero (already)
        h[10..12].copy_from_slice(&200i16.to_le_bytes());
        h[12..14].copy_from_slice(&100i16.to_le_bytes());
        h[14..16].copy_from_slice(&2540u16.to_le_bytes());
        // dwReserved = 0 (already)
        // checksum = XOR of first 10 LE words
        let mut cs: u16 = 0;
        for i in 0..10 {
            let off = i * 2;
            cs ^= u16::from_le_bytes([h[off], h[off + 1]]);
        }
        h[20..22].copy_from_slice(&cs.to_le_bytes());
        h
    }

    #[test]
    fn mrb_to_wmf_decodes_raw_packed_payload() {
        let payload = b"\x01\x02\x03\x04";
        let mrb = make_mrb_metafile(0, payload);

        let wmf = mrb_to_wmf(&mrb).expect("metafile decode should succeed");

        // Header + payload length.
        assert_eq!(wmf.len(), APM_HEADER_LEN + payload.len());
        // First four bytes are the Aldus magic.
        assert_eq!(
            u32::from_le_bytes([wmf[0], wmf[1], wmf[2], wmf[3]]),
            APM_MAGIC,
        );
        // Header matches independent recomputation.
        assert_eq!(&wmf[..APM_HEADER_LEN], &expected_apm_header_200x100()[..]);
        // Payload follows verbatim for raw packing.
        assert_eq!(&wmf[APM_HEADER_LEN..], payload);
    }

    #[test]
    fn mrb_to_wmf_decompresses_runlen_payload() {
        // RunLen control=5 + data='A' → five 'A' bytes.
        let payload = [5u8, b'A'];
        let mrb = make_mrb_metafile(1, &payload);

        let wmf = mrb_to_wmf(&mrb).expect("RunLen metafile decode should succeed");

        assert_eq!(&wmf[..4], &APM_MAGIC.to_le_bytes());
        assert_eq!(&wmf[APM_HEADER_LEN..], b"AAAAA");
    }

    #[test]
    fn mrb_to_wmf_returns_none_for_dib_picture() {
        // A DIB-typed MRB should not be picked up by the WMF path — that's
        // what `mrb_to_bmp` is for.  Build a degenerate type=6 picture.
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_DIB); // type=6 DIB, not metafile
        mrb.push(0); // raw packed
                     // Pad with junk; we only care that the type discriminator is wrong.
        mrb.extend_from_slice(&[0u8; 64]);

        assert!(mrb_to_wmf(&mrb).is_none());
    }

    #[test]
    fn is_wmf_recognises_aldus_magic() {
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&APM_MAGIC.to_le_bytes());
        assert!(is_wmf(&data));
    }

    #[test]
    fn is_wmf_rejects_other_signatures() {
        // BMP magic
        assert!(!is_wmf(&[b'B', b'M', 0, 0]));
        // PNG magic
        assert!(!is_wmf(&[0x89, 0x50, 0x4E, 0x47]));
        // Empty / truncated
        assert!(!is_wmf(&[]));
        assert!(!is_wmf(&[0xD7, 0xCD]));
    }

    #[test]
    fn extract_bitmap_round_trips_metafile_through_mrb_dispatch() {
        // End-to-end: feed a synthetic MRB metafile through the same
        // dispatch path the real container code uses, by constructing a
        // fake container in-memory.
        let mrb = make_mrb_metafile(0, b"\xDE\xAD\xBE\xEF");
        // Skip the container plumbing — exercise the helper composition.
        let wmf = mrb_to_wmf(&mrb).unwrap();
        assert!(is_wmf(&wmf));
        assert_eq!(&wmf[APM_HEADER_LEN..], b"\xDE\xAD\xBE\xEF");
    }

    // -- SHG (hotspot) parsing tests --

    /// One hotspot record's worth of test input: id bytes, geometry, hash,
    /// and the two trailing strings (`name`, `target`).
    struct HotspotFixture {
        id0: u8,
        id1: u8,
        id2: u8,
        rect: HotspotRect,
        hash: u32,
        name: &'static str,
        target: &'static str,
    }

    /// Build a synthetic on-disk hotspot block (the bytes pointed to by
    /// `dwHotspotOffset`). `macro_data` is inserted verbatim between the
    /// record array and the string pairs to mirror the real layout.
    fn build_hotspot_block(spots: &[HotspotFixture], macro_data: &[u8]) -> Vec<u8> {
        let mut block = Vec::new();
        block.push(HOTSPOT_BLOCK_MAGIC);
        block.extend_from_slice(&(spots.len() as u16).to_le_bytes());
        block.extend_from_slice(&(macro_data.len() as u32).to_le_bytes());
        for s in spots {
            block.push(s.id0);
            block.push(s.id1);
            block.push(s.id2);
            block.extend_from_slice(&s.rect.x.to_le_bytes());
            block.extend_from_slice(&s.rect.y.to_le_bytes());
            block.extend_from_slice(&s.rect.w.to_le_bytes());
            block.extend_from_slice(&s.rect.h.to_le_bytes());
            block.extend_from_slice(&s.hash.to_le_bytes());
        }
        block.extend_from_slice(macro_data);
        for s in spots {
            block.extend_from_slice(s.name.as_bytes());
            block.push(0);
            block.extend_from_slice(s.target.as_bytes());
            block.push(0);
        }
        block
    }

    /// Variant of `make_mrb_metafile` that appends a hotspot block.  The
    /// metafile payload is stored raw (`by_packed = 0`) so the test has a
    /// predictable on-disk layout; the hotspot block lives immediately
    /// after the payload (sequential positioning, matching `parse_shg`).
    fn make_mrb_metafile_with_hotspots(payload: &[u8], hotspot_block: &[u8]) -> Vec<u8> {
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_METAFILE);
        mrb.push(0); // by_packed = 0 raw
        mrb.push(0x00); // mapping_mode cword(0)
        mrb.extend_from_slice(&200u16.to_le_bytes()); // width
        mrb.extend_from_slice(&100u16.to_le_bytes()); // height
        mrb.extend_from_slice(&0u16.to_le_bytes()); // wcaller_inch cdword(0)
        let ds_encoded = (payload.len() as u16) << 1;
        mrb.extend_from_slice(&ds_encoded.to_le_bytes()); // data_size cdword
        let hs_encoded = (hotspot_block.len() as u16) << 1;
        mrb.extend_from_slice(&hs_encoded.to_le_bytes()); // hotspot_size cdword
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwPictureOffset
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwHotspotOffset (unused)
        mrb.extend_from_slice(payload);
        mrb.extend_from_slice(hotspot_block);
        mrb
    }

    /// Build a synthetic 24-bit raw-packed MRB DIB: 1×1 red pixel, 4-byte
    /// row (3 BGR bytes + 1 pad), no palette. `hotspot_block` is appended
    /// immediately after the pixel data.
    fn make_mrb_dib_with_hotspots(hotspot_block: &[u8]) -> Vec<u8> {
        // 1×1 24-bit pixel row: BB GG RR then 1 pad byte = 4 bytes.
        let pixels: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_DIB);
        mrb.push(0); // by_packed = 0 raw

        // All CWord / CDWord values encoded LSB=0 → one or two bytes each.
        // CDWord(0) = u16 0x0000 (2 bytes). CWord(0) = u8 0x00.
        mrb.extend_from_slice(&0u16.to_le_bytes()); // x_ppm cdword
        mrb.extend_from_slice(&0u16.to_le_bytes()); // y_ppm cdword
        mrb.push(0x02); // planes cword(1) → (1<<1)|0 = 0x02
        mrb.push(24 << 1); // bit_count cword(24) = 0x30
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // width cdword(1) = 2
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // height cdword(1) = 2
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_used cdword(0)
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_important cdword(0)
        mrb.extend_from_slice(&((pixels.len() as u16) << 1).to_le_bytes()); // data_size
        mrb.extend_from_slice(&((hotspot_block.len() as u16) << 1).to_le_bytes()); // hotspot_size
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwPictureOffset
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwHotspotOffset
                                                    // No palette for 24-bit.
        mrb.extend_from_slice(&pixels);
        mrb.extend_from_slice(hotspot_block);
        mrb
    }

    #[test]
    fn parse_shg_extracts_metafile_and_hotspots() {
        let spots = [
            HotspotFixture {
                id0: 0xE3,
                id1: 0x00,
                id2: 0x00,
                rect: HotspotRect {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 40,
                },
                hash: 0xDEAD_BEEF,
                name: "spot1",
                target: "TopicName",
            },
            HotspotFixture {
                id0: 0xC8,
                id1: 0x00,
                id2: 0x00,
                rect: HotspotRect {
                    x: 5,
                    y: 6,
                    w: 7,
                    h: 8,
                },
                hash: 1,
                name: "spot2",
                target: "JumpContents()",
            },
        ];
        let block = build_hotspot_block(&spots, b"JumpContents()\0");
        let mrb = make_mrb_metafile_with_hotspots(b"\xAA\xBB\xCC\xDD", &block);

        let (wmf, hotspots) = parse_shg(&mrb).expect("SHG parse succeeds");

        assert!(is_wmf(&wmf));
        assert_eq!(&wmf[APM_HEADER_LEN..], b"\xAA\xBB\xCC\xDD");
        assert_eq!(hotspots.len(), 2);
        assert_eq!(hotspots[0].action, HotspotAction::TopicJumpVisible);
        assert_eq!(
            hotspots[0].rect,
            HotspotRect {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
        );
        assert_eq!(hotspots[0].hash, 0xDEAD_BEEF);
        assert_eq!(hotspots[0].name, "spot1");
        assert_eq!(hotspots[0].target, "TopicName");
        assert_eq!(hotspots[1].action, HotspotAction::MacroVisible);
        assert_eq!(hotspots[1].target, "JumpContents()");
    }

    #[test]
    fn parse_shg_extracts_dib_and_hotspots() {
        let spots = [HotspotFixture {
            id0: 0xE7,
            id1: 0x04,
            id2: 0x00,
            rect: HotspotRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            hash: 42,
            name: "invis",
            target: "SomeTopic",
        }];
        let block = build_hotspot_block(&spots, b"");
        let mrb = make_mrb_dib_with_hotspots(&block);

        let (bmp, hotspots) = parse_shg(&mrb).expect("SHG DIB parse succeeds");

        // Produced bytes are a valid standalone BMP.
        assert_eq!(&bmp[0..2], &BMP_MAGIC);
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0].action, HotspotAction::TopicJumpInvisible);
        assert_eq!(hotspots[0].name, "invis");
        assert_eq!(hotspots[0].target, "SomeTopic");
    }

    #[test]
    fn parse_shg_returns_empty_list_when_hotspot_size_zero() {
        // An ordinary MRB metafile (no hotspot block) should parse as a
        // zero-hotspot SHG rather than failing.
        let mrb = make_mrb_metafile(0, b"\x01\x02\x03\x04");
        let (wmf, hotspots) = parse_shg(&mrb).expect("MRB without hotspots still parses");
        assert!(is_wmf(&wmf));
        assert!(hotspots.is_empty());
    }

    #[test]
    fn parse_shg_rejects_non_mrb_input() {
        assert!(parse_shg(&[]).is_none());
        assert!(parse_shg(b"BM\x00\x00").is_none());
        assert!(parse_shg(&[0x89, 0x50]).is_none());
    }

    #[test]
    fn hotspot_action_decodes_all_documented_id_triples() {
        let table: [(u8, u8, u8, HotspotAction); 10] = [
            (0xC8, 0x00, 0x00, HotspotAction::MacroVisible),
            (0xCC, 0x04, 0x00, HotspotAction::MacroInvisible),
            (0xE2, 0x00, 0x00, HotspotAction::PopupJumpVisible),
            (0xE3, 0x00, 0x00, HotspotAction::TopicJumpVisible),
            (0xE6, 0x04, 0x00, HotspotAction::PopupJumpInvisible),
            (0xE7, 0x04, 0x00, HotspotAction::TopicJumpInvisible),
            (0xEA, 0x00, 0x00, HotspotAction::ExternalPopupVisible),
            (0xEB, 0x00, 0x00, HotspotAction::ExternalTopicVisible),
            (0xEE, 0x04, 0x00, HotspotAction::ExternalPopupInvisible),
            (0xEF, 0x04, 0x00, HotspotAction::ExternalTopicInvisible),
        ];
        for (id0, id1, id2, expected) in table {
            assert_eq!(HotspotAction::from_id(id0, id1, id2), expected);
        }
    }

    #[test]
    fn hotspot_action_preserves_unknown_triples() {
        assert_eq!(
            HotspotAction::from_id(0x12, 0x34, 0x56),
            HotspotAction::Unknown(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn parse_hotspot_block_rejects_truncated_header() {
        // Too short to contain the full 7-byte header.
        assert!(parse_hotspot_block(&[HOTSPOT_BLOCK_MAGIC, 0, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn parse_hotspot_block_rejects_wrong_magic() {
        let mut bad = vec![0x02u8]; // should be 0x01
        bad.extend_from_slice(&0u16.to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        assert!(parse_hotspot_block(&bad).is_none());
    }

    #[test]
    fn parse_hotspot_block_rejects_truncated_strings() {
        // Header claims one hotspot but provides no strings after the
        // record — stringz reader should fail to find a terminator.
        let spots = [HotspotFixture {
            id0: 0xE3,
            id1: 0,
            id2: 0,
            rect: HotspotRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            hash: 0,
            name: "x",
            target: "y",
        }];
        let full = build_hotspot_block(&spots, b"");
        // Truncate just before the target's terminating NUL.
        let truncated = &full[..full.len() - 1];
        assert!(parse_hotspot_block(truncated).is_none());
    }

    #[test]
    fn parse_hotspot_block_preserves_empty_strings() {
        // Name and target are legitimately empty: two zero bytes in a row.
        let spots = [HotspotFixture {
            id0: 0xE2,
            id1: 0,
            id2: 0,
            rect: HotspotRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            hash: 0,
            name: "",
            target: "",
        }];
        let block = build_hotspot_block(&spots, b"");
        let parsed = parse_hotspot_block(&block).expect("empty strings still parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "");
        assert_eq!(parsed[0].target, "");
    }

    // -- DDB (MRB type=5) tests --

    /// Build a synthetic 1-bit monochrome MRB DDB with `width × height`
    /// pixels packed to 2-byte scanlines (DDB stride) and the given
    /// `by_packed` flag.  `pixel_bytes` must already be packed to
    /// `ddb_stride * height` bytes when `by_packed == 0`, or to an
    /// already-RunLen-encoded byte stream when `by_packed == 1`.
    fn make_mrb_ddb_1bit(
        width: u16,
        height: u16,
        by_packed: u8,
        pixel_bytes: &[u8],
        hotspot_block: &[u8],
    ) -> Vec<u8> {
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_DDB);
        mrb.push(by_packed);
        // All CDWord(0) fields emit as the 2-byte form 0x0000; CWord(N<0x80)
        // emits as a single byte `N << 1`.
        mrb.extend_from_slice(&0u16.to_le_bytes()); // x_ppm cdword(0)
        mrb.extend_from_slice(&0u16.to_le_bytes()); // y_ppm cdword(0)
        mrb.push(0x02); // planes cword(1) = (1 << 1)
        mrb.push(0x02); // bit_count cword(1) = (1 << 1)
        mrb.extend_from_slice(&(width << 1).to_le_bytes()); // width cdword
        mrb.extend_from_slice(&(height << 1).to_le_bytes()); // height cdword
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_used cdword(0)
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_important cdword(0)
        mrb.extend_from_slice(&((pixel_bytes.len() as u16) << 1).to_le_bytes()); // data_size
        mrb.extend_from_slice(&((hotspot_block.len() as u16) << 1).to_le_bytes()); // hotspot_size
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwPictureOffset
        mrb.extend_from_slice(&0u32.to_le_bytes()); // dwHotspotOffset
        mrb.extend_from_slice(pixel_bytes);
        mrb.extend_from_slice(hotspot_block);
        mrb
    }

    /// Read back the palette slice from a decoded BMP so assertions can
    /// inspect it directly without re-implementing the stride math.
    fn bmp_palette(bmp: &[u8]) -> &[u8] {
        let pixel_offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]) as usize;
        &bmp[BMP_FILE_HEADER_SIZE + BITMAPINFOHEADER_SIZE as usize..pixel_offset]
    }

    /// Read back the pixel slice from a decoded BMP.
    fn bmp_pixels(bmp: &[u8]) -> &[u8] {
        let pixel_offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]) as usize;
        &bmp[pixel_offset..]
    }

    #[test]
    fn mrb_to_bmp_converts_raw_ddb_with_stride_padding() {
        // 8×2 1-bit DDB: stride = ((8*1 + 15)/16)*2 = 2 bytes/row, so
        // 4 bytes of pixel data.  Target DIB stride = 4 bytes/row →
        // 2 pad bytes appended per row.
        let ddb_pixels = [0xAA, 0x55, 0xFF, 0x00];
        let mrb = make_mrb_ddb_1bit(8, 2, 0, &ddb_pixels, &[]);

        let bmp = mrb_to_bmp(&mrb).expect("DDB decode succeeds");
        assert_eq!(&bmp[0..2], &BMP_MAGIC);

        // BITMAPINFOHEADER field spot-checks.
        let bit_count = u16::from_le_bytes([bmp[28], bmp[29]]);
        let colors_used = u32::from_le_bytes([bmp[46], bmp[47], bmp[48], bmp[49]]);
        assert_eq!(bit_count, 1);
        assert_eq!(colors_used, 2);

        // Palette: 2 entries = black, white in BGR0 order.
        let palette = bmp_palette(&bmp);
        assert_eq!(palette.len(), 8);
        assert_eq!(&palette[0..4], &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&palette[4..8], &[0xFF, 0xFF, 0xFF, 0x00]);

        // Pixels: each DDB row padded to 4 bytes with 0x20 pad bytes.
        let pixels = bmp_pixels(&bmp);
        assert_eq!(pixels, &[0xAA, 0x55, 0x20, 0x20, 0xFF, 0x00, 0x20, 0x20]);
    }

    #[test]
    fn mrb_to_bmp_decompresses_runlen_ddb() {
        // RunLen-pack the DDB stream: control=4 + data=0x55 → four 0x55
        // bytes, which is exactly two 8×2 DDB rows of 0x55 each
        // (ddb_stride = 2, height = 2 → 4 bytes total).
        let encoded = [4u8, 0x55];
        let mrb = make_mrb_ddb_1bit(8, 2, 1, &encoded, &[]);

        let bmp = mrb_to_bmp(&mrb).expect("RunLen DDB decode succeeds");
        let pixels = bmp_pixels(&bmp);
        // 2 DDB bytes + 2 pad per row, 2 rows.
        assert_eq!(pixels, &[0x55, 0x55, 0x20, 0x20, 0x55, 0x55, 0x20, 0x20]);
    }

    #[test]
    fn mrb_to_bmp_rejects_multi_bit_ddb() {
        // DDB has no in-file palette, so we don't support higher bit depths.
        // Hand-build a 4-bit DDB and assert the decoder bails out.
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_DDB);
        mrb.push(0);
        mrb.extend_from_slice(&0u16.to_le_bytes()); // x_ppm
        mrb.extend_from_slice(&0u16.to_le_bytes()); // y_ppm
        mrb.push(0x02); // planes cword(1)
        mrb.push(4 << 1); // bit_count cword(4) — unsupported
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // width=1
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // height=1
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_used
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_important
        mrb.extend_from_slice(&(2u16 << 1).to_le_bytes()); // data_size=2
        mrb.extend_from_slice(&0u16.to_le_bytes()); // hotspot_size
        mrb.extend_from_slice(&0u32.to_le_bytes());
        mrb.extend_from_slice(&0u32.to_le_bytes());
        mrb.extend_from_slice(&[0xFF, 0xFF]);

        assert!(mrb_to_bmp(&mrb).is_none());
    }

    #[test]
    fn mrb_to_bmp_rejects_lz77_packed_ddb() {
        // DDB supports only byPacked 0 and 1 per helpdeco splitmrb.c:372.
        // byPacked=2 (LZ77) must be rejected rather than mis-decoded.
        let mrb = make_mrb_ddb_1bit(8, 1, 2, &[0x00, 0x00], &[]);
        assert!(mrb_to_bmp(&mrb).is_none());
    }

    #[test]
    fn mrb_to_bmp_dib_path_unchanged_by_ddb_refactor() {
        // Regression guard: the existing DIB code path — palette read from
        // the file, sequential compressed pixel data — must still work
        // after the type-5 branch was added.  Build a 1×1 24-bit DIB and
        // verify the output has no palette and the pixel bytes round-trip.
        let pixels: [u8; 4] = [0x11, 0x22, 0x33, 0x00];
        let mut mrb = Vec::new();
        mrb.extend_from_slice(&0x506Cu16.to_le_bytes());
        mrb.extend_from_slice(&1u16.to_le_bytes());
        mrb.extend_from_slice(&8u32.to_le_bytes());
        mrb.push(MRB_TYPE_DIB);
        mrb.push(0);
        mrb.extend_from_slice(&0u16.to_le_bytes()); // x_ppm
        mrb.extend_from_slice(&0u16.to_le_bytes()); // y_ppm
        mrb.push(0x02); // planes
        mrb.push(24 << 1); // bit_count=24
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // width=1
        mrb.extend_from_slice(&(1u16 << 1).to_le_bytes()); // height=1
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_used
        mrb.extend_from_slice(&0u16.to_le_bytes()); // clr_important
        mrb.extend_from_slice(&((pixels.len() as u16) << 1).to_le_bytes());
        mrb.extend_from_slice(&0u16.to_le_bytes()); // hotspot_size
        mrb.extend_from_slice(&0u32.to_le_bytes());
        mrb.extend_from_slice(&0u32.to_le_bytes());
        mrb.extend_from_slice(&pixels);

        let bmp = mrb_to_bmp(&mrb).expect("24-bit DIB decode still succeeds");
        assert_eq!(&bmp[0..2], &BMP_MAGIC);
        let bit_count = u16::from_le_bytes([bmp[28], bmp[29]]);
        assert_eq!(bit_count, 24);
        // 24-bit has no palette — pixel offset should equal the sum of file
        // header and info header with no gap.
        let pixel_offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]) as usize;
        assert_eq!(
            pixel_offset,
            BMP_FILE_HEADER_SIZE + BITMAPINFOHEADER_SIZE as usize
        );
        assert_eq!(bmp_pixels(&bmp), &pixels);
    }

    #[test]
    fn parse_shg_extracts_ddb_and_hotspots() {
        // SHG with a 1-bit DDB picture and a single hotspot — exercises
        // the DDB branch in `extract_hotspot_bytes` so the hotspot block
        // is located after the unpacked pixel data.
        let spots = [HotspotFixture {
            id0: 0xE3,
            id1: 0x00,
            id2: 0x00,
            rect: HotspotRect {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            },
            hash: 0xCAFE_BABE,
            name: "ddb_spot",
            target: "DdbTarget",
        }];
        let hotspot_block = build_hotspot_block(&spots, b"");
        // 8×2 1-bit DDB → 2-byte DDB stride × 2 rows = 4 pixel bytes.
        let mrb = make_mrb_ddb_1bit(8, 2, 0, &[0xF0, 0x0F, 0xAA, 0x55], &hotspot_block);

        let (bmp, hotspots) = parse_shg(&mrb).expect("DDB SHG parse succeeds");
        assert_eq!(&bmp[0..2], &BMP_MAGIC);
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0].action, HotspotAction::TopicJumpVisible);
        assert_eq!(hotspots[0].name, "ddb_spot");
        assert_eq!(hotspots[0].hash, 0xCAFE_BABE);
    }

    #[test]
    fn ddb_to_dib_pixels_rejects_truncated_input() {
        // 16-pixel wide, 2 rows → DDB stride = 2 bytes → need 4 bytes total.
        // Provide only 3 and assert None.
        assert!(ddb_to_dib_pixels(&[0u8; 3], 16, 2, 1).is_none());
    }

    #[test]
    fn ddb_to_dib_pixels_rejects_non_positive_dimensions() {
        assert!(ddb_to_dib_pixels(&[0u8; 4], 0, 2, 1).is_none());
        assert!(ddb_to_dib_pixels(&[0u8; 4], 8, 0, 1).is_none());
        assert!(ddb_to_dib_pixels(&[0u8; 4], -1, 2, 1).is_none());
    }
}
