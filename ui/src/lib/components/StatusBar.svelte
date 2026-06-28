<script lang="ts">
  import Icon from '../icons/Icon.svelte';

  let {
    selecting = false,
    selCount = 0,
    onClearSel,
    onShare,
    onAddAlbum,
    onFavorite,
    onArchive,
    onDelete,
  }: {
    selecting?: boolean;
    selCount?: number;
    onClearSel: () => void;
    onShare: () => void;
    onAddAlbum: () => void;
    onFavorite: () => void;
    onArchive: () => void;
    onDelete: () => void;
  } = $props();
</script>

{#if selecting && selCount > 0}
  <footer class="pk-statusbar">
    <div class="pk-selbar">
      <span class="pk-sel-count">{selCount} selected</span>
      <div class="pk-sel-actions">
        <button class="pk-sel-act" onclick={onShare}><Icon name="share-2" size={14} />Share</button>
        <button class="pk-sel-act" onclick={onAddAlbum}><Icon name="folder-plus" size={14} />Add to album</button>
        <button class="pk-sel-act" onclick={onFavorite}><Icon name="star" size={14} />Favorite</button>
        <button class="pk-sel-act" onclick={onArchive}><Icon name="archive" size={14} />Archive</button>
        <button class="pk-sel-act danger" onclick={onDelete}><Icon name="trash-2" size={14} />Delete</button>
      </div>
      <div class="pk-sb-right">
        <button class="pk-sel-act" onclick={onClearSel}><Icon name="x" size={14} />Clear</button>
      </div>
    </div>
  </footer>
{/if}
