//! Example EDITOR plugin: photo-edit operations (bytes in → bytes out).
//!
//! Exposes two ops the Photon editor UI can apply to a photo's original:
//! - `grayscale` — desaturate the whole image.
//! - `watermark` — darken a band across the bottom (a faux watermark bar), with
//!   an `opacity` parameter (0–100).
//!
//! Both decode the original with the `image` crate, transform, and re-encode to
//! PNG (lossless, universally decodable for the preview).

use std::collections::HashMap;
use std::io::Cursor;

use photon_plugin_sdk::*;

struct Watermark;

#[async_trait]
impl EditorPlugin for Watermark {
    fn ops(&self) -> Vec<EditorOp> {
        let mut watermark = EditorOp::new("watermark", "Watermark Bar", "Darken a band across the bottom");
        watermark.params = vec![OpParam {
            name: "opacity".to_string(),
            label: "Opacity (0–100)".to_string(),
            default: "40".to_string(),
        }];
        vec![
            EditorOp::new("grayscale", "Grayscale", "Desaturate the image"),
            watermark,
        ]
    }

    async fn apply(
        &self,
        op_id: &str,
        image: Vec<u8>,
        _content_type: &str,
        params: &HashMap<String, String>,
    ) -> Result<EditedImage, PluginError> {
        tracing::info!(op = op_id, bytes = image.len(), "watermark plugin: applying op");

        let img = image::load_from_memory(&image)
            .map_err(|e| PluginError::new(format!("decode failed: {e}")))?;

        let out = match op_id {
            "grayscale" => img.grayscale(),
            "watermark" => {
                let opacity = params
                    .get("opacity")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(40.0)
                    .clamp(0.0, 100.0)
                    / 100.0;
                let mut rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let band = ((h as f32) * 0.12).round() as u32; // bottom 12%
                for y in h.saturating_sub(band)..h {
                    for x in 0..w {
                        let p = rgba.get_pixel_mut(x, y);
                        p[0] = (p[0] as f32 * (1.0 - opacity)) as u8;
                        p[1] = (p[1] as f32 * (1.0 - opacity)) as u8;
                        p[2] = (p[2] as f32 * (1.0 - opacity)) as u8;
                    }
                }
                image::DynamicImage::ImageRgba8(rgba)
            }
            other => return Err(PluginError::new(format!("unknown op {other}"))),
        };

        let mut buf = Cursor::new(Vec::new());
        out.write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| PluginError::new(format!("encode failed: {e}")))?;
        Ok(EditedImage::new(buf.into_inner(), "image/png"))
    }
}

#[tokio::main]
async fn main() {
    serve(editor(
        PluginMeta::new("watermark", "Watermark Editor", env!("CARGO_PKG_VERSION")),
        Watermark,
    ))
    .await
}
