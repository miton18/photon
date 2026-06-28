<script lang="ts">
  import L from 'leaflet';
  import 'leaflet/dist/leaflet.css';
  import Icon from '../icons/Icon.svelte';
  import { thumb, type UIPhoto } from '../media';

  let { photos, onOpenCity }: { photos: UIPhoto[]; onOpenCity: (city: string) => void } = $props();

  let el = $state<HTMLDivElement | null>(null);
  let failed = $state(false);

  // "45.7640° N" / "4.8357° E" → signed decimal degrees.
  function parseCoord(s: string | null | undefined): number | null {
    if (!s) return null;
    const m = s.match(/-?\d+(\.\d+)?/);
    if (!m) return null;
    let v = parseFloat(m[0]);
    if (/[SW]/i.test(s)) v = -Math.abs(v);
    return Number.isFinite(v) ? v : null;
  }

  interface Place { city: string; lat: number; lng: number; count: number; seed: number }
  const places = $derived.by<Place[]>(() => {
    const by = new Map<string, Place>();
    for (const p of photos) {
      const lat = parseCoord(p.lat);
      const lng = parseCoord(p.lng);
      if (lat == null || lng == null || !p.city) continue;
      const g = by.get(p.city);
      if (g) g.count++;
      else by.set(p.city, { city: p.city, lat, lng, count: 1, seed: p.seed });
    }
    return [...by.values()];
  });

  $effect(() => {
    const container = el;
    const list = places;
    if (!container) return;
    let map: L.Map;
    try {
      map = L.map(container, { attributionControl: true });
      L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
        maxZoom: 19,
        attribution: '© OpenStreetMap contributors',
      }).addTo(map);

      const pts: [number, number][] = [];
      const accent =
        getComputedStyle(document.documentElement).getPropertyValue('--accent').trim() || '#6366F1';
      for (const pl of list) {
        pts.push([pl.lat, pl.lng]);
        const marker = L.circleMarker([pl.lat, pl.lng], {
          radius: 8 + Math.min(12, Math.log2(pl.count + 1) * 2),
          color: '#fff',
          weight: 2,
          fillColor: accent,
          fillOpacity: 0.85,
        }).addTo(map);
        marker.bindPopup(
          `<div style="display:flex;gap:8px;align-items:center">
             <img src="${thumb(pl.seed)}" width="44" height="44" style="border-radius:6px;object-fit:cover"/>
             <div><b>${pl.city}</b><br>${pl.count} photo${pl.count === 1 ? '' : 's'}</div>
           </div>`,
        );
        marker.on('click', () => onOpenCity(pl.city));
      }
      if (pts.length > 1) map.fitBounds(pts, { padding: [48, 48] });
      else if (pts.length === 1) map.setView(pts[0], 11);
      else map.setView([46.6, 2.4], 4);
      // Leaflet needs a layout tick once the container has its real size.
      setTimeout(() => map.invalidateSize(), 0);
      failed = false;
    } catch (e) {
      failed = true;
      return;
    }
    return () => map.remove();
  });
</script>

{#if failed}
  <div class="pk-map-fallback">
    <Icon name="map-pin-off" size={28} />
    <div>Map failed to initialise.</div>
    <div class="pk-map-fallback-sub">{places.length} place{places.length === 1 ? '' : 's'} with coordinates.</div>
  </div>
{:else}
  <div class="pk-map" bind:this={el}></div>
{/if}

<style>
  .pk-map { flex: 1; min-height: 0; }
  :global(.pk-map.leaflet-container) { background: var(--bg-subtle); font-family: var(--font-sans); }
  .pk-map-fallback {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    color: var(--text-faint);
    text-align: center;
    padding: 40px;
  }
  .pk-map-fallback-sub { font-size: var(--text-xs); }
</style>
