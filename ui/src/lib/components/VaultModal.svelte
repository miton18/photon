<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import { api, type User } from '../api';
  import { displayThumb, type UIPhoto } from '../media';

  let { me, onClose }: { me: User; onClose: () => void } = $props();

  let configured = $state(false);
  let count = $state(0);
  let phase = $state<'loading' | 'setpin' | 'locked' | 'unlocked'>('loading');
  let pin = $state('');
  let pin2 = $state('');
  let busy = $state(false);
  let photos = $state<UIPhoto[]>([]);

  async function init() {
    try {
      const st = await api.vaultStatus(me.id);
      configured = st.configured;
      count = st.count;
      phase = st.configured ? 'locked' : 'setpin';
    } catch (e) {
      toast({ tone: 'error', title: 'Vault unavailable', message: String(e) });
      onClose();
    }
  }
  init();

  async function setPin() {
    if (pin.length < 4 || pin !== pin2) {
      toast({ tone: 'error', message: 'PIN must be ≥4 digits and match' });
      return;
    }
    busy = true;
    try {
      await api.vaultSetPin(me.id, pin);
      toast({ tone: 'success', message: 'Vault PIN set' });
      configured = true;
      phase = 'locked';
      pin = '';
      pin2 = '';
    } catch (e) {
      toast({ tone: 'error', message: String(e) });
    } finally {
      busy = false;
    }
  }

  async function unlock() {
    busy = true;
    try {
      photos = await api.vaultUnlock(me.id, pin);
      phase = 'unlocked';
      pin = '';
    } catch {
      toast({ tone: 'error', icon: 'lock', message: 'Wrong PIN' });
      pin = '';
    } finally {
      busy = false;
    }
  }
</script>

<Modal title="Vault" sub="PIN-locked · hidden from timeline & search" icon="lock" {onClose}>
  {#if phase === 'loading'}
    <div class="pk-vault-center"><Icon name="loader-circle" size={22} spin /></div>
  {:else if phase === 'setpin'}
    <p class="pk-vault-lead">Create a PIN to protect your vault. You'll enter it every time you open the vault.</p>
    <div class="pk-field"><label for="vp1">New PIN</label><input id="vp1" type="password" inputmode="numeric" bind:value={pin} placeholder="••••" /></div>
    <div class="pk-field"><label for="vp2">Confirm PIN</label><input id="vp2" type="password" inputmode="numeric" bind:value={pin2} placeholder="••••" /></div>
    <button class="pk-btn pk-btn-primary" onclick={setPin} disabled={busy}><Icon name="shield-check" size={15} />Set PIN</button>
  {:else if phase === 'locked'}
    <div class="pk-vault-locked">
      <div class="pk-vault-badge"><Icon name="lock" size={26} /></div>
      <p class="pk-vault-lead">{count} item{count === 1 ? '' : 's'} locked. Enter your PIN to open.</p>
      <form
        class="pk-vault-pinform"
        onsubmit={(e) => { e.preventDefault(); unlock(); }}
      >
        <input type="password" inputmode="numeric" bind:value={pin} placeholder="••••" autofocus />
        <button class="pk-btn pk-btn-primary" type="submit" disabled={busy || !pin}><Icon name="key" size={15} />Unlock</button>
      </form>
    </div>
  {:else}
    <div class="pk-vault-bar">
      <span><Icon name="lock-open" size={14} /> {photos.length} item{photos.length === 1 ? '' : 's'}</span>
      <button class="pk-pill" onclick={() => { phase = 'locked'; photos = []; }}><Icon name="lock" size={12} />Lock</button>
    </div>
    {#if photos.length === 0}
      <div class="pk-vault-center" style="color:var(--text-faint)">Vault is empty.</div>
    {:else}
      <div class="pk-vault-grid">
        {#each photos as p (p.id)}
          <div class="pk-vault-tile" title={p.filename}><img loading="lazy" src={displayThumb(p)} alt={p.filename} /></div>
        {/each}
      </div>
    {/if}
  {/if}
</Modal>

<style>
  .pk-vault-center { display: grid; place-items: center; padding: 32px; color: var(--accent); }
  .pk-vault-lead { font-size: var(--text-sm); color: var(--text-muted); margin: 0 0 14px; line-height: var(--lh-snug); }
  .pk-vault-locked { display: flex; flex-direction: column; align-items: center; text-align: center; padding: 8px 0; }
  .pk-vault-badge { width: 64px; height: 64px; border-radius: var(--radius-pill); background: var(--accent-soft); color: var(--accent-text); display: grid; place-items: center; margin-bottom: 14px; }
  .pk-vault-pinform { display: flex; gap: 8px; width: 100%; max-width: 280px; }
  .pk-vault-pinform input { flex: 1; height: var(--control-h); padding: 0 12px; background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); color: var(--text); font: inherit; letter-spacing: 0.3em; text-align: center; outline: none; }
  .pk-vault-pinform input:focus { border-color: var(--accent-soft-bd); }
  .pk-vault-bar { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; font-size: var(--text-xs); color: var(--text-muted); }
  .pk-vault-bar span { display: inline-flex; align-items: center; gap: 6px; }
  .pk-vault-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(96px, 1fr)); gap: 6px; }
  .pk-vault-tile { aspect-ratio: 1; border-radius: var(--radius-sm); overflow: hidden; background: var(--photo-bg); }
  .pk-vault-tile img { width: 100%; height: 100%; object-fit: cover; display: block; }
</style>
