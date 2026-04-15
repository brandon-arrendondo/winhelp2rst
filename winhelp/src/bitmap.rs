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
//! 2. Returns the synthesised BMP bytes so the downstream RST writer can
//!    feed them to an image decoder and re-encode as PNG.
//!
//! Reference: helpdeco/src/splitmrb.c (`main`, `decompress`).

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

/// MRB packing method bits: bit 0 = RunLen, bit 1 = LZ77.
const MRB_PACK_LZ77: u8 = 2;

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
            // Fall through to raw bytes if MRB decode fails — callers can at
            // least save them verbatim for inspection.
            return Ok(Some(raw));
        }
    }

    Ok(Some(ensure_bmp_header(&raw)))
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
    let pixels = if by_packed & MRB_PACK_LZ77 != 0 {
        match lz77_decompress(compressed_pixels) {
            Ok(v) => v,
            Err(_) => return None,
        }
    } else if by_packed == 0 {
        compressed_pixels.to_vec()
    } else {
        // RunLen-only (byPacked == 1) and RunLen+LZ77 (==3) are rare for
        // embedded help bitmaps — skip for now.
        return None;
    };

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
}
