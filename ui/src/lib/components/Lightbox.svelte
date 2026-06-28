<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import InfoPanel from './InfoPanel.svelte';
  import Editor from './Editor.svelte';
  import { displayFull, displayThumb, type UIPhoto } from '../media';
  import { toast } from '../toast.svelte';
  import { castPhoto } from '../cast';
  import { printPhoto } from '../print';
  import { api, API, type PhotoFaces } from '../api';
  import { authedUrl } from '../session';

  function downloadOriginal(ph: UIPhoto) {
    const href = authedUrl(`${API}/api/photos/${ph.id}/original`);
    if (!href) return;
    const a = document.createElement('a');
    a.href = href;
    a.download = ph.filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    toast({ tone: 'success', icon: 'download', message: `Downloading ${ph.filename}` });
  }

  async function shareLink(ph: UIPhoto) {
    const link = `${location.origin}/#photo=${ph.id}`;
    try {
      await navigator.clipboard.writeText(link);
      toast({ tone: 'success', icon: 'link', message: 'Link copied to clipboard' });
    } catch {
      toast({ tone: 'error', message: 'Could not copy the link' });
    }
  }

  let {
    photos,
    index,
    onClose,
    onSetIndex,
    onFav,
    onRate,
    onArchive,
    onDelete,
    onSaved,
    onRestore,
    restoreLabel,
  }: {
    photos: UIPhoto[];
    index: number;
    onClose: () => void;
    onSetIndex: (i: number) => void;
    onFav: (id: string) => void;
    onRate: (id: string, n: number) => void;
    onArchive: (id: string) => void;
    onDelete: (id: string) => void;
    onSaved: (p: UIPhoto) => void;
    onRestore?: (id: string) => void;
    restoreLabel?: string;
  } = $props();

  let info = $state(true);
  let zoom = $state(false);
  let editing = $state(false);
  let showFaces = $state(false);

  const p = $derived(photos[index]);

  // Face overlay: fetched lazily when toggled on, cached per photo id.
  const faceCache = new Map<string, PhotoFaces>();
  let faces = $state<PhotoFaces | null>(null);

  // The rendered <img>'s actual rect WITHIN the wrapper (offset + size). Face
  // boxes are positioned against THIS, not the wrapper, so they land on the image
  // even when it's letterboxed/centered inside a larger wrapper.
  let imgEl = $state<HTMLImageElement>();
  let imgBox = $state({ l: 0, t: 0, w: 0, h: 0 });
  function measureImg() {
    if (imgEl) imgBox = { l: imgEl.offsetLeft, t: imgEl.offsetTop, w: imgEl.offsetWidth, h: imgEl.offsetHeight };
  }
  $effect(() => {
    // Re-measure on window resize, and whenever the photo/zoom/faces change
    // (referenced so the effect re-runs; rAF waits for layout to settle).
    void p?.id; void zoom; void faces;
    const onResize = () => measureImg();
    window.addEventListener('resize', onResize);
    const r = requestAnimationFrame(measureImg);
    return () => {
      window.removeEventListener('resize', onResize);
      cancelAnimationFrame(r);
    };
  });

  $effect(() => {
    const id = p?.id;
    if (!showFaces || !id) {
      faces = null;
      return;
    }
    const cached = faceCache.get(id);
    if (cached) {
      faces = cached;
      return;
    }
    faces = null;
    let cancelled = false;
    api
      .photoFaces(id)
      .then((res) => {
        faceCache.set(id, res);
        if (!cancelled && p?.id === id) faces = res;
      })
      .catch((e) => console.warn('photoFaces failed', e));
    return () => {
      cancelled = true;
    };
  });

  /** Stable string → HSL color so the same person reads the same hue everywhere. */
  function hueColor(s: string | null, light: number, sat = 70): string {
    if (!s) return `hsl(0 0% ${light}%)`; // neutral for faces with no person_id
    let h = 0;
    for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) % 360;
    return `hsl(${h} ${sat}% ${light}%)`;
  }

  $effect(() => {
    void index;
    zoom = false;
  });

  // current filmstrip thumb kept in view
  let stripEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    void index;
    const parent = stripEl;
    const el = parent?.querySelector<HTMLElement>('.is-current');
    if (parent && el) {
      const target = el.offsetLeft - parent.clientWidth / 2 + el.clientWidth / 2;
      parent.scrollTo({ left: Math.max(0, target), behavior: 'smooth' });
    }
  });

  $effect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (editing) return;
      if (e.key === 'Escape') onClose();
      else if (e.key === 'ArrowRight') onSetIndex(Math.min(photos.length - 1, index + 1));
      else if (e.key === 'ArrowLeft') onSetIndex(Math.max(0, index - 1));
      else if (e.key.toLowerCase() === 'i') info = !info;
      else if (e.key.toLowerCase() === 'f') onFav(p.id);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if p}
  <div class="pk-lb">
    <div class="pk-lb-top">
      <button class="pk-lb-ic" onclick={onClose} title="Close (Esc)"><Icon name="x" size={20} /></button>
      <div>
        <div class="pk-lb-name">{p.filename}</div>
        <div class="pk-lb-date">{p.city}{p.country ? `, ${p.country}` : ''} · {p.taken_at.slice(0, 10)}</div>
      </div>
      <div class="pk-lb-actions">
        <button class={'pk-lb-ic' + (p.favorite ? ' is-fav' : '')} onclick={() => { onFav(p.id); toast({ tone: 'success', message: (p.favorite ? 'Removed from' : 'Added to') + ' favorites' }); }} title="Favorite (f)">
          <Icon name="star" size={19} fill={p.favorite ? 'currentColor' : 'none'} />
        </button>
        <button class="pk-lb-ic" onclick={() => (editing = true)} title="Edit"><Icon name="sliders-horizontal" size={19} /></button>
        <button class="pk-lb-ic" onclick={() => shareLink(p)} title="Share"><Icon name="share-2" size={19} /></button>
        <button class="pk-lb-ic" onclick={() => castPhoto(displayFull(p), p.filename)} title="Cast to TV"><Icon name="cast" size={19} /></button>
        <button class="pk-lb-ic" onclick={() => printPhoto(displayFull(p), p.portrait)} title="Print (10×15)"><Icon name="printer" size={19} /></button>
        <button class="pk-lb-ic" onclick={() => downloadOriginal(p)} title="Download"><Icon name="download" size={19} /></button>
        <span class="pk-lb-sep"></span>
        <button class={'pk-lb-ic' + (showFaces ? ' is-on' : '')} onclick={() => (showFaces = !showFaces)} title="Show faces"><Icon name="scan-face" size={19} /></button>
        <button class={'pk-lb-ic' + (info ? ' is-on' : '')} onclick={() => (info = !info)} title="Info (i)"><Icon name="info" size={19} /></button>
        {#if onRestore}
          <button class="pk-lb-ic" onclick={() => { onRestore?.(p.id); onClose(); }} title={restoreLabel ?? 'Restore'}><Icon name="undo-2" size={19} /></button>
        {:else}
          <button class="pk-lb-ic" onclick={() => { onArchive(p.id); toast({ tone: 'default', icon: 'archive', message: 'Archived' }); }} title="Archive"><Icon name="archive" size={19} /></button>
          <button class="pk-lb-ic" onclick={() => { onDelete(p.id); onClose(); }} title="Delete"><Icon name="trash-2" size={19} /></button>
        {/if}
      </div>
    </div>

    <div class="pk-lb-body">
      <div class="pk-lb-stage">
        {#if index > 0}
          <button class="pk-lb-nav prev" onclick={() => onSetIndex(index - 1)} title="Previous (←)"><Icon name="chevron-left" size={22} /></button>
        {/if}
        <div class={'pk-lb-imgwrap' + (zoom ? ' is-zoom' : '')}>
          <img
            bind:this={imgEl}
            onload={measureImg}
            class={'pk-lb-img' + (zoom ? ' is-zoom' : '')}
            src={displayFull(p)}
            alt={p.filename}
            onclick={() => (zoom = !zoom)}
            title={zoom ? 'Click to fit' : 'Click to zoom'}
          />
          {#if showFaces && !editing && faces && faces.source_width > 0 && faces.source_height > 0 && imgBox.w > 0}
            {#each faces.faces as f (f.id)}
              {@const label = f.person_name ?? f.person_label ?? ''}
              {@const col = hueColor(f.person_id, 62)}
              <div
                class="pk-face-box"
                style="left:{imgBox.l + (f.bbox[0] / faces.source_width) * imgBox.w}px; top:{imgBox.t + (f.bbox[1] / faces.source_height) * imgBox.h}px; width:{(f.bbox[2] / faces.source_width) * imgBox.w}px; height:{(f.bbox[3] / faces.source_height) * imgBox.h}px; border-color:{col};"
              >
                {#if label}
                  <span class="pk-face-label" style="background:{col};">{label}</span>
                {/if}
              </div>
            {/each}
          {/if}
        </div>
        {#if index < photos.length - 1}
          <button class="pk-lb-nav next" onclick={() => onSetIndex(index + 1)} title="Next (→)"><Icon name="chevron-right" size={22} /></button>
        {/if}
        <div class="pk-lb-counter pk-mono">{index + 1} / {photos.length}</div>
      </div>
      {#if info}<InfoPanel {p} onRate={(n) => onRate(p.id, n)} onUpdated={onSaved} />{/if}
    </div>

    <div class="pk-filmstrip" bind:this={stripEl}>
      {#each photos as ph, i (ph.id)}
        <button
          class={'pk-strip-thumb' + (i === index ? ' is-current' : '')}
          onclick={() => onSetIndex(i)}
        >
          <img loading="lazy" src={displayThumb(ph)} alt="" />
        </button>
      {/each}
    </div>

    {#if editing}
      <Editor photo={p} onClose={() => (editing = false)} onSaved={(u) => { onSaved(u); editing = false; }} />
    {/if}
  </div>
{/if}
