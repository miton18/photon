<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import { api, type AdminStats, type Invite, type SmtpConfig, type User } from '../api';

  let {
    me,
    users,
    onClose,
    onChanged,
  }: { me: User; users: User[]; onClose: () => void; onChanged: () => Promise<void> | void } = $props();

  type Tab = 'stats' | 'users' | 'email';
  let tab = $state<Tab>('stats');

  let stats = $state<AdminStats | null>(null);
  let smtp = $state<SmtpConfig>({ host: '', port: 587, username: '', password: '', from: '', tls: true });
  let invites = $state<Invite[]>([]);
  let newUserName = $state('');
  let newUserEmail = $state('');
  let inviteEmail = $state('');

  async function loadStats() {
    try { stats = await api.adminStats(); } catch (e) { toast({ tone: 'error', message: `Stats: ${e}` }); }
  }
  async function loadEmail() {
    try {
      const cfg = await api.getSmtp();
      if (cfg) smtp = { ...cfg, password: '' };
      invites = await api.invites();
    } catch { /* endpoints may be warming up */ }
  }
  let gravatarEnabled = $state(false);
  async function loadSettings() {
    try { gravatarEnabled = (await api.getSettings()).gravatar_enabled; } catch { /* ignore */ }
  }
  async function toggleGravatar() {
    const next = !gravatarEnabled;
    try {
      gravatarEnabled = (await api.setGravatar(next)).gravatar_enabled;
      toast({ tone: 'success', message: `Gravatar ${gravatarEnabled ? 'enabled' : 'disabled'}` });
      await onChanged(); // reload users so avatars refresh everywhere
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  loadStats();
  loadEmail();
  loadSettings();

  async function createUser() {
    if (!newUserName.trim() || !newUserEmail.trim()) return;
    try {
      await api.createUser({ name: newUserName.trim(), email: newUserEmail.trim() });
      toast({ tone: 'success', message: 'User created (they set their own password)' });
      newUserName = ''; newUserEmail = '';
      await onChanged();
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function toggleAdmin(u: User & { is_admin?: boolean }) {
    try { await api.patchUser(u.id, [{ op: 'add', path: '/is_admin', value: !u.is_admin }]); await onChanged(); }
    catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function toggleDisabled(u: User & { disabled?: boolean }) {
    try { await api.patchUser(u.id, [{ op: 'add', path: '/disabled', value: !u.disabled }]); await onChanged(); }
    catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  // Storage quota in GB (blank = unlimited). Stored server-side as quota_mb.
  async function setQuota(u: User, gb: string) {
    const t = gb.trim();
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

  async function saveSmtp() {
    try {
      const body = { ...smtp };
      if (!body.password) delete (body as any).password;
      await api.putSmtp(body as SmtpConfig);
      toast({ tone: 'success', message: 'SMTP settings saved' });
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
  async function sendInvite() {
    if (!inviteEmail.trim()) return;
    try {
      await api.createInvite(inviteEmail.trim(), me.id);
      toast({ tone: 'success', icon: 'mail', message: `Invitation sent to ${inviteEmail}` });
      inviteEmail = '';
      invites = await api.invites();
    } catch (e) { toast({ tone: 'error', message: String(e) }); }
  }
</script>

<Modal title="Admin console" sub="Server administration" icon="server-cog" {onClose}>
  <div class="pk-tabs2">
    {#each [['stats', 'chart-no-axes-column', 'Statistics'], ['users', 'users', 'Users'], ['email', 'mail', 'Email']] as [t, ic, lbl] (t)}
      <button class={'pk-tab2' + (tab === t ? ' is-on' : '')} onclick={() => (tab = t as Tab)}>
        <Icon name={ic} size={15} />{lbl}
      </button>
    {/each}
  </div>

  {#if tab === 'stats'}
    {#if !stats}
      <div class="pk-admin-center"><Icon name="loader-circle" size={20} spin /></div>
    {:else}
      <p class="pk-sec-title">Background jobs</p>
      <div class="pk-list">
        {#each stats.jobs as jb (jb.name)}
          <div class="pk-listrow">
            <span class="pk-listrow-ic"><Icon name={jb.status === 'running' ? 'loader-circle' : 'check'} size={14} spin={jb.status === 'running'} /></span>
            <div class="pk-listrow-main">
              <div class="pk-listrow-name">{jb.name}</div>
              <div class="pk-listrow-sub">last run {jb.last_run_at ?? 'never'}{jb.last_result ? ` · ${jb.last_result}` : ''}</div>
            </div>
            <span class={'pk-pill' + (jb.status === 'running' ? ' is-on' : '')}>{jb.status}</span>
          </div>
        {/each}
      </div>

      <p class="pk-sec-title" style="margin-top:16px">Library</p>
      <div class="pk-stat-grid">
        {#each Object.entries(stats.counts) as [k, v] (k)}
          <div class="pk-stat"><div class="pk-stat-n pk-mono">{v.toLocaleString()}</div><div class="pk-stat-k">{k}</div></div>
        {/each}
      </div>

      <p class="pk-sec-title" style="margin-top:16px">Storage · {stats.storage.mode}</p>
      <div class="pk-store-bar"><i style={`width:${Math.min(100, Math.round(((stats.storage.disk_used_mb + stats.storage.s3_used_mb) / stats.storage.quota_mb) * 100))}%`}></i></div>
      <div class="pk-stat-row pk-mono">
        <span>disk {(stats.storage.disk_used_mb / 1024).toFixed(1)} GB</span>
        <span>S3 {(stats.storage.s3_used_mb / 1024).toFixed(1)} GB</span>
        <span>quota {(stats.storage.quota_mb / 1024).toFixed(0)} GB</span>
      </div>

      <p class="pk-sec-title" style="margin-top:16px">Appearance</p>
      <div class="pk-listrow">
        <span class="pk-listrow-ic"><Icon name="user-round" size={15} /></span>
        <div class="pk-listrow-main">
          <div class="pk-listrow-name">Gravatar avatars</div>
          <div class="pk-listrow-sub">Use each user's email-based Gravatar instead of the placeholder avatar</div>
        </div>
        <button class={'pk-switch pk-listrow-act' + (gravatarEnabled ? ' is-on' : '')} onclick={toggleGravatar} aria-label="Toggle Gravatar"></button>
      </div>
    {/if}
  {:else if tab === 'users'}
    <p class="pk-sec-title">Add a user</p>
    <div class="pk-share-form">
      <input class="pk-field-inline" placeholder="Name" bind:value={newUserName} />
      <input class="pk-field-inline" placeholder="Email" bind:value={newUserEmail} />
      <button class="pk-btn pk-btn-primary" onclick={createUser}><Icon name="user-plus" size={15} />Create</button>
    </div>
    <div class="pk-admin-note"><Icon name="shield" size={12} /> Passwords are never visible — the user sets their own, or you send a reset email.</div>

    <div class="pk-list" style="margin-top:6px">
      {#each users as u (u.id)}
        {@const uu = u as User & { is_admin?: boolean; disabled?: boolean }}
        <div class="pk-listrow">
          <img src={u.avatar_url || 'https://i.pravatar.cc/64?img=12'} alt={u.name} />
          <div class="pk-listrow-main">
            <div class="pk-listrow-name">{u.name}{#if uu.is_admin} · admin{/if}{#if uu.disabled} · disabled{/if}</div>
            <div class="pk-listrow-sub">{u.email}</div>
          </div>
          <label class="pk-quota" title="Storage quota in GB (blank = unlimited)">
            <input
              type="number"
              min="0"
              step="1"
              placeholder="∞"
              value={u.quota_mb != null ? (u.quota_mb / 1024) : ''}
              onchange={(e) => setQuota(u, (e.currentTarget as HTMLInputElement).value)}
            />
            <span>GB</span>
          </label>
          <button class={'pk-pill' + (uu.is_admin ? ' is-on' : '')} onclick={() => toggleAdmin(uu)} title="Toggle admin"><Icon name="shield" size={12} /></button>
          <button class="pk-pill" onclick={() => resetPw(u)} title="Send reset email"><Icon name="mail" size={12} /></button>
          <button class="pk-pill" onclick={() => toggleDisabled(uu)} title="Enable/disable"><Icon name={uu.disabled ? 'user-check' : 'user-x'} size={12} /></button>
          {#if u.id !== me.id}
            <button class="pk-pill danger" onclick={() => delUser(u)} title="Delete"><Icon name="trash-2" size={12} /></button>
          {/if}
        </div>
      {/each}
    </div>
  {:else}
    <p class="pk-sec-title">SMTP server</p>
    <div class="pk-grid2">
      <div class="pk-field"><label for="sm-host">Host</label><input id="sm-host" bind:value={smtp.host} placeholder="smtp.example.com" /></div>
      <div class="pk-field"><label for="sm-port">Port</label><input id="sm-port" type="number" bind:value={smtp.port} /></div>
      <div class="pk-field"><label for="sm-user">Username</label><input id="sm-user" bind:value={smtp.username} /></div>
      <div class="pk-field"><label for="sm-pass">Password</label><input id="sm-pass" type="password" bind:value={smtp.password} placeholder="•••••• (unchanged)" /></div>
      <div class="pk-field"><label for="sm-from">From</label><input id="sm-from" bind:value={smtp.from} placeholder="Photon <noreply@example.com>" /></div>
      <div class="pk-field" style="display:flex;align-items:flex-end;gap:8px">
        <button class={'pk-switch' + (smtp.tls ? ' is-on' : '')} onclick={() => (smtp.tls = !smtp.tls)} aria-label="TLS"></button>
        <span style="font-size:var(--text-xs);color:var(--text-muted)">Use TLS</span>
      </div>
    </div>
    <button class="pk-btn pk-btn-primary" onclick={saveSmtp}><Icon name="check" size={15} />Save SMTP</button>

    <p class="pk-sec-title" style="margin-top:18px">Invite a user</p>
    <div class="pk-share-form">
      <input class="pk-field-inline" placeholder="email@example.com" bind:value={inviteEmail} />
      <button class="pk-btn pk-btn-outline" onclick={sendInvite}><Icon name="send" size={14} />Send invite</button>
    </div>
    {#if invites.length}
      <div class="pk-list" style="margin-top:10px">
        {#each invites as inv (inv.token)}
          <div class="pk-listrow">
            <span class="pk-listrow-ic"><Icon name="mail" size={14} /></span>
            <div class="pk-listrow-main">
              <div class="pk-listrow-name">{inv.email}</div>
              <div class="pk-listrow-sub">{inv.accepted ? 'accepted' : 'pending'} · {inv.created_at.slice(0, 10)}</div>
            </div>
            <span class={'pk-pill' + (inv.accepted ? ' is-on' : '')}>{inv.accepted ? 'accepted' : 'pending'}</span>
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
  .pk-grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 0 12px; }
  .pk-admin-center { display: grid; place-items: center; padding: 28px; color: var(--accent); }
  .pk-admin-note { display: flex; align-items: center; gap: 6px; font-size: var(--text-2xs); color: var(--text-faint); margin: 8px 0 4px; }
  .pk-stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(96px, 1fr)); gap: 8px; }
  .pk-stat { padding: 12px; border-radius: var(--radius-md); background: var(--bg-subtle); border: 1px solid var(--border-faint); }
  .pk-stat-n { font-size: var(--text-lg); font-weight: var(--fw-semibold); }
  .pk-stat-k { font-size: var(--text-2xs); color: var(--text-faint); text-transform: capitalize; }
  .pk-stat-row { display: flex; gap: 14px; font-size: var(--text-2xs); color: var(--text-muted); margin-top: 8px; }
  .pk-quota { display: inline-flex; align-items: center; gap: 4px; color: var(--text-faint); font-size: var(--text-2xs); }
  .pk-quota input {
    width: 52px; height: var(--control-h-sm); padding: 0 5px; text-align: right;
    background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md);
    color: var(--text); font: inherit; font-size: var(--text-2xs); outline: none;
  }
  .pk-quota input:focus { border-color: var(--accent); }
</style>
