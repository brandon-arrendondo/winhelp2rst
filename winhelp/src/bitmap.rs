//! Bitmap extraction and BMP header fixup.
//!
//! Images in WinHelp files are stored as internal files. Most are Windows BMP
//! format, but some omit the 14-byte `BITMAPFILEHEADER` — they start directly
//! with the `BITMAPINFOHEADER`. This module detects the case and prepends the
//! missing header so standard BMP decoders can read the data.

use crate::container::HlpContainer;
use crate::{Error, Result};

/// BMP file magic bytes ("BM").
const BMP_MAGIC: [u8; 2] = [0x42, 0x4D];

/// Size of the BITMAPFILEHEADER (14 bytes).
const BMP_FILE_HEADER_SIZE: usize = 14;

/// BITMAPINFOHEADER size field value (40 bytes) — the most common variant.
const BITMAPINFOHEADER_SIZE: u32 = 40;

/// Extract a bitmap from the HLP container by internal filename.
///
/// Returns the raw BMP bytes, with a BITMAPFILEHEADER prepended if the
/// stored data was missing one. Returns `None` if the file doesn't exist
/// in the container.
pub fn extract_bitmap(container: &HlpContainer, name: &str) -> Result<Option<Vec<u8>>> {
    let raw = match container.read_file(name) {
        Ok(data) => data,
        Err(Error::FileNotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };

    if raw.is_empty() {
        return Ok(Some(raw));
    }

    Ok(Some(ensure_bmp_header(&raw)))
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
