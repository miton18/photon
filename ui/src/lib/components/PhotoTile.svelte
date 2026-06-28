<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { displayThumb, type UIPhoto } from '../media';

  let {
    photo,
    w,
    h,
    selected = false,
    selecting = false,
    onOpen,
    onToggleSel,
    onHover,
    onFav,
  }: {
    photo: UIPhoto;
    w: number;
    h: number;
    selected?: boolean;
    selecting?: boolean;
    onOpen: (p: UIPhoto) => void;
    onToggleSel: (id: string) => void;
    onHover: (p: UIPhoto | null) => void;
    onFav: (id: string) => void;
  } = $props();
</script>

<div
  class={'pk-tile' + (selected ? ' is-selected' : '')}
  style={`width:${w}px;height:${h}px`}
  role="button"
  tabindex="0"
  onclick={() => (selecting ? onToggleSel(photo.id) : onOpen(photo))}
  onkeydown={(e) => { if (e.key === 'Enter') (selecting ? onToggleSel(photo.id) : onOpen(photo)); }}
  onmouseenter={() => onHover(photo)}
  onmouseleave={() => onHover(null)}
>
  <img loading="lazy" src={displayThumb(photo)} alt={photo.filename} />
  <button class="pk-tile-check" onclick={(e) => { e.stopPropagation(); onToggleSel(photo.id); }} aria-label="Select">
    <Icon name="check" size={12} strokeWidth={3} />
  </button>
  <button
    class={'pk-tile-fav' + (photo.favorite ? ' is-fav' : '')}
    onclick={(e) => { e.stopPropagation(); onFav(photo.id); }}
    aria-label="Favorite"
  >
    <Icon name="star" size={15} fill={photo.favorite ? 'currentColor' : 'none'} />
  </button>
  {#if photo.shared}
    <span class="pk-badge-shared"><Icon name="users" size={9} strokeWidth={2.5} />Shared</span>
  {/if}
  {#if photo.kind === 'raw'}
    <span class="pk-badge-raw">RAW</span>
  {/if}
  {#if photo.companions.some((c) => c.kind === 'raw')}
    <span class="pk-badge-raw" style="right:auto;left:7px;top:auto;bottom:6px">+RAW</span>
  {/if}
  {#if photo.kind === 'video'}
    <span class="pk-badge-vid"><Icon name="play" size={11} fill="currentColor" />{photo.dur}</span>
  {/if}
  {#if photo.people.length > 0}
    <span class="pk-badge-people"><Icon name="user" size={13} />{photo.people.length}</span>
  {/if}
</div>
