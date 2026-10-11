//! Thumbnail fetching and decoding.
//!
//! Artwork is fetched with `reqwest` (the same stack the API client uses) and
//! handed to egui as a texture. We deliberately avoid egui's built-in HTTP
//! loader: the `ehttp` backend it pulls in failed to fetch these URLs.

use anyhow::Result;

/// Longest edge we keep after decoding. The API offers up to 544px, which is
/// wasteful for list rows of ~40px.
const MAX_EDGE: u32 = 256;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Fetch raw image bytes.
pub async fn fetch(client: &reqwest::Client, uri: &str) -> Result<Vec<u8>> {
    let response = client.get(uri).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

/// Decode image bytes into an egui image, downscaling large artwork (but never
/// upscaling small images).
pub fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > MAX_EDGE || image.height() > MAX_EDGE {
        image.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        image
    };
    let image = image.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the same decode + texture-registration path the UI uses for
    /// every thumbnail, using a JPEG built in memory.
    #[test]
    fn decodes_and_registers_texture() {
        let source = image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 4) as u8, 128])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(source)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .expect("encoding test JPEG");

        let decoded = decode(&bytes).expect("decoding test JPEG");
        assert_eq!(decoded.size, [64, 64]);
        assert_eq!(decoded.pixels.len(), 64 * 64);

        let ctx = egui::Context::default();
        let handle = ctx.load_texture("test", decoded, egui::TextureOptions::LINEAR);
        assert_eq!(handle.size(), [64, 64]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"definitely not an image").is_none());
    }
}
