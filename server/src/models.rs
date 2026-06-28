use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: String,
    pub avatar_url: String,
    /// `argon2id` PHC string for the user's password. The hash is computed with
    /// a server-wide secret key (env `PHOTON_PASSWORD_SALT`) AND this user's
    /// per-user random [`pepper`](Self::pepper); see [`User::set_password`].
    /// `None` until the user sets a password (e.g. via reset/accept). NEVER
    /// serialized into any API response (see `#[serde(skip_serializing)]`), so
    /// no response can ever leak it.
    #[serde(skip_serializing, default)]
    pub password_hash: Option<String>,
    /// Legacy per-user salt field, kept for storage/struct compatibility. The
    /// real argon2 salt is derived from [`pepper`](Self::pepper). NEVER serialized.
    #[serde(skip_serializing, default)]
    pub salt: String,
    /// Per-user random pepper, generated at user creation from the OS CSPRNG
    /// (`state::random_hex`). Mixed into the password hash on top
    /// of the server-wide secret key. NEVER serialized — like `password_hash`
    /// and `salt`, it is never returned by any API.
    #[serde(skip_serializing, default)]
    pub pepper: String,
    /// Admin flag: admins may create/patch/delete users and trigger resets, but
    /// can NEVER set or read another user's password.
    #[serde(default)]
    pub is_admin: bool,
    /// Disabled users are kept but flagged (e.g. cannot authenticate).
    #[serde(default)]
    pub disabled: bool,
    /// Optional per-user storage quota in MB. When `None`, the total is derived
    /// from the backend (filesystem capacity, or the S3 default quota).
    #[serde(default)]
    pub quota_mb: Option<u64>,
    /// PARTNER relationship (directed grant). The user ids THIS user has granted
    /// partner access to: when A lists B here, B gains read access to all of A's
    /// LIVE photos (trash/archive/vault excluded) in B's timeline and search. The
    /// grant is one-directional. `#[serde(default)]` so older rows/payloads load.
    #[serde(default)]
    pub partners: Vec<String>,
    /// TOTP (RFC-6238) two-factor secret, base32-encoded. `None` until the user
    /// enrolls (confirmed by a verified code in `POST /api/users/{id}/2fa/verify`);
    /// once set, login REQUIRES a valid 6-digit code for this user. Like
    /// `password_hash`/`pepper`, this is a credential and is NEVER serialized into
    /// any API response — the API only ever exposes `{ enabled: bool }`.
    #[serde(skip_serializing, default)]
    pub totp_secret: Option<String>,
}

/// Build the argon2id hasher bound to the server-wide `secret` key (the env
/// `PHOTON_PASSWORD_SALT`), using default argon2 params.
fn argon2_with_secret(secret: &[u8]) -> Argon2<'_> {
    Argon2::new_with_secret(secret, Algorithm::Argon2id, Version::V0x13, Params::default())
        .expect("argon2 secret key within bounds")
}

/// Run ONE argon2id verification against a fixed dummy hash, discarding the
/// result. Called on the login miss / disabled-user path so that an unknown or
/// disabled account costs the SAME wall-clock as a real password check — defeating
/// **user/account enumeration by timing**. The dummy hash uses the same params as
/// real password hashes, so the work performed is equivalent.
pub fn verify_dummy_password(secret: &[u8], password: &str) {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    let phc = DUMMY.get_or_init(|| {
        let pepper = "photon-dummy-pepper-for-constant-time-login-padding-0123456789ab";
        let salt = salt_from_pepper(pepper);
        argon2_with_secret(b"dummy-secret-key")
            .hash_password(format!("{pepper}x").as_bytes(), &salt)
            .map(|h| h.to_string())
            .unwrap_or_default()
    });
    if let Ok(parsed) = PasswordHash::new(phc) {
        // Recompute with the REAL server secret: verification fails (different
        // secret), but the argon2 work — the only thing an attacker can time — is
        // identical to a genuine check.
        let _ = argon2_with_secret(secret).verify_password(format!("dummy{password}").as_bytes(), &parsed);
    }
}

/// Derive the per-user argon2 [`SaltString`] from this user's `pepper`. The
/// pepper is a 64-char hex string; argon2 salts are base64-no-pad, so we take
/// the prefix that fits the salt length bounds.
fn salt_from_pepper(pepper: &str) -> SaltString {
    // SaltString::encode_b64 enforces the length bounds; the pepper bytes are
    // random per user, so the derived salt is too.
    let bytes = pepper.as_bytes();
    let take = bytes.len().min(argon2::password_hash::Salt::RECOMMENDED_LENGTH);
    SaltString::encode_b64(&bytes[..take]).expect("valid salt from pepper")
}

impl User {
    /// Set this user's password using `argon2id` mixing the server-wide `secret`
    /// key with this user's per-user `pepper`. The pepper becomes the salt
    /// material AND is also prepended to the password input, so two users with
    /// the same password get distinct hashes even beyond the salt. Stores only
    /// the PHC string, never the plaintext.
    pub fn set_password(&mut self, secret: &[u8], pepper: String, password: &str) {
        let salt = salt_from_pepper(&pepper);
        let argon = argon2_with_secret(secret);
        let material = format!("{pepper}{password}");
        let phc = argon
            .hash_password(material.as_bytes(), &salt)
            .expect("argon2id hashing")
            .to_string();
        self.password_hash = Some(phc);
        self.pepper = pepper;
        // Keep the legacy salt field populated (storage/back-compat); not used
        // for verification (the salt is embedded in the PHC string).
        self.salt = salt.as_str().to_string();
    }

    /// Verify a candidate password by recomputing with the same server `secret`
    /// key + this user's stored `pepper`. Returns false if no password is set.
    pub fn verify_password(&self, secret: &[u8], password: &str) -> bool {
        let phc = match &self.password_hash {
            Some(h) => h,
            None => return false,
        };
        let parsed = match PasswordHash::new(phc) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let argon = argon2_with_secret(secret);
        let material = format!("{}{}", self.pepper, password);
        argon.verify_password(material.as_bytes(), &parsed).is_ok()
    }
}

/// A single-use password reset token, keyed by `token` in `AppState.reset_tokens`.
/// Issued by an admin via `POST /api/users/{id}/reset`; consumed by the user via
/// `POST /api/users/{id}/password`. Holds no plaintext password.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetToken {
    pub token: String,
    pub user_id: String,
    pub created_at: String,
    #[serde(default)]
    pub used: bool,
}

/// POST /api/users body — admin creates a passwordless user.
#[derive(Debug, Deserialize)]
pub struct CreateUser {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// POST /api/register body — public self-registration (gated by the
/// `features.public_signup` flag). Creates a non-admin user with the password
/// set immediately from the body.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub name: String,
    pub email: String,
    pub password: String,
}

// PATCH /api/users/{id} now takes an RFC 6902 JSON Patch document (a
// `json_patch::Patch` array of ops) directly in the handler, not a typed body —
// so there is no `UpdateUser` struct any more (see `handlers::update_user`).

/// POST /api/users/{id}/password body — the user sets their OWN password using
/// either their current password OR a valid unused reset token.
#[derive(Debug, Deserialize)]
pub struct SetPasswordBody {
    #[serde(default)]
    pub current_password: Option<String>,
    pub new_password: String,
    #[serde(default)]
    pub reset_token: Option<String>,
}

/// POST /api/users/{id}/partners body — grant `partner_id` partner access to
/// user `{id}`'s live photos (directed: `{id}` is the grantor).
#[derive(Debug, Deserialize)]
pub struct AddPartner {
    pub partner_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub member_ids: Vec<String>,
}

/// A per-user PIN-locked private album. Its photos never appear in the
/// timeline or in search; they are only returned by an authenticated unlock.
/// The plaintext PIN is NEVER stored: `pin_hash` holds an argon2id PHC string
/// computed with the server-wide secret key + this vault's random `salt` (see
/// [`Vault::set_pin`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vault {
    /// `None` until a PIN is configured. An argon2id PHC string. Never serialized
    /// to API responses (the only handler that touches `Vault` returns derived
    /// views/status).
    pub pin_hash: Option<String>,
    /// Per-vault random salt (CSPRNG, see state::new_salt) used as the argon2id
    /// salt material for the PIN.
    pub salt: String,
    pub photo_ids: Vec<String>,
}

impl Vault {
    /// Set this vault's PIN using argon2id, mixing the server-wide `secret` key
    /// with this vault's random `salt`. Stores only the PHC string, never the
    /// plaintext PIN. Mirrors [`User::set_password`].
    pub fn set_pin(&mut self, secret: &[u8], pin: &str) {
        let salt = salt_from_pepper(&self.salt);
        let argon = argon2_with_secret(secret);
        let material = format!("{}{}", self.salt, pin);
        let phc = argon
            .hash_password(material.as_bytes(), &salt)
            .expect("argon2id hashing")
            .to_string();
        self.pin_hash = Some(phc);
    }

    /// Verify a candidate PIN against this vault, recomputing with the same
    /// server `secret` key + stored `salt`. Returns false if no PIN is set.
    /// Mirrors [`User::verify_password`].
    pub fn verify_pin(&self, secret: &[u8], pin: &str) -> bool {
        let phc = match &self.pin_hash {
            Some(h) => h,
            None => return false,
        };
        let parsed = match PasswordHash::new(phc) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let argon = argon2_with_secret(secret);
        let material = format!("{}{}", self.salt, pin);
        argon.verify_password(material.as_bytes(), &parsed).is_ok()
    }
}

/// Immutable original capture metadata extracted from the uploaded file.
/// Original files are NEVER modified, so this is read-only after ingest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Exif {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    pub shutter: Option<String>,
    pub fnum: Option<String>,
    pub focal: Option<String>,
    pub taken_at: String,
    pub width: u32,
    pub height: u32,
    pub city: Option<String>,
    pub country: Option<String>,
    pub lat: Option<String>,
    pub lng: Option<String>,
}

/// DB-stored metadata overrides. When a field is `Some`, it wins over the
/// corresponding EXIF value; when `None`, the EXIF value shows. The photo
/// editor edits ONLY these overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataOverride {
    pub taken_at: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub rating: Option<u8>,
    pub favorite: Option<bool>,
    pub tags: Option<Vec<String>>,
    pub people: Option<Vec<String>>,
    pub lat: Option<String>,
    pub lng: Option<String>,
}

/// A companion file of a photo (e.g. the RAW sidecar of a JPG shot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Companion {
    pub filename: String,
    pub ext: String,
    /// "raw" | "jpeg" | "video" | "other"
    pub kind: String,
    pub size_mb: f64,
    pub downloadable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Photo {
    pub id: String,
    pub owner_id: String,
    pub filename: String,
    pub seed: u32,
    /// "photo" | "video" | "raw"
    pub kind: String,
    pub exif: Exif,
    #[serde(default)]
    pub overrides: MetadataOverride,
    #[serde(default)]
    pub companions: Vec<Companion>,
    /// Lifecycle: archived photos are hidden from timeline + search but kept.
    #[serde(default)]
    pub archived: bool,
    /// Lifecycle: RFC3339 timestamp when soft-deleted (moved to trash).
    /// `None` means the photo is not trashed.
    #[serde(default)]
    pub deleted_at: Option<String>,
    /// Whether this photo has been pushed to the S3 backup target.
    #[serde(default)]
    pub backed_up: bool,
    /// URL of the generated thumbnail (small, metadata-stripped), e.g.
    /// "/api/photos/{id}/thumb". `None` until a thumbnail is generated.
    #[serde(default)]
    pub thumb_url: Option<String>,
    /// Size of the primary file in megabytes (companions add their own size_mb).
    #[serde(default)]
    pub size_mb: f64,
    /// AI ANALYSIS (import stage 4) — derived, non-authoritative metadata.
    /// Detected/recognized text (OCR). `None` until analyzed or when none found.
    /// The heuristic analyzer leaves this `None`; a real OCR backend fills it.
    #[serde(default)]
    pub ocr_text: Option<String>,
    /// Context/scene labels derived during AI analysis (e.g. "night",
    /// "telephoto", "geotagged", "raw"). Distinct from user-editable
    /// `overrides.tags`; these are machine-generated.
    #[serde(default)]
    pub ai_tags: Vec<String>,
    /// Detected people/faces. Kept separate from user-curated
    /// `overrides.people`; the heuristic analyzer leaves this empty and a real
    /// face-detection backend would populate it.
    #[serde(default)]
    pub ai_people: Vec<String>,
    /// Whether the AI-analysis import stage has run for this photo.
    #[serde(default)]
    pub analyzed: bool,
    /// CONTEXT RECOGNITION (CLIP) — the photo's open-vocabulary image embedding
    /// in the CLIP space, produced by the ML sidecar over the thumbnail bytes
    /// when `PHOTON_ML_URL` is configured. `None` when ML is disabled (offline)
    /// or embedding failed. Server-side only: it is NOT serialized into
    /// [`PhotoView`] API responses (see `effective()` — only the derived
    /// `has_embedding` bool is exposed). Skipped on serialize so it never leaks
    /// into any JSON view that happens to serialize a `Photo` directly.
    #[serde(default, skip_serializing)]
    pub clip_embedding: Option<Vec<f32>>,
    /// URL of the stored ORIGINAL / screen-adapted render, e.g.
    /// "/api/photos/{id}/render". `Some` only when an original blob is stored for
    /// this photo (set by `AppState::store_original`); `None` otherwise (e.g. the
    /// demo seed, which has no original bytes). This is a RUNTIME-only field: it is
    /// neither serialized into `Photo` JSON nor persisted to Postgres — it is
    /// recomputed from the originals store on each load, and surfaced to clients
    /// via [`PhotoView::full_url`].
    #[serde(default, skip)]
    pub full_url: Option<String>,
}

impl Photo {
    /// Produce the resolved API view (override-if-some-else-exif).
    pub fn effective(&self) -> PhotoView {
        let o = &self.overrides;
        let e = &self.exif;
        let edited = o.taken_at.is_some()
            || o.city.is_some()
            || o.country.is_some()
            || o.title.is_some()
            || o.caption.is_some()
            || o.rating.is_some()
            || o.favorite.is_some()
            || o.tags.is_some()
            || o.people.is_some()
            || o.lat.is_some()
            || o.lng.is_some();

        PhotoView {
            id: self.id.clone(),
            owner_id: self.owner_id.clone(),
            filename: self.filename.clone(),
            seed: self.seed,
            kind: self.kind.clone(),
            width: e.width,
            height: e.height,
            taken_at: o.taken_at.clone().unwrap_or_else(|| e.taken_at.clone()),
            city: o.city.clone().or_else(|| e.city.clone()),
            country: o.country.clone().or_else(|| e.country.clone()),
            favorite: o.favorite.unwrap_or(false),
            rating: o.rating.unwrap_or(0),
            title: o.title.clone(),
            caption: o.caption.clone(),
            tags: o.tags.clone().unwrap_or_default(),
            people: o.people.clone().unwrap_or_default(),
            lat: o.lat.clone().or_else(|| e.lat.clone()),
            lng: o.lng.clone().or_else(|| e.lng.clone()),
            edited,
            archived: self.archived,
            deleted_at: self.deleted_at.clone(),
            thumb_url: self.thumb_url.clone(),
            full_url: self.full_url.clone(),
            exif: e.clone(),
            overrides: o.clone(),
            companions: self.companions.clone(),
            size_mb: self.size_mb,
            ocr_text: self.ocr_text.clone(),
            ai_tags: self.ai_tags.clone(),
            ai_people: self.ai_people.clone(),
            analyzed: self.analyzed,
            has_embedding: self.clip_embedding.is_some(),
        }
    }

    /// Effective capture timestamp (override wins over EXIF).
    pub fn effective_taken_at(&self) -> &str {
        self.overrides
            .taken_at
            .as_deref()
            .unwrap_or(&self.exif.taken_at)
    }
}

/// Resolved photo as returned by the API: effective values merged from
/// EXIF + overrides, plus the raw `exif`, raw `overrides` and `companions`.
#[derive(Debug, Clone, Serialize)]
pub struct PhotoView {
    pub id: String,
    pub owner_id: String,
    pub filename: String,
    pub seed: u32,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub taken_at: String,
    pub city: Option<String>,
    pub country: Option<String>,
    pub favorite: bool,
    pub rating: u8,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub tags: Vec<String>,
    pub people: Vec<String>,
    pub lat: Option<String>,
    pub lng: Option<String>,
    pub edited: bool,
    pub archived: bool,
    pub deleted_at: Option<String>,
    pub thumb_url: Option<String>,
    /// URL of the full / screen-adapted render (`/api/photos/{id}/render`), or
    /// `None` when no original blob is stored for the photo (e.g. the demo seed).
    pub full_url: Option<String>,
    pub exif: Exif,
    pub overrides: MetadataOverride,
    pub companions: Vec<Companion>,
    pub size_mb: f64,
    /// AI-analysis (stage 4) derived metadata, surfaced read-only in the API.
    pub ocr_text: Option<String>,
    pub ai_tags: Vec<String>,
    pub ai_people: Vec<String>,
    pub analyzed: bool,
    /// CONTEXT RECOGNITION (CLIP): whether a CLIP image embedding exists for this
    /// photo (so it participates in semantic search). The embedding vector itself
    /// stays server-side and is never serialized; only this bool is exposed.
    pub has_embedding: bool,
}

/// FACE RECOGNITION — a single detected face on a photo.
///
/// The `embedding` (an L2-normalized face-recognition vector from the ML sidecar)
/// is SENSITIVE: it is stored server-side only and NEVER serialized into any API
/// response (`#[serde(skip_serializing)]`), exactly like password hashes /
/// peppers / CLIP embeddings. `person_id` is the cluster this face was assigned
/// to by [`crate::state::AppState::cluster_faces`] (`None` until clustered).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Face {
    pub id: String,
    pub photo_id: String,
    pub owner_id: String,
    /// `[x, y, w, h]` bounding box in source-image pixels.
    pub bbox: [f32; 4],
    /// L2-normalized face embedding. NEVER serialized — sensitive biometric data.
    #[serde(skip_serializing, default)]
    pub embedding: Vec<f32>,
    /// Detector confidence score.
    #[serde(default)]
    pub score: f32,
    /// The Person cluster this face belongs to (`None` until clustered).
    #[serde(default)]
    pub person_id: Option<String>,
    /// The user marked this detection a non-face / intruder. Excluded from
    /// clustering AND from People. Stable (faces persist), so the decision sticks
    /// across re-clustering.
    #[serde(default)]
    pub ignored: bool,
    /// Stable identity tag for manual curation: every face sharing a label is the
    /// SAME person, authoritatively. This is how "move face to person X" and
    /// "merge A into B" survive re-clustering — [`crate::state::AppState::cluster_faces`]
    /// groups by label first, embedding second.
    #[serde(default)]
    pub assigned_label: Option<String>,
    /// A human confirmed this (otherwise low-confidence) face belongs to its
    /// person — it is no longer surfaced as "needs review". Stable.
    #[serde(default)]
    pub confirmed: bool,
}

/// FACE RECOGNITION — a cluster of faces believed to be the same person.
/// Built incrementally by [`crate::state::AppState::cluster_faces`]. The name is
/// optional (a user labels the cluster); naming a Person propagates its name into
/// the `ai_people` of every photo it appears in (so name search works).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub owner_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub face_ids: Vec<String>,
    /// A representative photo + bbox for the cluster's avatar.
    #[serde(default)]
    pub cover_photo_id: Option<String>,
    #[serde(default)]
    pub cover_bbox: Option<[f32; 4]>,
    /// KINSHIP — directed family/social links to OTHER people clusters of the same
    /// owner. `{person_id, relation}` reads "that person is this person's
    /// `<relation>`" (e.g. relation "mother" ⇒ the linked cluster is this one's
    /// mother). Reciprocal links are kept in sync (see [`PersonRelation::inverse`]).
    /// Person ids are NOT stable across re-clustering, so these are remapped by
    /// face-set overlap in [`crate::state::AppState::cluster_faces`], exactly like
    /// the user-assigned name.
    #[serde(default)]
    pub relationships: Vec<PersonRelation>,
    /// Date of birth (ISO `YYYY-MM-DD`); drives the displayed age. Carried across
    /// re-clusters by face-set overlap, like the name.
    #[serde(default)]
    pub birthdate: Option<String>,
    /// Real person, but kept out of the People surface (carried by overlap).
    #[serde(default)]
    pub hidden: bool,
    /// The user picked a cover face — don't let auto-clustering overwrite it.
    #[serde(default)]
    pub cover_locked: bool,
}

/// A directed kinship edge from a Person to another Person of the same owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersonRelation {
    /// The OTHER person cluster this edge points at.
    pub person_id: String,
    /// The role the other person plays relative to this one (e.g. "mother",
    /// "brother", "son"). Free-form, but the UI offers a curated vocabulary.
    pub relation: String,
}

impl PersonRelation {
    /// The reciprocal relation label: if B is A's `relation`, then A is B's
    /// `inverse(relation)`. Falls back to "relative" for anything not in the
    /// curated kinship vocabulary so the reverse edge is always meaningful.
    pub fn inverse(relation: &str) -> &'static str {
        match relation.trim().to_ascii_lowercase().as_str() {
            "mother" | "father" | "parent" => "child",
            "child" | "son" | "daughter" => "parent",
            "brother" | "sister" | "sibling" => "sibling",
            "grandmother" | "grandfather" | "grandparent" => "grandchild",
            "grandchild" | "grandson" | "granddaughter" => "grandparent",
            "spouse" => "spouse",
            "husband" => "wife",
            "wife" => "husband",
            "partner" => "partner",
            "uncle" | "aunt" => "nephew/niece",
            "nephew" | "niece" => "uncle/aunt",
            "cousin" => "cousin",
            "friend" => "friend",
            _ => "relative",
        }
    }
}

/// POST /api/people/{person_id}/relationships body — link this person to another
/// person cluster with a kinship label (reciprocal edge created automatically).
#[derive(Debug, Deserialize)]
pub struct RelationshipBody {
    pub other_person_id: String,
    pub relation: String,
}

/// GET /api/users/{id}/people response item: a face cluster summary. NEVER
/// includes embeddings — only counts, the cover crop, and sample photo ids.
#[derive(Debug, Clone, Serialize)]
pub struct PersonView {
    pub person_id: String,
    pub name: Option<String>,
    pub face_count: usize,
    pub cover: Option<PersonCover>,
    pub sample_photo_ids: Vec<String>,
    /// Resolved kinship edges (with the other person's current name).
    pub relationships: Vec<RelationView>,
    /// ISO date of birth, if recorded (drives the displayed age).
    #[serde(default)]
    pub birthdate: Option<String>,
}

/// One face of a person in the People Studio grid: bbox + source dims let the
/// client position a crop over the photo thumbnail; `score` flags low-confidence
/// (possible intruder) detections. NEVER includes the embedding.
#[derive(Debug, Clone, Serialize)]
pub struct StudioFace {
    pub id: String,
    pub photo_id: String,
    pub bbox: [f32; 4],
    pub score: f32,
    pub source_width: u32,
    pub source_height: u32,
    /// A human confirmed this match — the client stops flagging it for review.
    pub confirmed: bool,
}

/// GET /api/people/{id}/faces response.
#[derive(Debug, Clone, Serialize)]
pub struct PersonFacesResponse {
    pub person_id: String,
    pub faces: Vec<StudioFace>,
}

/// Request bodies for the People Studio curation endpoints.
#[derive(Debug, Deserialize)]
pub struct BirthdateBody {
    #[serde(default)]
    pub birthdate: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct CoverBody {
    pub face_id: String,
}
#[derive(Debug, Deserialize)]
pub struct FaceIdsBody {
    pub face_ids: Vec<String>,
}
#[derive(Debug, Deserialize)]
pub struct MoveFacesBody {
    pub face_ids: Vec<String>,
    pub to_person_id: String,
}
#[derive(Debug, Deserialize)]
pub struct MergeBody {
    pub into_person_id: String,
}

/// A kinship edge resolved for display: the other person's id + current name + role.
#[derive(Debug, Clone, Serialize)]
pub struct RelationView {
    pub person_id: String,
    pub name: Option<String>,
    pub relation: String,
}

/// The avatar crop for a person cluster: which photo + the face bbox within it.
#[derive(Debug, Clone, Serialize)]
pub struct PersonCover {
    pub photo_id: String,
    pub bbox: [f32; 4],
    /// Source-image dimensions of the cover photo, so the client can position the
    /// face crop (`bbox` is in these pixels) without loading the photo separately.
    pub source_width: u32,
    pub source_height: u32,
}

/// POST /api/people/{person_id}/name body — name (or rename) a face cluster.
#[derive(Debug, Deserialize)]
pub struct NamePersonBody {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub cover_seed: u32,
    pub photo_ids: Vec<String>,
    /// Shares are surfaced on the album in API responses.
    #[serde(default)]
    pub shares: Vec<Share>,
}

/// A share targets either a single user or a whole group.
/// Serialized in a tagged form, e.g. {"type":"user","id":"usr_bob"}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum ShareTarget {
    User(String),
    Group(String),
}

/// The permission level a share grants on the album.
/// - `Viewer`: can see the album (timeline/search), cannot contribute photos.
/// - `Contributor`: can also add their own photos to the album.
///
/// The role does NOT affect timeline visibility — both viewer and contributor
/// see a shared album in the timeline (subject to prefs). It only gates
/// contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareRole {
    Viewer,
    Contributor,
}

impl Default for ShareRole {
    fn default() -> Self {
        ShareRole::Viewer
    }
}

/// An album share: a target (user or group) plus the role it is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub target: ShareTarget,
    #[serde(default)]
    pub role: ShareRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePrefs {
    /// Global default for whether shared albums appear in the timeline.
    pub show_shared: bool,
    /// Per-album overrides (album_id -> visible). Wins over `show_shared`.
    #[serde(default)]
    pub per_album: std::collections::HashMap<String, bool>,
}

impl Default for TimelinePrefs {
    fn default() -> Self {
        Self {
            show_shared: true,
            per_album: std::collections::HashMap::new(),
        }
    }
}

impl TimelinePrefs {
    /// Effective visibility for a given album: per-album override wins over the
    /// global `show_shared` default.
    pub fn effective_visible(&self, album_id: &str) -> bool {
        self.per_album
            .get(album_id)
            .copied()
            .unwrap_or(self.show_shared)
    }
}

// ---- Request / response payloads ----

#[derive(Debug, Deserialize)]
pub struct CreateGroup {
    pub name: String,
    pub owner_id: String,
    #[serde(default)]
    pub member_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddMember {
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateAlbum {
    pub name: String,
    pub owner_id: String,
    #[serde(default)]
    pub photo_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddPhotos {
    pub photo_ids: Vec<String>,
}

/// POST /api/albums/{id}/shares body: a target plus an optional role
/// (default `viewer`). DELETE matches by target only (role ignored).
#[derive(Debug, Deserialize)]
pub struct ShareBody {
    pub target: ShareTarget,
    #[serde(default)]
    pub role: ShareRole,
}

/// POST /api/albums/{id}/contribute body. A contributor (or the owner) adds
/// their OWN photos to the album; ownership is unchanged. The contributor is the
/// authenticated caller — never a client-supplied id.
#[derive(Debug, Deserialize)]
pub struct ContributeBody {
    pub photo_ids: Vec<String>,
}

// ---- Vault (per-user PIN-locked private album) ----

/// PUT /api/users/{id}/vault/pin — set or change the vault PIN.
/// If a PIN is already configured, `current_pin` must be supplied and correct.
#[derive(Debug, Deserialize)]
pub struct SetPinBody {
    pub pin: String,
    #[serde(default)]
    pub current_pin: Option<String>,
}

/// POST /api/users/{id}/vault/unlock — body carries the PIN (no session).
#[derive(Debug, Deserialize)]
pub struct UnlockBody {
    pub pin: String,
}

/// POST/DELETE /api/users/{id}/vault/photos — PIN-gated mutation of contents.
#[derive(Debug, Deserialize)]
pub struct VaultPhotosBody {
    pub pin: String,
    pub photo_ids: Vec<String>,
}

/// GET /api/users/{id}/vault — status only, never contents or the hash.
#[derive(Debug, Serialize)]
pub struct VaultStatus {
    pub configured: bool,
    pub count: usize,
}

/// POST /api/users/{id}/vault/unlock success body.
#[derive(Debug, Serialize)]
pub struct VaultContents {
    pub photos: Vec<PhotoView>,
}

/// POST/DELETE /api/users/{id}/vault/photos success body.
#[derive(Debug, Serialize)]
pub struct VaultCount {
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePrefs {
    pub show_shared: Option<bool>,
    pub per_album: Option<std::collections::HashMap<String, bool>>,
}

/// A single uploaded file with minimal EXIF, fed to `ingest_upload`.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadedFile {
    pub filename: String,
    pub ext: String,
    #[serde(default)]
    pub size_mb: f64,
    pub taken_at: String,
    #[serde(default)]
    pub camera: Option<String>,
    #[serde(default)]
    pub lens: Option<String>,
    #[serde(default)]
    pub iso: Option<u32>,
    #[serde(default)]
    pub shutter: Option<String>,
    #[serde(default)]
    pub fnum: Option<String>,
    #[serde(default)]
    pub focal: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub lat: Option<String>,
    #[serde(default)]
    pub lng: Option<String>,
    /// Optional placeholder seed for the frontend image.
    #[serde(default)]
    pub seed: Option<u32>,
}

/// A raw uploaded file: the bytes are extracted (pure-Rust) for EXIF +
/// dimensions on the server. `bytes` is a JSON array of u8.
#[derive(Debug, Deserialize)]
pub struct RawUploadedFile {
    /// Original filename (the only client-provided hint we keep; the extension is
    /// re-derived from it server-side, never trusted from a separate field).
    pub filename: String,
    /// The actual file contents, base64-encoded. EVERYTHING else (EXIF, camera,
    /// dimensions, capture date, size) is extracted from these bytes server-side —
    /// no client-supplied metadata is trusted.
    pub bytes: String,
}

#[derive(Debug, Deserialize)]
pub struct RawUploadBody {
    pub owner_id: String,
    #[serde(default)]
    pub album_id: Option<String>,
    pub files: Vec<RawUploadedFile>,
}

// ---- Async multi-stage import (Upload → EXIF → Thumbnail → AI analysis) ----

/// The conceptual stage an import file is currently at (or has reached). The
/// pipeline advances each file Upload → Exif → Thumbnail → Analysis → Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportStage {
    Upload,
    Exif,
    Thumbnail,
    Analysis,
    Done,
}

/// The status of a file at its current stage.
/// - `Pending`: not started yet.
/// - `Processing`: actively running this stage.
/// - `Ok`: the stage (or whole import) completed successfully.
/// - `Error`: a recoverable processing error occurred.
/// - `Duplicate`: the file was a companion that merged into another photo.
/// - `Rejected`: the file was non-image / undecodable and cannot be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportStatus {
    Pending,
    Processing,
    Ok,
    Error,
    Duplicate,
    Rejected,
}

/// Per-file import progress, polled by the client. `photo_id` is filled once the
/// `Photo` is created (a companion file gets the PRIMARY photo's id). `error`
/// carries a short human-readable note (rejection reason, or a "merged into …"
/// note for companions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportItem {
    pub file_id: String,
    pub filename: String,
    pub stage: ImportStage,
    pub status: ImportStatus,
    #[serde(default)]
    pub photo_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A batch of files imported together, tracked transiently in-memory. Returned
/// (without the decoded bytes) by `POST /api/uploads/raw` and the polling
/// endpoint `GET /api/uploads/{batch_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportBatch {
    pub id: String,
    pub owner_id: String,
    #[serde(default)]
    pub album_id: Option<String>,
    pub items: Vec<ImportItem>,
    pub created_at: String,
}

/// `POST /api/uploads/raw` 202 Accepted body: the batch id + the initial items.
#[derive(Debug, Clone, Serialize)]
pub struct ImportAccepted {
    pub batch_id: String,
    pub items: Vec<ImportItem>,
}

#[derive(Debug, Serialize)]
pub struct TimelineSection {
    pub label: String,
    pub date: String,
    pub items: Vec<PhotoView>,
}

#[derive(Debug, Serialize)]
pub struct Timeline {
    pub sections: Vec<TimelineSection>,
}

// ---- Storage settings ----

/// Where the primary object store lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    /// Local filesystem is the source of truth (default).
    Filesystem,
    /// S3 replaces the local filesystem as the primary object store.
    S3Replacement,
}

impl Default for StorageMode {
    fn default() -> Self {
        StorageMode::Filesystem
    }
}

/// S3 connection config. The `secret_access_key` is WRITE-ONLY in the API:
/// it is accepted on PUT but never returned by GET (see `redacted()`), where
/// it is replaced by the sentinel "••••" so the UI can show "set / not set"
/// without ever leaking the secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct S3Config {
    #[serde(default)]
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Sentinel returned in place of a real secret in GET responses.
pub const REDACTED_SECRET: &str = "••••";

impl S3Config {
    /// Produce a copy safe to serialize in API responses: the secret is
    /// replaced by `REDACTED_SECRET` (or left empty if it was never set).
    pub fn redacted(&self) -> S3Config {
        let mut c = self.clone();
        if !c.secret_access_key.is_empty() {
            c.secret_access_key = REDACTED_SECRET.to_string();
        }
        c
    }
}

/// Hourly-by-default backup config. When `enabled` and `s3` is present, a job
/// pushes new photos to S3 while the filesystem stays the source of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    #[serde(default)]
    pub s3: Option<S3Config>,
    #[serde(default)]
    pub last_backup_at: Option<String>,
    #[serde(default)]
    pub last_backup_count: u64,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 3600,
            s3: None,
            last_backup_at: None,
            last_backup_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub mode: StorageMode,
    #[serde(default)]
    pub primary_s3: Option<S3Config>,
    #[serde(default)]
    pub backup: BackupConfig,
    pub trash_retention_days: u64,
    /// When true, users without an explicit `avatar_url` get a Gravatar derived
    /// from their email (admin-toggleable via `PATCH /api/settings`). This is the
    /// app-wide settings singleton, so the flag rides along with storage config.
    #[serde(default)]
    pub gravatar_enabled: bool,
    /// App-wide feature toggles (ML + security/media), admin-managed. Rides along
    /// in the same settings singleton; `#[serde(default)]` so old rows still load.
    #[serde(default)]
    pub features: FeatureFlags,
}

/// One completed background-job run, for the admin "Run history". Recorded by
/// every job execution (cron or on-demand).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub name: String,
    /// `success` | `failed` | `partial`.
    pub outcome: String,
    /// Items processed (purged / analyzed / rebuilt / …).
    pub items: i64,
    pub started_at: String,
    pub duration_ms: i64,
    /// `cron` | `manual`.
    pub trigger: String,
}

/// Admin-managed feature toggles. The ML flags (`faces`/`clip`/`ocr`) actually
/// gate the import-enrichment pipeline; the security/media flags are persisted
/// and surfaced for operators (enforced where a hook exists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlags {
    /// Face detection + clustering (People).
    pub faces: bool,
    /// CLIP smart/semantic search embeddings.
    pub clip: bool,
    /// OCR text extraction from images.
    pub ocr: bool,
    /// Reverse geocoding of GPS into place names.
    pub geocode: bool,
    /// Video transcoding to streamable versions.
    pub transcode: bool,
    /// Allow public (un-invited) sign-up.
    pub public_signup: bool,
    /// Allow sharing albums via public links (no account).
    pub public_links: bool,
    /// Require 2-factor auth for all accounts.
    pub require_2fa: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        // ML on by default (matches prior behavior — enrichment always ran when a
        // sidecar was configured); security/media conservative.
        Self {
            faces: true,
            clip: true,
            ocr: true,
            geocode: true,
            transcode: true,
            public_signup: false,
            public_links: false,
            require_2fa: false,
        }
    }
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            mode: StorageMode::Filesystem,
            primary_s3: None,
            backup: BackupConfig::default(),
            trash_retention_days: 7,
            gravatar_enabled: false,
            features: FeatureFlags::default(),
        }
    }
}

impl StorageSettings {
    /// Settings with all S3 secrets redacted, for GET responses.
    pub fn redacted(&self) -> StorageSettings {
        let mut s = self.clone();
        s.primary_s3 = s.primary_s3.map(|c| c.redacted());
        s.backup.s3 = s.backup.s3.map(|c| c.redacted());
        s
    }
}

/// PUT /api/storage body. Each field is optional; absent = leave unchanged.
/// An S3 config with `secret_access_key == REDACTED_SECRET` keeps the existing
/// stored secret instead of overwriting it (so the UI can re-PUT a redacted GET).
#[derive(Debug, Deserialize)]
pub struct UpdateStorage {
    #[serde(default)]
    pub mode: Option<StorageMode>,
    #[serde(default)]
    pub primary_s3: Option<S3Config>,
    #[serde(default)]
    pub backup: Option<BackupConfig>,
    #[serde(default)]
    pub trash_retention_days: Option<u64>,
}

// ---- SMTP config + invites (Feature B) ----

/// Configurable SMTP server. The `password` is WRITE-ONLY in the API exactly
/// like the S3 secret: accepted on PUT, never returned by GET (replaced by
/// [`REDACTED_SECRET`]), and a redacted/empty password on PUT preserves the
/// stored one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// The envelope `From` address (e.g. "Photon <noreply@photon.app>").
    pub from: String,
    /// `true` = implicit TLS relay; `false` = STARTTLS.
    #[serde(default)]
    pub tls: bool,
}

impl SmtpConfig {
    /// Copy safe to serialize in API responses: the password is replaced by
    /// [`REDACTED_SECRET`] (or left empty if never set).
    pub fn redacted(&self) -> SmtpConfig {
        let mut c = self.clone();
        if !c.password.is_empty() {
            c.password = REDACTED_SECRET.to_string();
        }
        c
    }
}

/// PUT /api/smtp body. A `password` equal to the redaction sentinel (or empty)
/// keeps the previously stored password instead of overwriting it.
#[derive(Debug, Deserialize)]
pub struct UpdateSmtp {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    pub from: String,
    #[serde(default)]
    pub tls: bool,
}

/// An invitation to a new user, keyed by `token` in `AppState.invites`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub token: String,
    pub email: String,
    pub inviter_id: String,
    pub created_at: String,
    #[serde(default)]
    pub accepted: bool,
}

/// POST /api/invites body.
#[derive(Debug, Deserialize)]
pub struct CreateInvite {
    pub email: String,
    pub inviter_id: String,
}

/// POST /api/invites/accept body.
#[derive(Debug, Deserialize)]
pub struct AcceptInvite {
    pub token: String,
    pub name: String,
}

// ---- Authentication (opt-in login primitive) ----

/// POST /api/login body. Authenticates by email + password.
#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
    /// Optional 6-digit TOTP code. Required (and validated) only when the user is
    /// 2FA-enrolled (`totp_secret.is_some()`). When omitted for an enrolled user,
    /// login responds `401 { "error": "totp_required" }` so the UI can prompt for
    /// the code and re-submit.
    #[serde(default)]
    pub totp: Option<String>,
}

/// POST /api/login success body: an opaque bearer `token` plus the public `User`
/// (which never serializes secrets).
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: User,
}

/// POST /api/users/{id}/2fa/setup success body. Returns a freshly generated
/// base32 TOTP `secret` and the `otpauth://` URI for QR enrollment. The secret
/// is NOT persisted here — the client must confirm it via `2fa/verify` (which
/// proves the authenticator is configured) before 2FA is enabled.
#[derive(Debug, Serialize)]
pub struct TotpSetupResponse {
    pub secret: String,
    pub otpauth_uri: String,
}

/// POST /api/users/{id}/2fa/verify body. `secret` is the base32 secret returned
/// by `2fa/setup`; `code` is the current 6-digit TOTP code the user reads from
/// their authenticator. On success the secret is persisted (enrollment complete).
#[derive(Debug, Deserialize)]
pub struct TotpVerifyBody {
    pub secret: String,
    pub code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_config_redacts_password() {
        let c = SmtpConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            username: "user".to_string(),
            password: "super-secret".to_string(),
            from: "noreply@photon.app".to_string(),
            tls: false,
        };
        let red = c.redacted();
        assert_eq!(red.password, REDACTED_SECRET);
        // non-secret fields preserved
        assert_eq!(red.host, "smtp.example.com");
        assert_eq!(red.username, "user");
        // original untouched
        assert_eq!(c.password, "super-secret");
        // empty stays empty
        let mut empty = c.clone();
        empty.password = String::new();
        assert_eq!(empty.redacted().password, "");
    }

    #[test]
    fn storage_settings_redacts_s3_secret() {
        let mut s = StorageSettings::default();
        s.primary_s3 = Some(S3Config {
            endpoint: None,
            region: "fr-par".to_string(),
            bucket: "photon".to_string(),
            access_key_id: "AKIA".to_string(),
            secret_access_key: "super-secret".to_string(),
            prefix: None,
        });
        s.backup.s3 = Some(S3Config {
            endpoint: Some("https://s3.example".to_string()),
            region: "us".to_string(),
            bucket: "bak".to_string(),
            access_key_id: "AKIB".to_string(),
            secret_access_key: "other-secret".to_string(),
            prefix: Some("p".to_string()),
        });

        let red = s.redacted();
        // Secrets are replaced with the sentinel, never leaked.
        assert_eq!(red.primary_s3.as_ref().unwrap().secret_access_key, REDACTED_SECRET);
        assert_eq!(red.backup.s3.as_ref().unwrap().secret_access_key, REDACTED_SECRET);
        // Non-secret fields are preserved (round-trip safe for the UI).
        assert_eq!(red.primary_s3.as_ref().unwrap().access_key_id, "AKIA");
        assert_eq!(red.backup.s3.as_ref().unwrap().bucket, "bak");
        // Original is untouched.
        assert_eq!(s.primary_s3.as_ref().unwrap().secret_access_key, "super-secret");
    }

    #[test]
    fn empty_secret_stays_empty_when_redacted() {
        let c = S3Config {
            endpoint: None,
            region: "r".to_string(),
            bucket: "b".to_string(),
            access_key_id: "k".to_string(),
            secret_access_key: String::new(),
            prefix: None,
        };
        assert_eq!(c.redacted().secret_access_key, "");
    }
}
