//! Pure-Rust photo metadata extraction.
//!
//! RATIONALE: this project deliberately does NOT shell out to ImageMagick /
//! `identify` (or any external binary) to read image metadata. All extraction
//! is done in-process with pure-Rust crates:
//!   * the `image` crate reads pixel dimensions,
//!   * the `kamadak-exif` crate (imported as `exif`) reads EXIF tags.
//! This keeps the server dependency-light, sandbox-friendly, and free of
//! subprocess/security concerns.

use exif::{In, Tag, Value};

use crate::models::Exif;

/// Extracts capture metadata from raw image bytes. Implementations are kept
/// behind a trait so future RAW-specific extractors can be slotted in without
/// touching the upload path.
pub trait MetadataExtractor: Send + Sync {
    fn extract(&self, bytes: &[u8], filename: &str) -> Exif;
}

/// Default extractor: `image` for dimensions + `kamadak-exif` for tags.
pub struct ExifExtractor;

impl MetadataExtractor for ExifExtractor {
    fn extract(&self, bytes: &[u8], filename: &str) -> Exif {
        extract(bytes, filename)
    }
}

/// Read dimensions (via `image`) and EXIF tags (via `kamadak-exif`) from
/// `bytes`. Missing tags map to `None`; if dimensions can't be read they fall
/// back to 0. Never panics on malformed input.
pub fn extract(bytes: &[u8], _filename: &str) -> Exif {
    let mut out = Exif::default();

    // ---- Pixel dimensions via the `image` crate ----
    // Read ONLY the header to get dimensions — never a full pixel decode. A full
    // `load_from_memory` here would decode multi-megabyte photos (and this runs
    // once per import phase), which previously blocked the import worker's write
    // lock for seconds. `into_dimensions()` parses just the header.
    if let Some((w, h)) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
    {
        out.width = w;
        out.height = h;
    }

    // ---- EXIF tags via kamadak-exif ----
    let exif_reader = exif::Reader::new();
    let mut cursor = std::io::Cursor::new(bytes);
    if let Ok(reader) = exif_reader.read_from_container(&mut cursor) {
        // Camera = "Make Model" (trimmed).
        let make = string_field(&reader, Tag::Make);
        let model = string_field(&reader, Tag::Model);
        out.camera = match (make, model) {
            (Some(mk), Some(md)) => Some(format!("{mk} {md}").trim().to_string()),
            (Some(mk), None) => Some(mk),
            (None, Some(md)) => Some(md),
            (None, None) => None,
        };
        out.lens = string_field(&reader, Tag::LensModel);

        // ISO — cameras store it as Short or Long (and under ISOSpeedRatings on
        // older files). Try a few tags/types.
        for tag in [Tag::PhotographicSensitivity, Tag::ISOSpeed] {
            if out.iso.is_some() {
                break;
            }
            if let Some(f) = reader.get_field(tag, In::PRIMARY) {
                out.iso = match &f.value {
                    Value::Short(v) => v.first().map(|n| *n as u32),
                    Value::Long(v) => v.first().copied(),
                    _ => f.display_value().to_string().parse::<u32>().ok(),
                };
            }
        }

        // Exposure time -> shutter (e.g. "1/250").
        out.shutter = reader
            .get_field(Tag::ExposureTime, In::PRIMARY)
            .map(|f| f.display_value().to_string());

        // FNumber -> "f/4.0".
        if let Some(f) = reader.get_field(Tag::FNumber, In::PRIMARY) {
            out.fnum = Some(format!("f/{}", f.display_value()));
        }

        // FocalLength -> "50 mm" style display value.
        out.focal = reader
            .get_field(Tag::FocalLength, In::PRIMARY)
            .map(|f| f.display_value().to_string());

        // DateTimeOriginal -> taken_at. EXIF uses "YYYY:MM:DD HH:MM:SS"; normalise
        // the date separators and the space so it sorts/parses like the rest.
        if let Some(dt) = string_field(&reader, Tag::DateTimeOriginal) {
            let norm = normalize_exif_datetime(&dt);
            if !norm.is_empty() {
                out.taken_at = norm;
            }
        }

        // GPS coordinates.
        out.lat = reader
            .get_field(Tag::GPSLatitude, In::PRIMARY)
            .map(|f| f.display_value().to_string());
        out.lng = reader
            .get_field(Tag::GPSLongitude, In::PRIMARY)
            .map(|f| f.display_value().to_string());
    }

    out
}

/// Normalise an EXIF datetime ("2008:05:30 15:56:01" or "2008-05-30 15:56:01")
/// to "2008-05-30T15:56:01" so it sorts/groups like the RFC3339 seed values.
fn normalize_exif_datetime(s: &str) -> String {
    let s = s.trim();
    if s.len() < 19 {
        return s.replace(' ', "T");
    }
    let (date, rest) = s.split_at(10);
    let date = date.replace(':', "-");
    format!("{date}{}", rest.replacen(' ', "T", 1))
}

/// Read an ASCII/string EXIF field as a plain `String`, if present.
fn string_field(reader: &exif::Exif, tag: Tag) -> Option<String> {
    let f = reader.get_field(tag, In::PRIMARY)?;
    match &f.value {
        Value::Ascii(_) => {
            let s = f.display_value().to_string();
            // kamadak wraps ASCII display values in quotes; strip them.
            let s = s.trim_matches('"').to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        _ => {
            let s = f.display_value().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a 2x2 RGB JPEG in-memory and assert dimensions come back as 2x2.
    /// EXIF tags will be absent (that's fine); the test asserts no panic and
    /// correct width/height.
    #[test]
    fn extract_reads_dimensions_from_tiny_jpeg() {
        use image::{ImageFormat, RgbImage};
        let mut img = RgbImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 255, 0]));
        img.put_pixel(0, 1, image::Rgb([0, 0, 255]));
        img.put_pixel(1, 1, image::Rgb([255, 255, 0]));

        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, ImageFormat::Jpeg)
            .expect("encode jpeg");
        let bytes = buf.into_inner();

        let exif = extract(&bytes, "tiny.jpg");
        assert_eq!(exif.width, 2);
        assert_eq!(exif.height, 2);
        // EXIF tags absent on a synthetic JPEG.
        assert!(exif.camera.is_none());
    }
}
