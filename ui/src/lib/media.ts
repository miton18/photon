/* Photon — image URL helpers + static chrome data.
   Placeholder imagery via picsum.photos seeds (needs network). Swap for
   self-hosted asset URLs in production. */
import { authedUrl } from './session';

export interface UIPhoto {
  id: string;
  owner_id: string;
  seed: number;
  kind: string; // photo | video | raw
  filename: string;
  width: number;
  height: number;
  portrait: boolean;
  taken_at: string;
  city: string;
  country: string; // cc
  favorite: boolean;
  rating: number | null;
  title: string | null;
  caption: string | null;
  tags: string[];
  people: string[];
  lat: string | null;
  lng: string | null;
  // original capture metadata (immutable EXIF)
  exif: {
    camera?: string;
    lens?: string;
    focal?: string;
    iso?: number | string;
    shutter?: string;
    fnum?: string;
    taken_at?: string;
    city?: string;
    country?: string;
  };
  edited: boolean;
  shared: boolean; // belongs to someone else, visible via a shared album
  sizeMB: number;
  dur: string | null;
  thumbUrl: string | null; // real server thumbnail (absolute URL) when available
  fullUrl: string | null; // server render endpoint (absolute URL) for the full/original image
  companions: { filename: string; ext: string; kind: string; size_mb: number; downloadable: boolean }[];
}

export interface Section {
  id: string;
  label: string;
  date: string;
  items: UIPhoto[];
}

export const thumb = (seed: number) => `https://picsum.photos/seed/ph${seed}/400/400`;
export const full = (p: { portrait: boolean; seed: number }) =>
  p.portrait
    ? `https://picsum.photos/seed/ph${p.seed}/1100/1500`
    : `https://picsum.photos/seed/ph${p.seed}/1600/1067`;

/** Prefer the real server thumbnail (uploaded photos) over the picsum placeholder.
 *  Server URLs carry the session token (the <img> can't send an auth header). */
export const displayThumb = (p: { thumbUrl?: string | null; seed: number }) =>
  p.thumbUrl ? authedUrl(p.thumbUrl)! : thumb(p.seed);

/** Target pixel size for the full view = viewport × DPR, capped. */
function screenBox(): { w: number; h: number } {
  const dpr = Math.min(2, (typeof window !== 'undefined' && window.devicePixelRatio) || 1);
  const w = typeof window !== 'undefined' ? window.innerWidth : 1920;
  const h = typeof window !== 'undefined' ? window.innerHeight : 1080;
  return { w: Math.min(4000, Math.round(w * dpr)), h: Math.min(4000, Math.round(h * dpr)) };
}

/** Big image for the lightbox/editor: the server render endpoint sized to the
 *  user's screen (real original, resized & never upscaled) when available; falls
 *  back to the picsum mock for seed photos with no stored original. */
export const displayFull = (p: { fullUrl?: string | null; thumbUrl?: string | null; portrait: boolean; seed: number }) => {
  if (p.fullUrl) {
    const { w, h } = screenBox();
    return authedUrl(`${p.fullUrl}?w=${w}&h=${h}`)!;
  }
  return p.thumbUrl ? authedUrl(p.thumbUrl)! : full(p);
};

export const RAIL = [
  { m: 'Jun', y: '2026', active: true },
  { m: 'May', y: '2026' },
  { m: 'Apr', y: '2026' },
  { m: 'Mar', y: '2026' },
  { m: 'Feb', y: '2026' },
  { m: 'Jan', y: '2026' },
  { m: 'Dec', y: '2025' },
  { m: 'Nov', y: '2025' },
  { m: 'Oct', y: '2025' },
];

export const STATS = { items: 12847, videos: 642, used: 184, quota: 500, people: 18, albums: 42 };
