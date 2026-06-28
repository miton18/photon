<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import PhotoTile from './PhotoTile.svelte';
  import { type Section, type UIPhoto } from '../media';

  let {
    sections,
    density = 7,
    selected,
    selecting = false,
    emptyLabel = 'No photos here yet.',
    onOpen,
    onToggleSel,
    onSelectMany,
    onHover,
    onFav,
  }: {
    sections: Section[];
    density?: number;
    selected: Set<string>;
    selecting?: boolean;
    emptyLabel?: string;
    onOpen: (p: UIPhoto) => void;
    onToggleSel: (id: string) => void;
    onSelectMany?: (ids: string[], select: boolean) => void;
    onHover: (p: UIPhoto | null) => void;
    onFav: (id: string) => void;
  } = $props();

  let feedEl = $state<HTMLDivElement | null>(null);

  // The timeline rail is DERIVED from the real sections (one marker per month),
  // not a static list — clicking a marker scrolls the feed to that month.
  const rail = $derived.by(() => {
    const seen = new Set<string>();
    const out: { m: string; y: string; secId: string }[] = [];
    for (const s of sections) {
      const d = s.items[0]?.taken_at ?? '';
      const key = d.slice(0, 7); // YYYY-MM
      if (!key || seen.has(key)) continue;
      seen.add(key);
      const dt = new Date(d);
      out.push({
        m: isNaN(+dt) ? key : dt.toLocaleString('en', { month: 'short' }),
        y: isNaN(+dt) ? '' : String(dt.getFullYear()),
        secId: s.id,
      });
    }
    return out;
  });

  function scrollToSection(secId: string) {
    feedEl?.querySelector<HTMLElement>(`[data-sec="${CSS.escape(secId)}"]`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  // A day-section is "fully selected" when every item is in the selection set.
  function dayAllSelected(section: Section) {
    return section.items.length > 0 && section.items.every((p) => selected.has(p.id));
  }
  function toggleDay(section: Section) {
    const ids = section.items.map((p) => p.id);
    onSelectMany?.(ids, !dayAllSelected(section));
  }

  /* density (segmented L/M/S) → target justified row height in px */
  const ROW_H: Record<number, number> = { 5: 248, 7: 178, 10: 122 };
  const GAP = 4;

  let feedW = $state(0); // clientWidth of the scroll container
  const width = $derived(Math.max(0, feedW - 36)); // minus 18px L/R padding

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

  const targetH = $derived(ROW_H[density] ?? 178);
</script>

<div class="pk-feed-wrap">
  <div class="pk-feed" bind:clientWidth={feedW} bind:this={feedEl}>
    {#if sections.length === 0}
      <div class="pk-empty"><Icon name="image-off" size={26} /><span>{emptyLabel}</span></div>
    {/if}
    {#each sections as section (section.id)}
      <section class="pk-section" data-sec={section.id}>
        <div class="pk-section-head">
          <button class={'pk-section-check' + (dayAllSelected(section) ? ' is-on' : '')} aria-label="Select day" title="Select this day" onclick={() => toggleDay(section)}><Icon name="check" size={11} strokeWidth={3} /></button>
          <span class="pk-sh-day">{section.label}</span>
          <span class="pk-sh-meta">{section.date} · {section.items.length} items</span>
          {#if section.items[0]?.city}
            <span class="pk-sh-loc"><Icon name="map-pin" size={13} />{section.items[0].city}{section.items[0].country ? `, ${section.items[0].country}` : ''}</span>
          {/if}
        </div>
        <div class="pk-grid">
          {#each buildRows(section.items, width, targetH) as r, ri (ri)}
            <div class="pk-row" style={`height:${r.h}px;gap:${GAP}px`}>
              {#each r.items as p (p.id)}
                <PhotoTile
                  photo={p}
                  w={Math.round(aspect(p) * r.h)}
                  h={Math.round(r.h)}
                  selected={selected.has(p.id)}
                  {selecting}
                  {onOpen}
                  {onToggleSel}
                  {onHover}
                  {onFav}
                />
              {/each}
            </div>
          {/each}
        </div>
      </section>
    {/each}
  </div>

  {#if rail.length > 1}
    <div class="pk-rail">
      {#each rail as r, i (r.secId)}
        <button class="pk-rail-item" onclick={() => scrollToSection(r.secId)} title={`Jump to ${r.m} ${r.y}`}>
          <span class="pk-rail-m">{r.m}</span>
          <span class="pk-rail-y">{r.y}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .pk-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
    height: 60%;
    color: var(--text-faint);
    font-size: var(--text-sm);
  }
</style>
