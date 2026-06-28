<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import type { Album } from '../api';

  let {
    albums,
    count,
    busy = false,
    onPick,
    onClose,
  }: {
    albums: Album[];
    count: number;
    busy?: boolean;
    onPick: (albumId: string) => void;
    onClose: () => void;
  } = $props();
</script>

<Modal title="Add to album" sub={`${count} selected`} icon="images" {onClose}>
  {#if albums.length === 0}
    <div class="pk-pick-empty"><Icon name="image-off" size={26} /><span>No albums yet — create one first.</span></div>
  {:else}
    <div class="pk-pick-list">
      {#each albums as a (a.id)}
        <button class="pk-pick-row" disabled={busy} onclick={() => onPick(a.id)} title={`Add to ${a.name}`}>
          <Icon name="folder" size={16} />
          <span class="pk-pick-name">{a.name}</span>
          <span class="pk-pick-count pk-mono">{a.photo_ids.length}</span>
        </button>
      {/each}
    </div>
  {/if}
</Modal>

<style>
  .pk-pick-list { display: flex; flex-direction: column; gap: 4px; max-height: 52vh; overflow-y: auto; }
  .pk-pick-row { display: flex; align-items: center; gap: 10px; width: 100%; text-align: left; padding: 10px 12px; border: 1px solid var(--border-faint); border-radius: var(--radius-md); background: var(--bg-subtle); color: var(--text); cursor: pointer; }
  .pk-pick-row:hover:not(:disabled) { background: var(--accent-soft); border-color: var(--accent-soft-bd); }
  .pk-pick-row:disabled { opacity: 0.5; cursor: default; }
  .pk-pick-name { flex: 1; font-weight: var(--fw-medium); }
  .pk-pick-count { color: var(--text-faint); font-size: var(--text-xs); }
  .pk-pick-empty { display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 40px; color: var(--text-faint); }
</style>
