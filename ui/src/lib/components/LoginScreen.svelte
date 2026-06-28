<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import Logo from './Logo.svelte';
  import { thumb } from '../media';
  import { toast } from '../toast.svelte';
  import { TotpRequired, TotpInvalid, api, API } from '../api';
  import { passkeysSupported, isUserCancel } from '../passkey';
  import { setPersistent } from '../session';

  let {
    onLogin,
    onPasskeyLogin,
  }: {
    onLogin: (email: string, password: string, totp?: string) => Promise<void>;
    onPasskeyLogin: () => Promise<void>;
  } = $props();

  const pkSupported = passkeysSupported();
  let pkBusy = $state(false);
  async function passkey() {
    pkBusy = true;
    error = '';
    try {
      await onPasskeyLogin();
    } catch (e) {
      if (!isUserCancel(e)) error = 'Passkey sign-in failed. Try your password.';
    } finally {
      pkBusy = false;
    }
  }

  let email = $state('');
  let password = $state('');
  let totp = $state('');
  // Revealed only after the server reports the account is 2FA-enrolled
  // (`totp_required`); the user then enters their authenticator code and re-submits.
  let needTotp = $state(false);
  let busy = $state(false);
  let error = $state('');
  let show = $state(false);
  let remember = $state(true);
  let dark = $state(
    typeof document !== 'undefined' && document.documentElement.classList.contains('dark'),
  );

  const MOSAIC: { seed: number; span?: boolean }[] = [
    { seed: 417, span: true },
    { seed: 156 },
    { seed: 33 },
    { seed: 261, span: true },
    { seed: 12 },
    { seed: 504 },
    { seed: 88, span: true },
    { seed: 199 },
    { seed: 324 },
    { seed: 142, span: true },
    { seed: 277 },
    { seed: 461 },
  ];

  async function submit(e: Event) {
    e.preventDefault();
    if (!email.trim() || !password) return;
    if (needTotp && !totp.trim()) return;
    busy = true;
    error = '';
    // Honor "Keep me signed in": persistent (localStorage) vs ephemeral (session).
    setPersistent(remember);
    try {
      await onLogin(email.trim(), password, needTotp ? totp.trim() : undefined);
    } catch (err) {
      if (err instanceof TotpRequired) {
        // Password was correct; the account needs a 2FA code. Reveal the field.
        needTotp = true;
        error = 'Enter the 6-digit code from your authenticator app.';
      } else if (err instanceof TotpInvalid) {
        error = 'Invalid authenticator code. Try again.';
        totp = '';
      } else {
        error = 'Invalid email or password.';
        password = '';
        totp = '';
        needTotp = false;
      }
    } finally {
      busy = false;
    }
  }

  function toggleTheme() {
    dark = document.documentElement.classList.toggle('dark');
  }

  function forgot() {
    toast({ tone: 'info', icon: 'mail', message: 'Ask your administrator to reset your password.' });
  }

  // Whether OIDC web login is configured server-side; checked on mount. The
  // button is only shown when true (else the page falls back to its toast).
  let oidcReady = $state(false);
  $effect(() => {
    api.oidcAvailable().then((ok) => (oidcReady = ok));
  });

  function oidc() {
    if (oidcReady) {
      // Full-page redirect: the server begins the auth-code flow and, after the
      // IdP round-trip, redirects back to `/?token=…` which App.svelte consumes.
      window.location.href = `${API}/api/auth/oidc/login`;
      return;
    }
    toast({ tone: 'info', message: 'OpenID sign-in is not configured on this instance.' });
  }

  function requestAccess() {
    toast({ tone: 'info', message: 'Sign-up is managed by your administrator.' });
  }

  function keyActivate(e: KeyboardEvent, handler: () => void) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      handler();
    }
  }
</script>

<div class="pk-auth">
  <!-- LEFT: showcase -->
  <div class="pk-auth-show">
    <div class="pk-auth-mosaic" aria-hidden="true">
      {#each MOSAIC as tile (tile.seed)}
        <div class={tile.span ? 'span2' : ''}>
          <img loading="lazy" src={thumb(tile.seed)} alt="" />
        </div>
      {/each}
    </div>
    <div class="pk-auth-wash"></div>
    <div class="pk-auth-show-top">
      <div class="pk-auth-show-brand">
        <span class="m"><Logo size={32} color="#fff" /></span>
        <span class="wm">Photon</span>
      </div>
    </div>
    <div class="pk-auth-show-btm">
      <div class="pk-auth-tag">Your memories.<br /><b>Your server.</b></div>
      <p class="pk-auth-sub">
        Self-hosted photo &amp; video backup. Everything stays on hardware you control — no cloud, no
        tracking, no limits.
      </p>
      <div class="pk-auth-trust">
        <span><Icon name="lock" size={13} /> End-to-end encrypted</span>
        <span><Icon name="server" size={13} /> Self-hosted</span>
        <span><Icon name="git-branch" size={13} /> Open source</span>
      </div>
    </div>
  </div>

  <!-- RIGHT: form -->
  <div class="pk-auth-form">
    <div class="pk-auth-form-top">
      <div class="pk-brand-logo"><Logo size={28} /></div>
      <span class="wm">Photon</span>
      <button type="button" class="pk-btn pk-btn-ghost" title="Toggle theme" onclick={toggleTheme}>
        <Icon name={dark ? 'sun' : 'moon'} size={16} />
      </button>
    </div>
    <form class="pk-auth-body" onsubmit={submit}>
      <div class="pk-auth-head">
        <h1>Welcome back</h1>
        <p>Sign in to your Photon instance to reach your library.</p>
      </div>
      <div class="pk-authfield">
        <label for="email">Email</label>
        <div class="pk-authinput">
          <Icon name="mail" size={16} />
          <input
            id="email"
            type="text"
            autocomplete="username"
            bind:value={email}
            placeholder="you@example.com or alice"
          />
        </div>
      </div>
      <div class="pk-authfield">
        <div class="pk-authfield-row">
          <label for="password">Password</label>
          <span
            class="link"
            role="button"
            tabindex="0"
            onclick={forgot}
            onkeydown={(e) => keyActivate(e, forgot)}>Forgot password?</span
          >
        </div>
        <div class="pk-authinput">
          <Icon name="lock" size={16} />
          <input
            id="password"
            type={show ? 'text' : 'password'}
            autocomplete="current-password"
            bind:value={password}
            placeholder="••••••••••"
          />
          <button type="button" class="reveal" title={show ? 'Hide' : 'Show'} onclick={() => (show = !show)}>
            <Icon name={show ? 'eye-off' : 'eye'} size={16} />
          </button>
        </div>
      </div>
      {#if needTotp}
        <div class="pk-authfield">
          <label for="totp">Authenticator code</label>
          <div class="pk-authinput">
            <Icon name="shield-check" size={16} />
            <input
              id="totp"
              type="text"
              inputmode="numeric"
              autocomplete="one-time-code"
              maxlength="6"
              bind:value={totp}
              placeholder="123456"
            />
          </div>
        </div>
      {/if}
      <div
        class="pk-auth-remember"
        role="button"
        tabindex="0"
        onclick={() => (remember = !remember)}
        onkeydown={(e) => keyActivate(e, () => (remember = !remember))}
      >
        <span class={'pk-auth-check' + (remember ? ' on' : '')}><Icon name="check" size={12} /></span>
        Keep me signed in on this device
      </div>
      {#if error}
        <div class="pk-auth-error"><Icon name="alert-circle" size={14} />{error}</div>
      {/if}
      <button type="submit" class="pk-btn pk-btn-primary pk-auth-submit" disabled={busy}>
        {#if busy}
          <Icon name="loader-circle" size={17} spin /> Signing in…
        {:else}
          Sign in <Icon name="arrow-right" size={16} />
        {/if}
      </button>
      {#if pkSupported || oidcReady}<div class="pk-auth-or">or</div>{/if}
      {#if pkSupported}
        <button type="button" class="pk-btn pk-btn-outline pk-auth-oauth" onclick={passkey} disabled={pkBusy}>
          <Icon name="key-round" size={16} /> {pkBusy ? 'Waiting for your device…' : 'Sign in with a passkey'}
        </button>
      {/if}
      {#if oidcReady}
        <button type="button" class="pk-btn pk-btn-outline pk-auth-oauth" onclick={oidc}>
          <Icon name="globe" size={16} /> Continue with OpenID
        </button>
      {/if}
      <div class="pk-auth-foot">
        <span class="pk-auth-signup">
          New here?
          <a
            role="button"
            tabindex="0"
            onclick={requestAccess}
            onkeydown={(e) => keyActivate(e, requestAccess)}>Request access</a
          >
        </span>
      </div>
      <div class="pk-auth-form-btm">
        <span class="ok"><Icon name="shield-check" size={13} /> Secure connection</span>
        <span class="dot"></span>
        <span>{location.host}</span>
        <span class="dot"></span>
        <span>Photon</span>
      </div>
    </form>
  </div>
</div>

<style>
  .pk-auth { display: grid; grid-template-columns: 1.05fr .95fr; min-height: 100vh; background: var(--bg-base); }

  /* showcase panel */
  .pk-auth-show { position: relative; overflow: hidden; display: flex; flex-direction: column; justify-content: space-between; padding: 34px 38px; }
  .pk-auth-mosaic { position: absolute; inset: 0; display: grid; grid-template-columns: repeat(4, 1fr); grid-auto-rows: 1fr; gap: 6px; padding: 6px; }
  .pk-auth-mosaic > div { overflow: hidden; border-radius: var(--radius-sm); }
  .pk-auth-mosaic img { width: 100%; height: 100%; object-fit: cover; display: block; }
  .pk-auth-mosaic .span2 { grid-row: span 2; }
  .pk-auth-wash { position: absolute; inset: 0; background:
    linear-gradient(180deg, color-mix(in srgb, var(--bg-base) 30%, transparent) 0%, color-mix(in srgb, var(--bg-base) 8%, transparent) 38%, color-mix(in srgb, var(--bg-base) 88%, transparent) 100%),
    radial-gradient(120% 80% at 12% 100%, color-mix(in srgb, var(--accent) 42%, transparent) 0%, transparent 62%); }
  .pk-auth-show-top, .pk-auth-show-btm { position: relative; z-index: 1; }
  .pk-auth-show-brand { display: flex; align-items: center; gap: 10px; }
  .pk-auth-show-brand .m { width: 34px; height: 34px; border-radius: var(--radius-md); background: var(--accent); color: var(--accent-fg); display: grid; place-items: center; box-shadow: var(--shadow-md); }
  .pk-auth-show-brand .wm { font-family: var(--font-display); font-weight: var(--fw-bold); font-size: var(--text-xl); letter-spacing: var(--ls-tight); color: #fff; text-shadow: 0 1px 12px rgba(0,0,0,.4); }
  .pk-auth-tag { font-family: var(--font-display); font-weight: var(--fw-bold); font-size: clamp(26px, 3vw, 38px); line-height: 1.08; letter-spacing: var(--ls-tight); color: #fff; max-width: 13ch; text-shadow: 0 2px 24px rgba(0,0,0,.5); }
  .pk-auth-tag b { color: var(--accent-200, #c7c5ff); }
  .pk-auth-sub { margin-top: 14px; font-size: var(--text-sm); line-height: var(--lh-normal); color: rgba(255,255,255,.82); max-width: 40ch; text-shadow: 0 1px 10px rgba(0,0,0,.5); }
  .pk-auth-trust { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 22px; }
  .pk-auth-trust span { display: inline-flex; align-items: center; gap: 6px; font-size: var(--text-xs); font-weight: var(--fw-medium); color: #fff; padding: 5px 11px; border-radius: var(--radius-pill); background: rgba(255,255,255,.13); border: 1px solid rgba(255,255,255,.18); backdrop-filter: blur(8px); }

  /* form panel */
  .pk-auth-form { display: flex; flex-direction: column; padding: 30px 40px; min-width: 0; }
  .pk-auth-form-top { display: flex; align-items: center; gap: 10px; }
  .pk-auth-form-top .wm { font-family: var(--font-display); font-weight: var(--fw-bold); font-size: var(--text-lg); letter-spacing: var(--ls-tight); }
  .pk-auth-form-top .pk-btn { margin-left: auto; }
  .pk-auth-body { flex: 1; display: flex; flex-direction: column; justify-content: center; max-width: 380px; width: 100%; margin: 0 auto; }
  .pk-auth-head h1 { font-family: var(--font-display); font-size: var(--text-2xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); margin: 0 0 6px; }
  .pk-auth-head p { font-size: var(--text-sm); color: var(--text-muted); margin: 0 0 24px; line-height: var(--lh-normal); }

  .pk-authfield { display: flex; flex-direction: column; gap: 6px; margin-bottom: 14px; }
  .pk-authfield-row { display: flex; align-items: baseline; justify-content: space-between; }
  .pk-authfield label { font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text); }
  .pk-authfield .link { font-size: var(--text-xs); color: var(--accent-text); font-weight: var(--fw-medium); cursor: pointer; }
  .pk-authfield .link:hover { text-decoration: underline; }
  .pk-authinput { display: flex; align-items: center; gap: 9px; height: 42px; padding: 0 12px; border-radius: var(--radius-md); background: var(--bg-subtle); border: 1px solid var(--border); transition: border-color var(--dur-fast) var(--ease-out), background var(--dur-fast) var(--ease-out), box-shadow var(--dur-fast) var(--ease-out); }
  .pk-authinput:focus-within { border-color: var(--accent); background: var(--surface); box-shadow: 0 0 0 3px var(--accent-soft); }
  .pk-authinput input { flex: 1; min-width: 0; background: none; border: 0; outline: none; color: var(--text); font-size: var(--text-sm); font-family: var(--font-sans); }
  .pk-authinput input::placeholder { color: var(--text-faint); }
  .pk-authinput .reveal { border: 0; background: none; color: var(--text-faint); cursor: pointer; display: grid; place-items: center; padding: 2px; border-radius: var(--radius-xs); }
  .pk-authinput .reveal:hover { color: var(--text); }

  .pk-auth-remember { display: flex; align-items: center; gap: 8px; margin: 4px 0 20px; cursor: pointer; user-select: none; font-size: var(--text-sm); color: var(--text-muted); }
  .pk-auth-check { width: 17px; height: 17px; border-radius: var(--radius-xs); border: 1px solid var(--border-strong); display: grid; place-items: center; color: transparent; flex: none; transition: all var(--dur-fast) var(--ease-out); }
  .pk-auth-check.on { background: var(--accent); border-color: var(--accent); color: var(--accent-fg); }

  .pk-auth-submit { width: 100%; justify-content: center; height: 44px; font-size: var(--text-sm); font-weight: var(--fw-semibold); }

  .pk-auth-or { display: flex; align-items: center; gap: 12px; margin: 20px 0; color: var(--text-faint); font-size: var(--text-xs); }
  .pk-auth-or::before, .pk-auth-or::after { content: ''; flex: 1; height: 1px; background: var(--border); }
  .pk-auth-oauth { width: 100%; justify-content: center; height: 42px; font-weight: var(--fw-medium); }

  .pk-auth-error { display: flex; align-items: center; gap: 7px; font-size: var(--text-xs); color: var(--danger); margin: -6px 0 14px; }
  .pk-auth-foot { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-top: 18px; }
  .pk-auth-signup { font-size: var(--text-sm); color: var(--text-muted); }
  .pk-auth-signup a { color: var(--accent-text); font-weight: var(--fw-medium); cursor: pointer; }
  .pk-auth-signup a:hover { text-decoration: underline; }
  .pk-auth-form-btm { display: flex; align-items: center; gap: 10px; font-family: var(--font-mono); font-size: 10.5px; color: var(--text-faint); padding-top: 8px; }
  .pk-auth-form-btm .dot { width: 3px; height: 3px; border-radius: 50%; background: currentColor; opacity: .5; }
  .pk-auth-form-btm .ok { color: var(--success); display: inline-flex; align-items: center; gap: 5px; }

  @media (max-width: 880px) { .pk-auth { grid-template-columns: 1fr; } .pk-auth-show { display: none; } }
</style>
