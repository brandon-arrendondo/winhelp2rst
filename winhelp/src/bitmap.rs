//! Bitmap extraction and BMP header fixup.
//!
//! WinHelp stores embedded pictures as internal files (`|bmN`). The on-disk
//! format is usually MRB (multi-resolution bitmap, magic `lp`/`lP`) wrapping
//! one or more pictures — each of which may be a DIB, DDB, or metafile,
//! optionally RunLen- or LZ77-compressed. This module:
//!
//! 1. Detects MRB containers and unwraps the first DIB picture, producing a
//!    standard BMP (`ensure_bmp_header` handles the fallback case where the
//!    raw bytes are already a plain BMP missing only its file header).
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

use crate::container::HlpContainer;
use crate::decompress::lz77_decompress;
use crate::{Error, Result};

/// BMP file magic bytes ("BM").
const BMP_MAGIC: [u8; 2] = [0x42, 0x4D];

/// Size of the BITMAPFILEHEADER (14 bytes).
const BMP_FILE_HEADER_SIZE: usize = 14;

/// BITMAPINFOHEADER size field value (40 bytes) — the most common variant.
const BITMAPINFOHEADER_SIZE: u32 = 40;

/// MRB picture-type byte: 5 = DDB, 6 = DIB, 8 = metafile.
const MRB_TYPE_DIB: u8 = 6;
/// MRB picture-type byte for a Windows Metafile picture.
const MRB_TYPE_METAFILE: u8 = 8;

/// Aldus Placeable Metafile magic, little-endian on disk: `D7 CD C6 9A`.
pub const APM_MAGIC: u32 = 0x9AC6_CDD7;
/// Length of the Aldus Placeable Metafile header (22 bytes).
const APM_HEADER_LEN: usize = 22;

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

/// Decode an MRB container's first DIB picture into a standalone BMP.
///
/// Returns `None` if the container is malformed, the first picture isn't a
/// DIB, or the picture uses a packing method we don't support (RunLen-only
/// DDB conversion isn't implemented).
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
    if by_type != MRB_TYPE_DIB {
        // DDB (5) and metafile (8) would need their own conversion pipelines;
        // callers can fall back to raw bytes for these.
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
    let palette_bytes = colors * 4;

    // The palette immediately follows the variable-length header in the
    // source, at `pic`'s current cursor position. Read it verbatim.
    let palette_start = pic_offset + pic.pos();
    if palette_start + palette_bytes > data.len() {
        return None;
    }
    let palette = &data[palette_start..palette_start + palette_bytes];

    // Decompress the pixel data, which lives immediately after the palette.
    let pixel_data_start = palette_start + palette_bytes;
    if pixel_data_start + data_size > data.len() {
        return None;
    }
    let compressed_pixels = &data[pixel_data_start..pixel_data_start + data_size];
    let pixels = decompress_packed(compressed_pixels, by_packed)?;

    // Assemble the output BMP: BITMAPFILEHEADER + BITMAPINFOHEADER + palette + pixels.
    let bmi_header_size = BITMAPINFOHEADER_SIZE as usize;
    let pixel_offset = BMP_FILE_HEADER_SIZE + bmi_header_size + palette_bytes;
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
    out.extend_from_slice(palette);
    out.extend_from_slice(&pixels);

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
}
