/* Photon — REST client for the Rust server, plus a normalizer that maps the
   server's PhotoView (effective metadata = DB overrides over immutable EXIF)
   into the UI's stable UIPhoto shape. Resilient to small schema differences. */
import type { Section, UIPhoto } from './media';
import { getToken, setToken } from './session';

export const API = (import.meta.env.VITE_API_URL as string) || 'http://localhost:3000';

/** Raised by `j()` on a 401 so the app can drop the session and show login. */
export class Unauthorized extends Error {
  constructor() {
    super('unauthorized');
  }
}

export interface User {
  id: string;
  name: string;
  email: string;
  avatar_url?: string;
  is_admin?: boolean;
  disabled?: boolean;
  partners?: string[];
  quota_mb?: number | null;
}

export type ShareRole = 'viewer' | 'contributor';

export interface ShareTarget {
  type: 'user' | 'group';
  id: string;
}

export interface Share {
  target: ShareTarget;
  role: ShareRole;
}

export interface Album {
  id: string;
  name: string;
  owner_id: string;
  cover_seed?: number;
  photo_ids: string[];
  // server may return either a list of bare targets (older) or {target, role} (with roles)
  shares?: (ShareTarget | Share)[];
}

export function shareTarget(s: ShareTarget | Share): ShareTarget {
  return 'target' in s ? s.target : s;
}
export function shareRole(s: ShareTarget | Share): ShareRole {
  return 'role' in s ? s.role : 'viewer';
}

export interface Group {
  id: string;
  name: string;
  owner_id: string;
  member_ids: string[];
}

export interface TimelinePrefs {
  show_shared: boolean;
  per_album: Record<string, boolean>;
}

/** RFC 6902 JSON Patch operation. */
export interface JsonPatchOp {
  op: 'add' | 'remove' | 'replace' | 'move' | 'copy' | 'test';
  path: string;
  value?: unknown;
  from?: string;
}

/** A face cluster (People). The server NEVER returns face embeddings. */
export interface PersonRelation {
  person_id: string;
  name: string | null;
  relation: string;
}

export interface Person {
  person_id: string;
  name: string | null;
  face_count: number;
  cover: {
    photo_id: string;
    bbox: [number, number, number, number];
    source_width: number;
    source_height: number;
  } | null;
  sample_photo_ids: string[];
  relationships: PersonRelation[];
  birthdate?: string | null;
}

/** One face of a person in the People Studio grid (no embedding). */
export interface StudioFace {
  id: string;
  photo_id: string;
  bbox: [number, number, number, number];
  score: number;
  source_width: number;
  source_height: number;
  confirmed: boolean;
}
export interface PersonFaces {
  person_id: string;
  faces: StudioFace[];
}

/** A detected face on a photo. bbox is [x, y, w, h] in SOURCE-IMAGE pixels.
 * person_id/person_name/person_label are all null when the viewer is not the
 * photo owner; person_name is null when the cluster is unnamed (use person_label). */
export interface PhotoFace {
  id: string;
  bbox: [number, number, number, number];
  score: number;
  person_id: string | null;
  person_name: string | null;
  person_label: string | null;
}
export interface PhotoFaces {
  source_width: number;
  source_height: number;
  faces: PhotoFace[];
}

export interface ImportItem {
  file_id: string;
  filename: string;
  stage: 'upload' | 'exif' | 'thumbnail' | 'analysis' | 'done';
  status: 'pending' | 'processing' | 'ok' | 'error' | 'duplicate' | 'rejected';
  photo_id: string | null;
  error: string | null;
}
export interface ImportBatch {
  id: string;
  owner_id: string;
  album_id: string | null;
  created_at: string;
  items: ImportItem[];
}
/** 202 response from POST /api/uploads/raw (note: `batch_id`, not `id`). */
export interface ImportAccepted {
  batch_id: string;
  items: ImportItem[];
}

export interface UploadedFileMeta {
  filename: string;
  ext: string;
  size_mb: number;
  taken_at: string;
  width?: number;
  height?: number;
}

export interface SmtpConfig {
  host: string;
  port: number;
  username: string;
  password?: string;
  from: string;
  tls: boolean;
}
export interface Invite {
  token: string;
  email: string;
  inviter_id: string;
  created_at: string;
  accepted: boolean;
}
export interface JobStat {
  name: string;
  status: string;
  last_run_at: string | null;
  last_result: string | null;
}
export interface JobRun {
  name: string;
  outcome: string;
  items: number;
  started_at: string;
  duration_ms: number;
  trigger: string;
}
export interface SystemStats {
  cpu_percent: number;
  mem_percent: number;
  mem_used_mb: number;
  mem_total_mb: number;
  uptime_secs: number;
  cpus: number;
}
export interface FeatureFlags {
  faces: boolean;
  clip: boolean;
  ocr: boolean;
  geocode: boolean;
  transcode: boolean;
  public_signup: boolean;
  public_links: boolean;
  require_2fa: boolean;
}
export interface AdminStats {
  jobs: JobStat[];
  counts: Record<string, number>;
  storage: { mode: string; disk_used_mb: number; s3_used_mb: number; quota_mb: number };
  system: SystemStats;
  history: JobRun[];
}

/** A photo-edit operation provided by a server-side subprocess plugin. */
export interface PluginEditorOp {
  plugin: string;
  id: string;
  label: string;
  description: string;
  params: { name: string; label: string; default: string }[];
}

export type StorageMode = 'filesystem' | 's3_replacement';
export interface S3Config {
  endpoint?: string | null;
  region: string;
  bucket: string;
  access_key_id: string;
  secret_access_key?: string; // redacted on GET
  prefix?: string | null;
}
export interface BackupConfig {
  enabled: boolean;
  interval_secs: number;
  s3?: S3Config | null;
  last_backup_at?: string | null;
  last_backup_count: number;
}
export interface StorageSettings {
  mode: StorageMode;
  primary_s3?: S3Config | null;
  backup: BackupConfig;
  trash_retention_days: number;
}

async function j<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken();
  const res = await fetch(API + path, {
    ...init,
    // The JSON API is dynamic — never serve a stale cached body (e.g. a re-fetch
    // right after a mutation must see the change). Image/blob URLs use <img src>,
    // not this path, so their caching is unaffected.
    cache: 'no-store',
    headers: {
      'content-type': 'application/json',
      ...(token ? { authorization: `Bearer ${token}` } : {}),
      ...(init?.headers ?? {}),
    },
  });
  if (res.status === 401) {
    setToken(null);
    throw new Unauthorized();
  }
  if (!res.ok) throw new Error(`${res.status} ${res.statusText} on ${path}`);
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T);
}

/* ---- normalization ---- */
function pick<T>(...vals: (T | undefined | null)[]): T | undefined {
  for (const v of vals) if (v !== undefined && v !== null) return v as T;
  return undefined;
}

export function normalizePhoto(raw: any, sharedFor?: string): UIPhoto {
  const exif = raw.exif ?? raw;
  const ov = raw.overrides ?? {};
  const width = pick<number>(raw.width, exif.width) ?? 6240;
  const height = pick<number>(raw.height, exif.height) ?? 4160;
  const kind = pick<string>(raw.kind) ?? 'photo';
  const taken_at = pick<string>(raw.taken_at, ov.taken_at, exif.taken_at) ?? '';
  return {
    id: raw.id,
    owner_id: raw.owner_id,
    seed: raw.seed ?? 0,
    kind,
    filename: raw.filename ?? '',
    width,
    height,
    portrait: height > width,
    taken_at,
    city: pick<string>(raw.city, ov.city, exif.city) ?? '',
    country: pick<string>(raw.country, raw.cc, ov.country, exif.country) ?? '',
    favorite: !!pick<boolean>(raw.favorite, ov.favorite, raw.fav),
    rating: pick<number>(raw.rating, ov.rating) ?? null,
    title: pick<string>(raw.title, ov.title) ?? null,
    caption: pick<string>(raw.caption, ov.caption) ?? null,
    tags: pick<string[]>(raw.tags, ov.tags) ?? [],
    people: pick<string[]>(raw.people, ov.people, exif.people) ?? [],
    lat: pick<string>(raw.lat, ov.lat, exif.lat) ?? null,
    lng: pick<string>(raw.lng, ov.lng, exif.lng) ?? null,
    exif: {
      camera: exif.camera,
      lens: exif.lens,
      focal: exif.focal,
      iso: exif.iso,
      shutter: exif.shutter,
      fnum: exif.fnum,
      taken_at: exif.taken_at,
      city: exif.city,
      country: exif.country ?? exif.cc,
    },
    edited: !!raw.edited,
    shared: sharedFor !== undefined && raw.owner_id !== sharedFor,
    sizeMB: pick<number>(raw.sizeMB, raw.size_mb) ?? 12,
    dur: raw.dur ?? null,
    thumbUrl: raw.thumb_url ? (raw.thumb_url.startsWith('http') ? raw.thumb_url : API + raw.thumb_url) : null,
    fullUrl: raw.full_url ? (raw.full_url.startsWith('http') ? raw.full_url : API + raw.full_url) : null,
    companions: (raw.companions ?? []).map((c: any) => ({
      filename: c.filename,
      ext: c.ext,
      kind: c.kind,
      size_mb: c.size_mb ?? c.sizeMB ?? 0,
      downloadable: c.downloadable ?? true,
    })),
  };
}

/* ---- endpoints ---- */
export interface LoginResponse {
  token: string;
  user: User;
}

/** A registered passkey's metadata (never the credential material). */
export interface PasskeyInfo {
  id: string;
  name: string | null;
  created_at: string;
  last_used_at: string | null;
}

/** Thrown by `api.login` when the account is 2FA-enrolled and needs a TOTP code.
 * The UI catches this to reveal the code field and re-submit with `totp`. */
export class TotpRequired extends Error {
  constructor() {
    super('totp_required');
    this.name = 'TotpRequired';
  }
}

/** Thrown by `api.login` when a supplied TOTP code is wrong/expired. */
export class TotpInvalid extends Error {
  constructor() {
    super('totp_invalid');
    this.name = 'TotpInvalid';
  }
}

export const api = {
  health: () => j<{ status: string }>('/api/health'),
  // ---- auth/session ----
  login: async (email: string, password: string, totp?: string): Promise<LoginResponse> => {
    // Login can 401 with a distinct JSON body for 2FA (`totp_required` /
    // `totp_invalid`), so it does NOT go through `j()` (which treats any 401 as a
    // dropped session). We inspect the body to surface a typed error the UI can
    // act on (reveal the code field) instead of bouncing to the login screen.
    const res = await fetch(API + '/api/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ email, password, ...(totp ? { totp } : {}) }),
    });
    if (res.status === 401) {
      const err = await res.json().catch(() => ({}));
      if (err?.error === 'totp_required') throw new TotpRequired();
      if (err?.error === 'totp_invalid') throw new TotpInvalid();
      throw new Unauthorized();
    }
    if (!res.ok) throw new Error(`${res.status} ${res.statusText} on /api/login`);
    const r = (await res.json()) as LoginResponse;
    setToken(r.token);
    return r;
  },
  // ---- TOTP two-factor enrollment (self or admin) ----
  twoFactorStatus: (userId: string) =>
    j<{ enabled: boolean }>(`/api/users/${userId}/2fa`),
  twoFactorSetup: (userId: string) =>
    j<{ secret: string; otpauth_uri: string }>(`/api/users/${userId}/2fa/setup`, {
      method: 'POST',
    }),
  twoFactorVerify: (userId: string, secret: string, code: string) =>
    j<{ enabled: boolean }>(`/api/users/${userId}/2fa/verify`, {
      method: 'POST',
      body: JSON.stringify({ secret, code }),
    }),
  twoFactorDisable: (userId: string) =>
    j<{ enabled: boolean }>(`/api/users/${userId}/2fa`, { method: 'DELETE' }),

  // ---- WebAuthn passkeys ----
  // Begin/finish enrolling a passkey on this device (returns server WebAuthn
  // options to hand to the browser, then stores the resulting credential).
  passkeyRegisterStart: (userId: string) =>
    j<{ handle: string; options: any }>(`/api/users/${userId}/passkeys/register/start`, {
      method: 'POST',
    }),
  passkeyRegisterFinish: (userId: string, handle: string, credential: any, name?: string) =>
    j<PasskeyInfo>(`/api/users/${userId}/passkeys/register/finish`, {
      method: 'POST',
      body: JSON.stringify({ handle, credential, name }),
    }),
  passkeys: (userId: string) => j<PasskeyInfo[]>(`/api/users/${userId}/passkeys`),
  passkeyDelete: (userId: string, credId: string) =>
    j<void>(`/api/users/${userId}/passkeys/${encodeURIComponent(credId)}`, { method: 'DELETE' }),
  // Usernameless passkey sign-in (PUBLIC). `finish` mints a session like password
  // login; mirror api.login by persisting the returned token.
  passkeyLoginStart: () =>
    j<{ handle: string; options: any }>('/api/login/passkey/start', { method: 'POST' }),
  passkeyLoginFinish: async (handle: string, credential: any): Promise<LoginResponse> => {
    const r = await j<LoginResponse>('/api/login/passkey/finish', {
      method: 'POST',
      body: JSON.stringify({ handle, credential }),
    });
    setToken(r.token);
    return r;
  },

  me: () => j<User>('/api/me'),
  // Whether OIDC web login is configured on this instance (drives the
  // "Continue with OpenID" button). Always resolves; defaults to false on error.
  oidcAvailable: async (): Promise<boolean> => {
    try {
      const r = await j<{ available: boolean }>('/api/auth/oidc/available');
      return !!r.available;
    } catch {
      return false;
    }
  },
  // app-wide settings (admin)
  getSettings: () => j<{ gravatar_enabled: boolean; features: FeatureFlags }>('/api/settings'),
  patchSettings: (ops: any[]) =>
    j<{ gravatar_enabled: boolean; features: FeatureFlags }>('/api/settings', {
      method: 'PATCH',
      headers: { 'content-type': 'application/json-patch+json' },
      body: JSON.stringify(ops),
    }),
  setGravatar: (enabled: boolean) =>
    api.patchSettings([{ op: 'replace', path: '/gravatar_enabled', value: enabled }]),
  logout: async () => {
    try {
      await j<void>('/api/logout', { method: 'POST' });
    } finally {
      setToken(null);
    }
  },
  users: () => j<User[]>('/api/users'),
  groups: () => j<Group[]>('/api/groups'),
  albums: () => j<Album[]>('/api/albums'),
  photos: () => j<any[]>('/api/photos'),
  // Detected faces for a photo (bounding boxes + person labels). Owner-only
  // labels: person_* fields are null for non-owners; `faces` may be empty.
  photoFaces: (id: string) => j<PhotoFaces>(`/api/photos/${id}/faces`),

  timeline: async (userId: string): Promise<Section[]> => {
    const data = await j<any>(`/api/users/${userId}/timeline`);
    const sections = Array.isArray(data) ? groupFlat(data) : (data.sections ?? []);
    return sections.map((s: any, i: number) => ({
      id: s.id ?? 's' + i,
      label: s.label ?? s.date ?? '',
      date: s.date ?? '',
      items: (s.items ?? []).map((p: any) => normalizePhoto(p, userId)),
    }));
  },

  prefs: (userId: string) => j<TimelinePrefs>(`/api/users/${userId}/timeline-prefs`),
  updatePrefs: (userId: string, body: Partial<TimelinePrefs>) =>
    j<TimelinePrefs>(`/api/users/${userId}/timeline-prefs`, {
      method: 'PUT',
      body: JSON.stringify(body),
    }),

  // RFC 6902 JSON Patch (op array). Use `add` for set-or-replace, value null to clear.
  patchMetadata: (photoId: string, ops: JsonPatchOp[]) =>
    j<any>(`/api/photos/${photoId}/metadata`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json-patch+json' },
      body: JSON.stringify(ops),
    }).then((p) => normalizePhoto(p)),

  archivePhoto: (id: string) => j<any>(`/api/photos/${id}/archive`, { method: 'POST' }),
  unarchivePhoto: (id: string) => j<any>(`/api/photos/${id}/unarchive`, { method: 'POST' }),
  trashPhoto: (id: string) => j<any>(`/api/photos/${id}`, { method: 'DELETE' }),
  restorePhoto: (id: string) => j<any>(`/api/photos/${id}/restore`, { method: 'POST' }),

  allPhotos: (userId: string) => j<any[]>('/api/photos').then((ps) => ps.map((p) => normalizePhoto(p, userId))),
  archived: (userId: string) => j<any[]>('/api/archive').then((ps) => ps.map((p) => normalizePhoto(p, userId))),
  trashed: (userId: string) => j<any[]>('/api/trash').then((ps) => ps.map((p) => normalizePhoto(p, userId))),

  // Bake a 90°-step rotation (+ optional H-flip) into the photo's edited
  // companion (original kept). `degrees` ∈ {0,90,180,270}; 0+no-flip reverts.
  rotatePhoto: (id: string, degrees: number, flip: boolean, owner?: string) =>
    j<any>(`/api/photos/${id}/rotate`, {
      method: 'POST',
      body: JSON.stringify({ degrees, flip }),
    }).then((p) => normalizePhoto(p, owner)),

  // Bake the editor's Light/Color tonal sliders into the edited companion
  // (original kept). All-zero sliders revert to the original.
  adjustPhoto: (id: string, adj: Record<string, number>, owner?: string) =>
    j<any>(`/api/photos/${id}/adjust`, {
      method: 'POST',
      body: JSON.stringify(adj),
    }).then((p) => normalizePhoto(p, owner)),

  // Single-file upload: only filename + base64 bytes are sent — the server
  // extracts EXIF/dimensions/size, stores the original + thumbnail, and pairs a
  // RAW with its same-base-name primary as a companion (any arrival order).
  // Returns the resulting photo synchronously (face detection runs server-side
  // in the background). The front uploads files individually, in parallel.
  uploadFile: (owner_id: string, filename: string, bytes: string, album_id?: string) =>
    j<any>('/api/uploads', { method: 'POST', body: JSON.stringify({ owner_id, album_id, filename, bytes }) }).then(
      (p) => normalizePhoto(p, owner_id),
    ),

  userStorage: (userId: string) =>
    j<{ used_mb: number; total_mb: number }>(`/api/users/${userId}/storage`),

  shareAlbum: (albumId: string, target: ShareTarget, role: ShareRole = 'viewer') =>
    j<Album>(`/api/albums/${albumId}/shares`, {
      method: 'POST',
      body: JSON.stringify({ target, role }),
    }),
  unshareAlbum: (albumId: string, target: ShareTarget) =>
    j<Album>(`/api/albums/${albumId}/shares`, {
      method: 'DELETE',
      body: JSON.stringify({ target }),
    }),

  createAlbum: (body: { name: string; owner_id: string; photo_ids?: string[] }) =>
    j<Album>('/api/albums', { method: 'POST', body: JSON.stringify(body) }),
  addAlbumPhotos: (albumId: string, photo_ids: string[]) =>
    j<Album>(`/api/albums/${albumId}/photos`, { method: 'POST', body: JSON.stringify({ photo_ids }) }),
  deleteAlbum: (id: string) => j<void>(`/api/albums/${id}`, { method: 'DELETE' }),
  addPartner: (userId: string, partner_id: string) =>
    j<User>(`/api/users/${userId}/partners`, { method: 'POST', body: JSON.stringify({ partner_id }) }),
  removePartner: (userId: string, partnerId: string) =>
    j<User>(`/api/users/${userId}/partners/${partnerId}`, { method: 'DELETE' }),
  duplicates: (userId: string) =>
    j<{ groups: any[][] }>(`/api/users/${userId}/duplicates`).then((r) => ({
      groups: r.groups.map((g) => g.map((p) => normalizePhoto(p, userId))),
    })),
  // Face recognition (People). Embeddings are NEVER returned by the server.
  // Route-capable plugins for the sidebar Tools section (empty if plugins off).
  routePlugins: () => j<{ id: string; label: string; ui_path: string | null }[]>('/api/plugins'),
  people: (userId: string) => j<Person[]>(`/api/users/${userId}/people`),
  namePerson: (personId: string, name: string) =>
    j<void>(`/api/people/${personId}/name`, { method: 'POST', body: JSON.stringify({ name }) }),
  personPhotos: (personId: string, owner?: string) => {
    const qs = owner ? `?owner=${encodeURIComponent(owner)}` : '';
    return j<any[]>(`/api/people/${personId}/photos${qs}`).then((ps) =>
      ps.map((p) => normalizePhoto(p, owner)),
    );
  },
  // KINSHIP — link/unlink two People clusters (reciprocal edge handled server-side)
  addRelationship: (personId: string, other_person_id: string, relation: string) =>
    j<void>(`/api/people/${personId}/relationships`, {
      method: 'POST',
      body: JSON.stringify({ other_person_id, relation }),
    }),
  removeRelationship: (personId: string, otherPersonId: string) =>
    j<void>(`/api/people/${personId}/relationships/${otherPersonId}`, { method: 'DELETE' }),
  // ---- People Studio curation ----
  personFaces: (personId: string) => j<PersonFaces>(`/api/people/${personId}/faces`),
  setPersonBirthdate: (personId: string, birthdate: string | null) =>
    j<void>(`/api/people/${personId}/birthdate`, { method: 'POST', body: JSON.stringify({ birthdate }) }),
  setPersonCover: (personId: string, faceId: string) =>
    j<void>(`/api/people/${personId}/cover`, { method: 'POST', body: JSON.stringify({ face_id: faceId }) }),
  ignoreFaces: (personId: string, faceIds: string[]) =>
    j<void>(`/api/people/${personId}/ignore`, { method: 'POST', body: JSON.stringify({ face_ids: faceIds }) }),
  approveFaces: (personId: string, faceIds: string[]) =>
    j<void>(`/api/people/${personId}/approve`, { method: 'POST', body: JSON.stringify({ face_ids: faceIds }) }),
  moveFaces: (personId: string, faceIds: string[], toPersonId: string) =>
    j<void>(`/api/people/${personId}/move`, {
      method: 'POST',
      body: JSON.stringify({ face_ids: faceIds, to_person_id: toPersonId }),
    }),
  mergePeople: (personId: string, intoPersonId: string) =>
    j<void>(`/api/people/${personId}/merge`, { method: 'POST', body: JSON.stringify({ into_person_id: intoPersonId }) }),
  hidePerson: (personId: string) =>
    j<void>(`/api/people/${personId}/hide`, { method: 'POST' }),
  createGroup: (body: { name: string; owner_id: string; member_ids: string[] }) =>
    j<Group>('/api/groups', { method: 'POST', body: JSON.stringify(body) }),
  addGroupMember: (groupId: string, user_id: string) =>
    j<Group>(`/api/groups/${groupId}/members`, { method: 'POST', body: JSON.stringify({ user_id }) }),

  // Contributor is derived from the session server-side; only photo_ids are sent.
  contribute: (albumId: string, photo_ids: string[]) =>
    j<Album>(`/api/albums/${albumId}/contribute`, {
      method: 'POST',
      body: JSON.stringify({ photo_ids }),
    }),
  search: (
    userId: string,
    f: { q?: string; camera?: string; from?: string; to?: string; place?: string; near?: string },
  ) => {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(f)) if (v) qs.set(k, v);
    return j<any[]>(`/api/users/${userId}/search?${qs.toString()}`).then((ps) =>
      ps.map((p) => normalizePhoto(p, userId)),
    );
  },

  // storage settings
  getStorage: () => j<StorageSettings>('/api/storage'),
  putStorage: (body: Partial<StorageSettings>) =>
    j<StorageSettings>('/api/storage', { method: 'PUT', body: JSON.stringify(body) }),
  runBackup: () => j<{ count: number; last_backup_at: string | null }>('/api/storage/backup/run', { method: 'POST' }),

  // vault
  vaultStatus: (userId: string) => j<{ configured: boolean; count: number }>(`/api/users/${userId}/vault`),
  vaultSetPin: (userId: string, pin: string, current_pin?: string) =>
    j<void>(`/api/users/${userId}/vault/pin`, { method: 'PUT', body: JSON.stringify({ pin, current_pin }) }),
  vaultUnlock: (userId: string, pin: string) =>
    j<{ photos: any[] }>(`/api/users/${userId}/vault/unlock`, { method: 'POST', body: JSON.stringify({ pin }) }).then(
      (r) => r.photos.map((p) => normalizePhoto(p)),
    ),
  vaultAddPhotos: (userId: string, pin: string, photo_ids: string[]) =>
    j<{ count: number }>(`/api/users/${userId}/vault/photos`, {
      method: 'POST',
      body: JSON.stringify({ pin, photo_ids }),
    }),

  // admin
  adminStats: () => j<AdminStats>('/api/admin/stats'),
  runJob: (name: string) => j<JobRun>('/api/admin/jobs/' + name + '/run', { method: 'POST' }),
  createUser: (body: { name: string; email: string; is_admin?: boolean }) =>
    j<User>('/api/users', { method: 'POST', body: JSON.stringify(body) }),
  patchUser: (id: string, ops: JsonPatchOp[]) =>
    j<User>(`/api/users/${id}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json-patch+json' },
      body: JSON.stringify(ops),
    }),
  deleteUser: (id: string) => j<void>(`/api/users/${id}`, { method: 'DELETE' }),
  resetPassword: (id: string) => j<{ ok: boolean }>(`/api/users/${id}/reset`, { method: 'POST' }),

  // smtp + invites
  getSmtp: () => j<SmtpConfig | null>('/api/smtp'),
  putSmtp: (body: SmtpConfig) => j<SmtpConfig>('/api/smtp', { method: 'PUT', body: JSON.stringify(body) }),
  invites: () => j<Invite[]>('/api/invites'),
  createInvite: (email: string, inviter_id: string) =>
    j<Invite>('/api/invites', { method: 'POST', body: JSON.stringify({ email, inviter_id }) }),

  // ---- editor plugins (subprocess-provided photo-edit ops) ----
  // Lists available plugin editor ops; returns [] when plugins are disabled.
  pluginEditorOps: () => j<PluginEditorOp[]>('/api/plugins/editor/ops'),
  // Applies a plugin op to a photo and returns an object URL for the edited
  // image. The response is RAW image bytes (image/png), not JSON, so it does
  // NOT go through `j()`. Owner-only (auth enforced server-side via the token).
  applyPluginEdit: async (
    photoId: string,
    plugin: string,
    op: string,
    params: Record<string, string>,
    save = false,
  ): Promise<string> => {
    const token = getToken();
    const qs = save ? '?save=true' : '';
    const res = await fetch(`${API}/api/photos/${photoId}/plugin-edit/${plugin}/${op}${qs}`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: JSON.stringify(params),
    });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText} on plugin-edit ${plugin}/${op}`);
    return URL.createObjectURL(await res.blob());
  },
};

function groupFlat(photos: any[]): any[] {
  const by = new Map<string, any>();
  for (const p of photos) {
    const date = (p.taken_at ?? p.exif?.taken_at ?? '').slice(0, 10);
    if (!by.has(date)) by.set(date, { id: date, label: date, date, items: [] });
    by.get(date).items.push(p);
  }
  return [...by.values()].sort((a, b) => (a.date < b.date ? 1 : -1));
}
