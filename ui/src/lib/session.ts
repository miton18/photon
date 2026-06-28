/* Photon — client session token store.
   Lives in its own module (imported by both api.ts and media.ts) to avoid a
   circular import. The bearer token is persisted so a refresh keeps you signed
   in; media URLs carry it as `?token=` since <img>/<a download> can't set an
   Authorization header.

   "Keep me signed in" chooses WHERE it's persisted: localStorage (survives a
   browser restart) when checked, sessionStorage (cleared when the tab closes)
   when not. */

const KEY = 'photon_token';

const hasStorage = typeof localStorage !== 'undefined' && typeof sessionStorage !== 'undefined';

// On load, prefer an ephemeral (session) token over a persistent one — an
// explicit "don't keep me signed in" login shouldn't be shadowed by a stale
// localStorage token.
let token: string | null = hasStorage
  ? sessionStorage.getItem(KEY) ?? localStorage.getItem(KEY)
  : null;

// True ⇒ persist to localStorage; false ⇒ sessionStorage only.
let persistent = hasStorage ? localStorage.getItem(KEY) != null : true;

export function getToken(): string | null {
  return token;
}

/** Choose persistence for the NEXT `setToken`. Call before logging in. */
export function setPersistent(p: boolean): void {
  persistent = p;
}

export function setToken(t: string | null): void {
  token = t;
  if (!hasStorage) return;
  // Clear from both stores first so a re-login can switch persistence cleanly.
  localStorage.removeItem(KEY);
  sessionStorage.removeItem(KEY);
  if (t) (persistent ? localStorage : sessionStorage).setItem(KEY, t);
}

/** Append the session token as a query param so plain <img>/<a> requests auth. */
export function authedUrl(url: string | null): string | null {
  if (!url || !token) return url;
  return url + (url.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(token);
}
