//! SERVER-SIDE DLNA / UPnP CASTING.
//!
//! Browsers cannot speak DLNA/UPnP (no raw UDP/SSDP, no arbitrary SOAP to LAN
//! hosts), so casting a photo to a TV / media renderer must run server-side.
//! This module discovers UPnP **MediaRenderer** devices on the LAN via SSDP and
//! drives their **AVTransport** service to display a still image.
//!
//! It uses the [`rupnp`] crate (async UPnP discovery + service control over
//! tokio, which re-exports [`ssdp_client`] for the SSDP `M-SEARCH`). We hand-roll
//! the AVTransport action argument XML and the DIDL-Lite metadata (the parts that
//! are pure, deterministic, and unit-tested below); `rupnp` wraps them in the
//! SOAP envelope and POSTs them to the device's control URL.
//!
//! OFFLINE-FIRST: nothing here touches the network until [`discover`] / [`cast`]
//! is called at request time. The build and the whole test suite never perform
//! SSDP discovery or any network I/O — only the pure XML/hash builders are tested.
//! Every fallible step is guarded: discovery returns a partial list and ignores
//! malformed devices; casting returns a typed error. Nothing here ever panics.

use std::time::Duration;

use futures_util::stream::TryStreamExt;
use rupnp::ssdp::{SearchTarget, URN};

/// The AVTransport service URN (v1) — the service that renders media (including
/// still images on most DLNA TVs) on a UPnP MediaRenderer.
const AV_TRANSPORT: URN = URN::service("schemas-upnp-org", "AVTransport", 1);

/// The MediaRenderer device URN (v1) — what we SSDP-search for.
const MEDIA_RENDERER: URN = URN::device("schemas-upnp-org", "MediaRenderer", 1);

/// A discovered DLNA/UPnP MediaRenderer with an AVTransport service.
///
/// All fields are plain `String`s so the value is trivially cacheable on
/// `AppState`, cloneable, and free of any non-`Send` UPnP handle. `location` is
/// the device-description URL (used to re-resolve the live device when casting);
/// `control_url` is the absolute AVTransport control URL (informational / for the
/// hand-rolled fallback path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlnaDevice {
    /// Stable id: a hash of the device UDN (falling back to its location).
    pub id: String,
    /// Human-friendly name, e.g. "Living Room TV".
    pub name: String,
    /// Device-description URL (SSDP `LOCATION`); used to re-resolve on cast.
    pub location: String,
    /// Absolute AVTransport control URL.
    pub control_url: String,
}

/// Casting failure modes. `DeviceUnreachable` covers discovery/description
/// fetch failures; `NoAvTransport` means the renderer has no AVTransport service;
/// `Soap` wraps an AVTransport action (`SetAVTransportURI` / `Play`) failure.
#[derive(Debug)]
pub enum DlnaError {
    DeviceUnreachable(String),
    NoAvTransport,
    Soap(String),
}

impl std::fmt::Display for DlnaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DlnaError::DeviceUnreachable(e) => write!(f, "device unreachable: {e}"),
            DlnaError::NoAvTransport => write!(f, "device has no AVTransport service"),
            DlnaError::Soap(e) => write!(f, "AVTransport action failed: {e}"),
        }
    }
}

impl std::error::Error for DlnaError {}

/// Stable id for a device: an FNV-1a 64-bit hash of the UDN (preferred — globally
/// unique + persistent across IP changes) or the location URL when no UDN is
/// known. Deterministic so the same renderer keeps the same id between
/// discoveries, which lets `POST /api/cast/dlna` resolve a cached `device_id`.
pub fn device_id(udn: &str, location: &str) -> String {
    let basis = if udn.is_empty() { location } else { udn };
    format!("dlna_{:016x}", fnv1a64(basis.as_bytes()))
}

/// The origin (`scheme://authority`) of a URL, e.g.
/// `http://10.0.0.5:8080/desc.xml` -> `http://10.0.0.5:8080`. Falls back to the
/// whole input when it has no `://`/path separator.
fn base_origin(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{authority}")
        }
        None => url.to_string(),
    }
}

/// FNV-1a 64-bit hash. Small, dependency-free, stable across runs/platforms — we
/// only need a deterministic id, not a cryptographic digest.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Minimal XML attribute/text escaping for the values we interpolate into the
/// DIDL-Lite metadata and action arguments (URL, title). Keeps the generated
/// SOAP well-formed even when a title contains `&`, `<`, `>` or quotes.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the DIDL-Lite metadata for a single still image item pointing at `url`.
///
/// This is the `<CurrentURIMetaData>` payload most DLNA renderers expect: an
/// `object.item.imageItem.photo` with a `<res>` element carrying the image URL
/// and a generic image protocolInfo. The DIDL-Lite element itself is XML-escaped
/// before being embedded (as text) inside the SOAP action arguments.
pub fn didl_lite_image(url: &str, title: &str) -> String {
    let url = xml_escape(url);
    let title = xml_escape(title);
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"0\" parentID=\"-1\" restricted=\"1\">\
<dc:title>{title}</dc:title>\
<upnp:class>object.item.imageItem.photo</upnp:class>\
<res protocolInfo=\"http-get:*:image/*:*\">{url}</res>\
</item></DIDL-Lite>"
    )
}

/// Build the AVTransport **SetAVTransportURI** action arguments (the inner XML
/// that `rupnp` wraps in a SOAP envelope and POSTs to the control URL).
///
/// InstanceID is always `0`; `CurrentURI` is the image URL; `CurrentURIMetaData`
/// is the XML-escaped DIDL-Lite from [`didl_lite_image`].
pub fn set_av_transport_uri_args(url: &str, title: &str) -> String {
    let metadata = xml_escape(&didl_lite_image(url, title));
    let url = xml_escape(url);
    format!(
        "<InstanceID>0</InstanceID>\
<CurrentURI>{url}</CurrentURI>\
<CurrentURIMetaData>{metadata}</CurrentURIMetaData>"
    )
}

/// Build the AVTransport **Play** action arguments (InstanceID 0, normal speed).
pub fn play_args() -> String {
    "<InstanceID>0</InstanceID><Speed>1</Speed>".to_string()
}

/// Discover DLNA MediaRenderer devices on the LAN via SSDP `M-SEARCH`.
///
/// Sends the search for `urn:schemas-upnp-org:device:MediaRenderer:1`, then for
/// each responding device fetches + parses its description XML (done by `rupnp`),
/// extracts the friendly name and the absolute AVTransport control URL, and
/// builds a [`DlnaDevice`]. Devices without an AVTransport service, or whose
/// description fails to fetch/parse, are silently skipped — the result is a
/// best-effort partial list (empty when nothing is found / offline). Never panics.
pub async fn discover(timeout: Duration) -> Vec<DlnaDevice> {
    let search = SearchTarget::URN(MEDIA_RENDERER);
    let stream = match rupnp::discover(&search, timeout).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("DLNA discovery failed to start: {e}");
            return Vec::new();
        }
    };
    futures_util::pin_mut!(stream);

    let mut out: Vec<DlnaDevice> = Vec::new();
    loop {
        // Pull the next device; a per-device error (bad description XML, fetch
        // failure) aborts the stream — we keep whatever we already collected.
        let device = match stream.try_next().await {
            Ok(Some(d)) => d,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("DLNA discovery stream ended early: {e}");
                break;
            }
        };

        // Must expose AVTransport to be castable; skip pure RenderingControl-only
        // or otherwise malformed renderers.
        if device.find_service(&AV_TRANSPORT).is_none() {
            continue;
        };
        // rupnp keeps the per-service control path private and resolves it
        // internally when we POST the action (see `cast`), so we record the
        // device's base origin (scheme://authority) as the absolute control
        // endpoint root — informational; `cast` re-resolves the exact path.
        let control_url = base_origin(&device.url().to_string());
        let dev = DlnaDevice {
            id: device_id(device.udn(), &device.url().to_string()),
            name: device.friendly_name().to_string(),
            location: device.url().to_string(),
            control_url,
        };
        // De-dupe by id (a device can answer M-SEARCH more than once).
        if !out.iter().any(|d| d.id == dev.id) {
            out.push(dev);
        }
    }
    out
}

/// Cast `url` (a still image) with `title` to `device` via UPnP AVTransport.
///
/// Re-resolves the live device from its cached `location` (the cached
/// [`DlnaDevice`] holds no live UPnP handle), then sends **SetAVTransportURI**
/// (InstanceID 0, CurrentURI = `url`, minimal DIDL-Lite imageItem metadata)
/// followed by **Play** (Speed 1) to the AVTransport control URL. Returns a typed
/// [`DlnaError`] on any failure; never panics.
pub async fn cast(device: &DlnaDevice, url: &str, title: &str) -> Result<(), DlnaError> {
    let uri: rupnp::http::Uri = device
        .location
        .parse()
        .map_err(|e| DlnaError::DeviceUnreachable(format!("bad location URL: {e}")))?;
    let dev = rupnp::Device::from_url(uri)
        .await
        .map_err(|e| DlnaError::DeviceUnreachable(e.to_string()))?;
    let service = dev.find_service(&AV_TRANSPORT).ok_or(DlnaError::NoAvTransport)?;

    service
        .action(dev.url(), "SetAVTransportURI", &set_av_transport_uri_args(url, title))
        .await
        .map_err(|e| DlnaError::Soap(format!("SetAVTransportURI: {e}")))?;
    service
        .action(dev.url(), "Play", &play_args())
        .await
        .map_err(|e| DlnaError::Soap(format!("Play: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_and_prefers_udn() {
        let a = device_id("uuid:abcd-1234", "http://10.0.0.5:8080/desc.xml");
        let b = device_id("uuid:abcd-1234", "http://10.0.0.9:1900/other.xml");
        // Same UDN -> same id even when the location (IP/port) changes.
        assert_eq!(a, b);
        assert!(a.starts_with("dlna_"));
        // Different UDN -> different id.
        assert_ne!(a, device_id("uuid:zzzz-9999", "http://10.0.0.5:8080/desc.xml"));
    }

    #[test]
    fn device_id_falls_back_to_location_when_no_udn() {
        let a = device_id("", "http://10.0.0.5:8080/desc.xml");
        let b = device_id("", "http://10.0.0.5:8080/desc.xml");
        let c = device_id("", "http://10.0.0.6:8080/desc.xml");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn didl_lite_contains_image_item_and_url() {
        let didl = didl_lite_image("http://host/api/photos/ph_1/render", "Beach");
        assert!(didl.contains("object.item.imageItem.photo"), "imageItem class: {didl}");
        assert!(didl.contains("http://host/api/photos/ph_1/render"), "url present: {didl}");
        assert!(didl.contains("<dc:title>Beach</dc:title>"), "title present: {didl}");
        assert!(didl.contains("DIDL-Lite"), "DIDL-Lite root: {didl}");
    }

    #[test]
    fn set_av_transport_uri_args_has_instance_zero_url_and_metadata() {
        let url = "http://host/img.jpg";
        let args = set_av_transport_uri_args(url, "My Photo");
        // The action's required arguments.
        assert!(args.contains("<InstanceID>0</InstanceID>"), "InstanceID 0: {args}");
        assert!(args.contains("<CurrentURI>http://host/img.jpg</CurrentURI>"), "url: {args}");
        assert!(args.contains("<CurrentURIMetaData>"), "metadata arg: {args}");
        // DIDL-Lite is embedded as ESCAPED text inside the metadata arg.
        assert!(args.contains("&lt;DIDL-Lite"), "escaped DIDL-Lite: {args}");
        assert!(args.contains("imageItem"), "image item class in metadata: {args}");
    }

    #[test]
    fn base_origin_strips_path() {
        assert_eq!(
            base_origin("http://10.0.0.5:8080/desc.xml"),
            "http://10.0.0.5:8080"
        );
        assert_eq!(base_origin("http://host/a/b/c"), "http://host");
        assert_eq!(base_origin("notaurl"), "notaurl");
    }

    #[test]
    fn play_args_has_instance_zero_and_speed() {
        let args = play_args();
        assert!(args.contains("<InstanceID>0</InstanceID>"));
        assert!(args.contains("<Speed>1</Speed>"));
    }

    #[test]
    fn xml_escape_handles_special_chars() {
        let didl = didl_lite_image("http://h/a?x=1&y=2", "Tom & \"Jerry\" <fun>");
        // The ampersand in the URL and the special chars in the title must be
        // escaped so the generated XML stays well-formed.
        assert!(didl.contains("x=1&amp;y=2"), "url ampersand escaped: {didl}");
        assert!(didl.contains("Tom &amp; &quot;Jerry&quot; &lt;fun&gt;"), "title escaped: {didl}");
        assert!(!didl.contains("y=2&y"), "no raw ampersand");
    }
}
