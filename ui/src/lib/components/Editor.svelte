<script lang="ts">
  import { onDestroy } from 'svelte';
  import Icon from '../icons/Icon.svelte';
  import { displayFull, type UIPhoto } from '../media';
  import { api, type PluginEditorOp } from '../api';
  import { toast } from '../toast.svelte';

  let {
    photo,
    onClose,
    onSaved,
  }: { photo: UIPhoto; onClose: () => void; onSaved: (p: UIPhoto) => void } = $props();

  type Mode = 'crop' | 'light' | 'color' | 'meta' | 'plugins';
  const LIGHT: [string, string][] = [
    ['exposure', 'Exposure'],
    ['brightness', 'Brightness'],
    ['contrast', 'Contrast'],
    ['highlights', 'Highlights'],
    ['shadows', 'Shadows'],
  ];
  const COLOR: [string, string][] = [
    ['saturation', 'Saturation'],
    ['vibrance', 'Vibrance'],
    ['warmth', 'Warmth'],
    ['tint', 'Tint'],
  ];
  const ASPECTS: [string, string][] = [
    ['Free', 'free'],
    ['1:1', 'sq'],
    ['4:5', '45'],
    ['3:2', '32'],
    ['16:9', '169'],
  ];
  const ZERO: Record<string, number> = {
    exposure: 0, brightness: 0, contrast: 0, highlights: 0, shadows: 0,
    saturation: 0, vibrance: 0, warmth: 0, tint: 0,
  };

  let mode = $state<Mode>('meta');
  let adj = $state<Record<string, number>>({ ...ZERO });
  let rot = $state(0);
  let flip = $state(false);
  let straighten = $state(0);
  let aspect = $state('free');

  // metadata override draft
  let mTitle = $state(photo.title ?? '');
  let mCaption = $state(photo.caption ?? '');
  let mCity = $state(photo.city ?? '');
  let mCountry = $state(photo.country ?? '');
  let mTags = $state(photo.tags.join(', '));
  let saving = $state(false);

  // editor plugins — loaded lazily the first time the Plugins tab opens
  let pluginOps = $state<PluginEditorOp[] | null>(null);
  let pluginsLoading = $state(false);
  // per-op param drafts, keyed by `${plugin}/${id}`
  let pluginParams = $state<Record<string, Record<string, string>>>({});
  let applying = $state<string | null>(null);
  let savingPlugin = $state<string | null>(null);
  // object URL of the plugin preview currently shown (null = original)
  let pluginPreview = $state<string | null>(null);

  function opKey(op: PluginEditorOp) {
    return `${op.plugin}/${op.id}`;
  }

  async function loadPlugins() {
    if (pluginOps !== null || pluginsLoading) return;
    pluginsLoading = true;
    try {
      const ops = await api.pluginEditorOps();
      pluginOps = ops;
      const drafts: Record<string, Record<string, string>> = {};
      for (const op of ops) {
        drafts[opKey(op)] = Object.fromEntries(op.params.map((p) => [p.name, p.default]));
      }
      pluginParams = drafts;
    } catch (e) {
      pluginOps = [];
      toast({ tone: 'error', title: 'Plugins unavailable', message: String(e) });
    } finally {
      pluginsLoading = false;
    }
  }

  function setMode(m: Mode) {
    mode = m;
    if (m === 'plugins') loadPlugins();
  }

  async function applyPlugin(op: PluginEditorOp) {
    const key = opKey(op);
    applying = key;
    try {
      const url = await api.applyPluginEdit(photo.id, op.plugin, op.id, pluginParams[key] ?? {});
      // swap preview, revoking the previous one to avoid leaking blobs
      if (pluginPreview) URL.revokeObjectURL(pluginPreview);
      pluginPreview = url;
      toast({ tone: 'success', message: `${op.label} applied (preview)`, actionLabel: 'OK' });
    } catch (e) {
      toast({ tone: 'error', title: 'Plugin edit failed', message: String(e) });
    } finally {
      applying = null;
    }
  }

  // Persist the edit: keeps the original, stores the edited version as a
  // companion, regenerates the thumbnail, and makes it the preferred display.
  async function savePlugin(op: PluginEditorOp) {
    const key = opKey(op);
    savingPlugin = key;
    try {
      const url = await api.applyPluginEdit(photo.id, op.plugin, op.id, pluginParams[key] ?? {}, true);
      if (pluginPreview) URL.revokeObjectURL(pluginPreview);
      pluginPreview = url;
      toast({ tone: 'success', message: `${op.label} saved`, actionLabel: 'OK' });
      onSaved(photo); // signal the parent so timeline/lightbox refresh the image
    } catch (e) {
      toast({ tone: 'error', title: 'Plugin edit failed', message: String(e) });
    } finally {
      savingPlugin = null;
    }
  }

  onDestroy(() => {
    if (pluginPreview) URL.revokeObjectURL(pluginPreview);
  });

  const dirtyDevelop = $derived(
    rot !== 0 || flip || straighten !== 0 || aspect !== 'free' || Object.values(adj).some((v) => v !== 0),
  );

  function filterFor(a: Record<string, number>) {
    const bright = 1 + (a.exposure * 0.6 + a.brightness + a.shadows * 0.25 - a.highlights * 0.18) / 200;
    const contrast = 1 + (a.contrast + a.highlights * 0.2 - a.shadows * 0.15) / 130;
    const sat = 1 + (a.saturation + a.vibrance * 0.7) / 110;
    const sepia = a.warmth > 0 ? a.warmth / 240 : 0;
    const hue = (a.warmth < 0 ? a.warmth * 0.35 : 0) + a.tint * 0.45;
    return `brightness(${bright.toFixed(3)}) contrast(${contrast.toFixed(3)}) saturate(${sat.toFixed(3)}) sepia(${sepia.toFixed(3)}) hue-rotate(${hue.toFixed(1)}deg)`;
  }

  const imgStyle = $derived(
    `filter:${filterFor(adj)};transform:rotate(${rot + straighten}deg) scaleX(${flip ? -1 : 1})`,
  );

  function reset() {
    adj = { ...ZERO };
    rot = 0;
    flip = false;
    straighten = 0;
    aspect = 'free';
  }

  function pct(v: number, min: number, max: number) {
    return ((v - min) / (max - min)) * 100;
  }

  // Geometry dirty in the Crop tab (90°-step rotation or flip — baked server-side).
  const geomDirty = $derived(((rot % 360) + 360) % 360 !== 0 || flip);
  let savingCrop = $state(false);
  async function saveCrop() {
    savingCrop = true;
    try {
      const degrees = ((rot % 360) + 360) % 360;
      const updated = await api.rotatePhoto(photo.id, degrees, flip, photo.owner_id);
      onSaved(updated); // refresh the lightbox/timeline image
      toast({ tone: 'success', message: 'Rotation saved (original kept)', actionLabel: 'OK' });
      onClose();
    } catch (e) {
      toast({ tone: 'error', title: 'Save failed', message: String(e) });
    } finally {
      savingCrop = false;
    }
  }

  // Bake the Light/Color tonal sliders into the edited companion (server applies
  // the same CSS-filter math the preview uses). Geometry (rotate/flip) is saved
  // separately from the Crop tab.
  const adjDirty = $derived(Object.values(adj).some((v) => v !== 0));
  let savingAdjust = $state(false);
  async function saveAdjust() {
    savingAdjust = true;
    try {
      const updated = await api.adjustPhoto(photo.id, adj, photo.owner_id);
      onSaved(updated);
      toast({ tone: 'success', message: 'Saved as a copy (original kept)', actionLabel: 'OK' });
      onClose();
    } catch (e) {
      toast({ tone: 'error', title: 'Save failed', message: String(e) });
    } finally {
      savingAdjust = false;
    }
  }

  async function saveMeta() {
    saving = true;
    try {
      // RFC 6902: `add` sets-or-replaces an override; a null value clears it
      // (effective view falls back to EXIF).
      const tags = mTags.trim() ? mTags.split(',').map((t) => t.trim()).filter(Boolean) : null;
      const ops = [
        { op: 'add' as const, path: '/title', value: mTitle || null },
        { op: 'add' as const, path: '/caption', value: mCaption || null },
        { op: 'add' as const, path: '/city', value: mCity || null },
        { op: 'add' as const, path: '/country', value: mCountry || null },
        { op: 'add' as const, path: '/tags', value: tags },
      ];
      const updated = await api.patchMetadata(photo.id, ops);
      onSaved(updated);
      toast({ tone: 'success', message: 'Metadata saved (originals untouched)', actionLabel: 'OK' });
      onClose();
    } catch (e) {
      toast({ tone: 'error', title: 'Save failed', message: String(e) });
    } finally {
      saving = false;
    }
  }
</script>

<div class="pk-ed">
  <div class="pk-ed-top">
    <button class="pk-lb-ic" onclick={onClose} title="Back to photo"><Icon name="chevron-left" size={20} /></button>
    <span class="pk-ed-title">Edit</span>
    <span class="pk-ed-file pk-mono">{photo.filename}</span>
    <div class="pk-ed-top-right">
      <button class="pk-btn pk-btn-ghost" style="color:rgba(255,255,255,.8)" onclick={reset} disabled={!dirtyDevelop}>
        <Icon name="rotate-ccw" size={15} /> Reset
      </button>
      <button class="pk-btn pk-btn-ghost" style="color:rgba(255,255,255,.8)" onclick={onClose}>Cancel</button>
      {#if mode === 'meta'}
        <button class="pk-btn pk-btn-primary" onclick={saveMeta} disabled={saving}>
          <Icon name="check" size={15} /> {saving ? 'Saving…' : 'Save metadata'}
        </button>
      {:else if mode === 'crop'}
        <button class="pk-btn pk-btn-primary" onclick={saveCrop} disabled={savingCrop || !geomDirty}>
          <Icon name="check" size={15} /> {savingCrop ? 'Saving…' : 'Save rotation'}
        </button>
      {:else if mode === 'light' || mode === 'color'}
        <button class="pk-btn pk-btn-primary" onclick={saveAdjust} disabled={savingAdjust || !adjDirty}>
          <Icon name="check" size={15} /> {savingAdjust ? 'Saving…' : 'Save copy'}
        </button>
      {/if}
    </div>
  </div>

  <div class="pk-ed-body">
    <div class="pk-ed-stage">
      <div class="pk-ed-imgwrap">
        <img class="pk-ed-img" src={pluginPreview ?? displayFull(photo)} alt={photo.filename} style={pluginPreview ? '' : imgStyle} />
        {#if mode === 'crop'}
          <div class={'pk-ed-crop ar-' + aspect}>
            <span></span><span></span><span></span><span></span>
            <i class="c tl"></i><i class="c tr"></i><i class="c bl"></i><i class="c br"></i>
          </div>
        {/if}
      </div>
    </div>

    <div class="pk-ed-panel">
      <div class="pk-ed-tabs">
        {#each [['meta', 'info', 'Info'], ['crop', 'crop', 'Crop'], ['light', 'sun', 'Light'], ['color', 'palette', 'Color'], ['plugins', 'puzzle', 'Plugins']] as [m, ic, lbl] (m)}
          <button class={'pk-ed-tab' + (mode === m ? ' is-on' : '')} onclick={() => setMode(m as Mode)}>
            <Icon name={ic} size={16} /><span>{lbl}</span>
          </button>
        {/each}
      </div>

      <div class="pk-ed-controls">
        {#if mode === 'meta'}
          <div class="pk-ed-group">
            <h5>Library metadata</h5>
            <div class="pk-ed-field">
              <label for="ed-title">Title</label>
              <input id="ed-title" bind:value={mTitle} placeholder="Untitled" />
            </div>
            <div class="pk-ed-field">
              <label for="ed-cap">Caption</label>
              <textarea id="ed-cap" bind:value={mCaption} placeholder="Add a description…"></textarea>
            </div>
            <div class="pk-ed-field">
              <label for="ed-city">City</label>
              <input id="ed-city" bind:value={mCity} placeholder={photo.exif.city || 'City'} />
              {#if photo.exif.city && mCity !== photo.exif.city}<div class="pk-ed-exif">EXIF: {photo.exif.city}</div>{/if}
            </div>
            <div class="pk-ed-field">
              <label for="ed-cc">Country</label>
              <input id="ed-cc" bind:value={mCountry} placeholder={photo.exif.country || 'Country'} />
            </div>
            <div class="pk-ed-field">
              <label for="ed-tags">Tags (comma separated)</label>
              <input id="ed-tags" bind:value={mTags} placeholder="mountains, golden hour" />
            </div>
          </div>
        {:else if mode === 'crop'}
          <div class="pk-ed-group">
            <h5>Aspect ratio</h5>
            <div class="pk-ed-aspects">
              {#each ASPECTS as [lbl, key] (key)}
                <button class={'pk-ed-aspect' + (aspect === key ? ' is-on' : '')} onclick={() => (aspect = key)}>{lbl}</button>
              {/each}
            </div>
          </div>
          <div class="pk-ed-group">
            <h5>Rotate &amp; flip</h5>
            <div class="pk-ed-rotrow">
              <button class="pk-ed-rotbtn" onclick={() => (rot = (rot - 90) % 360)} title="Rotate left"><Icon name="rotate-ccw" size={17} /></button>
              <button class="pk-ed-rotbtn" onclick={() => (rot = (rot + 90) % 360)} title="Rotate right"><Icon name="rotate-cw" size={17} /></button>
              <button class={'pk-ed-rotbtn' + (flip ? ' is-on' : '')} onclick={() => (flip = !flip)} title="Flip horizontal"><Icon name="flip-horizontal-2" size={17} /></button>
            </div>
          </div>
          <div class="pk-ed-group">
            <h5>Straighten</h5>
            <div class="pk-ed-slider">
              <div class="pk-ed-slider-head"><span>Angle</span><span class={'pk-mono pk-ed-val' + (straighten !== 0 ? ' on' : '')}>{straighten > 0 ? '+' : ''}{straighten}°</span></div>
              <input type="range" min={-45} max={45} bind:value={straighten} style={`--pct:${pct(straighten, -45, 45)}%`} ondblclick={() => (straighten = 0)} />
            </div>
          </div>
        {:else if mode === 'light'}
          <div class="pk-ed-group">
            {#each LIGHT as [k, lbl] (k)}
              <div class="pk-ed-slider">
                <div class="pk-ed-slider-head"><span>{lbl}</span><span class={'pk-mono pk-ed-val' + (adj[k] !== 0 ? ' on' : '')}>{adj[k] > 0 ? '+' : ''}{adj[k]}</span></div>
                <input type="range" min={-100} max={100} bind:value={adj[k]} style={`--pct:${pct(adj[k], -100, 100)}%`} ondblclick={() => (adj[k] = 0)} />
              </div>
            {/each}
          </div>
        {:else if mode === 'color'}
          <div class="pk-ed-group">
            {#each COLOR as [k, lbl] (k)}
              <div class="pk-ed-slider">
                <div class="pk-ed-slider-head"><span>{lbl}</span><span class={'pk-mono pk-ed-val' + (adj[k] !== 0 ? ' on' : '')}>{adj[k] > 0 ? '+' : ''}{adj[k]}</span></div>
                <input type="range" min={-100} max={100} bind:value={adj[k]} style={`--pct:${pct(adj[k], -100, 100)}%`} ondblclick={() => (adj[k] = 0)} />
              </div>
            {/each}
          </div>
        {:else if mode === 'plugins'}
          {#if pluginsLoading}
            <div class="pk-ed-group"><div class="pk-ed-empty">Loading plugins…</div></div>
          {:else if !pluginOps || pluginOps.length === 0}
            <div class="pk-ed-group"><div class="pk-ed-empty">No editor plugins installed</div></div>
          {:else}
            {#each [...new Set(pluginOps.map((o) => o.plugin))] as plugin (plugin)}
              <div class="pk-ed-group">
                <h5>{plugin}</h5>
                {#each pluginOps.filter((o) => o.plugin === plugin) as op (opKey(op))}
                  <div class="pk-ed-plugin">
                    <div class="pk-ed-plugin-head">
                      <div>
                        <div class="pk-ed-plugin-title">{op.label}</div>
                        {#if op.description}<div class="pk-ed-plugin-desc">{op.description}</div>{/if}
                      </div>
                      <div class="pk-ed-plugin-actions">
                        <button
                          class="pk-btn pk-ed-plugin-apply"
                          onclick={() => applyPlugin(op)}
                          disabled={applying !== null || savingPlugin !== null}
                        >
                          {applying === opKey(op) ? 'Applying…' : 'Preview'}
                        </button>
                        <button
                          class="pk-btn pk-btn-primary pk-ed-plugin-apply"
                          onclick={() => savePlugin(op)}
                          disabled={applying !== null || savingPlugin !== null}
                        >
                          {savingPlugin === opKey(op) ? 'Saving…' : 'Save'}
                        </button>
                      </div>
                    </div>
                    {#if op.params.length}
                      <div class="pk-ed-plugin-params">
                        {#each op.params as p (p.name)}
                          <div class="pk-ed-field">
                            <label for={`pl-${opKey(op)}-${p.name}`}>{p.label}</label>
                            <input
                              id={`pl-${opKey(op)}-${p.name}`}
                              bind:value={pluginParams[opKey(op)][p.name]}
                            />
                          </div>
                        {/each}
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            {/each}
          {/if}
        {/if}
      </div>

      <div class="pk-ed-foot pk-mono">
        <Icon name="info" size={13} /> Edits are non-destructive — the original file is never modified
      </div>
    </div>
  </div>
</div>
