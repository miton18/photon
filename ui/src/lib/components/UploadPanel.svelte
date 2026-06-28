<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { uploads, tray, clearFinished, STAGE_COUNT, type UploadItem, type UpStatus } from '../upload.svelte';

  // Single-file uploads are one logical step: the POST imports + stores the
  // photo (EXIF, thumbnail, companion pairing) and returns it. Face detection
  // runs server-side in the background, so there is nothing further to poll.
  const STAGES = [{ label: 'Uploading', icon: 'upload-cloud' }];
  const STATUS: Record<UpStatus, { color: string; icon: string | null }> = {
    active: { color: 'var(--accent)', icon: null },
    ok: { color: 'var(--success)', icon: 'circle-check' },
    duplicate: { color: 'var(--warning)', icon: 'copy' },
    rejected: { color: 'var(--danger)', icon: 'circle-x' },
  };

  const done = $derived(uploads.filter((i) => i.status !== 'active'));
  const active = $derived(uploads.length - done.length);
  const ok = $derived(uploads.filter((i) => i.status === 'ok').length);
  const dup = $derived(uploads.filter((i) => i.status === 'duplicate').length);
  const rej = $derived(uploads.filter((i) => i.status === 'rejected').length);
  const allDone = $derived(uploads.length > 0 && active === 0);
  const overall = $derived(
    uploads.length
      ? Math.round(
          (uploads.reduce(
            (a, i) =>
              a + (i.status === 'active' ? (i.stage * 100 + i.progress) / (STAGE_COUNT * 100) : 1),
            0,
          ) /
            uploads.length) *
            100,
        )
      : 0,
  );

  // Per-segment class + fill for one item's 4-stage gauge.
  function seg(item: UploadItem, i: number): { cls: string; fill: number } {
    if (item.status === 'rejected') return i === 0 ? { cls: 'is-fail rej', fill: 100 } : { cls: '', fill: 0 };
    if (item.status === 'duplicate') {
      if (i < item.stage) return { cls: 'is-done', fill: 100 };
      if (i === item.stage) return { cls: 'is-fail', fill: 100 };
      return { cls: '', fill: 0 };
    }
    if (item.status === 'ok') return { cls: 'is-done', fill: 100 };
    if (i < item.stage) return { cls: 'is-done', fill: 100 };
    if (i === item.stage) return { cls: 'is-active', fill: item.progress };
    return { cls: '', fill: 0 };
  }
  function caption(item: UploadItem): string {
    if (item.status === 'ok') return 'Imported';
    if (item.status === 'duplicate') return 'Already in library';
    if (item.status === 'rejected') return 'Unsupported / failed';
    return (STAGES[item.stage]?.label ?? 'Queued') + '…';
  }
</script>

{#if tray.open && uploads.length}
  <div class={'pk-up' + (tray.minimized ? ' is-min' : '')}>
    <div class="pk-up-head">
      <span class="pk-up-head-ic">
        {#if allDone}<Icon name="check" size={15} strokeWidth={2.5} />
        {:else}<Icon name="upload-cloud" size={16} class="pk-up-bob" />{/if}
      </span>
      <div class="pk-up-head-txt">
        <span class="pk-up-title">{allDone ? 'Import complete' : `Importing ${active} ${active === 1 ? 'item' : 'items'}`}</span>
        <span class="pk-up-counts">
          {#if ok > 0}<span class="ok"><Icon name="circle-check" size={12} />{ok}</span>{/if}
          {#if dup > 0}<span class="dup"><Icon name="copy" size={12} />{dup}</span>{/if}
          {#if rej > 0}<span class="rej"><Icon name="circle-x" size={12} />{rej}</span>{/if}
          {#if !allDone}<span class="pk-mono pk-up-overall">{overall}%</span>{/if}
        </span>
      </div>
      <button class="pk-up-hbtn" onclick={() => (tray.minimized = !tray.minimized)} title={tray.minimized ? 'Expand' : 'Minimize'}>
        <Icon name={tray.minimized ? 'chevron-up' : 'chevron-down'} size={16} />
      </button>
      <button class="pk-up-hbtn" onclick={() => (tray.open = false)} title="Close"><Icon name="x" size={16} /></button>
    </div>

    {#if !allDone}<div class="pk-up-bar"><i style={`width:${overall}%`}></i></div>{/if}

    {#if !tray.minimized}
      <div class="pk-up-list">
        {#each uploads as item (item.id)}
          {@const st = STATUS[item.status]}
          <div class="pk-up-item">
            <div class="pk-up-thumb">
              {#if item.isDoc}
                <span class="pk-up-doc"><Icon name="file-text" size={20} /></span>
              {:else}
                <span class="pk-up-doc" style="background:var(--bg-subtle);color:var(--text-faint);border-style:solid;border-color:var(--border)"><Icon name="image" size={18} /></span>
              {/if}
              <span class="pk-up-thumb-badge" style={`background:${st.color}`}>
                {#if st.icon}<Icon name={st.icon} size={11} strokeWidth={3} />
                {:else}<span class="pk-up-pct pk-mono">{item.progress}</span>{/if}
              </span>
            </div>
            <div class="pk-up-meta">
              <div class="pk-up-row1">
                <span class="pk-up-name">{item.name}</span>
                <span class="pk-up-size pk-mono">{item.size}</span>
              </div>
              <div class="pk-up-gauge">
                {#each STAGES as _s, i (i)}
                  {@const g = seg(item, i)}
                  <div class={'pk-up-seg ' + g.cls} title={STAGES[i].label}><i style={`width:${g.fill}%`}></i></div>
                {/each}
              </div>
              <div class="pk-up-cap" style={`color:${item.status === 'active' ? 'var(--text-muted)' : st.color}`}>
                {#if item.status === 'active' && STAGES[item.stage]}<Icon name={STAGES[item.stage].icon} size={12} />{/if}
                {caption(item)}
              </div>
            </div>
          </div>
        {/each}
      </div>
      <div class="pk-up-foot">
        <span class="pk-up-foot-info pk-mono"><Icon name="shield-check" size={13} /> Stored on your server</span>
        <button class="pk-up-clear" disabled={done.length === 0} onclick={clearFinished}>
          <Icon name="list-x" size={14} /> Clear {done.length || ''} done
        </button>
      </div>
    {/if}
  </div>
{/if}
