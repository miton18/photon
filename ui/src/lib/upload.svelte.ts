/* Photon — upload manager. Drag-and-drop + multi-file import panel. Each file is
   uploaded INDIVIDUALLY (POST /api/uploads, one file per request); the front runs
   up to `maxPerRequest` of them in parallel. The server extracts EXIF/dimensions,
   stores the original + thumbnail, pairs a RAW with its same-base JPG as a
   companion (any arrival order), and returns the resulting photo — which we add to
   the timeline immediately, no reload. Face detection runs server-side in the
   background, so an upload is "done" as soon as its POST returns. */
import { api } from './api';
import type { UIPhoto } from './media';

// A single-file upload is one logical step from the UI's point of view: the POST
// either succeeds (the photo is imported + stored) or it doesn't.
export const STAGE_COUNT = 1;

export type UpStatus = 'active' | 'ok' | 'duplicate' | 'rejected';

export interface UploadItem {
  id: string; // tmp id, then the server photo id
  name: string;
  size: string;
  thumbUrl: string; // unused (no preview in panel)
  isDoc: boolean;
  status: UpStatus;
  stage: number; // always 0 (single step) — kept for the gauge model
  progress: number; // animated fill while active
}

export const uploads = $state<UploadItem[]>([]);
export const tray = $state({ open: false, minimized: false });
/** Upload parallelism: at most `maxPerRequest` single-file uploads in flight at
 *  once (each request carries exactly one file). Configurable; default 4. */
export const uploadConfig = $state({ maxPerRequest: 4 });
export function setUploadMaxPerRequest(n: number) {
  uploadConfig.maxPerRequest = Math.max(1, Math.floor(n) || 1);
}

let ownerId = 'usr_alice';
let onComplete: () => void = () => {};
let onPhoto: (p: UIPhoto) => void = () => {};
/** Wire the manager to the app: the owner to upload as, a completion callback
 *  (a final authoritative refresh), and a per-photo callback that adds each
 *  freshly imported photo to the timeline the moment its upload returns. */
export function configureUploads(owner: string, done: () => void, photo?: (p: UIPhoto) => void) {
  ownerId = owner;
  onComplete = done;
  if (photo) onPhoto = photo;
}

function fmtSize(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}
function fileToBase64(file: File): Promise<string> {
  return new Promise((res, rej) => {
    const r = new FileReader();
    r.onload = () => {
      const s = r.result as string;
      res(s.slice(s.indexOf(',') + 1));
    };
    r.onerror = rej;
    r.readAsDataURL(file);
  });
}

// Animate the active segment fill while anything is still importing.
let tick: ReturnType<typeof setInterval> | undefined;
function ensureTick() {
  if (tick) return;
  tick = setInterval(() => {
    let any = false;
    for (const it of uploads) {
      if (it.status === 'active') {
        it.progress = Math.min(92, it.progress + 7);
        any = true;
      }
    }
    if (!any && tick) {
      clearInterval(tick);
      tick = undefined;
    }
  }, 120);
}

export function clearFinished() {
  for (let i = uploads.length - 1; i >= 0; i--) {
    if (uploads[i].status !== 'active') uploads.splice(i, 1);
  }
  if (!uploads.length) tray.open = false;
}

// Resolve the PROXIED item living in the `uploads` $state array by id. Mutating
// that proxy (not a detached original) is what Svelte tracks for re-render.
function rowById(id: string): UploadItem | undefined {
  return uploads.find((u) => u.id === id);
}

// Upload ONE file: read bytes, POST it, then either mark the row imported (and
// hand the photo to the timeline) or mark it rejected. Never throws.
async function uploadOne(file: File, tmpId: string, albumId?: string) {
  try {
    const bytes = await fileToBase64(file);
    const photo = await api.uploadFile(ownerId, file.name, bytes, albumId);
    const row = rowById(tmpId);
    if (row) {
      row.id = photo.id;
      row.status = 'ok';
      row.stage = 0;
      row.progress = 100;
    }
    onPhoto(photo);
  } catch {
    const row = rowById(tmpId);
    if (row) row.status = 'rejected';
  }
}

let seq = 0;
export async function enqueue(fileList: FileList | File[], albumId?: string) {
  const files = [...fileList];
  if (!files.length) return;

  // Optimistic rows for every file (queued ones sit active until their turn).
  const tmpIds = files.map(() => `up_${++seq}`);
  uploads.push(
    ...files.map((f, i) => ({
      id: tmpIds[i],
      name: f.name,
      size: fmtSize(f.size),
      thumbUrl: '',
      isDoc: false,
      status: 'active' as UpStatus,
      stage: 0,
      progress: 0,
    })),
  );
  tray.open = true;
  tray.minimized = false;
  ensureTick();

  // Bounded-parallel worker pool: at most `maxPerRequest` files upload at once.
  // Workers pull from a shared cursor so a slow file never blocks the others.
  let next = 0;
  const worker = async () => {
    while (next < files.length) {
      const i = next++;
      await uploadOne(files[i], tmpIds[i], albumId);
    }
  };
  const lanes = Math.min(uploadConfig.maxPerRequest, files.length);
  await Promise.all(Array.from({ length: lanes }, worker));
  onComplete();
}
