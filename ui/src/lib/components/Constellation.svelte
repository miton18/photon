<script lang="ts" module>
  /* Shared with PeopleBrowse: the hue palette + relationship → layout weight. */
  export const CONST_HUES = ['#818cf8', '#2dd4bf', '#fbbf24', '#fb7185', '#38bdf8', '#c084fc', '#34d399', '#f472b6'];

  export function relWeight(t: string): number {
    const s = t.toLowerCase();
    if (/(partner|spouse|husband|wife)/.test(s)) return 3;
    if (/(mother|father|parent|son|daughter|child|grand)/.test(s)) return 2.4;
    if (/(brother|sister|sibling)/.test(s)) return 2;
    if (/friend/.test(s)) return 1.2;
    return 1;
  }
</script>

<script lang="ts">
  import type { Person } from '../api';

  let {
    people,
    selectedId = null,
    onSelect,
    onHover,
    portrait,
  }: {
    people: Person[];
    selectedId?: string | null;
    onSelect: (id: string | null) => void;
    onHover?: (id: string | null) => void;
    portrait: (p: Person) => string;
  } = $props();

  let canvasEl = $state<HTMLCanvasElement>();
  let wrapEl = $state<HTMLDivElement>();

  // A small mutable holder the render loop reads from, so selection/hover update
  // without re-mounting the canvas effect.
  const live: { selectedId: string | null; hover: string | null; focusOn?: (id: string) => void } = {
    selectedId: null,
    hover: null,
  };
  $effect(() => {
    live.selectedId = selectedId;
    if (selectedId && live.focusOn) live.focusOn(selectedId);
  });

  type Vec = { x: number; y: number; z: number };

  function buildGraph(ppl: Person[]) {
    const ids = new Set(ppl.map((p) => p.person_id));
    const seen = new Set<string>();
    const edges: { a: string; b: string; w: number }[] = [];
    for (const p of ppl) {
      for (const r of p.relationships ?? []) {
        if (!ids.has(r.person_id)) continue;
        const key = [p.person_id, r.person_id].sort().join('|');
        if (seen.has(key)) continue;
        seen.add(key);
        edges.push({ a: p.person_id, b: r.person_id, w: relWeight(r.relation) });
      }
    }
    // union-find → connected components
    const parent: Record<string, string> = {};
    ids.forEach((id) => (parent[id] = id));
    const find = (x: string): string => (parent[x] === x ? x : (parent[x] = find(parent[x])));
    edges.forEach((e) => (parent[find(e.a)] = find(e.b)));
    const groups: Record<string, string[]> = {};
    ids.forEach((id) => {
      const r = find(id);
      (groups[r] = groups[r] ?? []).push(id);
    });
    const comps = Object.values(groups).sort((a, b) => b.length - a.length);
    return { edges, comps };
  }

  function layout(comps: string[][], edges: { a: string; b: string; w: number }[]) {
    const pos: Record<string, Vec> = {};
    const R = 380;
    const nC = comps.length;
    const centroids: Vec[] = [];
    comps.forEach((members, ci) => {
      const phi = Math.acos(1 - 2 * ((ci + 0.5) / nC));
      const theta = Math.PI * (1 + Math.sqrt(5)) * ci;
      const cx = R * Math.sin(phi) * Math.cos(theta);
      const cy = R * Math.cos(phi) * 0.62;
      const cz = R * Math.sin(phi) * Math.sin(theta);
      members.forEach((id, mi) => {
        const rr = 60 + members.length * 5;
        const a = Math.PI * (1 + Math.sqrt(5)) * mi;
        const ph = Math.acos(1 - 2 * ((mi + 0.5) / members.length));
        pos[id] = {
          x: cx + rr * Math.sin(ph) * Math.cos(a),
          y: cy + rr * Math.sin(ph) * Math.sin(a) * 0.8,
          z: cz + rr * Math.cos(ph),
        };
      });
      centroids[ci] = { x: cx, y: cy, z: cz };
    });
    for (let it = 0; it < 140; it++) {
      comps.forEach((members) => {
        for (let i = 0; i < members.length; i++) {
          for (let j = i + 1; j < members.length; j++) {
            const A = pos[members[i]], B = pos[members[j]];
            const dx = A.x - B.x, dy = A.y - B.y, dz = A.z - B.z;
            const d2 = dx * dx + dy * dy + dz * dz + 0.01, d = Math.sqrt(d2);
            const rep = 1700 / d2;
            A.x += (dx / d) * rep; A.y += (dy / d) * rep; A.z += (dz / d) * rep;
            B.x -= (dx / d) * rep; B.y -= (dy / d) * rep; B.z -= (dz / d) * rep;
          }
        }
      });
      edges.forEach((e) => {
        const A = pos[e.a], B = pos[e.b];
        if (!A || !B) return;
        const dx = B.x - A.x, dy = B.y - A.y, dz = B.z - A.z;
        const d = Math.sqrt(dx * dx + dy * dy + dz * dz) + 0.01;
        const target = 132 - e.w * 14;
        const f = (d - target) * 0.022;
        A.x += (dx / d) * f; A.y += (dy / d) * f; A.z += (dz / d) * f;
        B.x -= (dx / d) * f; B.y -= (dy / d) * f; B.z -= (dz / d) * f;
      });
    }
    comps.forEach((members, ci) => {
      let cx = 0, cy = 0, cz = 0;
      members.forEach((id) => { cx += pos[id].x; cy += pos[id].y; cz += pos[id].z; });
      centroids[ci] = { x: cx / members.length, y: cy / members.length, z: cz / members.length };
    });
    return { pos, centroids };
  }

  function makeStars(n: number) {
    const s: { x: number; y: number; z: number; m: number; tw: number }[] = [];
    for (let i = 0; i < n; i++) {
      const ph = Math.acos(1 - 2 * Math.random()), th = Math.random() * Math.PI * 2;
      const r = 900 + Math.random() * 700;
      s.push({
        x: r * Math.sin(ph) * Math.cos(th), y: r * Math.sin(ph) * Math.sin(th), z: r * Math.cos(ph),
        m: 0.3 + Math.random() * 0.9, tw: Math.random() * Math.PI * 2,
      });
    }
    return s;
  }

  // (Re)mount the render loop whenever the set of people changes.
  $effect(() => {
    const canvas = canvasEl, wrap = wrapEl;
    if (!canvas || !wrap) return;
    const ppl = people;
    const ctx = canvas.getContext('2d')!;

    const g = buildGraph(ppl);
    const { pos, centroids } = layout(g.comps, g.edges);
    const hueOf: Record<string, string> = {};
    g.comps.forEach((m, ci) => m.forEach((id) => (hueOf[id] = CONST_HUES[ci % CONST_HUES.length])));

    const imgs: Record<string, HTMLImageElement> = {};
    ppl.forEach((p) => { const im = new Image(); im.src = portrait(p); imgs[p.person_id] = im; });
    const stars = makeStars(260);

    const cam = { yaw: 0.5, pitch: -0.18, dist: 820, tYaw: 0.5, tPitch: -0.18, tDist: 820 };
    const S = { drag: false, moved: false, lastX: 0, lastY: 0, idle: 0, hits: [] as { id: string; sx: number; sy: number; r: number; viewZ: number }[], t0: performance.now(), raf: 0, w: 0, h: 0, dpr: 1 };

    const focal = 720;
    function resize() {
      const r = wrap.getBoundingClientRect();
      S.w = r.width; S.h = r.height; S.dpr = Math.min(2, window.devicePixelRatio || 1);
      canvas.width = S.w * S.dpr; canvas.height = S.h * S.dpr;
      canvas.style.width = S.w + 'px'; canvas.style.height = S.h + 'px';
      ctx.setTransform(S.dpr, 0, 0, S.dpr, 0, 0);
    }
    resize();
    const ro = new ResizeObserver(resize); ro.observe(wrap);

    function project(p: Vec) {
      const cy = Math.cos(cam.yaw), sy = Math.sin(cam.yaw);
      const x = p.x * cy - p.z * sy;
      let z = p.x * sy + p.z * cy;
      const cp = Math.cos(cam.pitch), sp = Math.sin(cam.pitch);
      const y = p.y * cp - z * sp;
      z = p.y * sp + z * cp;
      const viewZ = cam.dist - z;
      if (viewZ < 60) return null;
      const f = focal / viewZ;
      return { sx: S.w / 2 + x * f, sy: S.h / 2 - y * f, scale: f, viewZ };
    }

    function frame() {
      const now = performance.now(), time = (now - S.t0) / 1000;
      cam.yaw += (cam.tYaw - cam.yaw) * 0.12;
      cam.pitch += (cam.tPitch - cam.pitch) * 0.12;
      cam.dist += (cam.tDist - cam.dist) * 0.10;
      S.idle += 16;
      if (!S.drag && S.idle > 2600) cam.tYaw += 0.0016;

      ctx.clearRect(0, 0, S.w, S.h);
      const bg = ctx.createRadialGradient(S.w * 0.5, S.h * 0.42, 40, S.w * 0.5, S.h * 0.5, Math.max(S.w, S.h) * 0.75);
      bg.addColorStop(0, '#15122a'); bg.addColorStop(0.5, '#0c0a16'); bg.addColorStop(1, '#06050c');
      ctx.fillStyle = bg; ctx.fillRect(0, 0, S.w, S.h);

      for (const st of stars) {
        const pr = project(st);
        if (!pr) continue;
        const tw = 0.45 + 0.55 * (0.5 + 0.5 * Math.sin(time * 1.6 + st.tw));
        const a = Math.min(1, pr.scale * 1.4) * st.m * tw * 0.9;
        if (a < 0.03) continue;
        ctx.globalAlpha = a; ctx.fillStyle = '#fff';
        const sz = Math.max(0.5, pr.scale * 1.1 * st.m);
        ctx.fillRect(pr.sx, pr.sy, sz, sz);
      }
      ctx.globalAlpha = 1;

      ctx.globalCompositeOperation = 'lighter';
      centroids.forEach((c, ci) => {
        const pr = project(c); if (!pr) return;
        const hue = CONST_HUES[ci % CONST_HUES.length];
        const rad = 120 * pr.scale * 1.6 + 40;
        const gr = ctx.createRadialGradient(pr.sx, pr.sy, 0, pr.sx, pr.sy, rad);
        gr.addColorStop(0, hue + '30'); gr.addColorStop(0.5, hue + '12'); gr.addColorStop(1, hue + '00');
        ctx.fillStyle = gr; ctx.beginPath(); ctx.arc(pr.sx, pr.sy, rad, 0, Math.PI * 2); ctx.fill();
      });
      ctx.globalCompositeOperation = 'source-over';

      const sel = live.selectedId, hov = live.hover;
      const focusId = hov || sel;
      const neighbors = new Set<string>();
      if (focusId) {
        neighbors.add(focusId);
        g.edges.forEach((e) => { if (e.a === focusId) neighbors.add(e.b); if (e.b === focusId) neighbors.add(e.a); });
      }

      g.edges.forEach((e) => {
        const A = project(pos[e.a]), B = project(pos[e.b]);
        if (!A || !B) return;
        const hue = hueOf[e.a];
        const near = (A.scale + B.scale) / 2;
        let alpha = Math.min(0.5, near * 1.1) * (0.5 + e.w * 0.14);
        let width = Math.max(0.6, near * (1 + e.w * 0.5));
        const active = !!focusId && neighbors.has(e.a) && neighbors.has(e.b) && (e.a === focusId || e.b === focusId);
        if (focusId && !active) alpha *= 0.18;
        if (active) { alpha = Math.min(0.95, alpha * 2.4); width *= 1.7; }
        ctx.globalAlpha = alpha; ctx.strokeStyle = hue; ctx.lineWidth = width;
        if (active) { ctx.shadowColor = hue; ctx.shadowBlur = 12; }
        ctx.beginPath(); ctx.moveTo(A.sx, A.sy); ctx.lineTo(B.sx, B.sy); ctx.stroke();
        ctx.shadowBlur = 0;
      });
      ctx.globalAlpha = 1;

      const nodes = ppl.map((p) => ({ p, pr: project(pos[p.person_id]) })).filter((n) => n.pr) as { p: Person; pr: NonNullable<ReturnType<typeof project>> }[];
      nodes.sort((a, b) => b.pr.viewZ - a.pr.viewZ);
      S.hits = [];
      nodes.forEach(({ p, pr }) => {
        const hue = hueOf[p.person_id];
        const r = Math.max(7, 30 * pr.scale);
        const isSel = p.person_id === sel, isHov = p.person_id === hov;
        const dim = !!focusId && !neighbors.has(p.person_id);
        const alpha = dim ? 0.32 : 1;
        S.hits.push({ id: p.person_id, sx: pr.sx, sy: pr.sy, r, viewZ: pr.viewZ });

        ctx.save();
        ctx.globalAlpha = alpha;
        if (isSel || isHov) { ctx.shadowColor = hue; ctx.shadowBlur = 22; }
        ctx.beginPath(); ctx.arc(pr.sx, pr.sy, r, 0, Math.PI * 2);
        ctx.fillStyle = '#0c0a14'; ctx.fill();
        ctx.shadowBlur = 0;
        ctx.save(); ctx.beginPath(); ctx.arc(pr.sx, pr.sy, r - 1, 0, Math.PI * 2); ctx.clip();
        const im = imgs[p.person_id];
        if (im && im.complete && im.naturalWidth) ctx.drawImage(im, pr.sx - r, pr.sy - r, r * 2, r * 2);
        else { ctx.fillStyle = hue; ctx.fillRect(pr.sx - r, pr.sy - r, r * 2, r * 2); }
        ctx.restore();
        ctx.lineWidth = Math.max(1.3, r * (isSel ? 0.16 : 0.085));
        ctx.strokeStyle = hue;
        ctx.beginPath(); ctx.arc(pr.sx, pr.sy, r, 0, Math.PI * 2); ctx.stroke();
        ctx.restore();

        const showLabel = !dim && (r > 16 || isSel || isHov);
        if (showLabel && p.name) {
          const fs = Math.max(10, Math.min(15, r * 0.5));
          ctx.globalAlpha = alpha * (isSel || isHov ? 1 : Math.min(1, (r - 12) / 8));
          ctx.font = `${isSel || isHov ? 600 : 500} ${fs}px Geist, system-ui, sans-serif`;
          ctx.textAlign = 'center'; ctx.textBaseline = 'top';
          const ly = pr.sy + r + 5;
          ctx.lineWidth = 3; ctx.strokeStyle = 'rgba(6,5,12,0.85)';
          ctx.strokeText(p.name, pr.sx, ly);
          ctx.fillStyle = '#ECE9F6'; ctx.fillText(p.name, pr.sx, ly);
          ctx.globalAlpha = 1;
        }
      });

      S.raf = requestAnimationFrame(frame);
    }
    S.raf = requestAnimationFrame(frame);

    function pick(mx: number, my: number): string | null {
      let best: string | null = null, bd = 999;
      for (const h of S.hits) {
        const d = Math.hypot(mx - h.sx, my - h.sy);
        if (d < h.r + 4 && d < bd) { bd = d; best = h.id; }
      }
      return best;
    }
    function onDown(e: PointerEvent) {
      S.drag = true; S.moved = false; S.lastX = e.clientX; S.lastY = e.clientY; S.idle = 0;
      canvas.setPointerCapture?.(e.pointerId);
    }
    function onMove(e: PointerEvent) {
      const r = canvas.getBoundingClientRect();
      const mx = e.clientX - r.left, my = e.clientY - r.top;
      if (S.drag) {
        const dx = e.clientX - S.lastX, dy = e.clientY - S.lastY;
        if (Math.abs(dx) + Math.abs(dy) > 3) S.moved = true;
        cam.tYaw += dx * 0.006; cam.tPitch += dy * 0.006;
        cam.tPitch = Math.max(-1.2, Math.min(1.2, cam.tPitch));
        S.lastX = e.clientX; S.lastY = e.clientY; S.idle = 0;
      } else {
        const id = pick(mx, my);
        if (id !== live.hover) { live.hover = id; onHover?.(id); canvas.style.cursor = id ? 'pointer' : 'grab'; }
      }
    }
    function onUp(e: PointerEvent) {
      if (S.drag && !S.moved) {
        const r = canvas.getBoundingClientRect();
        const id = pick(e.clientX - r.left, e.clientY - r.top);
        if (id) focusOn(id); else onSelect(null);
      }
      S.drag = false; S.idle = 0;
    }
    function onWheel(e: WheelEvent) {
      e.preventDefault();
      cam.tDist = Math.max(260, Math.min(1500, cam.tDist + e.deltaY * 0.6));
      S.idle = 0;
    }
    function focusOn(id: string) {
      onSelect(id);
      const p = pos[id]; if (!p) return;
      cam.tYaw = Math.atan2(p.x, p.z);
      cam.tPitch = Math.max(-1.2, Math.min(1.2, Math.atan2(p.y, Math.hypot(p.x, p.z))));
      cam.tDist = 440; S.idle = -6000;
    }
    live.focusOn = focusOn;

    canvas.addEventListener('pointerdown', onDown);
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    canvas.addEventListener('wheel', onWheel, { passive: false });
    canvas.style.cursor = 'grab';

    return () => {
      cancelAnimationFrame(S.raf); ro.disconnect();
      canvas.removeEventListener('pointerdown', onDown);
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      canvas.removeEventListener('wheel', onWheel);
      live.focusOn = undefined;
    };
  });
</script>

<div class="pk-cos-canvas-wrap" bind:this={wrapEl}>
  <canvas bind:this={canvasEl} class="pk-cos-canvas"></canvas>
</div>

<style>
  .pk-cos-canvas-wrap { position: absolute; inset: 0; }
  .pk-cos-canvas { display: block; width: 100%; height: 100%; touch-action: none; }
</style>
