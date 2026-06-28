<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { displayThumb, type UIPhoto } from '../media';
  import type { Person } from '../api';

  type FacetType = 'person' | 'place' | 'thing' | 'date' | 'media';
  interface Facet {
    type: FacetType;
    id: string;
    label: string;
    icon: string;
  }

  let {
    photos,
    people,
    onPickFacet,
    onOpen,
  }: {
    photos: UIPhoto[];
    people: Person[];
    onPickFacet?: (f: Facet) => void;
    onOpen: (p: UIPhoto) => void;
  } = $props();

  let filters = $state<Facet[]>([]);

  // ---- person → photo lookups ----
  const photoById = $derived(new Map(photos.map((p) => [p.id, p])));

  // ---- discovery groupings (all from REAL photos) ----
  // People: straight from the people prop.
  function personThumb(p: Person): { url: string; alt: string } {
    const coverId = p.cover?.photo_id ?? p.sample_photo_ids[0];
    const cover = coverId ? photoById.get(coverId) : undefined;
    if (cover) return { url: displayThumb(cover), alt: p.name ?? 'Person' };
    // No backing photo loaded — fall back to a seed thumb via displayThumb shape.
    return { url: displayThumb({ seed: 100 }), alt: p.name ?? 'Person' };
  }

  // Places: group by city.
  const places = $derived.by(() => {
    const by = new Map<string, UIPhoto[]>();
    for (const p of photos) {
      const c = p.city?.trim();
      if (!c) continue;
      (by.get(c) ?? by.set(c, []).get(c)!).push(p);
    }
    return [...by.entries()]
      .map(([city, items]) => ({ city, country: items[0].country, items, count: items.length }))
      .sort((a, b) => b.count - a.count)
      .slice(0, 12);
  });

  // Things: flatten tags.
  const things = $derived.by(() => {
    const by = new Map<string, UIPhoto[]>();
    for (const p of photos) {
      for (const t of p.tags ?? []) {
        const tag = t?.trim();
        if (!tag) continue;
        (by.get(tag) ?? by.set(tag, []).get(tag)!).push(p);
      }
    }
    return [...by.entries()]
      .map(([tag, items]) => ({ tag, items, count: items.length }))
      .sort((a, b) => b.count - a.count)
      .slice(0, 12);
  });

  // Moments: group by year, newest first.
  const years = $derived.by(() => {
    const by = new Map<string, number>();
    for (const p of photos) {
      const y = p.taken_at?.slice(0, 4);
      if (!y) continue;
      by.set(y, (by.get(y) ?? 0) + 1);
    }
    return [...by.entries()]
      .map(([year, count]) => ({ year, count }))
      .sort((a, b) => (a.year < b.year ? 1 : -1));
  });

  // Media types: counts by kind (+ favorites).
  const mediaCards = $derived.by(() => {
    let photoN = 0,
      videoN = 0,
      rawN = 0,
      favN = 0;
    for (const p of photos) {
      if (p.kind === 'video') videoN++;
      else if (p.kind === 'raw') rawN++;
      else photoN++;
      if (p.favorite) favN++;
    }
    const cards: { id: string; label: string; icon: string; count: number }[] = [];
    if (photoN) cards.push({ id: 'photo', label: 'Photos', icon: 'image', count: photoN });
    if (videoN) cards.push({ id: 'video', label: 'Videos', icon: 'video', count: videoN });
    if (rawN) cards.push({ id: 'raw', label: 'RAW', icon: 'camera', count: rawN });
    if (favN) cards.push({ id: 'favorite', label: 'Favorites', icon: 'star', count: favN });
    return cards;
  });

  // ---- adding / removing facets ----
  function has(type: FacetType, id: string) {
    return filters.some((f) => f.type === type && f.id === id);
  }
  function addFacet(f: Facet) {
    if (has(f.type, f.id)) return;
    filters = [...filters, f];
    onPickFacet?.(f);
    menuOpen = false;
    submenu = null;
  }
  function removeFacet(f: Facet) {
    filters = filters.filter((x) => !(x.type === f.type && x.id === f.id));
  }
  function clearAll() {
    filters = [];
  }

  // ---- filtering (AND across facets) ----
  function matchFacet(p: UIPhoto, f: Facet): boolean {
    switch (f.type) {
      case 'person': {
        const person = people.find((q) => q.person_id === f.id);
        if (!person) return true; // data not available — skip gracefully
        return person.sample_photo_ids.includes(p.id);
      }
      case 'place':
        return p.city === f.id;
      case 'thing':
        return !!p.tags?.includes(f.id);
      case 'date':
        return !!p.taken_at?.startsWith(f.id);
      case 'media':
        if (f.id === 'favorite') return p.favorite;
        if (f.id === 'photo') return p.kind !== 'video' && p.kind !== 'raw';
        return p.kind === f.id;
      default:
        return true;
    }
  }
  const filtered = $derived(photos.filter((p) => filters.every((f) => matchFacet(p, f))));

  // ---- results grid packing (mirrors Feed) ----
  const GAP = 4;
  const TARGET_H = 180;
  let scrollW = $state(0);
  const width = $derived(Math.max(0, scrollW - 48)); // minus 24px L/R padding

  function aspect(p: UIPhoto) {
    const ar = p.width && p.height ? p.width / p.height : 1;
    return Math.max(0.55, Math.min(1.9, ar));
  }
  function buildRows(items: UIPhoto[], w: number, targetH: number) {
    if (!w) return items.map((p) => ({ items: [p], h: targetH }));
    const rows: { items: UIPhoto[]; h: number }[] = [];
    let row: UIPhoto[] = [];
    let arSum = 0;
    for (const p of items) {
      row.push(p);
      arSum += aspect(p);
      const projected = arSum * targetH + GAP * (row.length - 1);
      if (projected >= w) {
        const h = (w - GAP * (row.length - 1)) / arSum;
        rows.push({ items: row, h });
        row = [];
        arSum = 0;
      }
    }
    if (row.length) {
      const h = Math.min(targetH * 1.18, (w - GAP * (row.length - 1)) / arSum);
      rows.push({ items: row, h });
    }
    return rows;
  }

  // ---- add-filter menu ----
  let menuOpen = $state(false);
  let submenu = $state<FacetType | null>(null);

  const CATEGORIES: { type: FacetType; label: string; icon: string }[] = [
    { type: 'person', label: 'People', icon: 'users' },
    { type: 'place', label: 'Places', icon: 'map-pin' },
    { type: 'thing', label: 'Things', icon: 'tag' },
    { type: 'date', label: 'Date', icon: 'calendar' },
    { type: 'media', label: 'Media', icon: 'image' },
  ];
  // Hide a category whose options are all consumed / empty.
  const availableCategories = $derived(
    CATEGORIES.filter((c) => menuOptions(c.type).length > 0),
  );

  function menuOptions(type: FacetType): Facet[] {
    let opts: Facet[] = [];
    if (type === 'person')
      opts = people.map((p) => ({ type, id: p.person_id, label: p.name ?? 'Unnamed person', icon: 'users' }));
    else if (type === 'place')
      opts = places.map((pl) => ({ type, id: pl.city, label: pl.city, icon: 'map-pin' }));
    else if (type === 'thing')
      opts = things.map((t) => ({ type, id: t.tag, label: t.tag, icon: 'tag' }));
    else if (type === 'date')
      opts = years.map((y) => ({ type, id: y.year, label: y.year, icon: 'calendar' }));
    else if (type === 'media')
      opts = mediaCards.map((m) => ({ type, id: m.id, label: m.label, icon: m.icon }));
    return opts.filter((o) => !has(o.type, o.id));
  }

  function closeMenu() {
    menuOpen = false;
    submenu = null;
  }
</script>

<div class="pk-x">
  <div class="pk-x-top">
    <div>
      <h1 class="pk-x-title">Explore</h1>
      <p class="pk-x-sub">
        {#if filters.length === 0}
          Discover your library by people, places, things and moments.
        {:else}
          Filtered view — {filtered.length} photo{filtered.length === 1 ? '' : 's'}.
        {/if}
      </p>
    </div>
    {#if filters.length >= 1}
      <div class="pk-x-addwrap" onmouseleave={closeMenu} role="menu" tabindex="-1">
        <button type="button" class="pk-x-add" onclick={() => (menuOpen = !menuOpen)}>
          <Icon name="plus" size={14} />Add filter
        </button>
        {#if menuOpen}
          <div class="pk-x-menu">
            {#if submenu === null}
              {#each availableCategories as c (c.type)}
                <button type="button" class="pk-x-menu-item" onclick={() => (submenu = c.type)}>
                  <Icon name={c.icon} size={15} />{c.label}
                  <Icon name="chevron-right" size={14} class="pk-x-mi-arrow" />
                </button>
              {/each}
            {:else}
              {#each menuOptions(submenu) as o (o.id)}
                <button type="button" class="pk-x-menu-item" onclick={() => addFacet(o)}>
                  <Icon name={o.icon} size={15} />{o.label}
                </button>
              {/each}
            {/if}
          </div>
        {/if}
      </div>
    {/if}
  </div>

  {#if filters.length >= 1}
    <div class="pk-x-chips">
      {#each filters as f (f.type + f.id)}
        <span class="pk-x-chip">
          <Icon name={f.icon} size={13} />{f.label}
          <button type="button" onclick={() => removeFacet(f)} aria-label={`Remove ${f.label}`}>
            <Icon name="x" size={12} />
          </button>
        </span>
      {/each}
    </div>
  {/if}

  <div class="pk-x-scroll" bind:clientWidth={scrollW}>
    {#if filters.length === 0}
      <!-- ===== DISCOVERY MODE ===== -->

      {#if people.length}
        <section class="pk-x-sec">
          <div class="pk-x-head">
            <Icon name="users" size={17} />
            <h3>People</h3>
            <span class="pk-x-hint">{people.length} recognized</span>
          </div>
          <div class="pk-x-faces">
            {#each people as p (p.person_id)}
              {@const av = personThumb(p)}
              <button
                type="button"
                class="pk-x-face"
                onclick={() => addFacet({ type: 'person', id: p.person_id, label: p.name ?? 'Unnamed person', icon: 'users' })}
              >
                <img src={av.url} alt={av.alt} loading="lazy" />
                <span class="pk-x-face-name">{p.name ?? 'Unnamed'}</span>
                <span class="pk-x-face-count">{p.face_count} photo{p.face_count === 1 ? '' : 's'}</span>
              </button>
            {/each}
          </div>
        </section>
      {/if}

      {#if places.length}
        <section class="pk-x-sec">
          <div class="pk-x-head">
            <Icon name="map-pin" size={17} />
            <h3>Places</h3>
            <span class="pk-x-hint">{places.length} location{places.length === 1 ? '' : 's'}</span>
          </div>
          <div class="pk-x-places">
            {#each places as pl (pl.city)}
              <button
                type="button"
                class="pk-x-place"
                onclick={() => addFacet({ type: 'place', id: pl.city, label: pl.city, icon: 'map-pin' })}
              >
                <img src={displayThumb(pl.items[0])} alt={pl.city} loading="lazy" />
                <span class="pk-x-place-grad"></span>
                <span class="pk-x-place-meta">
                  <span class="pk-x-place-name">{pl.city}</span>
                  <span class="pk-x-place-sub">{pl.country ? pl.country + ' · ' : ''}{pl.count} photo{pl.count === 1 ? '' : 's'}</span>
                </span>
              </button>
            {/each}
          </div>
        </section>
      {/if}

      {#if things.length}
        <section class="pk-x-sec">
          <div class="pk-x-head">
            <Icon name="tag" size={17} />
            <h3>Things</h3>
            <span class="pk-x-hint">{things.length} tag{things.length === 1 ? '' : 's'}</span>
          </div>
          <div class="pk-x-things">
            {#each things as t (t.tag)}
              <button
                type="button"
                class="pk-x-thing"
                onclick={() => addFacet({ type: 'thing', id: t.tag, label: t.tag, icon: 'tag' })}
              >
                <img src={displayThumb(t.items[0])} alt={t.tag} loading="lazy" />
                <span class="pk-x-thing-grad"></span>
                <span class="pk-x-thing-label"><Icon name="tag" size={14} />{t.tag}</span>
                <span class="pk-x-thing-count">{t.count}</span>
              </button>
            {/each}
          </div>
        </section>
      {/if}

      {#if years.length}
        <section class="pk-x-sec">
          <div class="pk-x-head">
            <Icon name="calendar" size={17} />
            <h3>Moments</h3>
            <span class="pk-x-hint">By year</span>
          </div>
          <div class="pk-x-years">
            {#each years as y (y.year)}
              <button
                type="button"
                class="pk-x-year"
                onclick={() => addFacet({ type: 'date', id: y.year, label: y.year, icon: 'calendar' })}
              >
                <span class="pk-x-year-n">{y.year}</span>
                <span class="pk-x-year-c">{y.count} photo{y.count === 1 ? '' : 's'}</span>
              </button>
            {/each}
          </div>
        </section>
      {/if}

      {#if mediaCards.length}
        <section class="pk-x-sec">
          <div class="pk-x-head">
            <Icon name="image" size={17} />
            <h3>Media types</h3>
          </div>
          <div class="pk-x-media">
            {#each mediaCards as m (m.id)}
              <button
                type="button"
                class="pk-x-mediacard"
                onclick={() => addFacet({ type: 'media', id: m.id, label: m.label, icon: m.icon })}
              >
                <span class="pk-x-media-ic"><Icon name={m.icon} size={18} /></span>
                <span class="pk-x-media-lbl">{m.label}</span>
                <span class="pk-x-media-c">{m.count}</span>
              </button>
            {/each}
          </div>
        </section>
      {/if}
    {:else}
      <!-- ===== RESULTS MODE ===== -->
      <div class="pk-x-results">
        <div class="pk-x-resbar">
          <span class="pk-x-rescount"><b>{filtered.length}</b> photo{filtered.length === 1 ? '' : 's'}</span>
          <span class="pk-x-narrow">Narrow by</span>
          <div class="pk-x-addwrap" onmouseleave={closeMenu} role="menu" tabindex="-1">
            <button type="button" class="pk-x-add" onclick={() => (menuOpen = !menuOpen)}>
              <Icon name="plus" size={14} />Add filter
            </button>
            {#if menuOpen}
              <div class="pk-x-menu">
                {#if submenu === null}
                  {#each availableCategories as c (c.type)}
                    <button type="button" class="pk-x-menu-item" onclick={() => (submenu = c.type)}>
                      <Icon name={c.icon} size={15} />{c.label}
                      <Icon name="chevron-right" size={14} class="pk-x-mi-arrow" />
                    </button>
                  {/each}
                {:else}
                  {#each menuOptions(submenu) as o (o.id)}
                    <button type="button" class="pk-x-menu-item" onclick={() => addFacet(o)}>
                      <Icon name={o.icon} size={15} />{o.label}
                    </button>
                  {/each}
                {/if}
              </div>
            {/if}
          </div>
          <button type="button" class="pk-x-clear" onclick={clearAll}>
            <Icon name="x" size={13} />Clear all
          </button>
        </div>

        <div class="pk-grid">
          {#each buildRows(filtered, width, TARGET_H) as r, ri (ri)}
            <div class="pk-row" style={`height:${r.h}px;gap:${GAP}px`}>
              {#each r.items as p (p.id)}
                <button
                  type="button"
                  class="pk-tile"
                  style={`width:${Math.round(aspect(p) * r.h)}px;height:${Math.round(r.h)}px`}
                  onclick={() => onOpen(p)}
                >
                  <img loading="lazy" src={displayThumb(p)} alt={p.filename} />
                </button>
              {/each}
            </div>
          {/each}
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
.pk-x { display: flex; flex-direction: column; min-height: 0; flex: 1; }
.pk-x-top { display: flex; align-items: flex-end; gap: 16px; padding: 22px 24px 12px; flex: none; }
.pk-x-title { font-family: var(--font-display); font-size: var(--text-2xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); margin: 0; }
.pk-x-sub { font-size: var(--text-sm); color: var(--text-muted); margin: 4px 0 0; }
.pk-x-top .pk-x-addwrap { margin-left: auto; }
.pk-x-chips { display: flex; flex-wrap: wrap; gap: 7px; padding: 4px 24px 12px; flex: none; }
.pk-x-chip { display: inline-flex; align-items: center; gap: 7px; height: 30px; padding: 0 6px 0 11px; border-radius: var(--radius-pill); font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--accent-text); background: var(--accent-soft); border: 1px solid var(--accent-soft-bd); }
.pk-x-chip button { width: 18px; height: 18px; display: grid; place-items: center; border: 0; background: rgba(0,0,0,.08); color: inherit; border-radius: 50%; cursor: pointer; }
:global(.dark) .pk-x-chip button { background: rgba(255,255,255,.12); }
.pk-x-chip button:hover { background: var(--accent); color: var(--accent-fg); }
.pk-x-scroll { flex: 1; overflow-y: auto; padding: 4px 24px 32px; }

.pk-x-sec { margin-bottom: 26px; }
.pk-x-head { display: flex; align-items: center; gap: 9px; margin-bottom: 13px; }
.pk-x-head :global(.pk-ic) { color: var(--accent); }
.pk-x-head h3 { font-size: var(--text-base); font-weight: var(--fw-semibold); letter-spacing: var(--ls-tight); margin: 0; }
.pk-x-hint { font-size: var(--text-xs); color: var(--text-faint); }

/* people faces */
.pk-x-faces { display: flex; gap: 14px; overflow-x: auto; padding-bottom: 6px; }
.pk-x-face { flex: none; width: 76px; display: flex; flex-direction: column; align-items: center; gap: 7px; background: none; border: 0; cursor: pointer; padding: 0; }
.pk-x-face img { width: 72px; height: 72px; border-radius: var(--radius-pill); object-fit: cover; border: 2px solid var(--border); transition: border-color var(--dur-fast) var(--ease-out), transform var(--dur-fast) var(--ease-out); }
.pk-x-face:hover img { border-color: var(--accent); transform: translateY(-2px); }
.pk-x-face-name { font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text); max-width: 76px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.pk-x-face-count { font-size: 10px; color: var(--text-faint); margin-top: -4px; }

/* places */
.pk-x-places { display: grid; grid-template-columns: repeat(auto-fill, minmax(190px, 1fr)); gap: 12px; }
.pk-x-place { position: relative; height: 120px; border-radius: var(--radius-lg); overflow: hidden; border: 0; cursor: pointer; padding: 0; }
.pk-x-place img { width: 100%; height: 100%; object-fit: cover; transition: transform var(--dur-slow) var(--ease-out); }
.pk-x-place:hover img { transform: scale(1.05); }
.pk-x-place-grad { position: absolute; inset: 0; background: linear-gradient(to top, rgba(7,9,16,.8), transparent 65%); }
.pk-x-place-meta { position: absolute; left: 13px; bottom: 11px; right: 13px; display: flex; flex-direction: column; gap: 2px; text-align: left; }
.pk-x-place-name { font-size: var(--text-md); font-weight: var(--fw-semibold); color: #fff; }
.pk-x-place-sub { font-size: var(--text-2xs); color: rgba(255,255,255,.78); }

/* things */
.pk-x-things { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 10px; }
.pk-x-thing { position: relative; aspect-ratio: 3/2; border-radius: var(--radius-md); overflow: hidden; border: 0; cursor: pointer; padding: 0; }
.pk-x-thing img { width: 100%; height: 100%; object-fit: cover; transition: transform var(--dur-slow) var(--ease-out); }
.pk-x-thing:hover img { transform: scale(1.06); }
.pk-x-thing-grad { position: absolute; inset: 0; background: linear-gradient(to top, rgba(7,9,16,.7), transparent 55%); }
.pk-x-thing-label { position: absolute; left: 10px; bottom: 9px; display: flex; align-items: center; gap: 6px; font-size: var(--text-sm); font-weight: var(--fw-semibold); color: #fff; }
.pk-x-thing-count { position: absolute; top: 8px; right: 9px; font-size: 10px; color: #fff; background: rgba(0,0,0,.4); padding: 2px 6px; border-radius: var(--radius-pill); }

/* moments / years */
.pk-x-years { display: flex; gap: 10px; flex-wrap: wrap; }
.pk-x-year { display: flex; flex-direction: column; gap: 3px; padding: 13px 18px; border-radius: var(--radius-lg); background: var(--surface); border: 1px solid var(--border); cursor: pointer; text-align: left; transition: all var(--dur-fast) var(--ease-out); }
.pk-x-year:hover { border-color: var(--accent-soft-bd); background: var(--surface-hover); }
.pk-x-year-n { font-size: var(--text-lg); font-weight: var(--fw-semibold); color: var(--text); }
.pk-x-year-c { font-size: var(--text-2xs); color: var(--text-faint); }

/* media */
.pk-x-media { display: flex; gap: 10px; flex-wrap: wrap; }
.pk-x-mediacard { display: flex; align-items: center; gap: 10px; padding: 11px 15px 11px 12px; border-radius: var(--radius-lg); background: var(--surface); border: 1px solid var(--border); cursor: pointer; transition: all var(--dur-fast) var(--ease-out); }
.pk-x-mediacard:hover { border-color: var(--accent-soft-bd); background: var(--surface-hover); }
.pk-x-media-ic { width: 34px; height: 34px; display: grid; place-items: center; border-radius: var(--radius-md); background: var(--accent-soft); color: var(--accent-text); }
.pk-x-media-lbl { font-size: var(--text-sm); font-weight: var(--fw-medium); }
.pk-x-media-c { font-size: var(--text-xs); color: var(--text-faint); margin-left: 2px; }

/* results */
.pk-x-results { display: flex; flex-direction: column; }
.pk-x-resbar { display: flex; align-items: center; gap: 12px; margin-bottom: 14px; }
.pk-x-rescount { font-size: var(--text-md); color: var(--text); }
.pk-x-rescount b { font-weight: var(--fw-semibold); }
.pk-x-narrow { margin-left: auto; font-size: var(--text-xs); color: var(--text-faint); }
.pk-x-clear { display: inline-flex; align-items: center; gap: 5px; font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text-muted); background: none; border: 0; cursor: pointer; padding: 6px 8px; border-radius: var(--radius-md); }
.pk-x-clear:hover { background: var(--surface-hover); color: var(--text); }

/* add-filter menu */
.pk-x-addwrap { position: relative; }
.pk-x-add { display: inline-flex; align-items: center; gap: 6px; height: 30px; padding: 0 13px; border-radius: var(--radius-pill); font-size: var(--text-xs); font-weight: var(--fw-semibold); color: var(--accent-text); background: var(--accent-soft); border: 1px dashed var(--accent-soft-bd); cursor: pointer; }
.pk-x-add:hover { background: var(--accent); color: var(--accent-fg); border-style: solid; }
.pk-x-menu { position: absolute; top: calc(100% + 6px); right: 0; z-index: 20; min-width: 190px; max-height: 320px; overflow-y: auto; background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius-lg); box-shadow: var(--shadow-lg); padding: 5px; }
.pk-x-menu-item { display: flex; align-items: center; gap: 9px; width: 100%; padding: 8px 10px; border-radius: var(--radius-md); font-size: var(--text-sm); color: var(--text); background: none; border: 0; cursor: pointer; text-align: left; }
.pk-x-menu-item :global(.pk-ic) { color: var(--text-faint); }
.pk-x-menu-item:hover { background: var(--surface-hover); }
.pk-x-menu-item:disabled { opacity: .4; cursor: default; }
.pk-x-menu-item :global(.pk-x-mi-arrow) { margin-left: auto; }
</style>
