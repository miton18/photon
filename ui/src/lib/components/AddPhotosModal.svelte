<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import { api } from '../api';
  import { displayThumb, type UIPhoto } from '../media';

  let {
    albumId,
    albumName,
    photos,
    onClose,
    onAdded,
  }: {
    albumId: string;
    albumName: string;
    photos: UIPhoto[]; // candidates: already-uploaded photos not yet in the album
    onClose: () => void;
    onAdded: () => Promise<void> | void;
  } = $props();

  let selected = $state<Set<string>>(new Set());
  let saving = $state(false);

  function toggle(id: string) {
    const n = new Set(selected);
    n.has(id) ? n.delete(id) : n.add(id);
    selected = n;
  }

  async function add() {
    if (selected.size === 0) return;
    saving = true;
    try {
      await api.addAlbumPhotos(albumId, [...selected]);
      toast({ tone: 'success', message: `Added ${selected.size} photo${selected.size === 1 ? '' : 's'} to “${albumName}”` });
      await onAdded();
      onClose();
    } catch (e) {
      toast({ tone: 'error', title: 'Add failed', message: String(e) });
    } finally {
      saving = false;
    }
  }
</script>

{#snippet footer()}
  <button class="pk-btn pk-btn-ghost" onclick={onClose}>Cancel</button>
  <button class="pk-btn pk-btn-primary" onclick={add} disabled={saving || selected.size === 0}>
    <Icon name="plus" size={15} />{saving ? 'Adding…' : `Add ${selected.size || ''}`}
  </button>
{/snippet}

<Modal title="Add photos" sub={`to “${albumName}” · ${photos.length} available`} icon="images" {onClose} {footer}>
  {#if photos.length === 0}
    <div class="pk-addp-empty"><Icon name="image-off" size={26} /><span>No other photos to add.</span></div>
  {:else}
    <div class="pk-addp-grid">
      {#each photos as p (p.id)}
        <button
          class={'pk-addp-tile' + (selected.has(p.id) ? ' is-sel' : '')}
          onclick={() => toggle(p.id)}
          title={p.filename}
        >
          <img loading="lazy" src={displayThumb(p)} alt={p.filename} />
          {#if selected.has(p.id)}
            <span class="pk-addp-check"><Icon name="check" size={13} strokeWidth={3} /></span>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</Modal>

<style>
  .pk-addp-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(84px, 1fr)); gap: 6px; max-height: 52vh; overflow-y: auto; }
  .pk-addp-tile { position: relative; aspect-ratio: 1; border-radius: var(--radius-sm); overflow: hidden; background: var(--photo-bg); border: 0; padding: 0; cursor: pointer; }
  .pk-addp-tile img { width: 100%; height: 100%; object-fit: cover; display: block; transition: transform var(--dur-fast) var(--ease-out); }
  .pk-addp-tile.is-sel { outline: 2.5px solid var(--accent); outline-offset: -2px; }
  .pk-addp-tile.is-sel img { transform: scale(0.9); }
  .pk-addp-check { position: absolute; top: 5px; right: 5px; width: 19px; height: 19px; border-radius: var(--radius-pill); background: var(--accent); color: #fff; display: grid; place-items: center; }
  .pk-addp-empty { display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 40px; color: var(--text-faint); }
</style>
