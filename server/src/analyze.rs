//! AI ANALYSIS — the 4th import stage (Upload → EXIF → Thumbnail → AI analysis).
//!
//! This stage groups three kinds of DERIVED, non-authoritative metadata:
//!   * OCR text (text recognized inside the image),
//!   * face / people detection,
//!   * context / scene tags (e.g. "night", "telephoto", "geotagged").
//!
//! RATIONALE — no heavy ML / pluggable backend:
//! Real OCR and face-detection rely on large neural-network model files
//! (hundreds of MB) that cannot be assumed present in an offline / sandboxed
//! build, and pulling an ML toolchain in as a hard dependency would bloat the
//! server and break dependency-light, build-anywhere goals (the same principle
//! as `extract.rs` refusing to shell out to ImageMagick). So analysis is kept
//! behind the [`Analyzer`] trait with a real, dependency-light
//! [`HeuristicAnalyzer`] implementation shipped now. The heuristic derives
//! useful context/scene tags purely from already-extracted EXIF + the photo
//! kind (no pixels, no models). OCR and face detection are left as documented
//! stubs (return `None` / empty) with a clear extension point: a future
//! `ocrs`-based OCR backend or a face-net backend implements [`Analyzer`] and is
//! swapped in at the call site in `state.rs` WITHOUT touching the pipeline or
//! the data model.

use crate::models::Photo;

/// The result of running the AI-analysis stage over one photo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiAnalysis {
    /// Recognized text (OCR). `None` when no text was found / no OCR backend.
    pub ocr_text: Option<String>,
    /// Context / scene labels (machine-generated, deduplicated, ordered).
    pub tags: Vec<String>,
    /// Detected people / faces. Empty without a face-detection backend.
    pub people: Vec<String>,
}

/// Pluggable analyzer. `bytes` is the (decoded) thumbnail/source bytes when
/// available (e.g. for OCR / face nets); it is `None` when no bytes are stored
/// (the demo seed and synthetic ingest). Implementations MUST be cheap and must
/// never panic on missing data.
pub trait Analyzer {
    fn analyze(&self, bytes: Option<&[u8]>, photo: &Photo) -> AiAnalysis;
}

/// Pure-Rust, dependency-light analyzer. Derives context/scene tags from EXIF +
/// kind only — NO ML, NO pixel inspection, NO model files. OCR and face
/// detection are intentionally stubbed (see module docs); the `_bytes` argument
/// is where a real OCR/face backend would read the image.
pub struct HeuristicAnalyzer;

/// Parse the leading number out of an EXIF-style numeric string such as
/// "50 mm", "50mm", "1/250", "f/4.0". Returns the first parseable f64.
fn leading_number(s: &str) -> Option<f64> {
    let trimmed = s.trim().trim_start_matches("f/").trim();
    let num: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse().ok()
}

/// Parse a shutter speed string ("1/250", "0.5", "2", `"2 s"`) into seconds.
fn shutter_seconds(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('s').trim();
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.trim().parse().ok()?;
        let d: f64 = d.trim().parse().ok()?;
        if d == 0.0 {
            return None;
        }
        Some(n / d)
    } else {
        s.parse().ok()
    }
}

impl Analyzer for HeuristicAnalyzer {
    fn analyze(&self, _bytes: Option<&[u8]>, photo: &Photo) -> AiAnalysis {
        let mut tags: Vec<String> = Vec::new();
        let push = |t: &str, tags: &mut Vec<String>| {
            if !tags.iter().any(|x| x == t) {
                tags.push(t.to_string());
            }
        };

        let e = &photo.exif;

        // Kind-derived tags.
        match photo.kind.as_str() {
            "raw" => push("raw", &mut tags),
            "video" => push("video", &mut tags),
            _ => {}
        }

        // Aspect: portrait vs landscape (square falls through to neither).
        if e.width > 0 && e.height > 0 {
            if e.height > e.width {
                push("portrait", &mut tags);
            } else if e.width > e.height {
                push("landscape", &mut tags);
            }
        }

        // ISO: high ISO suggests low-light / night.
        if let Some(iso) = e.iso {
            if iso >= 1600 {
                push("night", &mut tags);
                push("high-iso", &mut tags);
            }
        }

        // Shutter: slow shutter suggests long-exposure (and low light / night).
        if let Some(sh) = e.shutter.as_deref().and_then(shutter_seconds) {
            if sh >= 1.0 {
                push("long-exposure", &mut tags);
                push("night", &mut tags);
            }
        }

        // Focal length: telephoto vs wide.
        if let Some(focal) = e.focal.as_deref().and_then(leading_number) {
            if focal >= 100.0 {
                push("telephoto", &mut tags);
            } else if focal > 0.0 && focal <= 28.0 {
                push("wide", &mut tags);
            }
        }

        // GPS presence.
        if e.lat.is_some() && e.lng.is_some() {
            push("geotagged", &mut tags);
        }

        // ---- OCR + faces: documented stubs ----
        // A real backend slots in HERE: e.g. decode `_bytes` and run `ocrs` for
        // OCR, or a face-detection net for `people`. We deliberately do not add
        // a model-downloading dependency, so both stay empty for now.
        let ocr_text: Option<String> = None;
        let people: Vec<String> = Vec::new();

        AiAnalysis {
            ocr_text,
            tags,
            people,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Exif, Photo};

    fn base_photo(kind: &str) -> Photo {
        Photo {
            id: "ph_t".to_string(),
            owner_id: "usr_a".to_string(),
            filename: "x.jpg".to_string(),
            seed: 1,
            kind: kind.to_string(),
            exif: Exif {
                taken_at: "2026-01-01T00:00:00Z".to_string(),
                width: 6000,
                height: 4000,
                ..Default::default()
            },
            overrides: Default::default(),
            companions: Vec::new(),
            archived: false,
            deleted_at: None,
            backed_up: false,
            thumb_url: None,
            size_mb: 1.0,
            ocr_text: None,
            ai_tags: Vec::new(),
            ai_people: Vec::new(),
            analyzed: false,
            clip_embedding: None,
            full_url: None,
        }
    }

    #[test]
    fn raw_photo_gets_raw_tag() {
        let p = base_photo("raw");
        let r = HeuristicAnalyzer.analyze(None, &p);
        assert!(r.tags.contains(&"raw".to_string()));
        assert!(r.tags.contains(&"landscape".to_string()));
    }

    #[test]
    fn slow_shutter_high_iso_is_night_long_exposure() {
        let mut p = base_photo("photo");
        p.exif.iso = Some(3200);
        p.exif.shutter = Some("2".to_string()); // 2 seconds
        let r = HeuristicAnalyzer.analyze(None, &p);
        assert!(r.tags.contains(&"night".to_string()));
        assert!(r.tags.contains(&"high-iso".to_string()));
        assert!(r.tags.contains(&"long-exposure".to_string()));
        // night must appear only once even though two rules add it.
        assert_eq!(r.tags.iter().filter(|t| *t == "night").count(), 1);
    }

    #[test]
    fn geotagged_when_gps_present() {
        let mut p = base_photo("photo");
        p.exif.lat = Some("45.76° N".to_string());
        p.exif.lng = Some("4.83° E".to_string());
        let r = HeuristicAnalyzer.analyze(None, &p);
        assert!(r.tags.contains(&"geotagged".to_string()));
    }

    #[test]
    fn telephoto_and_wide_from_focal() {
        let mut tele = base_photo("photo");
        tele.exif.focal = Some("200 mm".to_string());
        assert!(HeuristicAnalyzer
            .analyze(None, &tele)
            .tags
            .contains(&"telephoto".to_string()));

        let mut wide = base_photo("photo");
        wide.exif.focal = Some("16mm".to_string());
        assert!(HeuristicAnalyzer
            .analyze(None, &wide)
            .tags
            .contains(&"wide".to_string()));
    }

    #[test]
    fn ocr_and_people_are_stubbed_empty() {
        let p = base_photo("photo");
        let r = HeuristicAnalyzer.analyze(None, &p);
        assert!(r.ocr_text.is_none());
        assert!(r.people.is_empty());
    }
}
