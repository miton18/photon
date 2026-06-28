/* Photon — WebAuthn / passkey browser glue.
 *
 * The server (webauthn-rs) speaks JSON where binary fields (challenge, user id,
 * credential ids, attestation/assertion blobs) are base64url strings, but
 * `navigator.credentials.{create,get}` want/return ArrayBuffers. These helpers
 * convert between the two and serialize the credential back into the exact shape
 * webauthn-rs's `RegisterPublicKeyCredential` / `PublicKeyCredential` expect. */

/** Is the WebAuthn platform API available in this browser/context? */
export function passkeysSupported(): boolean {
  return (
    typeof window !== 'undefined' &&
    !!window.PublicKeyCredential &&
    typeof navigator !== 'undefined' &&
    !!navigator.credentials
  );
}

/** Best-effort: does this device have a usable platform authenticator (Touch ID,
 *  Windows Hello, Android)? Resolves false on anything unexpected. */
export async function platformAuthenticatorAvailable(): Promise<boolean> {
  if (!passkeysSupported()) return false;
  try {
    return await window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable();
  } catch {
    return false;
  }
}

function b64urlToBuf(s: string): ArrayBuffer {
  const pad = s.length % 4 === 0 ? '' : '='.repeat(4 - (s.length % 4));
  const b64 = (s + pad).replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function bufToB64url(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Run a registration ceremony from the server's CreationChallengeResponse and
 *  return the credential JSON to POST back to `.../register/finish`. */
export async function createPasskey(options: any): Promise<any> {
  const pk = JSON.parse(JSON.stringify(options.publicKey ?? options));
  pk.challenge = b64urlToBuf(pk.challenge);
  pk.user.id = b64urlToBuf(pk.user.id);
  if (Array.isArray(pk.excludeCredentials)) {
    pk.excludeCredentials = pk.excludeCredentials.map((c: any) => ({ ...c, id: b64urlToBuf(c.id) }));
  }
  const cred = (await navigator.credentials.create({ publicKey: pk })) as PublicKeyCredential | null;
  if (!cred) throw new Error('passkey creation cancelled');
  const resp = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bufToB64url(resp.attestationObject),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
    },
    extensions: cred.getClientExtensionResults(),
  };
}

/** Run an authentication ceremony from the server's RequestChallengeResponse and
 *  return the assertion JSON to POST back to `.../passkey/finish`. */
export async function getPasskey(options: any): Promise<any> {
  const pk = JSON.parse(JSON.stringify(options.publicKey ?? options));
  pk.challenge = b64urlToBuf(pk.challenge);
  if (Array.isArray(pk.allowCredentials)) {
    pk.allowCredentials = pk.allowCredentials.map((c: any) => ({ ...c, id: b64urlToBuf(c.id) }));
  }
  const cred = (await navigator.credentials.get({ publicKey: pk })) as PublicKeyCredential | null;
  if (!cred) throw new Error('passkey sign-in cancelled');
  const resp = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bufToB64url(resp.authenticatorData),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
      signature: bufToB64url(resp.signature),
      userHandle: resp.userHandle ? bufToB64url(resp.userHandle) : null,
    },
    extensions: cred.getClientExtensionResults(),
  };
}

/** A user cancelling the native prompt throws NotAllowedError/AbortError — these
 *  are not real failures, so callers can stay quiet. */
export function isUserCancel(e: unknown): boolean {
  return e instanceof DOMException && (e.name === 'NotAllowedError' || e.name === 'AbortError');
}
