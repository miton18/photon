<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import { uploadConfig, setUploadMaxPerRequest } from '../upload.svelte';
  import {
    api,
    type AdminStats,
    type Album,
    type FeatureFlags,
    type JobRun,
    type SmtpConfig,
    type StorageSettings,
    type User,
  } from '../api';

  let {
    me,
    users,
    onClose,
    onChanged,
    onOpenAlbum,
  }: {
    me: User;
    users: User[];
    onClose: () => void;
    onChanged: () => Promise<void> | void;
    onOpenAlbum?: (albumId: string) => void;
  } = $props();

  const VERSION = 'Photon 0.1.0';

  type View = 'Overview' | 'Users' | 'Albums' | 'Storage' | 'Jobs' | 'Settings';
  const NAV: [View, string][] = [
    ['Overview', 'layout-dashboard'],
    ['Users', 'users'],
    ['Albums', 'images'],
    ['Storage', 'hard-drive'],
    ['Jobs', 'cpu'],
    ['Settings', 'settings'],
  ];
  const SUBS: Record<View, string> = {
    Overview: 'Server status and activity at a glance',
    Users: 'Manage accounts, roles and quotas',
    Albums: 'All albums across every user',
    Storage: 'Disk usage and per-user breakdown',
    Jobs: 'Background processing pipeline',
    Settings: 'Server configuration',
  };
  let view = $state<View>('Overview');

  // ---- data ----
  let stats = $state<AdminStats | null>(null);
  let albums = $state<Album[]>([]);
  let storage = $state<StorageSettings | null>(null);
  let smtp = $state<SmtpConfig | null>(null);
  let gravatarEnabled = $state(false);
  let features = $state<FeatureFlags>({
    faces: false, clip: false, ocr: false, geocode: false,
    transcode: false, public_signup: false, public_links: false, require_2fa: false,
  });
  // per-job run-in-progress flags, keyed by backend job name
  let running = $state<Record<string, boolean>>({});
  // per-user storage cache: id -> {used_mb,total_mb}
  let userStore = $state<Record<string, { used_mb: number; total_mb: number }>>({});

  async function loadAll() {
    try { stats = await api.adminStats(); } catch (e) { toast({ tone: 'error', message: `Stats: ${e}` }); }
    try { albums = await api.albums(); } catch { /* ignore */ }
    try { storage = await api.getStorage(); } catch { /* ignore */ }
    try { const cfg = await api.getSmtp(); smtp = cfg ? { ...cfg, password: '' } : null; } catch { /* ignore */ }
    try { const s = await api.getSettings(); gravatarEnabled = s.gravatar_enabled; features = s.features; } catch { /* ignore */ }
    for (const u of users) {
      api.userStorage(u.id).then((s) => (userStore = { ...userStore, [u.id]: s })).catch(() => {});
    }
  }
  loadAll();

  // ---- derived ----
  const sharedCount = $derived(albums.filter((a) => (a.shares ?? []).length > 0).length);
  const ownerName = (id: string) => users.find((u) => u.id === id)?.name ?? id;
  const ownerAvatar = (id: string) => users.find((u) => u.id === id)?.avatar_url;

  function diskPct(): number {
    if (!stats) return 0;
    const q = stats.storage.quota_mb || 1;
    return Math.min(100, Math.round((stats.storage.disk_used_mb / q) * 100));
  }
  function barColor(pct: number): string {
    if (pct >= 92) return 'var(--danger)';
    if (pct >= 75) return 'var(--warning)';
    return 'var(--accent)';
  }
  function gb(mb: number): string { return (mb / 1024).toFixed(1); }
  function gb0(mb: number): string { return (mb / 1024).toFixed(0); }

  function jobIcon(name: string): string {
    if (name.includes('backup')) return 'cloud-upload';
    if (name.includes('trash') || name.includes('purge')) return 'trash-2';
    if (name.includes('ai') || name.includes('analysis')) return 'sparkles';
    if (name.includes('duplicate')) return 'copy';
    if (name.includes('thumbnail')) return 'image';
    return 'cpu';
  }
  function jobState(status: string): string {
    if (status === 'running') return 'running';
    if (status === 'queued') return 'queued';
    return 'idle';
  }

  async function refreshStats() {
    try { stats = await api.adminStats(); } catch { /* ignore */ }
  }

  // Run a backend job synchronously, toast its outcome, and refresh gauges/history.
  async function runJob(name: string) {
    if (running[name]) return;
    running = { ...running, [name]: true };
    try {
      const run = await api.runJob(name);
      toast({ tone: run.outcome === 'failed' ? 'error' : 'success', message: `${name} — ${run.outcome}, ${run.items} items` });
      await refreshStats();
    } catch (e) {
      toast({ tone: 'error', message: String(e) });
    } finally {
      running = { ...running, [name]: false };
    }
  }

  // Toggle a single feature flag via RFC-6902 patch, then sync local state.
  async function toggleFeature(key: keyof FeatureFlags) {
    const next = !features[key];
    try {
      const s = await api.patchSettings([{ op: 'replace', path: `/features/${key}`, value: next }]);
      features = s.features;
      toast({ tone: 'success', message: 'Setting saved' });
      await refreshStats();
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }

  function fmtUptime(secs: number): string {
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    const m = Math.floor((secs % 3600) / 60);
    return d > 0 ? `${d}d ${h}h` : `${h}h ${m}m`;
  }
  function fmtDuration(ms: number): string {
    if (ms < 1000) return '<1s';
    return `${(ms / 1000).toFixed(1)}s`;
  }
  function outcomeClass(o: string): { bg: string; fg: string; icon: string } {
    if (o === 'success') return { bg: 'var(--success-soft)', fg: 'var(--success)', icon: 'circle-check' };
    if (o === 'failed') return { bg: 'var(--danger-soft)', fg: 'var(--danger)', icon: 'circle-x' };
    return { bg: 'var(--warning-soft)', fg: 'var(--warning)', icon: 'circle-alert' };
  }

  // ---- Users actions (ported from AdminModal) ----
  async function toggleAdmin(u: User & { is_admin?: boolean }) {
    try { await api.patchUser(u.id, [{ op: 'add', path: '/is_admin', value: !u.is_admin }]); await onChanged(); }
    catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function toggleDisabled(u: User & { disabled?: boolean }) {
    try { await api.patchUser(u.id, [{ op: 'add', path: '/disabled', value: !u.disabled }]); await onChanged(); }
    catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function setQuota(u: User, gbStr: string) {
    const t = gbStr.trim();
    const value = t === '' ? null : Math.max(0, Math.round(parseFloat(t) * 1024));
    if (value !== null && Number.isNaN(value)) return;
    try {
      await api.patchUser(u.id, [{ op: 'add', path: '/quota_mb', value }]);
      toast({ tone: 'success', message: `Quota updated for ${u.name}` });
      await onChanged();
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function resetPw(u: User) {
    try { await api.resetPassword(u.id); toast({ tone: 'success', icon: 'mail', message: `Reset email sent to ${u.email}` }); }
    catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function delUser(u: User) {
    try { await api.deleteUser(u.id); await onChanged(); } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }

  // user toolbar / inline editors
  let q = $state('');
  const filtered = $derived(
    users.filter((u) => {
      const s = q.trim().toLowerCase();
      if (!s) return true;
      return u.name.toLowerCase().includes(s) || u.email.toLowerCase().includes(s);
    }),
  );
  let inviting = $state(false);
  let inviteEmail = $state('');
  async function sendInvite() {
    if (!inviteEmail.trim()) return;
    try {
      await api.createInvite(inviteEmail.trim(), me.id);
      toast({ tone: 'success', icon: 'mail', message: `Invitation sent to ${inviteEmail}` });
      inviteEmail = ''; inviting = false;
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  let quotaEdit = $state<string | null>(null);
  let quotaValue = $state('');

  // ---- Albums actions ----
  async function delAlbum(a: Album) {
    try {
      await api.deleteAlbum(a.id);
      albums = await api.albums();
      await onChanged();
      toast({ tone: 'success', message: `Album “${a.name}” deleted` });
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }

  // ---- Storage / Jobs / Settings actions ----
  async function persistStorage(patch: Partial<StorageSettings>) {
    try {
      storage = await api.putStorage(patch);
      toast({ tone: 'success', message: 'Storage settings saved' });
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function toggleBackup() {
    if (!storage) return;
    const next = { ...storage.backup, enabled: !storage.backup.enabled };
    await persistStorage({ backup: next });
  }
  async function setBackupInterval(hours: string) {
    if (!storage) return;
    const h = parseFloat(hours);
    if (Number.isNaN(h) || h <= 0) return;
    await persistStorage({ backup: { ...storage.backup, interval_secs: Math.round(h * 3600) } });
  }
  async function runBackup() {
    try {
      await api.runBackup();
      toast({ tone: 'success', icon: 'cloud-upload', message: 'Backup started' });
      try { stats = await api.adminStats(); } catch { /* ignore */ }
      try { storage = await api.getStorage(); } catch { /* ignore */ }
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function toggleGravatar() {
    try {
      gravatarEnabled = (await api.setGravatar(!gravatarEnabled)).gravatar_enabled;
      toast({ tone: 'success', message: `Gravatar ${gravatarEnabled ? 'enabled' : 'disabled'}` });
      await onChanged();
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function setMode(mode: 'filesystem' | 's3_replacement') {
    if (!storage || storage.mode === mode) return;
    await persistStorage({ mode });
  }
  async function setTrashRetention(days: string) {
    const d = parseInt(days, 10);
    if (Number.isNaN(d) || d < 0) return;
    await persistStorage({ trash_retention_days: d });
  }
  // primary_s3 field editing (filesystem -> s3 forms)
  function s3Patch(field: string, value: string) {
    if (!storage) return;
    const cur = storage.primary_s3 ?? { region: '', bucket: '', access_key_id: '' };
    const next: any = { ...cur, [field]: value };
    persistStorage({ primary_s3: next });
  }

  // SMTP
  let smtpMode = $state<'stdout' | 'smtp'>('stdout');
  $effect(() => {
    if (smtp && smtp.host) smtpMode = 'smtp';
  });
  let smtpDraft = $state<SmtpConfig>({ host: '', port: 587, username: '', password: '', from: '', tls: true });
  $effect(() => {
    if (smtp) smtpDraft = { ...smtp, password: '' };
  });
  async function saveSmtp() {
    try {
      const body: any = { ...smtpDraft };
      if (!body.password) delete body.password;
      await api.putSmtp(body as SmtpConfig);
      toast({ tone: 'success', message: 'SMTP settings saved' });
      const cfg = await api.getSmtp();
      smtp = cfg ? { ...cfg, password: '' } : null;
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }

  // upload parallelism
  let uploadParallel = $state(uploadConfig.maxPerRequest);
  function setParallel(n: string) {
    const v = parseInt(n, 10);
    if (Number.isNaN(v)) return;
    setUploadMaxPerRequest(v);
    uploadParallel = uploadConfig.maxPerRequest;
  }

  // storage view derived: users with used_mb > 0 sorted desc
  const usageUsers = $derived(
    users
      .map((u) => ({ u, s: userStore[u.id] }))
      .filter((x) => x.s && x.s.used_mb > 0)
      .sort((a, b) => (b.s!.used_mb) - (a.s!.used_mb)),
  );
</script>

<div class="pna-app">
  <aside class="pna-nav">
    <div class="pna-brand">
      <span class="pna-brand-mark"><Icon name="aperture" size={18} /></span>
      <span class="pna-brand-txt">
        <span class="pna-brand-wm">Photon</span>
        <span class="pna-brand-sub">Admin</span>
      </span>
    </div>
    <div class="pna-navlist">
      {#each NAV as [v, ic] (v)}
        <button type="button" class={'pna-navitem' + (view === v ? ' is-active' : '')} onclick={() => (view = v)}>
          <Icon name={ic} size={17} />{v}
        </button>
      {/each}
    </div>
    <button type="button" class="pna-back" onclick={onClose}>
      <Icon name="arrow-left" size={16} />Back to library
    </button>
  </aside>

  <div class="pna-main">
    <header class="pna-header">
      <div>
        <h1 class="pna-h1">{view}</h1>
        <p class="pna-h-sub">{SUBS[view]}</p>
      </div>
      <div class="pna-header-right">
        <span class="pna-server-chip"><span class="pna-server-dot"></span>online</span>
        {#if me.avatar_url}
          <img class="pna-header-av" src={me.avatar_url} alt="" />
        {/if}
      </div>
    </header>

    <div class="pna-scroll">
      <div class="pna-view">
        {#if view === 'Overview'}
          <!-- ===== OVERVIEW ===== -->
          <div class="pna-stats">
            <div class="pna-stat">
              <span class="pna-stat-ic"><Icon name="users" size={20} /></span>
              <div>
                <div class="pna-stat-val">{stats?.counts.users ?? '—'}</div>
                <div class="pna-stat-lbl">Users</div>
              </div>
            </div>
            <div class="pna-stat">
              <span class="pna-stat-ic" style="background:var(--success-soft);color:var(--success)"><Icon name="image" size={20} /></span>
              <div>
                <div class="pna-stat-val">{stats?.counts.photos ?? '—'}</div>
                <div class="pna-stat-lbl">Photos</div>
              </div>
              {#if stats?.counts.archived != null}<span class="pna-stat-sub">{stats.counts.archived} archived</span>{/if}
            </div>
            <div class="pna-stat">
              <span class="pna-stat-ic" style="background:var(--warning-soft);color:var(--warning)"><Icon name="images" size={20} /></span>
              <div>
                <div class="pna-stat-val">{albums.length}</div>
                <div class="pna-stat-lbl">Albums</div>
              </div>
              {#if sharedCount > 0}<span class="pna-stat-sub">{sharedCount} shared</span>{/if}
            </div>
            <div class="pna-stat">
              <span class="pna-stat-ic"><Icon name="hard-drive" size={20} /></span>
              <div>
                <div class="pna-stat-val">{stats ? gb0(stats.storage.disk_used_mb) : '—'} GB</div>
                <div class="pna-stat-lbl">Storage</div>
              </div>
              {#if stats}<span class="pna-stat-sub">of {gb0(stats.storage.quota_mb)} GB</span>{/if}
            </div>
          </div>

          <div class="pna-cols">
            <div class="pna-card">
              <div class="pna-card-head">
                <h3><Icon name="server" size={16} />Server health</h3>
                <span class="pna-chip-ok"><Icon name="circle-check" size={12} />Healthy</span>
              </div>
              <div class="pna-health">
                {#if stats?.system}
                  {@const cpu = stats.system.cpu_percent}
                  <div class="pna-health-row">
                    <span class="pna-health-k"><Icon name="cpu" size={14} />CPU</span>
                    <span class="pna-bar"><i style={`width:${cpu}%;background:${barColor(cpu)}`}></i></span>
                    <span class="pna-health-v">{cpu}%</span>
                  </div>
                {/if}
                {#if stats?.system}
                  {@const mem = stats.system.mem_percent}
                  <div class="pna-health-row">
                    <span class="pna-health-k"><Icon name="memory-stick" size={14} />Memory</span>
                    <span class="pna-bar"><i style={`width:${mem}%;background:${barColor(mem)}`}></i></span>
                    <span class="pna-health-v">{mem}%</span>
                  </div>
                {/if}
                <div class="pna-health-row">
                  <span class="pna-health-k"><Icon name="hard-drive" size={14} />Disk</span>
                  <span class="pna-bar"><i style={`width:${diskPct()}%;background:${barColor(diskPct())}`}></i></span>
                  <span class="pna-health-v">{diskPct()}%</span>
                </div>
              </div>
              <div class="pna-meta-grid">
                <div><span class="k">Version</span><span class="v">{VERSION}</span></div>
                <div><span class="k">Storage</span><span class="v">{stats?.storage.mode ?? '—'}</span></div>
                {#if stats?.system}
                  <div><span class="k">Uptime</span><span class="v">{fmtUptime(stats.system.uptime_secs)}</span></div>
                  <div><span class="k">CPUs</span><span class="v">{stats.system.cpus}</span></div>
                {/if}
                <div><span class="k">Trashed</span><span class="v">{stats?.counts.trashed ?? 0}</span></div>
                <div><span class="k">Status</span><span class="v" style="color:var(--success)">Online</span></div>
              </div>
            </div>

            <div class="pna-card">
              <div class="pna-card-head">
                <h3><Icon name="cpu" size={16} />Background jobs</h3>
                <button type="button" class="pna-link" onclick={() => (view = 'Jobs')}>Manage<Icon name="chevron-right" size={14} /></button>
              </div>
              <div class="pna-joblist">
                {#each (stats?.jobs ?? []).slice(0, 4) as jb (jb.name)}
                  <div class="pna-jobmini">
                    <span class="pna-jobmini-ic"><Icon name={jobIcon(jb.name)} size={15} /></span>
                    <div class="pna-jobmini-body">
                      <div class="pna-jobmini-top">
                        <span>{jb.name}</span>
                        <span class={'pna-jobstate ' + jobState(jb.status)}>{jb.status}</span>
                      </div>
                      <span class="pna-bar">
                        <i style={jb.status === 'running' ? 'width:100%;background:var(--accent)' : 'width:30%;background:var(--text-faint)'}></i>
                      </span>
                    </div>
                    {#if jb.last_result}<span class="pna-jobmini-pct" title={jb.last_result}>{jb.last_result.slice(0, 6)}</span>{/if}
                  </div>
                {/each}
              </div>
            </div>
          </div>

        {:else if view === 'Users'}
          <!-- ===== USERS ===== -->
          <div class="pna-toolbar">
            <div class="pna-search">
              <Icon name="search" size={15} />
              <input placeholder="Search users…" bind:value={q} />
            </div>
            <span class="pna-muted">{filtered.length} users</span>
            <div style="margin-left:auto;display:flex;align-items:center;gap:8px">
              {#if inviting}
                <input class="pna-input" style="min-width:220px" type="email" placeholder="email@example.com" bind:value={inviteEmail} />
                <button type="button" class="pna-btn pna-btn-primary" onclick={sendInvite}><Icon name="send" size={15} />Send</button>
                <button type="button" class="pna-btn pna-btn-outline" onclick={() => { inviting = false; inviteEmail = ''; }}>Cancel</button>
              {:else}
                <button type="button" class="pna-btn pna-btn-primary" onclick={() => (inviting = true)}><Icon name="user-plus" size={15} />Invite user</button>
              {/if}
            </div>
          </div>

          <div class="pna-table">
            <div class="pna-tr pna-th">
              <span>User</span><span>Role</span><span>Status</span><span>Storage</span><span>Photos</span><span>Last seen</span><span></span>
            </div>
            {#each filtered as u (u.id)}
              {@const uu = u as User & { is_admin?: boolean; disabled?: boolean }}
              {@const s = userStore[u.id]}
              {@const total = (u.quota_mb ?? s?.total_mb) || 0}
              <div class="pna-tr">
                <div class="pna-c-user">
                  {#if u.avatar_url}<img src={u.avatar_url} alt="" />{/if}
                  <div class="pna-user-id">
                    <span class="pna-user-name">{u.name}</span>
                    <span class="pna-user-email pk-mono">{u.email}</span>
                  </div>
                </div>
                <span class={'pna-role' + (uu.is_admin ? ' is-admin' : '')}>
                  <Icon name={uu.is_admin ? 'shield-check' : 'shield'} size={14} />{uu.is_admin ? 'Admin' : 'User'}
                </span>
                <span>
                  {#if uu.disabled}<span class="pna-pill pna-pill-warn">Suspended</span>{:else}<span class="pna-pill pna-pill-ok">Active</span>{/if}
                </span>
                <div class="pna-c-storage">
                  <span class="pna-bar"><i style={`width:${total ? Math.min(100, Math.round(((s?.used_mb ?? 0) / total) * 100)) : 0}%;background:var(--accent)`}></i></span>
                  <span class="pna-storage-v pk-mono">{s ? gb(s.used_mb) : '0.0'}/{u.quota_mb != null ? gb0(u.quota_mb) : '∞'}</span>
                </div>
                <span class="pk-mono">—</span>
                <span class="pna-muted">—</span>
                <div class="pna-c-act pna-rowacts" style="display:flex">
                  {#if quotaEdit === u.id}
                    <input
                      class="pna-input" style="min-width:90px;height:30px" type="number" min="0" placeholder="GB"
                      value={quotaValue}
                      onkeydown={(e) => { if (e.key === 'Enter') { setQuota(u, (e.currentTarget as HTMLInputElement).value); quotaEdit = null; } else if (e.key === 'Escape') quotaEdit = null; }}
                    />
                  {:else}
                    <button type="button" class={'pna-icon-btn' + (uu.is_admin ? ' is-on' : '')} title="Toggle admin" onclick={() => toggleAdmin(uu)}><Icon name="shield" size={15} /></button>
                    <button type="button" class="pna-icon-btn" title="Send reset email" onclick={() => resetPw(u)}><Icon name="key-round" size={15} /></button>
                    <button type="button" class="pna-icon-btn" title="Set quota" onclick={() => { quotaEdit = u.id; quotaValue = u.quota_mb != null ? gb0(u.quota_mb) : ''; }}><Icon name="hard-drive" size={15} /></button>
                    <button type="button" class={'pna-icon-btn' + (uu.disabled ? ' is-on' : '')} title={uu.disabled ? 'Re-enable' : 'Suspend'} onclick={() => toggleDisabled(uu)}><Icon name={uu.disabled ? 'circle-check' : 'ban'} size={15} /></button>
                    {#if u.id !== me.id}
                      <span class="pna-act-sep"></span>
                      <button type="button" class="pna-icon-btn danger" title="Delete user" onclick={() => delUser(u)}><Icon name="trash-2" size={15} /></button>
                    {/if}
                  {/if}
                </div>
              </div>
            {/each}
          </div>

        {:else if view === 'Albums'}
          <!-- ===== ALBUMS ===== -->
          <div class="pna-toolbar">
            <span class="pna-muted">{albums.length} albums · {sharedCount} shared</span>
          </div>
          <div class="pna-table pna-altable">
            <div class="pna-altr pna-th">
              <span>Album</span><span>Owner</span><span class="num">Items</span><span>Visibility</span><span class="num">Size</span><span>Updated</span><span></span>
            </div>
            {#each albums as a (a.id)}
              {@const shares = a.shares ?? []}
              <div class="pna-altr">
                <div class="pna-al-name"><Icon name="folder" size={16} /><b>{a.name}</b></div>
                <div class="pna-al-owner">
                  {#if ownerAvatar(a.owner_id)}<img src={ownerAvatar(a.owner_id)} alt="" />{/if}
                  <span>{ownerName(a.owner_id)}</span>
                </div>
                <span class="num pk-mono">{a.photo_ids.length}</span>
                <span>
                  {#if shares.length > 0}
                    <span class="pna-pill pna-pill-info" style="display:inline-flex;align-items:center;gap:4px"><Icon name="users" size={12} />{shares.length}</span>
                  {:else}
                    <span class="pna-al-private"><Icon name="lock" size={13} />Private</span>
                  {/if}
                </span>
                <span class="num">—</span>
                <span class="pna-muted">—</span>
                <div class="pna-c-act pna-rowacts" style="display:flex">
                  <button type="button" class="pna-icon-btn" title="Open album" onclick={() => onOpenAlbum?.(a.id)}><Icon name="external-link" size={15} /></button>
                  <span class="pna-act-sep"></span>
                  <button type="button" class="pna-icon-btn danger" title="Delete album" onclick={() => delAlbum(a)}><Icon name="trash-2" size={15} /></button>
                </div>
              </div>
            {/each}
          </div>

        {:else if view === 'Storage'}
          <!-- ===== STORAGE ===== -->
          {@const disk = stats?.storage.disk_used_mb ?? 0}
          {@const s3 = stats?.storage.s3_used_mb ?? 0}
          {@const quota = stats?.storage.quota_mb ?? 0}
          {@const free = Math.max(0, quota - disk - s3)}
          <div class="pna-card">
            <div class="pna-card-head">
              <h3><Icon name="hard-drive" size={16} />Disk usage</h3>
              <span class="pna-muted pk-mono" style="margin-left:auto">{gb(disk + s3)} GB / {gb0(quota)} GB</span>
            </div>
            <div class="pna-stack">
              <span style={`width:${quota ? (disk / quota) * 100 : 0}%;background:var(--accent)`}></span>
              <span style={`width:${quota ? (s3 / quota) * 100 : 0}%;background:var(--success)`}></span>
              <span style={`width:${quota ? (free / quota) * 100 : 100}%;background:var(--surface-active)`}></span>
            </div>
            <div class="pna-legend">
              <span class="pna-legend-it"><span class="pna-dot" style="background:var(--accent)"></span>Disk <b>{gb(disk)} GB</b></span>
              <span class="pna-legend-it"><span class="pna-dot" style="background:var(--success)"></span>S3 <b>{gb(s3)} GB</b></span>
              <span class="pna-legend-it"><span class="pna-dot" style="background:var(--surface-active)"></span>Free <b>{gb(free)} GB</b></span>
            </div>
          </div>

          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="users" size={16} />Usage by user</h3></div>
            <div class="pna-userbars">
              {#each usageUsers as { u, s } (u.id)}
                {@const total = (u.quota_mb ?? s!.total_mb) || 0}
                <div class="pna-userbar">
                  {#if u.avatar_url}<img src={u.avatar_url} alt="" />{/if}
                  <span class="pna-userbar-name">{u.name}</span>
                  <span class="pna-bar"><i style={`width:${total ? Math.min(100, Math.round((s!.used_mb / total) * 100)) : 0}%;background:var(--accent)`}></i></span>
                  <span class="pna-userbar-v pk-mono">{gb(s!.used_mb)} / {u.quota_mb != null ? gb0(u.quota_mb) : '∞'} GB</span>
                </div>
              {/each}
            </div>
          </div>

        {:else if view === 'Jobs'}
          <!-- ===== JOBS ===== -->
          {@const jobs = stats?.jobs ?? []}
          {@const runningCount = jobs.filter((j) => j.status === 'running').length}
          <div class="pna-toolbar">
            <span class="pna-muted">{runningCount} running · {jobs.length} total</span>
          </div>
          <div class="pna-jobgrid">
            {#each jobs as jb (jb.name)}
              <div class="pna-jobcard">
                <div class="pna-jobcard-top">
                  <span class="pna-jobcard-ic"><Icon name={jobIcon(jb.name)} size={17} /></span>
                  <span class="pna-jobcard-name">{jb.name}</span>
                  <span class={'pna-jobstate ' + jobState(jb.status)}>{jb.status}</span>
                </div>
                <span class="pna-bar">
                  <i style={jb.status === 'running' ? 'width:100%;background:var(--accent)' : 'width:30%;background:var(--text-faint)'}></i>
                </span>
                <div class="pna-jobcard-foot">
                  <span class="pna-muted">{jb.last_result ?? '—'}{jb.last_run_at ? ` · ${jb.last_run_at}` : ''}</span>
                  <button type="button" class="pna-icon-btn" title={`Run ${jb.name}`} disabled={running[jb.name]} onclick={() => runJob(jb.name)}>
                    {#if running[jb.name]}<Icon name="loader-2" size={15} spin />{:else}<Icon name="play" size={15} />{/if}
                  </button>
                </div>
              </div>
            {/each}
          </div>

          {#if storage}
            <div class="pna-card">
              <div class="pna-card-head"><h3><Icon name="cloud-upload" size={16} />S3 backup</h3></div>
              <div class="pna-field">
                <div class="pna-field-txt">
                  <span class="pna-field-lbl">Hourly S3 backup</span>
                  <span class="pna-field-hint">Periodically copy originals to the backup bucket.</span>
                </div>
                <div class="pna-field-ctl">
                  <button type="button" class={'pna-toggle' + (storage.backup.enabled ? ' is-on' : '')} aria-label="Toggle backup" onclick={toggleBackup}><span class="pna-toggle-knob"></span></button>
                </div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Interval</span><span class="pna-field-hint">Hours between automatic backups.</span></div>
                <div class="pna-field-ctl">
                  <input class="pna-input" type="number" min="1" style="min-width:120px"
                    value={(storage.backup.interval_secs / 3600).toString()}
                    onchange={(e) => setBackupInterval((e.currentTarget as HTMLInputElement).value)} />
                </div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt">
                  <span class="pna-field-lbl">Last backup</span>
                  <span class="pna-field-hint">{storage.backup.last_backup_at ?? 'never'} · {storage.backup.last_backup_count} files</span>
                </div>
                <div class="pna-field-ctl">
                  <button type="button" class="pna-btn pna-btn-outline" onclick={runBackup}><Icon name="cloud-upload" size={15} />Run backup now</button>
                </div>
              </div>
            </div>
          {/if}

          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="wrench" size={16} />Maintenance</h3></div>
            <p class="pna-muted">Re-process the library — runs as a background job</p>
            <div class="pna-maint">
              {#each [['Rebuild all thumbnails', 'Regenerate previews for every photo', 'image', 'rebuild_thumbnails', 'Rebuild'], ['Re-run face recognition', 'Re-detect and re-cluster people', 'scan-face', 'recluster_faces', 'Re-run'], ['Re-extract metadata', 'Re-read EXIF for the whole library', 'aperture', 'reextract_metadata', 'Re-extract']] as [label, hint, ic, jobName, cta] (jobName)}
                <div class="pna-maint-row">
                  <span class="pna-maint-ic"><Icon name={ic} size={17} /></span>
                  <div class="pna-maint-txt">
                    <span class="pna-field-lbl">{label}</span>
                    <span class="pna-field-hint">{hint}</span>
                  </div>
                  <button type="button" class="pna-btn pna-btn-outline" disabled={running[jobName]} onclick={() => runJob(jobName)}>
                    {#if running[jobName]}<Icon name="loader-2" size={15} spin />{:else}<Icon name="play" size={15} />{/if}{cta}
                  </button>
                </div>
              {/each}
            </div>
          </div>

          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="history" size={16} />Run history</h3></div>
            <div class="pna-histtable">
              <div class="pna-histhead">
                <span>Job</span><span>Outcome</span><span>Items</span><span>Started</span><span>Duration</span><span>Trigger</span>
              </div>
              {#each stats?.history ?? [] as run (run.started_at + run.name)}
                {@const oc = outcomeClass(run.outcome)}
                <div class="pna-histrow">
                  <div class="pna-histjob">
                    <span class="pna-histjob-ic"><Icon name={jobIcon(run.name)} size={15} /></span>
                    <div class="pna-histjob-txt"><span class="pna-histjob-name">{run.name}</span></div>
                  </div>
                  <span class="pna-histpill" style={`background:${oc.bg};color:${oc.fg}`}><Icon name={oc.icon} size={12} />{run.outcome}</span>
                  <span class="pk-mono">{run.items}</span>
                  <span class="pk-mono">{run.started_at.slice(11, 19)}</span>
                  <span class="pk-mono">{fmtDuration(run.duration_ms)}</span>
                  <span class="pna-histtrig">{run.trigger}</span>
                </div>
              {/each}
              {#if (stats?.history ?? []).length === 0}
                <div class="pna-hist-empty">No runs yet.</div>
              {/if}
            </div>
          </div>

        {:else if view === 'Settings'}
          <!-- ===== SETTINGS ===== -->
          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="settings" size={16} />General</h3></div>
            <div class="pna-field">
              <div class="pna-field-txt">
                <span class="pna-field-lbl">Use Gravatar</span>
                <span class="pna-field-hint">Show each user's email-based Gravatar instead of the placeholder.</span>
              </div>
              <div class="pna-field-ctl">
                <button type="button" class={'pna-toggle' + (gravatarEnabled ? ' is-on' : '')} aria-label="Toggle Gravatar" onclick={toggleGravatar}><span class="pna-toggle-knob"></span></button>
              </div>
            </div>
          </div>

          <div class="pna-card">
            <div class="pna-card-head">
              <h3><Icon name="sparkles" size={16} />Machine learning</h3>
              <span class="pna-chip-ok">On-device</span>
            </div>
            {#each [['faces', 'Face recognition', 'Detect & group people locally'], ['clip', 'Smart search (CLIP)', 'Natural-language photo search'], ['ocr', 'OCR text extraction', 'Index text inside images'], ['geocode', 'Reverse geocoding', 'Resolve place names from GPS']] as [key, label, hint] (key)}
              <div class="pna-field">
                <div class="pna-field-txt">
                  <span class="pna-field-lbl">{label}</span>
                  <span class="pna-field-hint">{hint}</span>
                </div>
                <div class="pna-field-ctl">
                  <button type="button" class={'pna-toggle' + (features[key as keyof FeatureFlags] ? ' is-on' : '')} aria-label={`Toggle ${label}`} onclick={() => toggleFeature(key as keyof FeatureFlags)}><span class="pna-toggle-knob"></span></button>
                </div>
              </div>
            {/each}
          </div>

          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="shield-check" size={16} />Security & media</h3></div>
            {#each [['require_2fa', 'Require 2-factor auth', 'For all accounts'], ['transcode', 'Video transcoding', 'Generate streamable versions'], ['public_links', 'Allow public links', 'Share albums without an account'], ['public_signup', 'Allow public sign-up', 'Anyone with the URL can register']] as [key, label, hint] (key)}
              <div class="pna-field">
                <div class="pna-field-txt">
                  <span class="pna-field-lbl">{label}</span>
                  <span class="pna-field-hint">{hint}</span>
                </div>
                <div class="pna-field-ctl">
                  <button type="button" class={'pna-toggle' + (features[key as keyof FeatureFlags] ? ' is-on' : '')} aria-label={`Toggle ${label}`} onclick={() => toggleFeature(key as keyof FeatureFlags)}><span class="pna-toggle-knob"></span></button>
                </div>
              </div>
            {/each}
          </div>

          {#if storage}
            <div class="pna-card">
              <div class="pna-card-head"><h3><Icon name="hard-drive" size={16} />Storage</h3></div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Primary store</span><span class="pna-field-hint">Where originals are stored.</span></div>
                <div class="pna-field-ctl">
                  <div class="pna-segmented" style="width:auto">
                    <button type="button" class={storage.mode === 'filesystem' ? 'is-on' : ''} style="width:auto;padding:0 12px" onclick={() => setMode('filesystem')}>Filesystem</button>
                    <button type="button" class={storage.mode === 's3_replacement' ? 'is-on' : ''} style="width:auto;padding:0 12px" onclick={() => setMode('s3_replacement')}>S3</button>
                  </div>
                </div>
              </div>
              {#if storage.mode === 's3_replacement'}
                {@const s3c = storage.primary_s3 ?? { region: '', bucket: '', access_key_id: '' }}
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Bucket</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" value={s3c.bucket ?? ''} onchange={(e) => s3Patch('bucket', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Region</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" value={s3c.region ?? ''} onchange={(e) => s3Patch('region', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Endpoint</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" value={s3c.endpoint ?? ''} onchange={(e) => s3Patch('endpoint', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Prefix</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" value={s3c.prefix ?? ''} onchange={(e) => s3Patch('prefix', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Access key</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" value={s3c.access_key_id ?? ''} onchange={(e) => s3Patch('access_key_id', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
                <div class="pna-field">
                  <div class="pna-field-txt"><span class="pna-field-lbl">Secret key</span><span class="pna-field-hint">Blank = unchanged.</span></div>
                  <div class="pna-field-ctl"><input class="pna-input pk-mono" type="password" placeholder="••••••" onchange={(e) => s3Patch('secret_access_key', (e.currentTarget as HTMLInputElement).value)} /></div>
                </div>
              {:else}
                <div class="pna-note"><Icon name="info" size={15} />Originals are stored on the server filesystem; metadata always lives in Postgres.</div>
              {/if}
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Trash retention</span><span class="pna-field-hint">Days before trashed items are purged.</span></div>
                <div class="pna-field-ctl"><input class="pna-input" type="number" min="0" style="min-width:120px" value={storage.trash_retention_days.toString()} onchange={(e) => setTrashRetention((e.currentTarget as HTMLInputElement).value)} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Upload parallelism</span><span class="pna-field-hint">Max files uploaded at once (client).</span></div>
                <div class="pna-field-ctl"><input class="pna-input" type="number" min="1" style="min-width:120px" value={uploadParallel.toString()} onchange={(e) => setParallel((e.currentTarget as HTMLInputElement).value)} /></div>
              </div>
            </div>
          {/if}

          <div class="pna-card">
            <div class="pna-card-head"><h3><Icon name="mail" size={16} />Notifications</h3></div>
            <div class="pna-field">
              <div class="pna-field-txt"><span class="pna-field-lbl">Delivery method</span><span class="pna-field-hint">How outgoing e-mail is delivered.</span></div>
              <div class="pna-field-ctl">
                <div class="pna-segmented" style="width:auto">
                  <button type="button" class={smtpMode === 'stdout' ? 'is-on' : ''} style="width:auto;padding:0 12px" onclick={() => (smtpMode = 'stdout')}>stdout</button>
                  <button type="button" class={smtpMode === 'smtp' ? 'is-on' : ''} style="width:auto;padding:0 12px" onclick={() => (smtpMode = 'smtp')}>SMTP</button>
                </div>
              </div>
            </div>
            {#if smtpMode === 'stdout'}
              <div class="pna-note"><Icon name="info" size={15} />E-mails are written to the server log instead of being sent.</div>
            {:else}
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Host</span></div>
                <div class="pna-field-ctl"><input class="pna-input pk-mono" placeholder="smtp.example.com" bind:value={smtpDraft.host} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Port</span></div>
                <div class="pna-field-ctl"><input class="pna-input" type="number" style="min-width:120px" bind:value={smtpDraft.port} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Username</span></div>
                <div class="pna-field-ctl"><input class="pna-input pk-mono" bind:value={smtpDraft.username} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Password</span><span class="pna-field-hint">Blank = unchanged.</span></div>
                <div class="pna-field-ctl"><input class="pna-input pk-mono" type="password" placeholder="••••••" bind:value={smtpDraft.password} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">From</span></div>
                <div class="pna-field-ctl"><input class="pna-input pk-mono" placeholder="Photon <noreply@example.com>" bind:value={smtpDraft.from} /></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"><span class="pna-field-lbl">Use TLS</span></div>
                <div class="pna-field-ctl"><button type="button" class={'pna-toggle' + (smtpDraft.tls ? ' is-on' : '')} aria-label="Toggle TLS" onclick={() => (smtpDraft.tls = !smtpDraft.tls)}><span class="pna-toggle-knob"></span></button></div>
              </div>
              <div class="pna-field">
                <div class="pna-field-txt"></div>
                <div class="pna-field-ctl"><button type="button" class="pna-btn pna-btn-primary" onclick={saveSmtp}><Icon name="send" size={15} />Save SMTP</button></div>
              </div>
            {/if}
          </div>
        {/if}
      </div>
    </div>
  </div>
</div>

<style>
  /* Photon Admin Console — styles (consumes design-system tokens) */
  .pna-app, .pna-app * { box-sizing: border-box; }
  .pna-app {
    position: fixed; inset: 0; z-index: 70; display: grid; grid-template-columns: 232px 1fr;
    font-family: var(--font-sans); color: var(--text); background: var(--bg-base); font-size: var(--text-sm);
  }
  .pna-app button { font: inherit; color: inherit; border: 0; background: none; cursor: pointer; }
  .pna-muted { color: var(--text-faint); font-size: var(--text-xs); }

  /* ---- nav ---- */
  .pna-nav { background: var(--surface); border-right: 1px solid var(--border); display: flex; flex-direction: column; padding: 16px 12px 14px; }
  .pna-brand { display: flex; align-items: center; gap: 10px; padding: 4px 8px 20px; }
  .pna-brand-mark { width: 30px; height: 30px; border-radius: var(--radius-md); background: var(--accent); color: #fff; display: grid; place-items: center; }
  .pna-brand-txt { display: flex; flex-direction: column; line-height: 1.1; }
  .pna-brand-wm { font-family: var(--font-display); font-weight: var(--fw-bold); font-size: var(--text-base); letter-spacing: var(--ls-tight); }
  .pna-brand-sub { font-size: 10px; font-weight: var(--fw-semibold); letter-spacing: var(--ls-caps); text-transform: uppercase; color: var(--accent-text); }
  .pna-navlist { display: flex; flex-direction: column; gap: 2px; }
  .pna-navitem { display: flex; align-items: center; gap: 11px; padding: 9px 10px; border-radius: var(--radius-md); color: var(--text-muted); font-weight: var(--fw-medium); text-align: left; transition: background var(--dur-fast) var(--ease-out), color var(--dur-fast) var(--ease-out); }
  .pna-navitem :global(.pk-ic) { color: var(--text-faint); }
  .pna-navitem:hover { background: var(--surface-hover); color: var(--text); }
  .pna-navitem.is-active { background: var(--accent-soft); color: var(--accent-text); }
  .pna-navitem.is-active :global(.pk-ic) { color: var(--accent); }
  .pna-back { margin-top: auto; display: flex; align-items: center; gap: 9px; padding: 10px; border-radius: var(--radius-md); color: var(--text-muted); font-weight: var(--fw-medium); text-decoration: none; border: 1px solid var(--border); }
  .pna-back:hover { background: var(--surface-hover); color: var(--text); }

  /* ---- header ---- */
  .pna-main { display: flex; flex-direction: column; min-width: 0; min-height: 0; }
  .pna-header { display: flex; align-items: center; gap: 16px; padding: 18px 28px; border-bottom: 1px solid var(--border); background: var(--surface); flex: none; }
  .pna-h1 { font-family: var(--font-display); font-size: var(--text-xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); margin: 0; }
  .pna-h-sub { font-size: var(--text-xs); color: var(--text-muted); margin: 2px 0 0; }
  .pna-header-right { margin-left: auto; display: flex; align-items: center; gap: 12px; }
  .pna-server-chip { display: inline-flex; align-items: center; gap: 7px; height: 30px; padding: 0 12px; border-radius: var(--radius-pill); font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text-muted); background: var(--bg-subtle); border: 1px solid var(--border); }
  .pna-server-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--success); box-shadow: 0 0 0 3px var(--success-soft); }
  .pna-header-av { width: 32px; height: 32px; border-radius: var(--radius-pill); object-fit: cover; border: 1px solid var(--border); }

  .pna-scroll { flex: 1; overflow-y: auto; padding: 24px 28px 40px; }
  .pna-view { display: flex; flex-direction: column; gap: 18px; max-width: 1180px; }

  /* ---- cards / generic ---- */
  .pna-card { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius-xl); padding: 18px 20px; }
  .pna-card-head { display: flex; align-items: center; gap: 10px; margin-bottom: 16px; }
  .pna-card-head h3 { display: flex; align-items: center; gap: 8px; font-size: var(--text-md); font-weight: var(--fw-semibold); margin: 0; letter-spacing: var(--ls-tight); }
  .pna-card-head h3 :global(.pk-ic) { color: var(--accent); }
  .pna-cols { display: grid; grid-template-columns: 1fr 1fr; gap: 18px; }
  .pna-link { margin-left: auto; display: inline-flex; align-items: center; gap: 3px; font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--accent-text); }
  .pna-link:hover { color: var(--accent); }
  .pna-chip-ok { display: inline-flex; align-items: center; gap: 5px; font-size: var(--text-2xs); font-weight: var(--fw-semibold); color: var(--success); background: var(--success-soft); padding: 3px 8px; border-radius: var(--radius-pill); }

  /* bars */
  .pna-bar { flex: 1; height: 6px; border-radius: var(--radius-pill); background: var(--surface-active); overflow: hidden; }
  .pna-bar > i { display: block; height: 100%; border-radius: var(--radius-pill); transition: width var(--dur-base) var(--ease-out); }

  /* buttons */
  .pna-btn { display: inline-flex; align-items: center; gap: 7px; height: 34px; padding: 0 14px; border-radius: var(--radius-md); font-size: var(--text-sm); font-weight: var(--fw-medium); transition: background var(--dur-fast) var(--ease-out), border-color var(--dur-fast) var(--ease-out); }
  .pna-btn-primary { background: var(--accent); color: #fff; }
  .pna-btn-primary:hover { background: var(--accent-hover); }
  .pna-btn-outline { border: 1px solid var(--border); color: var(--text); background: var(--surface); }
  .pna-btn-outline:hover { background: var(--surface-hover); }
  .pna-btn-danger { background: var(--danger-soft); color: var(--danger); }
  .pna-btn-danger:hover { background: var(--danger); color: #fff; }
  .pna-icon-btn { width: 32px; height: 32px; display: inline-grid; place-items: center; border-radius: var(--radius-md); color: var(--text-muted); }
  .pna-icon-btn:hover { background: var(--surface-hover); color: var(--text); }
  .pna-icon-btn.lg { width: 36px; height: 36px; }

  /* ---- stat cards ---- */
  .pna-stats { display: grid; grid-template-columns: repeat(4, 1fr); gap: 14px; }
  .pna-stat { position: relative; background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius-lg); padding: 16px; display: flex; align-items: center; gap: 13px; }
  .pna-stat-ic { width: 40px; height: 40px; flex: none; border-radius: var(--radius-md); display: grid; place-items: center; background: var(--accent-soft); color: var(--accent-text); }
  .pna-stat-val { font-size: var(--text-xl); font-weight: var(--fw-bold); letter-spacing: var(--ls-tight); line-height: 1.1; }
  .pna-stat-lbl { font-size: var(--text-xs); color: var(--text-muted); margin-top: 1px; }
  .pna-stat-sub { position: absolute; top: 14px; right: 14px; font-size: 10px; color: var(--text-faint); }

  /* ---- health ---- */
  .pna-health { display: flex; flex-direction: column; gap: 11px; margin-bottom: 16px; }
  .pna-health-row { display: flex; align-items: center; gap: 12px; }
  .pna-health-k { display: inline-flex; align-items: center; gap: 7px; width: 96px; flex: none; font-size: var(--text-xs); color: var(--text-muted); }
  .pna-health-k :global(.pk-ic) { color: var(--text-faint); }
  .pna-health-v { width: 42px; text-align: right; font-size: var(--text-xs); color: var(--text); }
  .pna-meta-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 10px 18px; padding-top: 16px; border-top: 1px solid var(--border-faint); }
  .pna-meta-grid > div { display: flex; flex-direction: column; gap: 2px; }
  .pna-meta-grid .k { font-size: 10px; text-transform: uppercase; letter-spacing: var(--ls-caps); color: var(--text-faint); font-weight: var(--fw-semibold); }
  .pna-meta-grid .v { font-size: var(--text-sm); font-weight: var(--fw-medium); }

  /* ---- jobs ---- */
  .pna-joblist { display: flex; flex-direction: column; gap: 14px; }
  .pna-jobmini { display: flex; align-items: center; gap: 11px; }
  .pna-jobmini-ic { width: 30px; height: 30px; flex: none; border-radius: var(--radius-md); display: grid; place-items: center; background: var(--bg-subtle); color: var(--text-muted); }
  .pna-jobmini-body { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 6px; }
  .pna-jobmini-top { display: flex; justify-content: space-between; font-size: var(--text-xs); font-weight: var(--fw-medium); }
  .pna-jobmini-pct { font-size: var(--text-xs); color: var(--text-muted); width: 36px; text-align: right; }
  .pna-jobstate { font-size: 10px; font-weight: var(--fw-semibold); text-transform: uppercase; letter-spacing: .04em; padding: 2px 6px; border-radius: var(--radius-xs); }
  .pna-jobstate.running { color: var(--accent-text); background: var(--accent-soft); }
  .pna-jobstate.queued { color: var(--warning); background: var(--warning-soft); }
  .pna-jobstate.idle { color: var(--text-faint); background: var(--bg-subtle); }
  .pna-jobgrid { display: grid; grid-template-columns: repeat(2, 1fr); gap: 14px; }
  .pna-jobcard { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius-lg); padding: 15px 16px; display: flex; flex-direction: column; gap: 12px; }
  .pna-jobcard-top { display: flex; align-items: center; gap: 10px; }
  .pna-jobcard-ic { width: 32px; height: 32px; flex: none; border-radius: var(--radius-md); display: grid; place-items: center; background: var(--accent-soft); color: var(--accent-text); }
  .pna-jobcard-name { font-weight: var(--fw-semibold); font-size: var(--text-sm); flex: 1; }
  .pna-jobcard-foot { display: flex; align-items: center; justify-content: space-between; }

  /* ---- run history ---- */
  .pna-hist-filters { display: flex; gap: 6px; }
  .pna-histtable { display: flex; flex-direction: column; }
  .pna-histhead, .pna-histrow { display: grid; grid-template-columns: minmax(0,2.3fr) minmax(0,1fr) minmax(0,.8fr) minmax(0,1.2fr) minmax(0,1fr) minmax(0,.9fr); align-items: center; gap: 12px; }
  .pna-histhead { padding: 0 4px 10px; font-size: 10px; font-weight: var(--fw-semibold); text-transform: uppercase; letter-spacing: .05em; color: var(--text-faint); border-bottom: 1px solid var(--border); }
  .pna-histrow { padding: 11px 4px; border-bottom: 1px solid var(--border-faint); font-size: var(--text-sm); }
  .pna-histrow:last-child { border-bottom: 0; }
  .pna-histrow:hover { background: var(--surface-hover); }
  .pna-histjob { display: flex; align-items: center; gap: 10px; min-width: 0; }
  .pna-histjob-ic { width: 30px; height: 30px; flex: none; border-radius: var(--radius-md); display: grid; place-items: center; background: var(--bg-subtle); color: var(--text-muted); }
  .pna-histjob-txt { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
  .pna-histjob-name { font-weight: var(--fw-medium); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-histjob-note { font-size: 11px; color: var(--text-faint); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-histpill { justify-self: start; display: inline-flex; align-items: center; gap: 5px; padding: 3px 9px; border-radius: var(--radius-pill); font-size: var(--text-xs); font-weight: var(--fw-semibold); }
  .pna-histtrig { font-size: var(--text-xs); color: var(--text-muted); }
  .pna-hist-empty { padding: 24px; text-align: center; color: var(--text-faint); font-size: var(--text-sm); }

  /* ---- toolbar / search ---- */
  .pna-toolbar { display: flex; align-items: center; gap: 12px; }
  .pna-search { display: flex; align-items: center; gap: 8px; height: 34px; padding: 0 12px; width: 280px; border-radius: var(--radius-md); background: var(--surface); border: 1px solid var(--border); }
  .pna-search :global(.pk-ic) { color: var(--text-faint); }
  .pna-search input { flex: 1; background: none; border: 0; outline: none; color: var(--text); font-size: var(--text-sm); }
  .pna-segmented { display: inline-flex; background: var(--bg-subtle); border: 1px solid var(--border-faint); border-radius: var(--radius-md); padding: 2px; gap: 2px; }
  .pna-segmented button { width: 32px; height: 26px; display: grid; place-items: center; border-radius: var(--radius-sm); color: var(--text-faint); }
  .pna-segmented button.is-on { background: var(--surface); color: var(--accent-text); box-shadow: var(--shadow-xs); }

  /* ---- table ---- */
  .pna-table { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius-lg); overflow: visible; }
  .pna-tr { display: grid; grid-template-columns: 2.1fr .8fr .95fr 1.25fr .6fr .85fr 218px; align-items: center; gap: 12px; padding: 10px 16px; border-bottom: 1px solid var(--border-faint); }
  .pna-tr:last-child { border-bottom: 0; }
  .pna-th { font-size: 10px; text-transform: uppercase; letter-spacing: var(--ls-caps); color: var(--text-faint); font-weight: var(--fw-semibold); padding-top: 12px; padding-bottom: 12px; }
  .pna-tr:not(.pna-th):hover { background: var(--surface-hover); }
  .pna-c-user { display: flex; align-items: center; gap: 11px; min-width: 0; }
  .pna-c-user img { width: 34px; height: 34px; border-radius: var(--radius-pill); object-fit: cover; flex: none; }
  .pna-user-id { display: flex; flex-direction: column; min-width: 0; }
  .pna-user-name { font-weight: var(--fw-medium); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-user-email { font-size: 11px; color: var(--text-faint); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-role { display: inline-flex; align-items: center; gap: 5px; font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text-muted); }
  .pna-role.is-admin { color: var(--accent-text); }
  .pna-pill { font-size: var(--text-2xs); font-weight: var(--fw-semibold); padding: 3px 9px; border-radius: var(--radius-pill); }
  .pna-pill-ok { color: var(--success); background: var(--success-soft); }
  .pna-pill-info { color: var(--accent-text); background: var(--accent-soft); }
  .pna-pill-warn { color: var(--warning); background: var(--warning-soft); }
  .pna-c-storage { display: flex; align-items: center; gap: 9px; }
  .pna-storage-v { font-size: 10px; color: var(--text-faint); flex: none; }
  .pna-c-act { position: relative; display: flex; justify-content: flex-end; }
  .pna-rowacts { align-items: center; gap: 1px; }
  .pna-rowacts .pna-icon-btn { width: 30px; height: 30px; opacity: .55; transition: opacity var(--dur-fast) var(--ease-out), background var(--dur-fast) var(--ease-out), color var(--dur-fast) var(--ease-out); }
  .pna-tr:hover .pna-rowacts .pna-icon-btn { opacity: 1; }
  .pna-rowacts .pna-icon-btn.is-on { color: var(--success); opacity: 1; }
  .pna-rowacts .pna-icon-btn.danger:hover { background: var(--danger-soft); color: var(--danger); }
  .pna-act-sep { width: 1px; height: 18px; background: var(--border); margin: 0 4px; }

  /* storage */
  .pna-stack { display: flex; height: 30px; border-radius: var(--radius-md); overflow: hidden; gap: 2px; margin-bottom: 16px; }
  .pna-stack > span { transition: width var(--dur-base) var(--ease-out); }
  .pna-legend { display: flex; flex-wrap: wrap; gap: 16px; }
  .pna-legend-it { display: inline-flex; align-items: center; gap: 7px; font-size: var(--text-xs); color: var(--text-muted); }
  .pna-legend-it b { color: var(--text); font-weight: var(--fw-semibold); }
  .pna-dot { width: 10px; height: 10px; border-radius: 3px; }
  .pna-userbars { display: flex; flex-direction: column; gap: 13px; }
  .pna-userbar { display: flex; align-items: center; gap: 12px; }
  .pna-userbar img { width: 28px; height: 28px; border-radius: var(--radius-pill); object-fit: cover; flex: none; }
  .pna-userbar-name { width: 130px; flex: none; font-size: var(--text-xs); font-weight: var(--fw-medium); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-userbar-v { width: 96px; flex: none; text-align: right; font-size: 11px; color: var(--text-muted); }

  /* settings fields */
  .pna-field { display: flex; align-items: center; gap: 16px; padding: 13px 0; border-bottom: 1px solid var(--border-faint); }
  .pna-field:last-child { border-bottom: 0; }
  .pna-field-txt { display: flex; flex-direction: column; gap: 2px; flex: 1; min-width: 0; }
  .pna-field-lbl { font-size: var(--text-sm); font-weight: var(--fw-medium); }
  .pna-field-hint { font-size: var(--text-xs); color: var(--text-faint); }
  .pna-field-ctl { flex: none; }
  .pna-input { height: 34px; padding: 0 12px; border-radius: var(--radius-md); background: var(--bg-subtle); border: 1px solid var(--border); color: var(--text); font-size: var(--text-sm); font-family: inherit; outline: none; min-width: 240px; transition: border-color var(--dur-fast) var(--ease-out); }
  .pna-input:focus { border-color: var(--accent-soft-bd); background: var(--surface); }
  .pna-input-suffix { display: inline-flex; align-items: center; gap: 7px; }
  .pna-input-suffix span { font-size: var(--text-xs); color: var(--text-muted); }
  .pna-note { display: flex; align-items: center; gap: 9px; margin-top: 12px; padding: 11px 13px; border-radius: var(--radius-md); background: var(--bg-subtle); border: 1px solid var(--border-faint); font-size: var(--text-xs); color: var(--text-muted); }
  .pna-note :global(.pk-ic) { color: var(--accent); flex: none; }

  /* toggle */
  .pna-toggle { width: 40px; height: 23px; border-radius: var(--radius-pill); background: var(--surface-active); position: relative; transition: background var(--dur-base) var(--ease-out); flex: none; }
  .pna-toggle.is-on { background: var(--accent); }
  .pna-toggle-knob { position: absolute; top: 3px; left: 3px; width: 17px; height: 17px; border-radius: 50%; background: #fff; box-shadow: var(--shadow-sm); transition: transform var(--dur-base) var(--ease-out); }
  .pna-toggle.is-on .pna-toggle-knob { transform: translateX(17px); }

  /* maintenance */
  .pna-maint { display: flex; flex-direction: column; }
  .pna-maint-row { display: flex; align-items: center; gap: 13px; padding: 12px 0; border-bottom: 1px solid var(--border-faint); }
  .pna-maint-row:last-child { border-bottom: 0; }
  .pna-maint-ic { width: 34px; height: 34px; flex: none; border-radius: var(--radius-md); display: grid; place-items: center; background: var(--bg-subtle); color: var(--text-muted); }
  .pna-maint-txt { flex: 1; display: flex; flex-direction: column; gap: 2px; }

  /* albums condensed table */
  .pna-altable { overflow: hidden; }
  .pna-altr { display: grid; grid-template-columns: 2.6fr 1.6fr .8fr 1fr .8fr 1fr 116px; align-items: center; gap: 12px; padding: 9px 16px; border-bottom: 1px solid var(--border-faint); }
  .pna-altr:last-child { border-bottom: 0; }
  .pna-altr.pna-th { font-size: 10px; text-transform: uppercase; letter-spacing: var(--ls-caps); color: var(--text-faint); font-weight: var(--fw-semibold); padding: 11px 16px; }
  .pna-altr:not(.pna-th):hover { background: var(--surface-hover); }
  .pna-altr .num { text-align: right; }
  .pna-al-name { display: flex; align-items: center; gap: 9px; min-width: 0; }
  .pna-al-name :global(.pk-ic) { color: var(--accent); flex: none; }
  .pna-al-name b { font-weight: var(--fw-medium); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pna-al-owner { display: flex; align-items: center; gap: 8px; min-width: 0; font-size: var(--text-xs); color: var(--text-muted); }
  .pna-al-owner img { width: 22px; height: 22px; border-radius: var(--radius-pill); object-fit: cover; flex: none; }
  .pna-al-private { display: inline-flex; align-items: center; gap: 5px; font-size: var(--text-xs); color: var(--text-faint); }

  @media (max-width: 1080px) { .pna-cols, .pna-jobgrid { grid-template-columns: 1fr; } .pna-stats { grid-template-columns: repeat(2, 1fr); } }
</style>
