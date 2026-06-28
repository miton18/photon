<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import {
    api,
    shareRole,
    shareTarget,
    type Album,
    type Group,
    type ShareRole,
    type TimelinePrefs,
    type User,
  } from '../api';

  let {
    me,
    users,
    groups,
    albums,
    prefs,
    onClose,
    onChanged,
  }: {
    me: User;
    users: User[];
    groups: Group[];
    albums: Album[];
    prefs: TimelinePrefs;
    onClose: () => void;
    onChanged: () => Promise<void> | void;
  } = $props();

  type Tab = 'albums' | 'groups' | 'timeline' | 'partners';
  let tab = $state<Tab>('albums');

  // --- partners ---
  const myPartners = $derived(new Set(me.partners ?? []));
  async function togglePartner(uid: string) {
    try {
      if (myPartners.has(uid)) await api.removePartner(me.id, uid);
      else await api.addPartner(me.id, uid);
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', message: String(e) });
    }
  }

  const userById = (id: string) => users.find((u) => u.id === id);
  const groupById = (id: string) => groups.find((g) => g.id === id);

  const myAlbums = $derived(albums.filter((a) => a.owner_id === me.id));

  // albums shared TO me (directly or via a group I'm in)
  const sharedToMe = $derived(
    albums.filter((a) => {
      if (a.owner_id === me.id) return false;
      return (a.shares ?? []).some((s) => {
        const t = shareTarget(s);
        if (t.type === 'user') return t.id === me.id;
        const g = groupById(t.id);
        return !!g && g.member_ids.includes(me.id);
      });
    }),
  );

  // --- album sharing form ---
  let shareAlbumId = $state('');
  let shareTargetKey = $state(''); // "user:usr_x" | "group:grp_y"
  let shareRoleSel = $state<ShareRole>('viewer');

  async function addShare() {
    if (!shareAlbumId || !shareTargetKey) return;
    const [type, id] = shareTargetKey.split(':') as ['user' | 'group', string];
    try {
      await api.shareAlbum(shareAlbumId, { type, id }, shareRoleSel);
      toast({ tone: 'success', message: 'Album shared' });
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', title: 'Share failed', message: String(e) });
    }
  }
  async function removeShare(album: Album, s: any) {
    try {
      await api.unshareAlbum(album.id, shareTarget(s));
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', title: 'Unshare failed', message: String(e) });
    }
  }

  function targetLabel(s: any) {
    const t = shareTarget(s);
    return t.type === 'user' ? (userById(t.id)?.name ?? t.id) : (groupById(t.id)?.name ?? t.id);
  }

  // --- groups ---
  let newGroupName = $state('');
  let newGroupMembers = $state<Set<string>>(new Set([me.id]));
  async function createGroup() {
    if (!newGroupName.trim()) return;
    try {
      await api.createGroup({ name: newGroupName.trim(), owner_id: me.id, member_ids: [...newGroupMembers] });
      toast({ tone: 'success', message: `Group “${newGroupName}” created` });
      newGroupName = '';
      newGroupMembers = new Set([me.id]);
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', title: 'Create failed', message: String(e) });
    }
  }
  function toggleMember(id: string) {
    const n = new Set(newGroupMembers);
    n.has(id) ? n.delete(id) : n.add(id);
    newGroupMembers = n;
  }

  // --- timeline visibility prefs ---
  function effectiveVisible(albumId: string) {
    return prefs.per_album[albumId] ?? prefs.show_shared;
  }
  async function setGlobal(v: boolean) {
    try {
      await api.updatePrefs(me.id, { show_shared: v });
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', message: String(e) });
    }
  }
  async function setPerAlbum(albumId: string, v: boolean) {
    try {
      await api.updatePrefs(me.id, { per_album: { ...prefs.per_album, [albumId]: v } });
      await onChanged();
    } catch (e) {
      toast({ tone: 'error', message: String(e) });
    }
  }
</script>

<Modal title="Sharing & groups" sub={`Signed in as ${me.name}`} icon="share-2" {onClose}>
  <div class="pk-tabs2">
    {#each [['albums', 'images', 'Albums'], ['groups', 'users-round', 'Groups'], ['partners', 'heart-handshake', 'Partners'], ['timeline', 'layout-grid', 'Timeline']] as [t, ic, lbl] (t)}
      <button class={'pk-tab2' + (tab === t ? ' is-on' : '')} onclick={() => (tab = t as Tab)}>
        <Icon name={ic} size={15} />{lbl}
      </button>
    {/each}
  </div>

  {#if tab === 'albums'}
    <p class="pk-sec-title">Share one of your albums</p>
    <div class="pk-share-form">
      <select class="pk-field-inline" bind:value={shareAlbumId}>
        <option value="" disabled selected>Album…</option>
        {#each myAlbums as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
      </select>
      <select class="pk-field-inline" bind:value={shareTargetKey}>
        <option value="" disabled selected>With…</option>
        <optgroup label="People">
          {#each users.filter((u) => u.id !== me.id) as u (u.id)}<option value={`user:${u.id}`}>{u.name}</option>{/each}
        </optgroup>
        <optgroup label="Groups">
          {#each groups as g (g.id)}<option value={`group:${g.id}`}>{g.name}</option>{/each}
        </optgroup>
      </select>
      <select class="pk-field-inline" bind:value={shareRoleSel}>
        <option value="viewer">Viewer (read-only)</option>
        <option value="contributor">Contributor</option>
      </select>
      <button class="pk-btn pk-btn-primary" onclick={addShare}><Icon name="plus" size={15} />Share</button>
    </div>

    <div class="pk-list" style="margin-top:14px">
      {#each myAlbums as a (a.id)}
        <div class="pk-album-block">
          <div class="pk-listrow" style="background:transparent;border:0;padding:6px 2px">
            <span class="pk-listrow-ic"><Icon name="images" size={15} /></span>
            <div class="pk-listrow-main">
              <div class="pk-listrow-name">{a.name}</div>
              <div class="pk-listrow-sub">{a.photo_ids.length} photos · {(a.shares ?? []).length} shares</div>
            </div>
          </div>
          {#each a.shares ?? [] as s (shareTarget(s).type + shareTarget(s).id)}
            <div class="pk-listrow" style="margin-left:14px">
              <span class="pk-listrow-ic"><Icon name={shareTarget(s).type === 'group' ? 'users-round' : 'user'} size={14} /></span>
              <div class="pk-listrow-main">
                <div class="pk-listrow-name">{targetLabel(s)}</div>
              </div>
              <span class={'pk-pill pk-listrow-act' + (shareRole(s) === 'contributor' ? ' is-on' : '')}>
                <Icon name={shareRole(s) === 'contributor' ? 'pencil' : 'eye'} size={12} />{shareRole(s)}
              </span>
              <button class="pk-pill danger" onclick={() => removeShare(a, s)} aria-label="Remove"><Icon name="x" size={12} /></button>
            </div>
          {/each}
        </div>
      {/each}
    </div>
  {:else if tab === 'groups'}
    <p class="pk-sec-title">Create a group</p>
    <div class="pk-field"><input placeholder="Group name" bind:value={newGroupName} /></div>
    <div class="pk-chips" style="flex-wrap:wrap;margin-bottom:12px">
      {#each users as u (u.id)}
        <button class={'pk-chip' + (newGroupMembers.has(u.id) ? ' is-active' : '')} onclick={() => toggleMember(u.id)}>
          <Icon name={newGroupMembers.has(u.id) ? 'check' : 'plus'} size={12} />{u.name}
        </button>
      {/each}
    </div>
    <button class="pk-btn pk-btn-primary" onclick={createGroup}><Icon name="plus" size={15} />Create group</button>

    <p class="pk-sec-title" style="margin-top:18px">Existing groups</p>
    <div class="pk-list">
      {#each groups as g (g.id)}
        <div class="pk-listrow">
          <span class="pk-listrow-ic"><Icon name="users-round" size={15} /></span>
          <div class="pk-listrow-main">
            <div class="pk-listrow-name">{g.name}</div>
            <div class="pk-listrow-sub">owner {userById(g.owner_id)?.name ?? g.owner_id} · {g.member_ids.length} members</div>
          </div>
        </div>
      {/each}
    </div>
  {:else if tab === 'partners'}
    <p class="pk-sec-title">Partners</p>
    <div class="pk-admin-note" style="display:flex;align-items:center;gap:6px;font-size:var(--text-2xs);color:var(--text-faint);margin-bottom:8px">
      <Icon name="heart-handshake" size={12} /> A partner sees ALL your photos (except trash, archive and vault).
    </div>
    <div class="pk-list">
      {#each users.filter((u) => u.id !== me.id) as u (u.id)}
        <div class="pk-listrow">
          <img src={u.avatar_url || 'https://i.pravatar.cc/64?img=12'} alt={u.name} />
          <div class="pk-listrow-main">
            <div class="pk-listrow-name">{u.name}</div>
            <div class="pk-listrow-sub">{u.email}</div>
          </div>
          <button
            class={'pk-pill' + (myPartners.has(u.id) ? ' is-on' : '')}
            onclick={() => togglePartner(u.id)}
          >
            <Icon name={myPartners.has(u.id) ? 'heart' : 'plus'} size={12} />
            {myPartners.has(u.id) ? 'Partner' : 'Add'}
          </button>
        </div>
      {/each}
    </div>
  {:else}
    <p class="pk-sec-title">Show shared albums in my timeline</p>
    <div class="pk-listrow">
      <span class="pk-listrow-ic"><Icon name="globe" size={15} /></span>
      <div class="pk-listrow-main">
        <div class="pk-listrow-name">All shared albums</div>
        <div class="pk-listrow-sub">Global default for albums shared with you</div>
      </div>
      <button class={'pk-switch pk-listrow-act' + (prefs.show_shared ? ' is-on' : '')} onclick={() => setGlobal(!prefs.show_shared)} aria-label="Toggle global"></button>
    </div>

    {#if sharedToMe.length}
      <p class="pk-sec-title" style="margin-top:16px">Per-album override</p>
      <div class="pk-list">
        {#each sharedToMe as a (a.id)}
          <div class="pk-listrow">
            <span class="pk-listrow-ic"><Icon name="images" size={14} /></span>
            <div class="pk-listrow-main">
              <div class="pk-listrow-name">{a.name}</div>
              <div class="pk-listrow-sub">from {userById(a.owner_id)?.name ?? a.owner_id}{prefs.per_album[a.id] !== undefined ? ' · overridden' : ''}</div>
            </div>
            <button class={'pk-switch pk-listrow-act' + (effectiveVisible(a.id) ? ' is-on' : '')} onclick={() => setPerAlbum(a.id, !effectiveVisible(a.id))} aria-label="Toggle album"></button>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</Modal>

<style>
  .pk-tabs2 { display: flex; gap: 4px; margin-bottom: 16px; border-bottom: 1px solid var(--border); }
  .pk-tab2 { display: inline-flex; align-items: center; gap: 6px; padding: 8px 12px 10px; font-size: var(--text-sm); font-weight: var(--fw-medium); color: var(--text-muted); border-bottom: 2px solid transparent; margin-bottom: -1px; }
  .pk-tab2:hover { color: var(--text); }
  .pk-tab2.is-on { color: var(--accent-text); border-bottom-color: var(--accent); }
  .pk-share-form { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
  .pk-field-inline { height: var(--control-h); padding: 0 10px; background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); color: var(--text); font: inherit; font-size: var(--text-sm); outline: none; }
  .pk-field-inline:focus { border-color: var(--accent-soft-bd); }
  .pk-album-block { margin-bottom: 6px; }
</style>
