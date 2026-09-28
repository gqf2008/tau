//! Image sniffing by magic number — a port of pi's `utils/mime.ts` image
//! branch. Only the formats the model can actually be shown are recognized;
//! everything else reads as text.

/// How many leading bytes are sniffed (pi reads 4100 into a buffer).
pub const SNIFF_BYTES: usize = 4100;

const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// The image MIME type of `buffer`, or `None` when it is not an image this
/// harness can attach (or a format variant it does not support, e.g. an
/// animated PNG).
pub fn detect(buffer: &[u8]) -> Option<&'static str> {
    // JPEG: FF D8 FF, except the F7 variant (JPEG-LS), which pi also rejects.
    if buffer.len() >= 4 && buffer[0] == 0xff && buffer[1] == 0xd8 && buffer[2] == 0xff {
        return (buffer[3] != 0xf7).then_some("image/jpeg");
    }
    // Exactly three bytes: no room for the F7 marker, so a JPEG.
    if buffer.len() == 3 && buffer[0] == 0xff && buffer[1] == 0xd8 && buffer[2] == 0xff {
        return Some("image/jpeg");
    }
    if is_png(buffer) {
        return if is_animated_png(buffer) {
            None
        } else {
            Some("image/png")
        };
    }
    if starts_with_ascii(buffer, 0, b"GIF87a") || starts_with_ascii(buffer, 0, b"GIF89a") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, b"RIFF") && starts_with_ascii(buffer, 8, b"WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, b"BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// PNG signature plus a well-formed `IHDR` (length 13 at offset 8, type at
/// 12) — pi's `isPng`.
fn is_png(buffer: &[u8]) -> bool {
    if buffer.len() < 16 || buffer[..8] != PNG_MAGIC {
        return false;
    }
    let length = u32_be(buffer, 8);
    length == Some(13) && starts_with_ascii(buffer, 12, b"IHDR")
}

/// True when the PNG carries an `acTL` chunk (APNG). Walked chunk by chunk
/// because `acTL` may sit past the sniff window — an unreadable chunk header
/// ends the walk, which reports "not animated" (pi does the same).
fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = 8usize; // past the signature
    while offset + 8 <= buffer.len() {
        let Some(length) = u32_be(buffer, offset) else {
            return false;
        };
        if buffer[offset + 4..offset + 8] == *b"acTL" {
            return true;
        }
        // 4 (length) + 4 (type) + data + 4 (CRC)
        let Some(next) = offset
            .checked_add(12)
            .and_then(|p| p.checked_add(length as usize))
        else {
            return false;
        };
        if next <= offset {
            return false;
        }
        offset = next;
    }
    false
}

/// BMP is only trusted with a sane DIB header — pi's `isBmp`. A `BM`-leading
/// text file must not be handed to the model as an image.
fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = u32_le(buffer, 2).unwrap_or(0);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    let pixel_data_offset = u32_le(buffer, 10).unwrap_or(0);
    let dib_header_size = u32_le(buffer, 14).unwrap_or(0);
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }
    let (planes_at, bpp_at) = match dib_header_size {
        12 => (22, 24),
        40..=124 => (26, 28),
        _ => return false,
    };
    let color_planes = u16_le(buffer, planes_at).unwrap_or(0);
    let bits_per_pixel = u16_le(buffer, bpp_at).unwrap_or(0);
    color_planes == 1 && [1u16, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

fn starts_with_ascii(buffer: &[u8], at: usize, prefix: &[u8]) -> bool {
    buffer.len() >= at + prefix.len() && &buffer[at..at + prefix.len()] == prefix
}

fn u32_be(buffer: &[u8], at: usize) -> Option<u32> {
    let bytes = buffer.get(at..at + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u32_le(buffer: &[u8], at: usize) -> Option<u32> {
    let bytes = buffer.get(at..at + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u16_le(buffer: &[u8], at: usize) -> Option<u16> {
    let bytes = buffer.get(at..at + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let mut v = PNG_MAGIC.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0u8; 21]); // the rest of the header
        v
    }

    #[test]
    fn a_png_is_recognized() {
        assert_eq!(detect(&png_bytes()), Some("image/png"));
    }

    #[test]
    fn an_apng_is_not_offered_as_a_still_image() {
        let mut v = png_bytes();
        // An acTL chunk right after IHDR (13 + 12 bytes of header/data).
        let ihdr_end = 8 + 12 + 13;
        v.truncate(ihdr_end);
        v.extend_from_slice(&0u32.to_be_bytes()); // acTL length
        v.extend_from_slice(b"acTL");
        v.extend_from_slice(&0u32.to_be_bytes()); // CRC
        assert_eq!(detect(&v), None);
    }

    #[test]
    fn a_png_signature_without_ihdr_is_not_a_png() {
        let mut v = PNG_MAGIC.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IDAT");
        v.extend_from_slice(&[0u8; 21]);
        assert_eq!(detect(&v), None);
        assert_eq!(detect(&PNG_MAGIC), None);
    }

    #[test]
    fn jpeg_is_recognized_except_the_ls_variant() {
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xe0, 0x00]), Some("image/jpeg"));
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xf7, 0x00]), None);
        // A bare three-byte prefix is still a JPEG.
        assert_eq!(detect(&[0xff, 0xd8, 0xff]), Some("image/jpeg"));
    }

    #[test]
    fn gif_and_webp_are_recognized() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0u8; 8]);
        assert_eq!(detect(&gif), Some("image/gif"));
        let mut gif = b"GIF87a".to_vec();
        gif.extend_from_slice(&[0u8; 8]);
        assert_eq!(detect(&gif), Some("image/gif"));

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0u8; 4]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(detect(&webp), Some("image/webp"));
        // RIFF of another flavour (a WAV, say) is not an image.
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        assert_eq!(detect(&wav), None);
    }

    /// A 24-bit BMP with a 40-byte DIB header and 54-byte pixel offset.
    fn bmp_bytes() -> Vec<u8> {
        let mut v = vec![0u8; 54];
        v[0] = b'B';
        v[1] = b'M';
        v[2..6].copy_from_slice(&70u32.to_le_bytes()); // declared file size
        v[10..14].copy_from_slice(&54u32.to_le_bytes()); // pixel data offset
        v[14..18].copy_from_slice(&40u32.to_le_bytes()); // DIB header size
        v[26..28].copy_from_slice(&1u16.to_le_bytes()); // color planes
        v[28..30].copy_from_slice(&24u16.to_le_bytes()); // bits per pixel
        v
    }

    #[test]
    fn a_well_formed_bmp_is_recognized() {
        assert_eq!(detect(&bmp_bytes()), Some("image/bmp"));
    }

    #[test]
    fn a_bm_prefixed_text_file_is_not_an_image() {
        // "BM" then prose: no sane DIB header.
        let text = b"BMW is a car maker, and this is a long enough file to pass the length check.";
        assert_eq!(detect(text), None);
    }

    #[test]
    fn a_bmp_with_odd_pixel_geometry_is_rejected() {
        let mut odd_planes = bmp_bytes();
        odd_planes[26..28].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(detect(&odd_planes), None);

        let mut odd_depth = bmp_bytes();
        odd_depth[28..30].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(detect(&odd_depth), None);

        let mut bad_dib = bmp_bytes();
        bad_dib[14..18].copy_from_slice(&39u32.to_le_bytes());
        assert_eq!(detect(&bad_dib), None);
    }

    #[test]
    fn text_and_short_inputs_are_not_images() {
        assert_eq!(detect(b""), None);
        assert_eq!(detect(b"\xff\xd8"), None);
        assert_eq!(detect(b"\xff"), None);
        assert_eq!(detect("fn main() {}\n".as_bytes()), None);
        assert_eq!(detect(&[0x00; 4]), None);
    }
}
