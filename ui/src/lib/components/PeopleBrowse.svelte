<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import Constellation, { CONST_HUES, relWeight } from './Constellation.svelte';
  import { API, type Person } from '../api';
  import { authedUrl } from '../session';

  let {
    people,
    onOpenPerson,
    onOpenStudio,
  }: {
    people: Person[];
    onOpenPerson: (p: Person) => void;
    onOpenStudio: () => void;
  } = $props();

  const named = $derived(people.filter((p) => !!p.name?.trim()));
  const unnamed = $derived(people.length - named.length);
  const byId = $derived(new Map(people.map((p) => [p.person_id, p])));

  let view = $state<'list' | 'cosmos'>('cosmos');
  let selId = $state<string | null>(null);
  const selected = $derived(selId ? byId.get(selId) ?? null : null);

  function age(dob: string | null | undefined = undefined): number | null {
    if (!dob) return null;
    const d = new Date(dob), n = new Date();
    let a = n.getFullYear() - d.getFullYear();
    if (n.getMonth() < d.getMonth() || (n.getMonth() === d.getMonth() && n.getDate() < d.getDate())) a--;
    return a;
  }
  const firstName = (n: string | null) => (n ? n.split(' ')[0] : 'this person');

  // Cover-photo thumbnail, face-cropped via CSS background (mirrors App.personAvatar).
  function avatar(p: Person): { url: string; pos: string; size: string } {
    if (!p.cover || !p.cover.source_width || !p.cover.source_height) {
      return { url: `https://picsum.photos/seed/${p.person_id}/200/200`, pos: 'center', size: 'cover' };
    }
    const url = authedUrl(`${API}/api/photos/${p.cover.photo_id}/thumb`)!;
    const [x, y, w, h] = p.cover.bbox;
    const pw = p.cover.source_width, ph = p.cover.source_height;
    if (!w || !h) return { url, pos: 'center', size: 'cover' };
    const cx = ((x + w / 2) / pw) * 100;
    const cy = ((y + h / 2) / ph) * 100;
    const zoom = Math.min(400, Math.max(120, Math.round((pw / w) * 60)));
    return { url, pos: `${cx.toFixed(1)}% ${cy.toFixed(1)}%`, size: `${zoom}%` };
  }
  // Plain thumbnail URL for the canvas stars (drawn, never read back → CORS-safe).
  const portrait = (p: Person): string =>
    p.cover?.photo_id ? authedUrl(`${API}/api/photos/${p.cover.photo_id}/thumb`) ?? '' : `https://picsum.photos/seed/${p.person_id}/200/200`;

  function bgStyle(p: Person): string {
    const a = avatar(p);
    return `background-image:url('${a.url}');background-position:${a.pos};background-size:${a.size};`;
  }

  // Connected components (constellations) over named people, for the legend.
  const constellations = $derived.by(() => {
    const ids = new Set(named.map((p) => p.person_id));
    const parent: Record<string, string> = {};
    ids.forEach((id) => (parent[id] = id));
    const find = (x: string): string => (parent[x] === x ? x : (parent[x] = find(parent[x])));
    for (const p of named) {
      for (const r of p.relationships ?? []) {
        if (ids.has(r.person_id)) parent[find(p.person_id)] = find(r.person_id);
      }
    }
    const groups = new Map<string, string[]>();
    ids.forEach((id) => {
      const r = find(id);
      (groups.get(r) ?? groups.set(r, []).get(r)!).push(id);
    });
    return [...groups.values()]
      .sort((a, b) => b.length - a.length)
      .map((members, ci) => {
        // Label a group by its most-connected member (the hub).
        let hub = members[0], best = -1;
        for (const id of members) {
          const n = byId.get(id)?.relationships.length ?? 0;
          if (n > best) { best = n; hub = id; }
        }
        const hubName = byId.get(hub)?.name ?? 'Group';
        return {
          hue: CONST_HUES[ci % CONST_HUES.length],
          count: members.length,
          label: members.length > 1 ? `${firstName(hubName)}'s circle` : (hubName ?? 'Group'),
        };
      });
  });

  function relLabel(rel: string): string {
    return rel.charAt(0).toUpperCase() + rel.slice(1);
  }
  function relIcon(rel: string): string {
    const s = rel.toLowerCase();
    if (/(partner|spouse|husband|wife)/.test(s)) return 'heart';
    if (/(mother|father|parent)/.test(s)) return 'git-branch';
    if (/(son|daughter|child)/.test(s)) return 'baby';
    if (/(brother|sister|sibling)/.test(s)) return 'users';
    if (/grand/.test(s)) return 'users-round';
    return 'user-round';
  }
  // Relationships of the selected person whose target is also a known person.
  const selRels = $derived(
    (selected?.relationships ?? []).filter((r) => byId.has(r.person_id)),
  );
</script>

<div class="pk-pb">
  <div class="pk-pb-head">
    <div class="pk-pb-head-txt">
      <h1>People</h1>
      <p>{named.length} {named.length === 1 ? 'person' : 'people'}{unnamed > 0 ? ` · ${unnamed} ${unnamed === 1 ? 'group' : 'groups'} to review` : ''}</p>
    </div>
    <div class="pk-pb-switch">
      <button class={view === 'list' ? 'is-on' : ''} onclick={() => (view = 'list')}><Icon name="layout-grid" size={15} /> List</button>
      <button class={view === 'cosmos' ? 'is-on' : ''} onclick={() => (view = 'cosmos')}><Icon name="sparkles" size={15} /> Constellation</button>
    </div>
    <button class="pk-pb-editor" onclick={onOpenStudio}>
      <Icon name="scan-face" size={15} /> People Editor
    </button>
  </div>

  {#if named.length === 0}
    <div class="pk-pb-empty">
      <Icon name="users" size={26} />
      <span>No named people yet — tag and name faces in the People Editor.</span>
      <button class="pk-btn pk-btn-primary" onclick={onOpenStudio}><Icon name="scan-face" size={15} /> Open People Editor</button>
    </div>
  {:else if view === 'list'}
    <div class="pk-pl-scroll">
      <div class="pk-pl-grid">
        {#each named as p (p.person_id)}
          <button class="pk-pl-card" onclick={() => onOpenPerson(p)} title={`View ${p.name}'s photos`}>
            <span class="pk-pl-photo" style={bgStyle(p)}></span>
            <span class="pk-pl-name">{p.name}</span>
            <span class="pk-pl-meta pk-mono">{p.face_count.toLocaleString()} photo{p.face_count === 1 ? '' : 's'}</span>
          </button>
        {/each}
      </div>
    </div>
  {:else}
    <div class="pk-cos">
      <Constellation people={named} selectedId={selId} onSelect={(id) => (selId = id)} portrait={portrait} />

      {#if constellations.length > 1}
        <div class="pk-cos-legend">
          <div class="pk-cos-legend-h">Constellations</div>
          {#each constellations as c (c.label)}
            <div class="pk-cos-legend-row">
              <span class="pk-cos-dot" style={`background:${c.hue}`}></span>
              <span class="pk-cos-legend-name">{c.label}</span>
              <span class="pk-cos-legend-n pk-mono">{c.count}</span>
            </div>
          {/each}
        </div>
      {/if}

      <div class="pk-cos-hint"><Icon name="move-3d" size={14} /> Drag to orbit · scroll to zoom · click a face to focus</div>

      {#if selected}
        <div class="pk-cos-card">
          <button class="pk-cos-card-x" onclick={() => (selId = null)}><Icon name="x" size={16} /></button>
          <div class="pk-cos-card-head">
            <span class="pk-cos-card-photo" style={bgStyle(selected)}></span>
            <div>
              <span class="pk-cos-card-name">{selected.name}</span>
              {#if selected.birthdate}<span class="pk-cos-card-grp">{age(selected.birthdate)} yrs</span>{/if}
            </div>
          </div>
          <div class="pk-cos-card-stats">
            <span><b>{selected.face_count.toLocaleString()}</b> photos</span>
            <span><b>{selRels.length}</b> link{selRels.length === 1 ? '' : 's'}</span>
          </div>
          {#if selRels.length}
            <div class="pk-cos-card-rels">
              {#each selRels as r (r.person_id + r.relation)}
                {@const o = byId.get(r.person_id)!}
                <button class="pk-cos-rel" onclick={() => (selId = o.person_id)}>
                  <span class="pk-cos-rel-ic"><Icon name={relIcon(r.relation)} size={13} /></span>
                  <span class="pk-cos-rel-photo" style={bgStyle(o)}></span>
                  <span class="pk-cos-rel-txt">
                    <span class="pk-cos-rel-name">{o.name ?? 'Unnamed'}</span>
                    <span class="pk-cos-rel-type">{relLabel(r.relation)} of {firstName(selected.name)}</span>
                  </span>
                  <Icon name="arrow-up-right" size={13} />
                </button>
              {/each}
            </div>
          {/if}
          <button class="pk-cos-card-open" onclick={() => onOpenPerson(selected!)}>
            <Icon name="image" size={14} /> View photos
          </button>
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .pk-pb { display: flex; flex-direction: column; height: 100%; min-height: 0; }
  .pk-pb-head { display: flex; align-items: center; gap: 16px; padding: 4px 4px 16px; }
  .pk-pb-head-txt h1 { margin: 0; font-size: var(--text-xl); font-weight: var(--fw-semibold); }
  .pk-pb-head-txt p { margin: 2px 0 0; color: var(--text-muted); font-size: var(--text-sm); }
  .pk-pb-switch { margin-left: auto; display: inline-flex; background: var(--bg-subtle); border: 1px solid var(--border-faint); border-radius: var(--radius-pill); padding: 3px; gap: 2px; }
  .pk-pb-switch button { display: inline-flex; align-items: center; gap: 6px; border: 0; background: none; color: var(--text-muted); font: inherit; font-size: var(--text-sm); font-weight: var(--fw-medium); padding: 6px 14px; border-radius: var(--radius-pill); cursor: pointer; }
  .pk-pb-switch button.is-on { background: var(--surface); color: var(--text); box-shadow: var(--shadow-sm); }
  .pk-pb-editor { display: inline-flex; align-items: center; gap: 6px; border: 1px solid var(--border-strong); background: transparent; color: var(--text); font: inherit; font-size: var(--text-sm); font-weight: var(--fw-medium); padding: 8px 14px; border-radius: var(--radius-md); cursor: pointer; }
  .pk-pb-editor:hover { background: var(--accent-soft); border-color: var(--accent-soft-bd); color: var(--accent-text); }
  .pk-pb-empty { flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 14px; color: var(--text-faint); font-size: var(--text-sm); }

  /* ── list view ── */
  .pk-pl-scroll { flex: 1; min-height: 0; overflow-y: auto; }
  .pk-pl-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(132px, 1fr)); gap: 16px; padding: 4px; }
  .pk-pl-card { display: flex; flex-direction: column; align-items: center; gap: 8px; border: 0; background: none; cursor: pointer; padding: 6px; border-radius: var(--radius-lg); }
  .pk-pl-card:hover { background: var(--bg-subtle); }
  .pk-pl-photo { width: 100%; aspect-ratio: 1; border-radius: var(--radius-pill); background-repeat: no-repeat; background-color: var(--photo-bg); box-shadow: inset 0 0 0 1px var(--border-faint); }
  .pk-pl-name { font-size: var(--text-sm); font-weight: var(--fw-medium); color: var(--text); text-align: center; }
  .pk-pl-meta { font-size: var(--text-2xs); color: var(--text-faint); }

  /* ── cosmos view ── */
  .pk-cos { position: relative; flex: 1; min-height: 0; border-radius: var(--radius-lg); overflow: hidden; background: #06050c; box-shadow: inset 0 0 0 1px var(--border-faint); }
  .pk-cos-legend { position: absolute; top: 14px; left: 14px; background: rgba(10, 8, 20, 0.62); backdrop-filter: blur(8px); border: 1px solid rgba(255,255,255,0.08); border-radius: var(--radius-md); padding: 10px 12px; min-width: 168px; color: #ECE9F6; }
  .pk-cos-legend-h { font-size: var(--text-2xs); text-transform: uppercase; letter-spacing: var(--ls-caps); color: rgba(236,233,246,0.5); margin-bottom: 8px; }
  .pk-cos-legend-row { display: flex; align-items: center; gap: 8px; padding: 3px 0; font-size: var(--text-xs); }
  .pk-cos-dot { width: 9px; height: 9px; border-radius: var(--radius-pill); flex: none; }
  .pk-cos-legend-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .pk-cos-legend-n { color: rgba(236,233,246,0.5); }
  .pk-cos-hint { position: absolute; bottom: 14px; left: 50%; transform: translateX(-50%); display: inline-flex; align-items: center; gap: 6px; background: rgba(10, 8, 20, 0.62); backdrop-filter: blur(8px); border: 1px solid rgba(255,255,255,0.08); border-radius: var(--radius-pill); padding: 7px 14px; color: rgba(236,233,246,0.7); font-size: var(--text-2xs); pointer-events: none; }

  .pk-cos-card { position: absolute; top: 14px; right: 14px; width: 264px; background: rgba(12, 10, 22, 0.78); backdrop-filter: blur(12px); border: 1px solid rgba(255,255,255,0.1); border-radius: var(--radius-lg); padding: 16px; color: #ECE9F6; box-shadow: var(--shadow-lg); }
  .pk-cos-card-x { position: absolute; top: 10px; right: 10px; display: grid; place-items: center; width: 26px; height: 26px; border: 0; background: rgba(255,255,255,0.06); color: #ECE9F6; border-radius: var(--radius-pill); cursor: pointer; }
  .pk-cos-card-x:hover { background: rgba(255,255,255,0.14); }
  .pk-cos-card-head { display: flex; align-items: center; gap: 12px; margin-bottom: 14px; }
  .pk-cos-card-photo { width: 52px; height: 52px; border-radius: var(--radius-pill); background-repeat: no-repeat; background-color: #0c0a14; box-shadow: inset 0 0 0 1px rgba(255,255,255,0.12); flex: none; }
  .pk-cos-card-name { display: block; font-size: var(--text-md); font-weight: var(--fw-semibold); }
  .pk-cos-card-grp { display: block; font-size: var(--text-xs); color: rgba(236,233,246,0.6); }
  .pk-cos-card-stats { display: flex; gap: 16px; padding: 10px 0; margin-bottom: 6px; border-top: 1px solid rgba(255,255,255,0.08); border-bottom: 1px solid rgba(255,255,255,0.08); font-size: var(--text-xs); color: rgba(236,233,246,0.7); }
  .pk-cos-card-stats b { color: #ECE9F6; font-weight: var(--fw-semibold); }
  .pk-cos-card-rels { display: flex; flex-direction: column; gap: 4px; margin: 10px 0; max-height: 220px; overflow-y: auto; }
  .pk-cos-rel { display: flex; align-items: center; gap: 8px; padding: 6px; border: 0; background: none; color: #ECE9F6; border-radius: var(--radius-md); cursor: pointer; text-align: left; width: 100%; }
  .pk-cos-rel:hover { background: rgba(255,255,255,0.07); }
  .pk-cos-rel-ic { display: grid; place-items: center; width: 22px; height: 22px; border-radius: var(--radius-pill); background: rgba(255,255,255,0.08); color: rgba(236,233,246,0.8); flex: none; }
  .pk-cos-rel-photo { width: 30px; height: 30px; border-radius: var(--radius-pill); background-repeat: no-repeat; background-color: #0c0a14; flex: none; }
  .pk-cos-rel-txt { flex: 1; min-width: 0; }
  .pk-cos-rel-name { display: block; font-size: var(--text-xs); font-weight: var(--fw-medium); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .pk-cos-rel-type { display: block; font-size: var(--text-2xs); color: rgba(236,233,246,0.55); }
  .pk-cos-card-open { display: flex; align-items: center; justify-content: center; gap: 6px; width: 100%; margin-top: 4px; padding: 9px; border: 1px solid rgba(255,255,255,0.14); background: rgba(255,255,255,0.06); color: #ECE9F6; border-radius: var(--radius-md); font: inherit; font-size: var(--text-sm); font-weight: var(--fw-medium); cursor: pointer; }
  .pk-cos-card-open:hover { background: rgba(255,255,255,0.12); }
</style>
