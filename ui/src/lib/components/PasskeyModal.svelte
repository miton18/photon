<script lang="ts">
  /* Passkey management: enroll a new passkey on this device, list existing ones,
     and revoke them. Opened from the post-login prompt and the sidebar. */
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { api, type PasskeyInfo } from '../api';
  import { createPasskey, passkeysSupported, isUserCancel } from '../passkey';
  import { toast } from '../toast.svelte';

  let { userId, onClose }: { userId: string; onClose: () => void } = $props();

  let list = $state<PasskeyInfo[]>([]);
  let loading = $state(true);
  let busy = $state(false);
  const supported = passkeysSupported();

  async function load() {
    loading = true;
    try {
      list = await api.passkeys(userId);
    } catch {
      list = [];
    } finally {
      loading = false;
    }
  }
  load();

  async function add() {
    if (busy) return;
    busy = true;
    try {
      const { handle, options } = await api.passkeyRegisterStart(userId);
      const credential = await createPasskey(options);
      const label =
        (typeof navigator !== 'undefined' && /Mac/.test(navigator.platform) && 'Mac') ||
        (typeof navigator !== 'undefined' && /Win/.test(navigator.platform) && 'Windows') ||
        'This device';
      const pk = await api.passkeyRegisterFinish(userId, handle, credential, label);
      list = [...list, pk];
      toast({ tone: 'success', message: 'Passkey enabled on this device' });
    } catch (e) {
      if (!isUserCancel(e)) {
        // The common post-ceremony failure is the authenticator completing WITHOUT
        // user verification, which passwordless passkeys require by policy.
        toast({
          tone: 'error',
          title: 'Could not enable passkey',
          message:
            "Your device must verify it's you (fingerprint, face or PIN). If it's a security key, set a PIN on it; in Chrome DevTools' virtual authenticator, enable “User verification”.",
          duration: 7000,
        });
      }
    } finally {
      busy = false;
    }
  }

  async function remove(id: string) {
    try {
      await api.passkeyDelete(userId, id);
      list = list.filter((p) => p.id !== id);
    } catch {
      toast({ tone: 'error', message: 'Could not remove passkey' });
    }
  }

  const fmt = (s: string | null) =>
    s ? new Date(s).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }) : '—';
</script>

<Modal title="Passkeys" sub="Sign in without a password" icon="key-round" {onClose}>
  <p class="pk-pk-intro">
    A passkey lets you sign in to Photon with your device's fingerprint, face, or PIN —
    no password to type or remember. The private key never leaves this device.
  </p>

  {#if !supported}
    <div class="pk-pk-warn"><Icon name="triangle-alert" size={15} /> This browser doesn't support passkeys.</div>
  {/if}

  {#if loading}
    <div class="pk-pk-empty">Loading…</div>
  {:else if list.length === 0}
    <div class="pk-pk-empty">No passkeys yet on your account.</div>
  {:else}
    <ul class="pk-pk-list">
      {#each list as p (p.id)}
        <li class="pk-pk-item">
          <span class="pk-pk-ic"><Icon name="key-round" size={16} /></span>
          <div class="pk-pk-meta">
            <span class="pk-pk-name">{p.name || 'Passkey'}</span>
            <span class="pk-pk-sub">Added {fmt(p.created_at)} · last used {fmt(p.last_used_at)}</span>
          </div>
          <button class="pk-btn pk-btn-ghost pk-btn-sm" onclick={() => remove(p.id)} title="Remove">
            <Icon name="trash-2" size={14} />
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  {#snippet footer()}
    <button class="pk-btn pk-btn-ghost" onclick={onClose}>Done</button>
    <button class="pk-btn pk-btn-primary" onclick={add} disabled={!supported || busy}>
      <Icon name="plus" size={15} />{busy ? 'Waiting for device…' : 'Add a passkey'}
    </button>
  {/snippet}
</Modal>

<style>
  .pk-pk-intro { font-size: var(--text-sm); color: var(--text-muted); line-height: 1.55; margin-bottom: 14px; }
  .pk-pk-warn { display: flex; align-items: center; gap: 8px; color: var(--warning); background: var(--warning-soft, rgba(200,140,0,.1)); padding: 8px 10px; border-radius: var(--radius-md); font-size: var(--text-xs); margin-bottom: 12px; }
  .pk-pk-empty { color: var(--text-faint); font-size: var(--text-sm); padding: 14px 0; text-align: center; }
  .pk-pk-list { display: flex; flex-direction: column; gap: 6px; }
  .pk-pk-item { display: flex; align-items: center; gap: 11px; padding: 10px 12px; border: 1px solid var(--border); border-radius: var(--radius-md); }
  .pk-pk-ic { color: var(--accent); display: grid; place-items: center; }
  .pk-pk-meta { display: flex; flex-direction: column; min-width: 0; }
  .pk-pk-name { font-weight: var(--fw-semibold); font-size: var(--text-sm); }
  .pk-pk-sub { font-size: var(--text-2xs); color: var(--text-faint); font-family: var(--font-mono); }
  .pk-pk-item .pk-btn { margin-left: auto; }
</style>
