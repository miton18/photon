<script lang="ts">
  /* Photon — People Studio (face tagging atelier).
     A 3-pane workspace to name detected face clusters, ignore intruder faces,
     move faces between people, merge clusters the recognizer split, set a cover,
     and record birthdates + relationships. Recognition runs on the server; this
     curates it. Person ids stay stable across in-place edits, and the decisions
     persist past a future re-cluster (server records them on the stable faces). */
  import Icon from '../icons/Icon.svelte';
  import { api, API, type Person, type StudioFace } from '../api';
  import { authedUrl } from '../session';
  import { toast } from '../toast.svelte';

  let { userId }: { userId: string } = $props();

  // Relationship vocabulary (the server computes the reciprocal edge).
  const REL_TYPES: { t: string; label: string; icon: string }[] = [
    { t: 'mother', label: 'Mother', icon: 'user' },
    { t: 'father', label: 'Father', icon: 'user' },
    { t: 'sibling', label: 'Sibling', icon: 'users' },
    { t: 'son', label: 'Son', icon: 'baby' },
    { t: 'daughter', label: 'Daughter', icon: 'baby' },
    { t: 'spouse', label: 'Spouse', icon: 'heart' },
    { t: 'partner', label: 'Partner', icon: 'heart' },
    { t: 'friend', label: 'Friend', icon: 'smile' },
  ];
  const relIcon = (t: string) => REL_TYPES.find((r) => r.t === t)?.icon ?? 'link';
  const SUS = 0.7; // below this confidence a face is flagged "to check"

  let people = $state<Person[]>([]);
  let selectedId = $state<string | null>(null);
  let faces = $state<StudioFace[]>([]);
  let sel = $state<Set<string>>(new Set());
  let modal = $state<null | 'move' | 'merge' | 'relation'>(null);
  let loading = $state(true);
  let naming = $state(false);
  let nameInput = $state('');

  // A face needs review when the detector wasn't confident AND a human hasn't
  // confirmed it yet.
  const sus = (f: StudioFace) => f.score < SUS && !f.confirmed;
  const person = $derived(people.find((p) => p.person_id === selectedId) ?? people[0] ?? null);
  const isCluster = $derived(!!person && !person.name);
  const susFaces = $derived(faces.filter(sus));
  const susCount = $derived(susFaces.length);
  const allSel = $derived(faces.length > 0 && sel.size === faces.length);

  const clusters = $derived(people.filter((p) => !p.name));
  const named = $derived(people.filter((p) => p.name));

  async function loadPeople(keepId: string | undefined = undefined) {
    people = await api.people(userId).catch(() => [] as Person[]);
    if (people.length === 0) {
      selectedId = null;
      faces = [];
      loading = false;
      return;
    }
    const want = keepId ?? selectedId;
    selectedId = people.some((p) => p.person_id === want) ? want! : people[0].person_id;
    await loadFaces();
    loading = false;
  }
  async function loadFaces() {
    sel = new Set();
    if (!selectedId) {
      faces = [];
      return;
    }
    try {
      const r = await api.personFaces(selectedId);
      faces = r.faces;
    } catch {
      faces = [];
    }
  }
  function select(id: string) {
    selectedId = id;
    naming = false;
    loadFaces();
  }
  loadPeople();

  // ---- face crop geometry: position an absolutely-placed photo thumbnail inside a
  // square tile so the face box is pixel-centered and fills it (works for face
  // tiles AND avatar covers — same math). `box` is [x,y,w,h] in source pixels. ----
  const thumb = (photoId: string) => authedUrl(`${API}/api/photos/${photoId}/thumb`) ?? '';
  function cropStyle(box: [number, number, number, number], sw: number): string {
    const [bx, by, bw, bh] = box;
    const m = Math.max(bw, bh) || 1;
    const widthPct = (sw / m) * 100;
    const leftPct = 50 - ((bx + bw / 2) / m) * 100;
    const topPct = 50 - ((by + bh / 2) / m) * 100;
    return `width:${widthPct}%;left:${leftPct}%;top:${topPct}%`;
  }

  function ageFrom(dob: string | null | undefined = undefined): number | null {
    if (!dob) return null;
    const d = new Date(dob), now = new Date();
    let a = now.getFullYear() - d.getFullYear();
    if (now.getMonth() < d.getMonth() || (now.getMonth() === d.getMonth() && now.getDate() < d.getDate())) a--;
    return a;
  }
  const first = (name: string | null | undefined = undefined) => (name ? name.split(' ')[0] : 'this person');

  function toggle(id: string) {
    const n = new Set(sel);
    n.has(id) ? n.delete(id) : n.add(id);
    sel = n;
  }
  const selectAll = () => (sel = allSel ? new Set() : new Set(faces.map((f) => f.id)));
  const selectSus = () => (sel = new Set(susFaces.map((f) => f.id)));

  // ---- actions ----
  async function rename(name: string) {
    if (!person) return;
    const id = person.person_id;
    try {
      await api.namePerson(id, name.trim());
      naming = false;
      await loadPeople(id);
      toast({ tone: 'success', message: name.trim() ? `Named “${name.trim()}”` : 'Name cleared' });
    } catch {
      toast({ tone: 'error', message: 'Could not save name' });
    }
  }
  async function setDob(dob: string) {
    if (!person) return;
    const id = person.person_id;
    try {
      await api.setPersonBirthdate(id, dob || null);
      await loadPeople(id);
    } catch {
      toast({ tone: 'error', message: 'Could not save birthdate' });
    }
  }
  async function setCover() {
    if (!person || sel.size !== 1) return;
    const id = person.person_id;
    try {
      await api.setPersonCover(id, [...sel][0]);
      await loadPeople(id);
      toast({ tone: 'success', message: 'Cover updated' });
    } catch {
      toast({ tone: 'error', message: 'Could not set cover' });
    }
  }
  async function ignore() {
    if (!person || sel.size === 0) return;
    const id = person.person_id;
    const n = sel.size;
    try {
      await api.ignoreFaces(id, [...sel]);
      await loadPeople(id);
      toast({ tone: 'info', message: `${n} face${n > 1 ? 's' : ''} ignored` });
    } catch {
      toast({ tone: 'error', message: 'Could not ignore faces' });
    }
  }
  // ---- low-confidence review: approve (it IS this person) / deny (intruder) ----
  async function approve(faceIds: string[]) {
    if (!person || faceIds.length === 0) return;
    const id = person.person_id;
    try {
      await api.approveFaces(id, faceIds);
      await loadFaces();
      toast({ tone: 'success', message: `${faceIds.length} confirmed` });
    } catch {
      toast({ tone: 'error', message: 'Could not confirm' });
    }
  }
  async function deny(faceIds: string[]) {
    if (!person || faceIds.length === 0) return;
    const id = person.person_id;
    try {
      await api.ignoreFaces(id, faceIds);
      await loadPeople(id);
      toast({ tone: 'info', message: `${faceIds.length} removed` });
    } catch {
      toast({ tone: 'error', message: 'Could not remove' });
    }
  }
  const approveAll = () => approve(susFaces.map((f) => f.id));
  const denyAll = () => deny(susFaces.map((f) => f.id));
  async function move(toId: string) {
    if (!person) return;
    const ids = [...sel];
    const fromId = person.person_id;
    modal = null;
    try {
      await api.moveFaces(fromId, ids, toId);
      await loadPeople(fromId);
      const dst = people.find((p) => p.person_id === toId);
      toast({ tone: 'success', message: `Moved ${ids.length} to ${dst?.name || 'cluster'}` });
    } catch {
      toast({ tone: 'error', message: 'Could not move faces' });
    }
  }
  async function merge(intoId: string) {
    if (!person) return;
    const srcId = person.person_id;
    modal = null;
    try {
      await api.mergePeople(srcId, intoId);
      await loadPeople(intoId);
      toast({ tone: 'success', message: 'Clusters merged' });
    } catch {
      toast({ tone: 'error', message: 'Could not merge' });
    }
  }
  async function hide() {
    if (!person) return;
    if (!confirm('Hide this person from People?')) return;
    const id = person.person_id;
    try {
      await api.hidePerson(id);
      await loadPeople();
      toast({ tone: 'info', message: 'Person hidden' });
    } catch {
      toast({ tone: 'error', message: 'Could not hide person' });
    }
  }
  async function addRelation(t: string, otherId: string) {
    if (!person) return;
    const id = person.person_id;
    modal = null;
    try {
      await api.addRelationship(id, otherId, t);
      await loadPeople(id);
      toast({ tone: 'success', message: 'Relationship added' });
    } catch {
      toast({ tone: 'error', message: 'Could not link people' });
    }
  }
  async function removeRelation(otherId: string) {
    if (!person) return;
    const id = person.person_id;
    try {
      await api.removeRelationship(id, otherId);
      await loadPeople(id);
    } catch {
      toast({ tone: 'error', message: 'Could not remove link' });
    }
  }

  // ---- modals (move / merge picker, relationship) ----
  let pickQ = $state('');
  let relType = $state('friend');
  const pickList = $derived(
    people
      .filter((p) => person && p.person_id !== person.person_id)
      .filter((p) => (p.name || 'Unnamed').toLowerCase().includes(pickQ.toLowerCase())),
  );
  const relList = $derived(
    people.filter(
      (p) =>
        person &&
        p.person_id !== person.person_id &&
        p.name &&
        !person.relationships.some((r) => r.person_id === p.person_id) &&
        p.name.toLowerCase().includes(pickQ.toLowerCase()),
    ),
  );
  function openModal(m: 'move' | 'merge' | 'relation') {
    pickQ = '';
    relType = 'friend';
    modal = m;
  }

  $effect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && modal) modal = null;
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

<!-- Reusable avatar: an exact face crop, or a user glyph fallback. -->
{#snippet avatar(p: Person | null, cls: string)}
  <span class={'pk-ps-av ' + cls + (p?.cover ? '' : ' is-empty')}>
    {#if p?.cover}
      <img src={thumb(p.cover.photo_id)} alt="" style={cropStyle(p.cover.bbox, p.cover.source_width)} draggable="false"
        onerror={(e) => ((e.currentTarget as HTMLImageElement).style.display = 'none')} />
    {:else}
      <Icon name="user" size={16} />
    {/if}
  </span>
{/snippet}

<div class="pk-ps" role="region" aria-label="People Studio">
  <div class="pk-ps-header">
    <div class="pk-ps-header-txt">
      <h1>People</h1>
      <p>Name faces, remove intruders, merge duplicates and record relationships. Recognition runs on your server.</p>
    </div>
    <div class="pk-ps-header-stats pk-mono">
      <span><b>{named.length}</b> named</span>
      <span class="pk-ps-stat-rev"><b>{clusters.length}</b> to review</span>
    </div>
  </div>

  {#if loading}
    <div class="pk-ps-loading">Loading people…</div>
  {:else if !person}
    <div class="pk-ps-loading">No faces detected yet. Upload photos or run face detection.</div>
  {:else}
  <div class="pk-ps-body">
    <!-- ROSTER -->
    <div class="pk-ps-roster">
      <div class="pk-ps-roster-scroll">
        {#if clusters.length > 0}
          <div class="pk-ps-group"><Icon name="scan-face" size={13} /> Needs review<span class="pk-ps-group-count">{clusters.length}</span></div>
          {#each clusters as p (p.person_id)}
            <button class={'pk-ps-row' + (selectedId === p.person_id ? ' is-active' : '')} onclick={() => select(p.person_id)}>
              {@render avatar(p, 'pk-ps-row-av is-unnamed')}
              <div class="pk-ps-row-txt"><span class="pk-ps-row-name">Unnamed</span><span class="pk-ps-row-meta pk-mono">{p.face_count} to review</span></div>
              <span class="pk-ps-row-dot" title="Needs review"></span>
            </button>
          {/each}
        {/if}
        <div class="pk-ps-group"><Icon name="users" size={13} /> Named people<span class="pk-ps-group-count">{named.length}</span></div>
        {#each named as p (p.person_id)}
          <button class={'pk-ps-row' + (selectedId === p.person_id ? ' is-active' : '')} onclick={() => select(p.person_id)}>
            {@render avatar(p, 'pk-ps-row-av')}
            <div class="pk-ps-row-txt"><span class="pk-ps-row-name">{p.name}</span><span class="pk-ps-row-meta pk-mono">{p.face_count} photos</span></div>
          </button>
        {/each}
      </div>
    </div>

    <!-- WORKSPACE -->
    <div class="pk-ps-work">
      <div class="pk-ps-work-head">
        {@render avatar(person, 'pk-ps-cover')}
        <div class="pk-ps-work-id">
          {#if isCluster || naming}
            <div class="pk-ps-name-row">
              <input class="pk-ps-name-input" placeholder="Add a name…" bind:value={nameInput}
                onkeydown={(e) => { if (e.key === 'Enter' && nameInput.trim()) rename(nameInput); }} />
              <button class="pk-btn pk-btn-primary" onclick={() => nameInput.trim() && rename(nameInput)}><Icon name="check" size={15} /> Name</button>
              {#if naming}<button class="pk-btn" onclick={() => (naming = false)}>Cancel</button>{/if}
            </div>
          {:else}
            <div class="pk-ps-name-row">
              <h2 class="pk-ps-name">{person.name}</h2>
              <button class="pk-iconbtn" title="Rename" onclick={() => { naming = true; nameInput = person.name ?? ''; }}><Icon name="pencil" size={15} /></button>
            </div>
          {/if}
          <div class="pk-ps-work-meta">
            <span><Icon name="image" size={13} /> {faces.length} faces</span>
            {#if isCluster}
              <span class="pk-ps-tag-review"><Icon name="scan-face" size={13} /> Needs review</span>
            {:else if person.birthdate}
              <span><Icon name="cake" size={13} /> {ageFrom(person.birthdate)} yrs</span>
            {/if}
            {#if susCount > 0}<button class="pk-ps-tag-sus" onclick={selectSus}><Icon name="triangle-alert" size={13} /> {susCount} to check</button>{/if}
          </div>
        </div>
        <div class="pk-ps-work-actions">
          <button class="pk-btn" onclick={() => openModal('merge')}><Icon name="git-merge" size={15} /> Merge…</button>
        </div>
      </div>

      <div class={'pk-ps-toolbar' + (sel.size ? ' is-active' : '')}>
        <button class="pk-ps-chk" onclick={selectAll}>
          <span class={'pk-ps-chk-box' + (allSel ? ' is-on' : '')}>{#if allSel}<Icon name="check" size={12} strokeWidth={3} />{/if}</span>
          {sel.size ? `${sel.size} selected` : 'Select all'}
        </button>
        <div class="pk-ps-toolbar-actions">
          <button class="pk-btn pk-btn-sm" disabled={!sel.size} onclick={ignore}><Icon name="eye-off" size={14} /> Ignore</button>
          <button class="pk-btn pk-btn-sm" disabled={!sel.size} onclick={() => openModal('move')}><Icon name="user-round-cog" size={14} /> Move to…</button>
          <button class="pk-btn pk-btn-sm" disabled={sel.size !== 1} onclick={setCover}><Icon name="image-up" size={14} /> Set cover</button>
        </div>
      </div>

      {#if susCount > 0}
        <div class="pk-ps-review-bar">
          <span class="pk-ps-review-lead"><Icon name="scan-face" size={15} /><b>{susCount}</b> face{susCount > 1 ? 's' : ''} need{susCount > 1 ? '' : 's'} review</span>
          <span class="pk-ps-review-hint">Low-confidence matches — confirm they're {isCluster ? 'the same person' : first(person?.name)}, or remove intruders.</span>
          <div class="pk-ps-review-actions">
            <button class="pk-btn pk-btn-sm" onclick={denyAll}><Icon name="x" size={14} /> Deny all</button>
            <button class="pk-btn pk-btn-sm pk-btn-primary" onclick={approveAll}><Icon name="check" size={14} /> Approve all</button>
          </div>
        </div>
      {/if}

      <div class="pk-ps-grid">
        {#each faces as f (f.id)}
          <button class={'pk-ps-face' + (sel.has(f.id) ? ' is-sel' : '') + (sus(f) ? ' is-sus' : '')} onclick={() => toggle(f.id)}>
            <span class="pk-ps-face-fallback"><Icon name="image-off" size={18} /></span>
            <img src={thumb(f.photo_id)} alt="" style={cropStyle(f.bbox, f.source_width)} draggable="false"
              onerror={(e) => ((e.currentTarget as HTMLImageElement).style.display = 'none')} />
            <span class="pk-ps-face-check"><Icon name="check" size={13} strokeWidth={3} /></span>
            {#if sus(f)}<span class="pk-ps-face-warn" title="Low confidence — needs review"><Icon name="triangle-alert" size={12} /></span>{/if}
            <span class="pk-ps-face-conf pk-mono">{Math.round(f.score * 100)}%</span>
            {#if sus(f)}
              <span class="pk-ps-face-review">
                <span class="pk-ps-face-rbtn deny" role="button" tabindex="-1" title="Not this person — remove"
                  onclick={(e) => { e.stopPropagation(); deny([f.id]); }} onkeydown={() => {}}><Icon name="x" size={15} strokeWidth={2.5} /></span>
                <span class="pk-ps-face-rbtn approve" role="button" tabindex="-1" title="Yes, this is them"
                  onclick={(e) => { e.stopPropagation(); approve([f.id]); }} onkeydown={() => {}}><Icon name="check" size={15} strokeWidth={2.5} /></span>
              </span>
            {/if}
          </button>
        {/each}
      </div>
    </div>

    <!-- INSPECTOR -->
    <div class="pk-ps-inspector">
      <div class="pk-ps-insp-id">
        {@render avatar(person, 'pk-ps-insp-av')}
        <div>
          <span class="pk-ps-insp-name">{person.name || 'Unnamed cluster'}</span>
          <span class="pk-ps-insp-sub pk-mono">{faces.length} faces{person.birthdate ? ` · ${ageFrom(person.birthdate)} yrs` : ''}</span>
        </div>
      </div>

      {#if isCluster}
        <div class="pk-ps-insp-note"><Icon name="info" size={15} /> Give this cluster a name to unlock birthdate &amp; relationships.</div>
      {:else}
        <div class="pk-ps-insp-sec">
          <label class="pk-ps-insp-label" for="ps-dob">Date of birth</label>
          <input id="ps-dob" type="date" class="pk-ps-dob" value={person.birthdate || ''} max={new Date().toISOString().slice(0, 10)}
            onchange={(e) => setDob((e.currentTarget as HTMLInputElement).value)} />
        </div>

        <div class="pk-ps-insp-sec">
          <div class="pk-ps-insp-sechead">
            <span class="pk-ps-insp-label">Relationships</span>
            <button class="pk-ps-add" onclick={() => openModal('relation')}><Icon name="plus" size={14} /> Add</button>
          </div>
          <div class="pk-ps-rels">
            {#if person.relationships.length === 0}<div class="pk-ps-rels-empty">No links yet.</div>{/if}
            {#each person.relationships as r (r.person_id)}
              <div class="pk-ps-rel">
                <span class="pk-ps-rel-ic"><Icon name={relIcon(r.relation)} size={14} /></span>
                <div class="pk-ps-rel-txt"><span class="pk-ps-rel-name">{r.name || 'Unnamed'}</span><span class="pk-ps-rel-sub">{r.relation} of {first(person.name)}</span></div>
                <button class="pk-ps-rel-x" title="Remove" onclick={() => removeRelation(r.person_id)}><Icon name="x" size={14} /></button>
              </div>
            {/each}
          </div>
        </div>

        <div class="pk-ps-insp-sec pk-ps-insp-danger">
          <button class="pk-ps-hide" onclick={hide}><Icon name="eye-off" size={15} /> Hide from People</button>
        </div>
      {/if}
    </div>
  </div>
  {/if}

  <!-- person picker (move / merge) -->
  {#if modal === 'move' || modal === 'merge'}
    <div class="pk-ps-modal-scrim" role="button" tabindex="-1" onclick={(e) => { if (e.target === e.currentTarget) modal = null; }} onkeydown={() => {}}>
      <div class="pk-ps-modal">
        <div class="pk-ps-modal-head">
          <div>
            <h3>{modal === 'move' ? 'Move faces to…' : 'Merge with…'}</h3>
            <p>{modal === 'move' ? `Reassign ${sel.size} selected face${sel.size > 1 ? 's' : ''} to another person.` : `Combine ${first(person?.name)} into the person you pick.`}</p>
          </div>
          <button class="pk-iconbtn" onclick={() => (modal = null)}><Icon name="x" size={18} /></button>
        </div>
        <div class="pk-ps-modal-search"><Icon name="search" size={15} /><input placeholder="Search people…" bind:value={pickQ} /></div>
        <div class="pk-ps-modal-list">
          {#each pickList as p (p.person_id)}
            <button class="pk-ps-pick" onclick={() => (modal === 'move' ? move(p.person_id) : merge(p.person_id))}>
              {@render avatar(p, "pk-ps-pick-av")}
              <div class="pk-ps-pick-txt"><span class="pk-ps-pick-name">{p.name || 'Unnamed cluster'}</span><span class="pk-ps-pick-meta pk-mono">{p.face_count} faces</span></div>
              <Icon name="chevron-right" size={16} />
            </button>
          {/each}
          {#if pickList.length === 0}<div class="pk-ps-empty">No matches.</div>{/if}
        </div>
      </div>
    </div>
  {/if}

  <!-- relationship -->
  {#if modal === 'relation'}
    <div class="pk-ps-modal-scrim" role="button" tabindex="-1" onclick={(e) => { if (e.target === e.currentTarget) modal = null; }} onkeydown={() => {}}>
      <div class="pk-ps-modal">
        <div class="pk-ps-modal-head">
          <div><h3>Add relationship</h3><p>How is this person connected to {first(person?.name)}?</p></div>
          <button class="pk-iconbtn" onclick={() => (modal = null)}><Icon name="x" size={18} /></button>
        </div>
        <div class="pk-ps-rel-types">
          {#each REL_TYPES as r (r.t)}
            <button class={'pk-ps-rel-type' + (relType === r.t ? ' is-on' : '')} onclick={() => (relType = r.t)}><Icon name={r.icon} size={15} />{r.label}</button>
          {/each}
        </div>
        <div class="pk-ps-modal-search"><Icon name="search" size={15} /><input placeholder="Search named people…" bind:value={pickQ} /></div>
        <div class="pk-ps-modal-list">
          {#each relList as p (p.person_id)}
            <button class="pk-ps-pick" onclick={() => addRelation(relType, p.person_id)}>
              {@render avatar(p, "pk-ps-pick-av")}
              <div class="pk-ps-pick-txt"><span class="pk-ps-pick-name">{p.name}</span><span class="pk-ps-pick-meta pk-mono">{relType} of {first(person?.name)}</span></div>
              <Icon name="plus" size={16} />
            </button>
          {/each}
          {#if relList.length === 0}<div class="pk-ps-empty">No named people left to link.</div>{/if}
        </div>
      </div>
    </div>
  {/if}
</div>

<style>
  /* Inline view: fill the main content area; nested pickers anchor to this box. */
  .pk-ps { position: relative; flex: 1; min-height: 0; display: flex; flex-direction: column; overflow: hidden; }
  .pk-ps-header { display: flex; align-items: flex-start; gap: 16px; padding: 20px 22px; border-bottom: 1px solid var(--border); }
  .pk-ps-header-txt h1 { font-family: var(--font-display); font-size: var(--text-2xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); }
  .pk-ps-header-txt p { font-size: var(--text-xs); color: var(--text-muted); margin-top: 3px; max-width: 60ch; }
  .pk-ps-header-stats { display: flex; gap: 16px; margin-left: auto; font-size: var(--text-xs); color: var(--text-muted); }
  .pk-ps-header-stats b { color: var(--text); }
  .pk-ps-stat-rev b { color: var(--accent); }
  .pk-ps-close { margin-left: 4px; }
  .pk-ps-loading { flex: 1; display: grid; place-items: center; color: var(--text-faint); font-size: var(--text-sm); }
  .pk-ps-body { flex: 1; display: grid; grid-template-columns: 248px 1fr 300px; min-height: 0; }

  /* roster */
  .pk-ps-roster { border-right: 1px solid var(--border); display: flex; flex-direction: column; min-height: 0; background: var(--surface); }
  .pk-ps-roster-scroll { overflow-y: auto; padding: 8px; min-height: 0; }
  .pk-ps-group { display: flex; align-items: center; gap: 6px; font-size: var(--text-2xs); text-transform: uppercase; letter-spacing: var(--ls-caps); font-weight: var(--fw-semibold); color: var(--text-faint); padding: 12px 8px 6px; }
  .pk-ps-group-count { margin-left: auto; font-family: var(--font-mono); }
  .pk-ps-row { display: flex; align-items: center; gap: 10px; padding: 7px 8px; border-radius: var(--radius-md); width: 100%; text-align: left; }
  .pk-ps-row:hover { background: var(--surface-hover); }
  .pk-ps-row.is-active { background: var(--accent-soft); }
  /* avatars: a square/round window onto an absolutely-positioned face crop */
  .pk-ps-av { position: relative; overflow: hidden; flex: none; background: var(--bg-subtle); display: block; }
  .pk-ps-av img { position: absolute; height: auto; max-width: none; }
  .pk-ps-av.is-empty { display: grid; place-items: center; color: var(--text-faint); }
  .pk-ps-row-av { width: 34px; height: 34px; border-radius: 50%; }
  .pk-ps-row-av.is-unnamed img { filter: grayscale(.5); }
  .pk-ps-row-txt { display: flex; flex-direction: column; min-width: 0; }
  .pk-ps-row-name { font-size: var(--text-sm); font-weight: var(--fw-medium); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pk-ps-row.is-active .pk-ps-row-name { color: var(--accent-text); }
  .pk-ps-row-meta { font-size: var(--text-2xs); color: var(--text-faint); }
  .pk-ps-row-dot { margin-left: auto; width: 7px; height: 7px; border-radius: 50%; background: var(--accent); flex: none; }

  /* workspace */
  .pk-ps-work { display: flex; flex-direction: column; min-height: 0; }
  .pk-ps-work-head { display: flex; align-items: center; gap: 14px; padding: 18px 20px; border-bottom: 1px solid var(--border); }
  .pk-ps-cover { width: 64px; height: 64px; border-radius: var(--radius-md); }
  .pk-ps-work-id { min-width: 0; flex: 1; }
  .pk-ps-name-row { display: flex; align-items: center; gap: 8px; }
  .pk-ps-name { font-family: var(--font-display); font-size: var(--text-xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); }
  .pk-ps-name-input { font-size: var(--text-lg); font-weight: var(--fw-semibold); background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); padding: 7px 11px; min-width: 220px; }
  .pk-ps-work-meta { display: flex; align-items: center; gap: 14px; margin-top: 7px; font-size: var(--text-xs); color: var(--text-muted); }
  .pk-ps-work-meta span, .pk-ps-tag-review, .pk-ps-tag-sus { display: inline-flex; align-items: center; gap: 5px; }
  .pk-ps-tag-review { color: var(--accent); }
  .pk-ps-tag-sus { color: var(--warning); cursor: pointer; }
  .pk-ps-work-actions { margin-left: auto; }

  .pk-ps-toolbar { display: flex; align-items: center; gap: 12px; padding: 10px 20px; border-bottom: 1px solid var(--border); background: var(--surface); transition: background var(--dur-fast); }
  .pk-ps-toolbar.is-active { background: var(--accent-soft); }
  .pk-ps-chk { display: inline-flex; align-items: center; gap: 9px; font-size: var(--text-sm); font-weight: var(--fw-medium); }
  .pk-ps-chk-box { width: 18px; height: 18px; border: 1.5px solid var(--border-strong, var(--border)); border-radius: 5px; display: grid; place-items: center; color: #fff; }
  .pk-ps-chk-box.is-on { background: var(--accent); border-color: var(--accent); }
  .pk-ps-toolbar-actions { margin-left: auto; display: flex; gap: 8px; }

  .pk-ps-grid { flex: 1; overflow-y: auto; padding: 16px 20px; display: grid; grid-template-columns: repeat(auto-fill, minmax(96px, 1fr)); gap: 10px; align-content: start; }
  .pk-ps-face { position: relative; aspect-ratio: 1; border-radius: var(--radius-md); overflow: hidden; background: var(--bg-subtle); border: 2px solid transparent; }
  .pk-ps-face img { position: absolute; height: auto; max-width: none; user-select: none; }
  /* Shown when the photo thumbnail is missing (img hidden by onerror) — no empty tile. */
  .pk-ps-face-fallback { position: absolute; inset: 0; display: grid; place-items: center; color: var(--text-faint); }
  .pk-ps-face.is-sel { border-color: var(--accent); }
  .pk-ps-face.is-sus { border-color: color-mix(in srgb, var(--warning) 60%, transparent); }
  .pk-ps-face-check { position: absolute; top: 5px; left: 5px; width: 18px; height: 18px; border-radius: 5px; background: var(--accent); color: #fff; display: grid; place-items: center; opacity: 0; transform: scale(.7); transition: all var(--dur-fast); }
  .pk-ps-face.is-sel .pk-ps-face-check { opacity: 1; transform: scale(1); }
  .pk-ps-face-warn { position: absolute; top: 5px; right: 5px; color: var(--warning); background: rgba(0,0,0,.45); border-radius: 5px; padding: 1px; display: grid; place-items: center; }
  .pk-ps-face-conf { position: absolute; bottom: 4px; right: 5px; font-size: 9px; color: #fff; background: rgba(0,0,0,.5); padding: 1px 4px; border-radius: 4px; }
  /* approve / deny buttons revealed on a low-confidence face */
  .pk-ps-face-review { position: absolute; inset: auto 0 0 0; display: flex; gap: 4px; padding: 5px; opacity: 0; transform: translateY(4px); transition: all var(--dur-fast); background: linear-gradient(to top, rgba(0,0,0,.55), transparent); }
  .pk-ps-face:hover .pk-ps-face-review, .pk-ps-face.is-sus .pk-ps-face-review { opacity: 1; transform: none; }
  .pk-ps-face-rbtn { flex: 1; height: 26px; border-radius: var(--radius-sm, 6px); display: grid; place-items: center; color: #fff; backdrop-filter: blur(2px); }
  .pk-ps-face-rbtn.deny { background: color-mix(in srgb, var(--danger) 82%, transparent); }
  .pk-ps-face-rbtn.deny:hover { background: var(--danger); }
  .pk-ps-face-rbtn.approve { background: color-mix(in srgb, var(--success, #16a34a) 82%, transparent); }
  .pk-ps-face-rbtn.approve:hover { background: var(--success, #16a34a); }

  .pk-ps-review-bar { display: flex; align-items: center; gap: 14px; padding: 11px 20px; border-bottom: 1px solid var(--border); background: color-mix(in srgb, var(--warning) 10%, var(--surface)); flex-wrap: wrap; }
  .pk-ps-review-lead { display: inline-flex; align-items: center; gap: 7px; font-size: var(--text-sm); font-weight: var(--fw-semibold); color: var(--warning); white-space: nowrap; }
  .pk-ps-review-hint { font-size: var(--text-xs); color: var(--text-muted); min-width: 0; }
  .pk-ps-review-actions { margin-left: auto; display: flex; gap: 8px; }

  /* inspector */
  .pk-ps-inspector { border-left: 1px solid var(--border); padding: 18px; overflow-y: auto; display: flex; flex-direction: column; gap: 18px; background: var(--surface); }
  .pk-ps-insp-id { display: flex; align-items: center; gap: 11px; }
  .pk-ps-insp-av { width: 48px; height: 48px; border-radius: 50%; }
  .pk-ps-insp-name { display: block; font-weight: var(--fw-semibold); font-size: var(--text-base); }
  .pk-ps-insp-sub { font-size: var(--text-2xs); color: var(--text-faint); }
  .pk-ps-insp-note { display: flex; gap: 9px; font-size: var(--text-xs); color: var(--text-muted); background: var(--bg-subtle); padding: 11px; border-radius: var(--radius-md); line-height: 1.5; }
  .pk-ps-insp-sec { display: flex; flex-direction: column; gap: 8px; }
  .pk-ps-insp-sechead { display: flex; align-items: center; justify-content: space-between; }
  .pk-ps-insp-label { font-size: var(--text-2xs); text-transform: uppercase; letter-spacing: var(--ls-caps); font-weight: var(--fw-semibold); color: var(--text-faint); }
  .pk-ps-add { display: inline-flex; align-items: center; gap: 4px; font-size: var(--text-xs); color: var(--accent); }
  .pk-ps-dob { background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); padding: 8px 10px; font-size: var(--text-sm); color: var(--text); }
  .pk-ps-rels { display: flex; flex-direction: column; gap: 6px; }
  .pk-ps-rels-empty { font-size: var(--text-xs); color: var(--text-faint); }
  .pk-ps-rel { display: flex; align-items: center; gap: 9px; padding: 7px 9px; border: 1px solid var(--border); border-radius: var(--radius-md); }
  .pk-ps-rel-ic { color: var(--accent); display: grid; place-items: center; }
  .pk-ps-rel-txt { display: flex; flex-direction: column; min-width: 0; }
  .pk-ps-rel-name { font-size: var(--text-sm); font-weight: var(--fw-medium); }
  .pk-ps-rel-sub { font-size: var(--text-2xs); color: var(--text-faint); }
  .pk-ps-rel-x { margin-left: auto; color: var(--text-faint); }
  .pk-ps-rel-x:hover { color: var(--danger); }
  .pk-ps-insp-danger { margin-top: auto; }
  .pk-ps-hide { display: inline-flex; align-items: center; gap: 8px; font-size: var(--text-sm); color: var(--danger); }

  /* picker / relation modals */
  .pk-ps-modal-scrim { position: absolute; inset: 0; background: rgba(0,0,0,.4); display: grid; place-items: center; z-index: 5; }
  .pk-ps-modal { width: 440px; max-width: 92%; max-height: 80%; background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius-lg); display: flex; flex-direction: column; overflow: hidden; box-shadow: var(--shadow-lg, 0 18px 50px -18px rgba(0,0,0,.6)); }
  .pk-ps-modal-head { display: flex; align-items: flex-start; gap: 12px; padding: 16px 18px; border-bottom: 1px solid var(--border); }
  .pk-ps-modal-head h3 { font-size: var(--text-base); font-weight: var(--fw-bold); }
  .pk-ps-modal-head p { font-size: var(--text-xs); color: var(--text-muted); margin-top: 3px; }
  .pk-ps-modal-head .pk-iconbtn { margin-left: auto; }
  .pk-ps-modal-search { display: flex; align-items: center; gap: 8px; padding: 10px 16px; border-bottom: 1px solid var(--border); color: var(--text-faint); }
  .pk-ps-modal-search input { flex: 1; background: none; font-size: var(--text-sm); }
  .pk-ps-modal-list { overflow-y: auto; padding: 8px; min-height: 80px; }
  .pk-ps-pick { display: flex; align-items: center; gap: 11px; width: 100%; padding: 8px 10px; border-radius: var(--radius-md); text-align: left; }
  .pk-ps-pick:hover { background: var(--surface-hover); }
  .pk-ps-pick-av { width: 38px; height: 38px; border-radius: 50%; }
  .pk-ps-pick-txt { display: flex; flex-direction: column; min-width: 0; flex: 1; }
  .pk-ps-pick-name { font-size: var(--text-sm); font-weight: var(--fw-medium); }
  .pk-ps-pick-meta { font-size: var(--text-2xs); color: var(--text-faint); }
  .pk-ps-empty { text-align: center; color: var(--text-faint); font-size: var(--text-sm); padding: 18px; }
  .pk-ps-rel-types { display: flex; flex-wrap: wrap; gap: 6px; padding: 12px 16px; border-bottom: 1px solid var(--border); }
  .pk-ps-rel-type { display: inline-flex; align-items: center; gap: 6px; padding: 6px 10px; border: 1px solid var(--border); border-radius: var(--radius-pill); font-size: var(--text-xs); }
  .pk-ps-rel-type.is-on { background: var(--accent); border-color: var(--accent); color: #fff; }
</style>
