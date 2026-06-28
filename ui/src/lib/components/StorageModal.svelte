<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import { toast } from '../toast.svelte';
  import { api, type S3Config, type StorageSettings } from '../api';
  import { uploadConfig } from '../upload.svelte';

  let { onClose }: { onClose: () => void } = $props();

  let s = $state<StorageSettings | null>(null);
  let saving = $state(false);
  let err = $state('');

  // editable copies
  let mode = $state<'filesystem' | 's3_replacement'>('filesystem');
  let retention = $state(7);
  let backupEnabled = $state(false);
  let intervalHours = $state(1);
  let s3 = $state<S3Config>({ region: 'us-east-1', bucket: '', endpoint: '', access_key_id: '', secret_access_key: '', prefix: '' });

  async function load() {
    try {
      const data = await api.getStorage();
      s = data;
      mode = data.mode;
      retention = data.trash_retention_days;
      backupEnabled = data.backup.enabled;
      intervalHours = Math.max(1, Math.round((data.backup.interval_secs || 3600) / 3600));
      const cfg = data.primary_s3 ?? data.backup.s3;
      if (cfg) s3 = { ...s3, ...cfg, secret_access_key: '' };
    } catch (e) {
      err = String(e);
    }
  }
  load();

  async function save() {
    saving = true;
    err = '';
    try {
      const s3body: S3Config = { ...s3 };
      // don't send an empty secret (server keeps the stored one on a redacted/empty value)
      if (!s3body.secret_access_key) delete (s3body as any).secret_access_key;
      const body: Partial<StorageSettings> = {
        mode,
        trash_retention_days: retention,
        primary_s3: mode === 's3_replacement' ? s3body : null,
        backup: {
          enabled: backupEnabled,
          interval_secs: intervalHours * 3600,
          s3: backupEnabled ? s3body : null,
          last_backup_at: s?.backup.last_backup_at ?? null,
          last_backup_count: s?.backup.last_backup_count ?? 0,
        },
      };
      await api.putStorage(body);
      toast({ tone: 'success', message: 'Storage settings saved' });
      onClose();
    } catch (e) {
      err = String(e);
      toast({ tone: 'error', title: 'Save failed', message: String(e) });
    } finally {
      saving = false;
    }
  }

  async function backupNow() {
    try {
      const r = await api.runBackup();
      toast({ tone: 'success', icon: 'cloud-upload', message: `Backed up ${r.count} item(s) to S3` });
    } catch (e) {
      toast({ tone: 'error', title: 'Backup failed', message: String(e) });
    }
  }
</script>

{#snippet footer()}
  <button class="pk-btn pk-btn-ghost" onclick={onClose}>Cancel</button>
  <button class="pk-btn pk-btn-primary" onclick={save} disabled={saving}>
    <Icon name="check" size={15} />{saving ? 'Saving…' : 'Save settings'}
  </button>
{/snippet}

<Modal title="Storage" sub="Where image & video files live (metadata is always in Postgres)" icon="hard-drive" {onClose} {footer}>
  {#if err}<div class="pk-storage-err"><Icon name="circle-alert" size={14} />{err}</div>{/if}

  <p class="pk-sec-title">Primary store</p>
  <div class="pk-chips" style="margin-bottom:16px">
    <button class={'pk-chip' + (mode === 'filesystem' ? ' is-active' : '')} onclick={() => (mode = 'filesystem')}>
      <Icon name="folder" size={13} />Filesystem
    </button>
    <button class={'pk-chip' + (mode === 's3_replacement' ? ' is-active' : '')} onclick={() => (mode = 's3_replacement')}>
      <Icon name="cloud" size={13} />S3 (replace filesystem)
    </button>
  </div>

  <p class="pk-sec-title">S3 bucket</p>
  <div class="pk-grid2">
    <div class="pk-field"><label for="s3-bucket">Bucket</label><input id="s3-bucket" bind:value={s3.bucket} placeholder="photon-library" /></div>
    <div class="pk-field"><label for="s3-region">Region</label><input id="s3-region" bind:value={s3.region} placeholder="us-east-1" /></div>
    <div class="pk-field"><label for="s3-endpoint">Endpoint (optional)</label><input id="s3-endpoint" bind:value={s3.endpoint} placeholder="https://s3.example.com" /></div>
    <div class="pk-field"><label for="s3-prefix">Prefix (optional)</label><input id="s3-prefix" bind:value={s3.prefix} placeholder="library/" /></div>
    <div class="pk-field"><label for="s3-key">Access key ID</label><input id="s3-key" bind:value={s3.access_key_id} /></div>
    <div class="pk-field"><label for="s3-secret">Secret access key</label><input id="s3-secret" type="password" bind:value={s3.secret_access_key} placeholder={s?.primary_s3 || s?.backup.s3 ? '•••••• (unchanged)' : ''} /></div>
  </div>

  <p class="pk-sec-title" style="margin-top:8px">Hourly S3 backup</p>
  <div class="pk-listrow">
    <span class="pk-listrow-ic"><Icon name="cloud-upload" size={15} /></span>
    <div class="pk-listrow-main">
      <div class="pk-listrow-name">Backup new photos to S3</div>
      <div class="pk-listrow-sub">Filesystem stays source of truth · last run {s?.backup.last_backup_at ?? 'never'} ({s?.backup.last_backup_count ?? 0})</div>
    </div>
    <button class={'pk-switch pk-listrow-act' + (backupEnabled ? ' is-on' : '')} onclick={() => (backupEnabled = !backupEnabled)} aria-label="Toggle backup"></button>
  </div>
  <div class="pk-grid2" style="margin-top:10px">
    <div class="pk-field"><label for="bk-int">Interval (hours)</label><input id="bk-int" type="number" min="1" bind:value={intervalHours} /></div>
    <div class="pk-field"><label for="tr-ret">Trash retention (days)</label><input id="tr-ret" type="number" min="1" bind:value={retention} /></div>
  </div>
  <button class="pk-btn pk-btn-outline" onclick={backupNow}><Icon name="play" size={14} />Run backup now</button>

  <p class="pk-sec-title" style="margin-top:18px">Import</p>
  <div class="pk-grid2">
    <div class="pk-field">
      <label for="up-par">Upload parallelism (files at once)</label>
      <input id="up-par" type="number" min="1" max="20" bind:value={uploadConfig.maxPerRequest} />
    </div>
  </div>
</Modal>

<style>
  .pk-grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 0 12px; }
  .pk-storage-err { display: flex; align-items: center; gap: 7px; color: var(--danger); background: var(--danger-soft); padding: 8px 10px; border-radius: var(--radius-md); font-size: var(--text-xs); margin-bottom: 12px; }
</style>
