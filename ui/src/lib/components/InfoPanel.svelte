<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { API, api, type PhotoFaces } from '../api';
  import { authedUrl } from '../session';
  import { toast } from '../toast.svelte';
  import type { UIPhoto } from '../media';

  let {
    p,
    onRate,
    onUpdated,
  }: { p: UIPhoto; onRate: (n: number) => void; onUpdated?: (p: UIPhoto) => void } = $props();

  // Inline tag editing: reveal an input, PATCH the override `tags` array (RFC 6902).
  let addingTag = $state(false);
  let tagInput = $state('');
  let savingTag = $state(false);

  async function commitTag() {
    const t = tagInput.trim();
    if (!t || savingTag) return;
    if (p.tags.includes(t)) {
      tagInput = '';
      addingTag = false;
      return;
    }
    savingTag = true;
    try {
      const updated = await api.patchMetadata(p.id, [{ op: 'add', path: '/tags', value: [...p.tags, t] }]);
      tagInput = '';
      addingTag = false;
      onUpdated?.(updated);
    } catch (e) {
      console.warn('add tag failed', e);
      toast({ tone: 'error', title: 'Could not add tag', message: String(e) });
    } finally {
      savingTag = false;
    }
  }

  async function removeTag(tag: string) {
    if (savingTag) return;
    savingTag = true;
    try {
      const updated = await api.patchMetadata(p.id, [{ op: 'add', path: '/tags', value: p.tags.filter((x) => x !== tag) }]);
      onUpdated?.(updated);
    } catch (e) {
      console.warn('remove tag failed', e);
      toast({ tone: 'error', title: 'Could not remove tag', message: String(e) });
    } finally {
      savingTag = false;
    }
  }

  // override indicator: a field whose effective value differs from EXIF
  const cityOverridden = $derived(!!p.exif.city && p.city !== p.exif.city);
  const dateOverridden = $derived(!!p.exif.taken_at && p.taken_at !== p.exif.taken_at);

  function fmtDate(s: string) {
    return s ? s.replace('T', ' ').slice(0, 16) : '—';
  }

  /** Stable string → HSL color, mirroring Lightbox so the same person reads the
   * same hue everywhere (same hashing constants). */
  function hueColor(s: string | null, light: number, sat = 70): string {
    if (!s) return `hsl(0 0% ${light}%)`;
    let h = 0;
    for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) % 360;
    return `hsl(${h} ${sat}% ${light}%)`;
  }

  // Faces for the current photo. Re-fetched whenever p.id changes.
  let faces = $state<PhotoFaces | null>(null);
  $effect(() => {
    const id = p.id;
    faces = null;
    let cancelled = false;
    api
      .photoFaces(id)
      .then((res) => {
        if (!cancelled && p.id === id) faces = res;
      })
      .catch((e) => console.warn('photoFaces failed', e));
    return () => {
      cancelled = true;
    };
  });

  async function refetchFaces() {
    try {
      const res = await api.photoFaces(p.id);
      faces = res;
    } catch (e) {
      console.warn('photoFaces failed', e);
    }
  }

  interface PersonRow {
    person_id: string;
    label: string;
    named: boolean;
    count: number;
  }

  // Distinct identified people in the photo, deduped by person_id, with counts.
  const people = $derived.by<PersonRow[]>(() => {
    const list = faces?.faces ?? [];
    const byId = new Map<string, PersonRow>();
    for (const f of list) {
      if (!f.person_id) continue;
      const existing = byId.get(f.person_id);
      if (existing) existing.count++;
      else
        byId.set(f.person_id, {
          person_id: f.person_id,
          label: f.person_name ?? f.person_label ?? '',
          named: f.person_name != null,
          count: 1,
        });
    }
    return [...byId.values()];
  });

  // Faces with no person_id at all → identities hidden (non-owner viewer).
  const anonFaces = $derived((faces?.faces ?? []).filter((f) => !f.person_id).length);
  const faceCount = $derived(faces?.faces.length ?? 0);

  // Inline naming state for unnamed clusters.
  let editingId = $state<string | null>(null);
  let nameInput = $state('');
  let submitting = $state(false);

  function startEdit(personId: string) {
    editingId = personId;
    nameInput = '';
  }

  async function submitName(personId: string) {
    const name = nameInput.trim();
    if (!name || submitting) return;
    submitting = true;
    try {
      await api.namePerson(personId, name);
      editingId = null;
      nameInput = '';
      await refetchFaces();
    } catch (e) {
      console.warn('namePerson failed', e);
      toast({ tone: 'error', title: 'Could not name person', message: String(e) });
    } finally {
      submitting = false;
    }
  }
</script>

<div class="pk-lb-info">
  <div class="pk-info-title">{p.title || p.filename}</div>
  <div class="pk-info-sub">
    {p.width.toLocaleString()} × {p.height.toLocaleString()} · {p.sizeMB.toFixed(1)} MB · {p.kind.toUpperCase()}
  </div>

  <div class="pk-info-rating">
    {#each [1, 2, 3, 4, 5] as s (s)}
      <button onclick={() => onRate(s === (p.rating ?? 0) ? 0 : s)} aria-label={`Rate ${s}`}>
        <Icon
          name="star"
          size={16}
          fill={s <= (p.rating ?? 0) ? 'currentColor' : 'none'}
          color={s <= (p.rating ?? 0) ? 'var(--amber-400)' : 'var(--text-faint)'}
        />
      </button>
    {/each}
  </div>

  {#if p.caption}
    <div class="pk-info-sec">
      <h4>Caption{#if p.edited}<span class="pk-edited"><Icon name="pencil" size={11} />edited</span>{/if}</h4>
      <div style="font-size:var(--text-xs);color:var(--text-muted);line-height:var(--lh-snug)">{p.caption}</div>
    </div>
  {/if}

  <div class="pk-info-sec">
    <h4>Capture</h4>
    <div class="pk-info-row"><span class="k"><Icon name="camera" size={14} />Camera</span><span class="v">{p.exif.camera || '—'}</span></div>
    <div class="pk-info-row"><span class="k"><Icon name="aperture" size={14} />Lens</span><span class="v">{p.exif.lens || '—'}</span></div>
    <div class="pk-info-row"><span class="k"><Icon name="sun" size={14} />Exposure</span><span class="v">{p.exif.shutter || '—'} · {p.exif.fnum || '—'} · ISO{p.exif.iso ?? '—'}</span></div>
    <div class="pk-info-row"><span class="k"><Icon name="ruler" size={14} />Focal</span><span class="v">{p.exif.focal || '—'}</span></div>
    <div class="pk-info-row"><span class="k"><Icon name="calendar" size={14} />Taken</span><span class={'v' + (dateOverridden ? ' is-override' : '')}>{fmtDate(p.taken_at)}</span></div>
  </div>

  <div class="pk-info-sec">
    <h4>Location</h4>
    <div class="pk-info-map">
      <span class="pin"><Icon name="map-pin" size={22} fill="currentColor" /></span>
      {#if p.lat}<span class="loc">{p.lat}, {p.lng}</span>{/if}
    </div>
    <div class="pk-info-row"><span class="k"><Icon name="map-pin" size={14} />Place</span><span class={'v' + (cityOverridden ? ' is-override' : '')}>{p.city || '—'}{p.country ? `, ${p.country}` : ''}</span></div>
  </div>

  {#if faceCount > 0}
    <div class="pk-info-sec">
      <h4>People</h4>
      {#if people.length > 0}
        <div class="pk-faces-list">
          {#each people as person (person.person_id)}
            <div class="pk-face-person">
              <span class="pk-face-dot" style={`background:${hueColor(person.person_id, 62)}`}></span>
              {#if editingId === person.person_id}
                <form
                  class="pk-face-name-form"
                  onsubmit={(e) => {
                    e.preventDefault();
                    submitName(person.person_id);
                  }}
                >
                  <!-- svelte-ignore a11y_autofocus -->
                  <input
                    class="pk-face-input"
                    type="text"
                    placeholder="Name"
                    bind:value={nameInput}
                    disabled={submitting}
                    autofocus
                  />
                  <button class="pk-face-btn" type="submit" disabled={submitting || !nameInput.trim()}>
                    Name
                  </button>
                </form>
              {:else}
                <span class="pk-face-label">{person.label}</span>
                {#if person.count > 1}<span class="pk-face-count">×{person.count}</span>{/if}
                {#if !person.named}
                  <button class="pk-face-add" type="button" onclick={() => startEdit(person.person_id)}>
                    <Icon name="pencil" size={11} />Name
                  </button>
                {/if}
              {/if}
            </div>
          {/each}
        </div>
      {:else}
        <div class="pk-face-muted">{anonFaces} face{anonFaces === 1 ? '' : 's'} detected</div>
      {/if}
    </div>
  {/if}

  <div class="pk-info-sec">
    <h4>Tags</h4>
    <div class="pk-info-tags">
      {#each p.tags as t (t)}
        <span class="pk-info-tag">
          {t}
          <button class="pk-info-tag-x" type="button" title="Remove tag" aria-label={`Remove ${t}`} disabled={savingTag} onclick={() => removeTag(t)}>
            <Icon name="x" size={11} />
          </button>
        </span>
      {/each}
      {#if addingTag}
        <form
          class="pk-info-tag-form"
          onsubmit={(e) => {
            e.preventDefault();
            commitTag();
          }}
        >
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="pk-info-tag-input"
            type="text"
            placeholder="New tag"
            bind:value={tagInput}
            disabled={savingTag}
            autofocus
            onblur={() => { if (!tagInput.trim()) addingTag = false; }}
          />
        </form>
      {:else}
        <button class="pk-info-tag add" type="button" onclick={() => (addingTag = true)}><Icon name="plus" size={12} />Add</button>
      {/if}
    </div>
  </div>

  {#if p.companions.length > 0}
    <div class="pk-info-sec">
      <h4>Companion files</h4>
      {#each p.companions as c (c.filename)}
        <div class="pk-info-row">
          <span class="k"><Icon name="file" size={14} />{c.ext.toUpperCase()}</span>
          <span class="v" style="display:flex;align-items:center;gap:8px">
            {c.size_mb.toFixed(1)} MB
            {#if c.downloadable}
              <a
                class="pk-pill is-on"
                href={authedUrl(`${API}/api/photos/${p.id}/companions/${c.ext.toLowerCase()}/download`)!}
                download={c.filename}
              >
                <Icon name="download" size={12} />{c.ext}
              </a>
            {/if}
          </span>
        </div>
      {/each}
    </div>
  {/if}

  <div class="pk-info-sec">
    <h4>File</h4>
    <div class="pk-info-row"><span class="k"><Icon name="hard-drive" size={14} />Stored</span><span class="v">/library/{p.taken_at.slice(0, 7).replace('-', '/')}</span></div>
    <div class="pk-info-row"><span class="k"><Icon name="file" size={14} />Name</span><span class="v">{p.filename}</span></div>
  </div>
</div>
