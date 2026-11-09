//! Image normalisation for the two places an image can be consumed.
//!
//! There is no collage any more. The gate tags each image on its own (a tiled grid only
//! ever cost resolution and made OCR worse), and the agent reads at most one picture per
//! batch, so nothing is combined into a canvas.
//!
//! Both consumers need a bounded size, so this module also owns the shrink-until-it-fits
//! encoders: JPEG for the agent's `read_file` path (1 MiB on disk) and a data URL budget
//! for the gate's `image_url` path.

use super::download::MediaError;
use super::resize::PreparedImage;

/// How many images one batch may carry. Anything beyond this is reported as unavailable
/// rather than silently dropped.
pub const MAX_BATCH_IMAGES: usize = 8;

/// Maximum dimension for a single staged image after resizing. Guards against absurd
/// source dimensions; normal photos land far below it.
pub const MAX_EDGE: u32 = 3_200;

/// Whether the current implementation can prepare an image file for Agy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The local image file is staged inside the per-batch Agy workspace.
    SandboxFile,
    /// A real Agy image-reading contract has been verified.
    Verified,
    /// The workspace or local staging path is unavailable.
    Unavailable,
}

impl Transport {
    /// Current code stages image files, but does not claim Agy successfully interpreted one.
    pub fn current() -> Self {
        Transport::SandboxFile
    }

    pub fn is_available(self) -> bool {
        matches!(self, Transport::SandboxFile | Transport::Verified)
    }

    /// The note appended when no image can be staged.
    pub fn prompt_note(self, image_count: usize) -> Option<String> {
        match self {
            Transport::SandboxFile | Transport::Verified => None,
            Transport::Unavailable if image_count == 0 => None,
            Transport::Unavailable => Some(format!(
                "本批有 {image_count} 张图片，但没有准备出可读取文件。请不要猜测图片内容。"
            )),
        }
    }
}

/// Largest image file Agy is allowed to read.
pub const AGY_IMAGE_MAX_BYTES: usize = 1_048_576;

/// Largest base64 payload the gate may spend per call.
///
/// Four images of ~500 KiB each already exceed the useful resolution of the tag model,
/// so the budget is per call rather than per image: it caps the whole request and lets a
/// single big picture use most of it.
pub const VISION_MAX_BASE64_BYTES: usize = 1_400_000;

/// Largest base64 payload one outgoing sticker may take.
///
/// One mebibyte of encoded picture is the frame budget the send path was written
/// against; anything larger risks a rejected or truncated frame, and a sticker is
/// never worth that.
pub const STICKER_MAX_BASE64_BYTES: usize = 1_048_576;

/// Re-encode a sticker as JPEG small enough that its base64 fits the frame budget.
///
/// The library file itself may be close to a mebibyte, and base64 adds a third, so the
/// bytes cannot be sent verbatim. Returns `None` when the input is not an image or no
/// encoding fits.
pub fn fit_for_sticker(bytes: &[u8]) -> Option<Vec<u8>> {
    let jpeg = fit_for_agy_within(bytes, STICKER_MAX_BASE64_BYTES / 4 * 3)?;
    if base64_encode(&jpeg).len() > STICKER_MAX_BASE64_BYTES {
        return None;
    }
    Some(jpeg)
}

/// Re-encode as JPEG, shrinking until the bytes fit in one mebibyte.
///
/// Returns `None` when the input is not an image or no encoding fits.
pub fn fit_for_agy(bytes: &[u8]) -> Option<Vec<u8>> {
    fit_for_agy_within(bytes, AGY_IMAGE_MAX_BYTES)
}

/// Same shrink loop with an explicit byte ceiling.
fn fit_for_agy_within(bytes: &[u8], ceiling: usize) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    let mut image = image::load_from_memory(bytes).ok()?.into_rgb8();
    for _ in 0..6 {
        for quality in [75u8, 55, 35, 20] {
            let mut buf = Vec::new();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            if encoder
                .encode(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .is_ok()
                && !buf.is_empty()
                && buf.len() <= ceiling
            {
                return Some(buf);
            }
        }
        let next_w = (image.width() / 2).max(1);
        let next_h = (image.height() / 2).max(1);
        if next_w == image.width() && next_h == image.height() {
            break;
        }
        image = image::imageops::thumbnail(&image, next_w, next_h);
    }
    None
}

/// Encode one image as a JPEG data URL small enough for the gate's multimodal call.
///
/// Base64 inflates by 4/3, so the encoder is given the deflated ceiling. If even the
/// smallest encoding is too large the image is reported as unavailable instead of being
/// silently truncated — a half-decoded image would tag the wrong picture.
pub fn fit_for_vision(bytes: &[u8]) -> Option<String> {
    let jpeg = fit_for_agy_within(bytes, VISION_MAX_BASE64_BYTES / 4 * 3)?;
    let base64 = base64_encode(&jpeg);
    if base64.len() > VISION_MAX_BASE64_BYTES {
        return None;
    }
    Some(format!("data:image/jpeg;base64,{base64}"))
}

/// Minimal standard base64 (RFC 4648, padded). Avoids a new dependency for every call site.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Normalise a batch's raw images, honouring the per-batch cap.
///
/// Resizing happens here rather than at fetch time so the same decoded bytes can serve
/// both consumers without a second decode.
pub async fn prepare_batch(
    transport: Transport,
    raw: &[(String, Vec<u8>)],
    max_pixels: u64,
) -> Vec<(String, Result<PreparedImage, MediaError>)> {
    if !transport.is_available() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(raw.len().min(MAX_BATCH_IMAGES));
    for (label, bytes) in raw.iter().take(MAX_BATCH_IMAGES) {
        let prepared = super::resize::prepare_async(bytes.clone(), max_pixels).await;
        out.push((label.clone(), prepared));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(w: u32, h: u32) -> PreparedImage {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("编码");
        PreparedImage {
            width: w,
            height: h,
            bytes,
            animated: false,
            original: None,
        }
    }

    #[test]
    fn sandbox_file_staging_is_enabled_but_not_claimed_verified() {
        assert_eq!(Transport::current(), Transport::SandboxFile);
        assert!(Transport::current().is_available());
        assert!(!crate::config::image_transport_verified());
    }

    #[test]
    fn prompt_note_only_appears_when_staging_is_unavailable() {
        assert!(Transport::Unavailable.prompt_note(0).is_none());
        let note = Transport::Unavailable.prompt_note(2).expect("应有提示");
        assert!(note.contains('2'));
        assert!(note.contains("不要猜测"));
        assert!(Transport::SandboxFile.prompt_note(2).is_none());
    }

    #[tokio::test]
    async fn unavailable_transport_decodes_nothing() {
        let raw = vec![("u1".to_string(), vec![0u8; 100])];
        let out = prepare_batch(Transport::Unavailable, &raw, 20_000_000).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn prepare_batch_stops_at_the_batch_cap() {
        let raw: Vec<(String, Vec<u8>)> = (0..10)
            .map(|i| (format!("i{i}"), prepared(20, 20).bytes))
            .collect();
        let out = prepare_batch(Transport::SandboxFile, &raw, 20_000_000).await;
        assert_eq!(out.len(), MAX_BATCH_IMAGES);
    }

    #[test]
    fn fitted_jpeg_file_is_at_most_one_mebibyte() {
        let big = prepared(4_000, 3_000);
        let out = fit_for_agy(&big.bytes).expect("应能压缩");
        assert!(out.len() <= AGY_IMAGE_MAX_BYTES);
        assert_eq!(&out[..2], &[0xFF, 0xD8], "必须是 JPEG");
    }

    #[test]
    fn vision_data_url_is_bounded_and_well_formed() {
        let big = prepared(4_000, 3_000);
        let url = fit_for_vision(&big.bytes).expect("应能压缩");
        assert!(url.starts_with("data:image/jpeg;base64,"));
        let payload = url.trim_start_matches("data:image/jpeg;base64,");
        assert!(payload.len() <= VISION_MAX_BASE64_BYTES);
        assert!(!payload.contains(' '));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn non_image_bytes_are_reported_rather_than_guessed() {
        assert!(fit_for_agy(b"not an image").is_none());
        assert!(fit_for_vision(b"not an image").is_none());
        assert!(fit_for_agy(b"").is_none());
    }
}
