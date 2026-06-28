<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import Logo from './Logo.svelte';
  import type { User } from '../api';
  import { STATS } from '../media';

  let {
    active = 'Timeline',
    theme = 'dark',
    user,
    counts = {},
    onNav,
    onToggleTheme,
    onSwitchAccount,
    onOpenSettings,
    onOpenAdmin,
    onOpenSecurity,
    plugins = [],
    onOpenPlugin,
    storage = null,
  }: {
    active?: string;
    theme?: 'dark' | 'light';
    user: User;
    counts?: Record<string, number>;
    onNav: (label: string) => void;
    onToggleTheme: () => void;
    onSwitchAccount: () => void;
    onOpenSettings: () => void;
    onOpenAdmin: () => void;
    onOpenSecurity: () => void;
    plugins?: { id: string; label: string; ui_path: string | null }[];
    onOpenPlugin: (p: { id: string; ui_path: string | null }) => void;
    storage?: { used_mb: number; total_mb: number } | null;
  } = $props();

  const primary = $derived<[string, string, number | null][]>([
    ['Timeline', 'layout-grid', null],
    ['Albums', 'images', counts.albums ?? STATS.albums],
    ['People', 'users', counts.people || STATS.people],
    ['Places', 'map-pin', null],
    ['Explore', 'compass', null],
  ]);
  const library = $derived<[string, string, number | null][]>([
    ['Favorites', 'star', null],
    ['Vault', 'lock', null],
    ['Shared', 'share-2', counts.shared ?? null],
    ['Groups', 'users-round', counts.groups ?? null],
    ['Archive', 'archive', null],
    ['Trash', 'trash-2', counts.trash ?? null],
  ]);
  const tools = $derived<[string, string, number | null][]>([
    ['Duplicates', 'copy', counts.duplicates || null],
    ['Large files', 'file-stack', null],
    ['Tag faces', 'scan-face', null],
  ]);

  const usedGb = $derived((storage ? storage.used_mb / 1024 : STATS.used));
  const totalGb = $derived((storage ? storage.total_mb / 1024 : STATS.quota));
  const pct = $derived(totalGb > 0 ? Math.min(100, Math.round((usedGb / totalGb) * 100)) : 0);
</script>

<aside class="pk-sidebar">
  <div class="pk-brand">
    <div class="pk-brand-logo"><Logo size={30} /></div>
    <span class="pk-brand-wm">Photon</span>
  </div>

  <nav class="pk-nav">
    {#each primary as [label, icon, count] (label)}
      <button class={'pk-nav-item' + (active === label ? ' is-active' : '')} onclick={() => onNav(label)}>
        <Icon name={icon} size={17} /><span>{label}</span>
        {#if count != null}<span class="pk-nav-count">{count}</span>{/if}
      </button>
    {/each}
    <div class="pk-nav-sep"></div>
    <div class="pk-nav-label">Library</div>
    {#each library as [label, icon, count] (label)}
      <button class={'pk-nav-item' + (active === label ? ' is-active' : '')} onclick={() => onNav(label)}>
        <Icon name={icon} size={17} /><span>{label}</span>
        {#if count != null}<span class="pk-nav-count">{count}</span>{/if}
      </button>
    {/each}
    <div class="pk-nav-sep"></div>
    <div class="pk-nav-label">Tools</div>
    {#each tools as [label, icon, count] (label)}
      <button class={'pk-nav-item' + (active === label ? ' is-active' : '')} onclick={() => onNav(label)}>
        <Icon name={icon} size={17} /><span>{label}</span>
        {#if count != null}<span class="pk-nav-count">{count}</span>{/if}
      </button>
    {/each}
    {#each plugins as p (p.id)}
      <button class="pk-nav-item" onclick={() => onOpenPlugin(p)} title={`${p.label} (plugin)`}>
        <Icon name="puzzle" size={17} /><span>{p.label}</span>
        <Icon name="external-link" size={13} class="pk-nav-count" />
      </button>
    {/each}
  </nav>

  <div class="pk-store">
    <div class="pk-store-row">
      <span class="pk-store-lbl">Storage</span>
      <span class="pk-mono">{usedGb.toFixed(usedGb < 10 ? 1 : 0)} / {totalGb.toFixed(0)} GB</span>
    </div>
    <div class="pk-store-bar"><i style={`width:${pct}%`}></i></div>
    <div class="pk-store-cta">Self-hosted · {pct}% used</div>
  </div>

  <div class="pk-side-foot">
    <button class="pk-iconbtn" onclick={onToggleTheme} title="Toggle theme">
      <Icon name={theme === 'dark' ? 'sun' : 'moon'} size={17} />
    </button>
    <button class="pk-iconbtn" onclick={onOpenSecurity} title="Passkeys & security"><Icon name="key-round" size={17} /></button>
    <button class="pk-iconbtn" onclick={onOpenSettings} title="Storage & settings"><Icon name="settings" size={17} /></button>
    <button class="pk-admin-badge" onclick={onOpenAdmin} title="Admin console · server administration">
      <Icon name="server-cog" size={17} />
    </button>
    <button class="pk-avatar-btn" onclick={onSwitchAccount} title={`Signed in as ${user.name} — switch account`}>
      <img class="pk-avatar" alt={user.name} src={user.avatar_url || 'https://i.pravatar.cc/64?img=12'} />
    </button>
  </div>
</aside>

<style>
  .pk-avatar-btn { padding: 0; margin-left: auto; line-height: 0; border-radius: var(--radius-pill); }
  .pk-avatar-btn .pk-avatar { margin-left: 0; }
</style>
