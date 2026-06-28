//! Transcoding engine + device-aware format/resolution negotiation.
//!
//! Goal: deliver each device the right RESOLUTION and FORMAT. If a device can't
//! play an mp4, serve a container/codec it supports (MOV/MKV/WebM); for images,
//! resize and convert (WebP/JPEG/PNG).
//!
//! IMAGES are transcoded with the pure-Rust `image` crate ONLY (never
//! ImageMagick). The `image` 0.25 `webp` feature is enabled and CAN encode
//! WebP, so no extra `webp` crate is needed. AVIF encoding is intentionally NOT
//! pulled in (it would require the heavy `ravif` toolchain): an `Avif` request
//! is mapped down to WebP/JPEG inside [`negotiate`] (see `prefer_image`).
//!
//! VIDEO transcoding wraps the EXTERNAL `ffmpeg` binary via
//! `std::process::Command` (a documented external dependency). When the
//! `ffmpeg` binary is not on `PATH`, [`RealTranscoder::transcode_video`] returns
//! [`TranscodeError::VideoToolMissing`] instead of panicking, so the demo and
//! tests never require ffmpeg to be installed.

use std::io::Cursor;

use image::{ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};

/// A media container/codec format we can negotiate, encode or describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
    Mp4,
    Mov,
    Mkv,
    Webm,
}

impl MediaFormat {
    /// Best-effort mapping from a file extension (case-insensitive).
    pub fn from_ext(ext: &str) -> Option<MediaFormat> {
        match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "jpe" => Some(MediaFormat::Jpeg),
            "png" => Some(MediaFormat::Png),
            "webp" => Some(MediaFormat::Webp),
            "avif" => Some(MediaFormat::Avif),
            "mp4" | "m4v" => Some(MediaFormat::Mp4),
            "mov" => Some(MediaFormat::Mov),
            "mkv" => Some(MediaFormat::Mkv),
            "webm" => Some(MediaFormat::Webm),
            _ => None,
        }
    }

    /// The IANA MIME type for this format.
    pub fn mime(&self) -> &'static str {
        match self {
            MediaFormat::Jpeg => "image/jpeg",
            MediaFormat::Png => "image/png",
            MediaFormat::Webp => "image/webp",
            MediaFormat::Avif => "image/avif",
            MediaFormat::Mp4 => "video/mp4",
            MediaFormat::Mov => "video/quicktime",
            MediaFormat::Mkv => "video/x-matroska",
            MediaFormat::Webm => "video/webm",
        }
    }

    /// The canonical lowercase file extension for this format.
    pub fn ext(&self) -> &'static str {
        match self {
            MediaFormat::Jpeg => "jpg",
            MediaFormat::Png => "png",
            MediaFormat::Webp => "webp",
            MediaFormat::Avif => "avif",
            MediaFormat::Mp4 => "mp4",
            MediaFormat::Mov => "mov",
            MediaFormat::Mkv => "mkv",
            MediaFormat::Webm => "webm",
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(
            self,
            MediaFormat::Jpeg | MediaFormat::Png | MediaFormat::Webp | MediaFormat::Avif
        )
    }

    pub fn is_video(&self) -> bool {
        matches!(
            self,
            MediaFormat::Mp4 | MediaFormat::Mov | MediaFormat::Mkv | MediaFormat::Webm
        )
    }

    /// The `image` crate `ImageFormat` we encode to. AVIF is intentionally
    /// unsupported for encoding here (mapped away in `negotiate`); video formats
    /// have no `image` equivalent.
    fn image_format(&self) -> Option<ImageFormat> {
        match self {
            MediaFormat::Jpeg => Some(ImageFormat::Jpeg),
            MediaFormat::Png => Some(ImageFormat::Png),
            MediaFormat::Webp => Some(ImageFormat::WebP),
            _ => None,
        }
    }

    /// Map an IANA MIME type back to a [`MediaFormat`] (inverse of [`Self::mime`]).
    /// Used to recover a canonical file extension for a stored blob's content type.
    pub fn from_mime(mime: &str) -> Option<MediaFormat> {
        Self::from_mime_token(mime)
    }

    /// Match an HTTP Accept token (e.g. "image/webp", "video/mp4").
    fn from_mime_token(token: &str) -> Option<MediaFormat> {
        let t = token.trim().split(';').next().unwrap_or("").trim();
        match t {
            "image/jpeg" | "image/jpg" => Some(MediaFormat::Jpeg),
            "image/png" => Some(MediaFormat::Png),
            "image/webp" => Some(MediaFormat::Webp),
            "image/avif" => Some(MediaFormat::Avif),
            "video/mp4" => Some(MediaFormat::Mp4),
            "video/quicktime" => Some(MediaFormat::Mov),
            "video/x-matroska" => Some(MediaFormat::Mkv),
            "video/webm" => Some(MediaFormat::Webm),
            _ => None,
        }
    }
}

/// What a device can play, plus optional max display dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub supported: Vec<MediaFormat>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub max_height: Option<u32>,
}

impl DeviceProfile {
    /// A modern browser: webp + the usual rasters, webm + mp4 for video.
    pub fn modern_web() -> Self {
        Self {
            supported: vec![
                MediaFormat::Webp,
                MediaFormat::Jpeg,
                MediaFormat::Png,
                MediaFormat::Webm,
                MediaFormat::Mp4,
            ],
            max_width: None,
            max_height: None,
        }
    }

    /// An older/limited device: only baseline rasters + QuickTime video.
    /// Exposed as a preset for callers/tests; not referenced by a route yet.
    #[allow(dead_code)]
    pub fn legacy() -> Self {
        Self {
            supported: vec![MediaFormat::Jpeg, MediaFormat::Png, MediaFormat::Mov],
            max_width: None,
            max_height: None,
        }
    }

    /// Build a profile from an HTTP `Accept` header value plus optional query
    /// params. Precedence for the supported-format list:
    ///   1. an explicit `supports=webp,mp4,...` comma list, else
    ///   2. formats parsed from the `Accept` header, else
    ///   3. the `modern_web` preset.
    /// `?w=`/`?h=` set the max dimensions; an explicit `?fmt=` is always
    /// guaranteed to be in the supported list (so the caller's preference wins).
    pub fn from_request(
        accept: Option<&str>,
        supports: Option<&str>,
        fmt: Option<MediaFormat>,
        w: Option<u32>,
        h: Option<u32>,
    ) -> Self {
        let mut supported: Vec<MediaFormat> = Vec::new();

        if let Some(list) = supports {
            for tok in list.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                // accept either bare names ("webp") or mime tokens ("image/webp")
                let f = MediaFormat::from_ext(tok).or_else(|| MediaFormat::from_mime_token(tok));
                if let Some(f) = f {
                    if !supported.contains(&f) {
                        supported.push(f);
                    }
                }
            }
        }

        if supported.is_empty() {
            if let Some(accept) = accept {
                for tok in accept.split(',') {
                    if let Some(f) = MediaFormat::from_mime_token(tok) {
                        if !supported.contains(&f) {
                            supported.push(f);
                        }
                    }
                }
            }
        }

        if supported.is_empty() {
            supported = DeviceProfile::modern_web().supported;
        }

        // An explicitly requested format must be considered supported.
        if let Some(f) = fmt {
            if !supported.contains(&f) {
                supported.insert(0, f);
            }
        }

        Self {
            supported,
            max_width: w,
            max_height: h,
        }
    }

    fn supports(&self, f: MediaFormat) -> bool {
        self.supported.contains(&f)
    }
}

/// A resolved plan: what to encode, at which dimensions, from what source, and
/// whether any work is actually required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscodePlan {
    pub format: MediaFormat,
    pub width: u32,
    pub height: u32,
    pub source_format: MediaFormat,
    pub needs_transcode: bool,
}

/// Clamp (w, h) into (max_w, max_h) preserving aspect ratio. A `None` bound is
/// treated as unbounded. Never upscales; result is at least 1x1.
fn clamp_dims(w: u32, h: u32, max_w: Option<u32>, max_h: Option<u32>) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }
    // Scale factor needed to fit each bound (>= 1.0 means no shrink needed).
    let mut scale = 1.0f64;
    if let Some(mw) = max_w {
        if w > mw {
            scale = scale.min(mw as f64 / w as f64);
        }
    }
    if let Some(mh) = max_h {
        if h > mh {
            scale = scale.min(mh as f64 / h as f64);
        }
    }
    if scale >= 1.0 {
        return (w, h);
    }
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    (nw, nh)
}

/// Pick the best supported IMAGE target, preferring webp, then jpeg, then png,
/// then anything else supported & image. AVIF maps down to webp/jpeg because we
/// don't encode AVIF.
fn prefer_image(device: &DeviceProfile) -> Option<MediaFormat> {
    for cand in [MediaFormat::Webp, MediaFormat::Jpeg, MediaFormat::Png] {
        if device.supports(cand) {
            return Some(cand);
        }
    }
    device
        .supported
        .iter()
        .copied()
        .find(|f| f.is_image() && *f != MediaFormat::Avif)
}

/// Pick the best supported VIDEO target, preferring webm, then mov, then mkv,
/// then mp4, then anything else supported & video.
fn prefer_video(device: &DeviceProfile) -> Option<MediaFormat> {
    for cand in [
        MediaFormat::Webm,
        MediaFormat::Mov,
        MediaFormat::Mkv,
        MediaFormat::Mp4,
    ] {
        if device.supports(cand) {
            return Some(cand);
        }
    }
    device.supported.iter().copied().find(|f| f.is_video())
}

/// Negotiate the delivery plan for `source` (at `src_w` x `src_h`) against a
/// `device`.
///
/// - If the source format is already supported AND within the device's max
///   dimensions → `needs_transcode = false`, keeping the source format & dims.
/// - If the source format is supported but OVERSIZED → same format, clamped
///   dims, `needs_transcode = true`.
/// - If the source format is NOT supported → pick the best supported target of
///   the same media kind (image/video) and clamp dims; `needs_transcode = true`.
///   When no same-kind target exists the source is returned unchanged as a
///   last-resort (`needs_transcode = false`).
pub fn negotiate(
    source: MediaFormat,
    src_w: u32,
    src_h: u32,
    device: &DeviceProfile,
) -> TranscodePlan {
    let (clamped_w, clamped_h) = clamp_dims(src_w, src_h, device.max_width, device.max_height);
    let oversized = (clamped_w, clamped_h) != (src_w, src_h);

    if device.supports(source) {
        // Source already playable; only resize if it exceeds the bounds.
        return TranscodePlan {
            format: source,
            width: clamped_w,
            height: clamped_h,
            source_format: source,
            needs_transcode: oversized,
        };
    }

    // Source not supported: choose a same-kind target.
    let target = if source.is_image() {
        prefer_image(device)
    } else {
        prefer_video(device)
    };

    match target {
        Some(t) => TranscodePlan {
            format: t,
            width: clamped_w,
            height: clamped_h,
            source_format: source,
            needs_transcode: true,
        },
        // No compatible target at all: hand back the source untouched.
        None => TranscodePlan {
            format: source,
            width: clamped_w,
            height: clamped_h,
            source_format: source,
            needs_transcode: oversized,
        },
    }
}

/// Errors produced while actually transcoding bytes. The video-tool variants
/// are part of the engine's public surface (used by `transcode_video`); they
/// are not constructed by the image-only HTTP path.
#[allow(dead_code)]
#[derive(Debug)]
pub enum TranscodeError {
    /// The image bytes could not be decoded.
    Decode(String),
    /// Encoding to the target format failed.
    Encode(String),
    /// The target format is not an encodable image (e.g. AVIF / video).
    UnsupportedTarget(MediaFormat),
    /// The external `ffmpeg` binary was not found on PATH.
    VideoToolMissing,
    /// ffmpeg ran but failed.
    VideoToolFailed(String),
    /// I/O error talking to the external tool.
    Io(String),
}

impl std::fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscodeError::Decode(e) => write!(f, "decode error: {e}"),
            TranscodeError::Encode(e) => write!(f, "encode error: {e}"),
            TranscodeError::UnsupportedTarget(fmt) => {
                write!(f, "unsupported transcode target: {fmt:?}")
            }
            TranscodeError::VideoToolMissing => {
                write!(f, "ffmpeg binary not found on PATH")
            }
            TranscodeError::VideoToolFailed(e) => write!(f, "ffmpeg failed: {e}"),
            TranscodeError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

/// Produces actual transcoded bytes. Kept behind a trait so a fake can be
/// slotted in for tests / different toolchains.
pub trait Transcoder {
    fn transcode_image(&self, bytes: &[u8], plan: &TranscodePlan) -> Result<Vec<u8>, TranscodeError>;
    /// Wraps the external `ffmpeg` binary. Part of the engine API; no HTTP route
    /// exercises it (the demo stores no video bytes), so it is unused by main.
    #[allow(dead_code)]
    fn transcode_video(
        &self,
        input_path: &str,
        plan: &TranscodePlan,
    ) -> Result<Vec<u8>, TranscodeError>;
}

/// The real engine: `image` for rasters, `ffmpeg` (via `Command`) for video.
pub struct RealTranscoder;

impl Transcoder for RealTranscoder {
    /// Decode `bytes` (auto-detecting the source format), resize to the plan
    /// dimensions, and encode to the plan's target format using the pure-Rust
    /// `image` crate. Never shells out.
    fn transcode_image(
        &self,
        bytes: &[u8],
        plan: &TranscodePlan,
    ) -> Result<Vec<u8>, TranscodeError> {
        let target = plan
            .format
            .image_format()
            .ok_or(TranscodeError::UnsupportedTarget(plan.format))?;

        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| TranscodeError::Decode(e.to_string()))?;
        let img = reader
            .decode()
            .map_err(|e| TranscodeError::Decode(e.to_string()))?;

        // Resize (downscale) to the planned dims. `thumbnail` preserves aspect
        // ratio and is fast; we pass the already-clamped plan dims.
        let w = plan.width.max(1);
        let h = plan.height.max(1);
        let resized = img.thumbnail(w, h);

        let mut out = Cursor::new(Vec::new());
        resized
            .write_to(&mut out, target)
            .map_err(|e| TranscodeError::Encode(e.to_string()))?;
        Ok(out.into_inner())
    }

    /// Transcode a video file on disk to the plan's container/dimensions using
    /// the external `ffmpeg` binary, reading the result from its stdout.
    /// Returns [`TranscodeError::VideoToolMissing`] when ffmpeg is absent.
    fn transcode_video(
        &self,
        input_path: &str,
        plan: &TranscodePlan,
    ) -> Result<Vec<u8>, TranscodeError> {
        use std::process::Command;

        // The container ffmpeg should mux to.
        let container = match plan.format {
            MediaFormat::Mp4 => "mp4",
            MediaFormat::Mov => "mov",
            MediaFormat::Mkv => "matroska",
            MediaFormat::Webm => "webm",
            other => return Err(TranscodeError::UnsupportedTarget(other)),
        };

        // e.g. ffmpeg -i input -vf scale=w:h -f <container> pipe:1
        let scale = format!("scale={}:{}", plan.width, plan.height);
        let output = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-i",
                input_path,
                "-vf",
                &scale,
                "-f",
                container,
                "pipe:1",
            ])
            .output();

        let output = match output {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(TranscodeError::VideoToolMissing);
            }
            Err(e) => return Err(TranscodeError::Io(e.to_string())),
        };

        if !output.status.success() {
            return Err(TranscodeError::VideoToolFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(output.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_ext_and_mime_helpers() {
        assert_eq!(MediaFormat::from_ext("JPG"), Some(MediaFormat::Jpeg));
        assert_eq!(MediaFormat::from_ext(".webp"), Some(MediaFormat::Webp));
        assert_eq!(MediaFormat::from_ext("mkv"), Some(MediaFormat::Mkv));
        assert_eq!(MediaFormat::from_ext("xyz"), None);
        assert_eq!(MediaFormat::Webp.mime(), "image/webp");
        assert_eq!(MediaFormat::Mov.mime(), "video/quicktime");
        assert!(MediaFormat::Png.is_image());
        assert!(MediaFormat::Webm.is_video());
        assert!(!MediaFormat::Webm.is_image());
    }

    #[test]
    fn serde_is_lowercase() {
        let j = serde_json::to_string(&MediaFormat::Webp).unwrap();
        assert_eq!(j, "\"webp\"");
        let f: MediaFormat = serde_json::from_str("\"mp4\"").unwrap();
        assert_eq!(f, MediaFormat::Mp4);
    }

    #[test]
    fn negotiate_supported_within_bounds_no_transcode() {
        // jpeg into a modern web device with no size limit -> no work.
        let dev = DeviceProfile::modern_web();
        let plan = negotiate(MediaFormat::Jpeg, 1200, 800, &dev);
        assert_eq!(plan.format, MediaFormat::Jpeg);
        assert_eq!((plan.width, plan.height), (1200, 800));
        assert!(!plan.needs_transcode);
    }

    #[test]
    fn negotiate_oversized_supported_format_clamps_same_format() {
        let mut dev = DeviceProfile::modern_web();
        dev.max_width = Some(600);
        dev.max_height = Some(600);
        let plan = negotiate(MediaFormat::Jpeg, 1200, 800, &dev);
        // same format, but clamped (1200x800 -> fit in 600x600 keeping aspect).
        assert_eq!(plan.format, MediaFormat::Jpeg);
        assert!(plan.needs_transcode);
        assert!(plan.width <= 600 && plan.height <= 600);
        // aspect ratio 3:2 preserved -> 600x400.
        assert_eq!((plan.width, plan.height), (600, 400));
    }

    #[test]
    fn negotiate_image_downscale_picks_webp() {
        // A png source on a modern device, downscaled. Source png IS supported,
        // so it stays png; to assert webp selection we use an unsupported source.
        let dev = DeviceProfile {
            supported: vec![MediaFormat::Webp, MediaFormat::Jpeg],
            max_width: Some(50),
            max_height: None,
        };
        // AVIF source is not encodable/supported -> must pick webp (preferred).
        let plan = negotiate(MediaFormat::Avif, 100, 80, &dev);
        assert_eq!(plan.format, MediaFormat::Webp);
        assert!(plan.needs_transcode);
        // 100x80 clamped to width 50 -> 50x40.
        assert_eq!((plan.width, plan.height), (50, 40));
    }

    #[test]
    fn negotiate_unsupported_mp4_falls_back_to_first_supported_video() {
        // Legacy device cannot play mp4; only mov among video formats.
        let dev = DeviceProfile::legacy();
        let plan = negotiate(MediaFormat::Mp4, 1920, 1080, &dev);
        assert_eq!(plan.format, MediaFormat::Mov);
        assert!(plan.needs_transcode);
    }

    #[test]
    fn negotiate_unsupported_video_prefers_webm_on_modern() {
        let dev = DeviceProfile::modern_web();
        // mkv unsupported on modern_web -> webm preferred over mp4.
        let plan = negotiate(MediaFormat::Mkv, 1920, 1080, &dev);
        assert_eq!(plan.format, MediaFormat::Webm);
        assert!(plan.needs_transcode);
    }

    #[test]
    fn from_request_supports_list_wins() {
        let dev = DeviceProfile::from_request(
            Some("image/jpeg"),
            Some("webp,mp4"),
            None,
            Some(800),
            None,
        );
        assert_eq!(dev.supported, vec![MediaFormat::Webp, MediaFormat::Mp4]);
        assert_eq!(dev.max_width, Some(800));
    }

    #[test]
    fn from_request_explicit_fmt_is_supported() {
        let dev = DeviceProfile::from_request(None, Some("jpeg"), Some(MediaFormat::Png), None, None);
        assert!(dev.supported.contains(&MediaFormat::Png));
    }

    #[test]
    fn from_request_parses_accept_header() {
        let dev =
            DeviceProfile::from_request(Some("image/webp,image/jpeg;q=0.8"), None, None, None, None);
        assert!(dev.supported.contains(&MediaFormat::Webp));
        assert!(dev.supported.contains(&MediaFormat::Jpeg));
    }

    #[test]
    fn real_transcode_image_to_webp_downscales() {
        // Generate a 100x80 RGB image, encode to PNG bytes as the source.
        let mut src = image::RgbImage::new(100, 80);
        for (x, _y, px) in src.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, 100, 200]);
        }
        let mut png_bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(src)
            .write_to(&mut png_bytes, ImageFormat::Png)
            .unwrap();
        let png_bytes = png_bytes.into_inner();

        let plan = TranscodePlan {
            format: MediaFormat::Webp,
            width: 50,
            height: 40,
            source_format: MediaFormat::Png,
            needs_transcode: true,
        };
        let out = RealTranscoder
            .transcode_image(&png_bytes, &plan)
            .expect("transcode to webp");
        assert!(!out.is_empty());

        // The output must decode back, and be ~50px wide.
        let decoded = image::load_from_memory(&out).expect("decode webp output");
        use image::GenericImageView;
        let (w, _h) = decoded.dimensions();
        assert!((40..=50).contains(&w), "expected ~50px wide, got {w}");
    }

    #[test]
    fn transcode_image_rejects_video_target() {
        let plan = TranscodePlan {
            format: MediaFormat::Mp4,
            width: 10,
            height: 10,
            source_format: MediaFormat::Jpeg,
            needs_transcode: true,
        };
        let err = RealTranscoder.transcode_image(&[0u8; 4], &plan).unwrap_err();
        assert!(matches!(err, TranscodeError::UnsupportedTarget(_)));
    }
}
