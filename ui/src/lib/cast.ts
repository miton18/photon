/* Photon — cast the open photo to a TV / second screen.
   Provider-extensible: Google Cast (Chromecast) first, then the W3C Presentation
   API (Chrome second-screen) as a fallback. AirPlay (Safari) / DLNA can be added
   as further providers. All providers degrade gracefully when no device/SDK is
   available (e.g. offline, no receiver). */
import { toast } from './toast.svelte';
import { API } from './api';
import { getToken } from './session';

/** Bearer header for the protected cast endpoints (empty when signed out). */
const authHeaders = (): Record<string, string> => {
  const t = getToken();
  return t ? { authorization: `Bearer ${t}` } : {};
};

let castSdk: Promise<boolean> | null = null;

/** Lazy-load + init the Google Cast sender framework (gstatic). */
function loadGoogleCast(): Promise<boolean> {
  if (castSdk) return castSdk;
  castSdk = new Promise<boolean>((resolve) => {
    const w = window as any;
    if (w.cast?.framework) return resolve(true);
    w.__onGCastApiAvailable = (ok: boolean) => {
      if (!ok) return resolve(false);
      try {
        w.cast.framework.CastContext.getInstance().setOptions({
          receiverApplicationId: w.chrome.cast.media.DEFAULT_MEDIA_RECEIVER_APP_ID,
          autoJoinPolicy: w.chrome.cast.AutoJoinPolicy.ORIGIN_SCOPED,
        });
        resolve(true);
      } catch {
        resolve(false);
      }
    };
    const s = document.createElement('script');
    s.src = 'https://www.gstatic.com/cv/js/sender/v1/cast_sender.js?loadCastFramework=1';
    s.async = true;
    s.onerror = () => resolve(false);
    document.head.appendChild(s);
  });
  return castSdk;
}

async function googleCast(url: string, title: string): Promise<boolean> {
  const w = window as any;
  if (!(await loadGoogleCast()) || !w.cast?.framework) return false;
  const ctx = w.cast.framework.CastContext.getInstance();
  try {
    await ctx.requestSession(); // shows the device picker; throws if none/cancelled
    const session = ctx.getCurrentSession();
    if (!session) return false;
    const media = new w.chrome.cast.media.MediaInfo(url, 'image/jpeg');
    media.metadata = new w.chrome.cast.media.GenericMediaMetadata();
    media.metadata.title = title;
    await session.loadMedia(new w.chrome.cast.media.LoadRequest(media));
    return true;
  } catch {
    return false;
  }
}

/** DLNA/UPnP must be driven server-side (no browser API for SSDP). Ask the
 *  server for discovered renderers and have it push the photo URL to one. */
async function dlnaCast(url: string, title: string): Promise<boolean> {
  try {
    const res = await fetch(`${API}/api/cast/devices`, { headers: authHeaders() });
    if (!res.ok) return false;
    const devices: { id: string; name: string; kind?: string }[] = await res.json();
    const dlna = devices.filter((d) => (d.kind ?? 'dlna') === 'dlna');
    if (!dlna.length) return false;
    // TODO: device picker when several; cast to the first for now.
    const device = dlna[0];
    const r = await fetch(`${API}/api/cast/dlna`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...authHeaders() },
      body: JSON.stringify({ device_id: device.id, url, title }),
    });
    if (!r.ok) return false;
    toast({ tone: 'success', icon: 'cast', title: `Casting to ${device.name}`, message: title });
    return true;
  } catch {
    return false;
  }
}

function presentationCast(url: string): boolean {
  const w = window as any;
  if (typeof w.PresentationRequest !== 'function') return false;
  try {
    new w.PresentationRequest([url]).start().catch(() => {});
    return true;
  } catch {
    return false;
  }
}

/** Try each cast provider in order; toast the outcome. */
export async function castPhoto(url: string, title: string): Promise<void> {
  const id = toast({ tone: 'loading', icon: 'cast', message: 'Looking for a screen…', duration: 4000 });
  if (await googleCast(url, title)) {
    toast({ tone: 'success', icon: 'cast', title: 'Casting', message: title });
    return;
  }
  // DLNA renderers on the LAN (server-discovered) — toasts on success itself.
  if (await dlnaCast(url, title)) return;
  if (presentationCast(url)) {
    toast({ tone: 'success', icon: 'cast', title: 'Presenting', message: title });
    return;
  }
  void id;
  toast({
    tone: 'error',
    icon: 'cast',
    message: 'No cast device found (Google Cast / DLNA / second screen).',
  });
}
