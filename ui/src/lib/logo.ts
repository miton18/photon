/* Photon — logo marks.
 *
 * A faithful port of the `explorations/Logo Directions.html` design exploration:
 * ten parametric, geometry-only marks that survive from a 92px hero down to a
 * 16px favicon. Each generator returns the INNER markup of a `0 0 100 100`
 * viewBox; `logoSvg()` wraps it in an `<svg>`. `c` is the primary color, `c2`
 * (a.k.a. `bg`) the secondary/knock-out tint used to flip for dark surfaces.
 *
 * The default mark is `photon` — the exploration's conceptual hero (a light
 * particle orbiting a nucleus): it speaks to the product name, is the most
 * ownable, and renders as a single clean color on any background, so it doubles
 * as a theme-adaptive brand mark (pass `currentColor`). */

export type LogoMark =
  | 'photon'
  | 'iris'
  | 'hexalens'
  | 'apertureP'
  | 'pixel'
  | 'beam'
  | 'camera'
  | 'lensTile'
  | 'vintage'
  | 'vintageLine';

/** Spec brand color (indigo) — the exploration's accent. */
export const PHOTON_INDIGO = '#6366F1';
export const PHOTON_INK = '#0E121B';

const pt = (cx: number, cy: number, r: number, deg: number): [number, number] => [
  cx + r * Math.cos((deg / 180) * Math.PI),
  cy + r * Math.sin((deg / 180) * Math.PI),
];
const f = (n: number) => Math.round(n * 100) / 100;

// Unique-id counter so multiple marks on a page don't collide on gradient/clip ids.
let _uid = 0;
const uid = (p: string) => `${p}${(_uid = (_uid + 1) % 1e6)}`;

// The six aperture blades only (no center fill). Used both standalone (over a
// real mask hole, where the center should stay transparent) and by `irisAt`.
function irisBlades(cx: number, cy: number, R: number, r: number, c: string, swirl: number) {
  let blades = '';
  for (let k = 0; k < 6; k++) {
    const a = k * 60;
    const o1 = pt(cx, cy, R, a),
      o2 = pt(cx, cy, R, a + 60);
    const i2 = pt(cx, cy, r, a + 60 + swirl),
      i1 = pt(cx, cy, r, a + swirl);
    blades += `<path d="M${f(o1[0])} ${f(o1[1])} L${f(o2[0])} ${f(o2[1])} L${f(i2[0])} ${f(i2[1])} L${f(i1[0])} ${f(i1[1])} Z" fill="${c}" opacity="${k % 2 ? 0.78 : 1}"/>`;
  }
  return blades;
}

// shared iris core: hexagonal 6-blade aperture with a `bg`-filled center opening.
// (Used by marks rendered on an OPAQUE background — `bg` must be a solid color.)
function irisAt(cx: number, cy: number, R: number, r: number, c: string, bg: string, swirl: number) {
  let hex = '';
  for (let k = 0; k < 6; k++) {
    const p = pt(cx, cy, r, k * 60 + swirl);
    hex += `${k ? 'L' : 'M'}${f(p[0])} ${f(p[1])} `;
  }
  return `${irisBlades(cx, cy, R, r, c, swirl)}<path d="${hex}Z" fill="${bg}"/>`;
}

// 1. IRIS — hexagonal 6-blade aperture; straight blades + swirled hexagon opening
function iris(c: string, bg: string) {
  return `<g>${irisAt(50, 50, 46, 21, c, bg, -26)}</g>`;
}

// 2. PHOTON — orbit ring + light particle with motion trail + nucleus
function photon(c: string) {
  const cx = 50,
    cy = 50,
    R = 34,
    id = uid('ptr');
  const start = pt(cx, cy, R, -150),
    end = pt(cx, cy, R, -18);
  return `
    <defs><linearGradient id="${id}" x1="0" x2="1" y1="0.2" y2="0.1">
      <stop offset="0" stop-color="${c}" stop-opacity="0"/><stop offset="1" stop-color="${c}" stop-opacity="1"/>
    </linearGradient></defs>
    <circle cx="${cx}" cy="${cy}" r="${R}" fill="none" stroke="${c}" stroke-width="6" stroke-opacity="0.22"/>
    <path d="M${f(start[0])} ${f(start[1])} A${R} ${R} 0 0 1 ${f(end[0])} ${f(end[1])}" fill="none" stroke="url(#${id})" stroke-width="6" stroke-linecap="round"/>
    <circle cx="${cx}" cy="${cy}" r="8.5" fill="${c}"/>
    <circle cx="${f(end[0])}" cy="${f(end[1])}" r="8.5" fill="${c}"/>
  `;
}

// 3. HEXALENS — hexagon lens split into light facets (mono, faceted)
function hexalens(c: string) {
  const cx = 50,
    cy = 50,
    R = 44;
  let hex = '';
  const V: [number, number][] = [];
  for (let k = 0; k < 6; k++) {
    const p = pt(cx, cy, R, k * 60 - 90);
    V.push(p);
    hex += `${k ? 'L' : 'M'}${f(p[0])} ${f(p[1])} `;
  }
  const facets = [
    `<path d="M${cx} ${cy} L${f(V[0][0])} ${f(V[0][1])} L${f(V[1][0])} ${f(V[1][1])} Z" fill="${c}"/>`,
    `<path d="M${cx} ${cy} L${f(V[2][0])} ${f(V[2][1])} L${f(V[3][0])} ${f(V[3][1])} Z" fill="${c}" opacity="0.5"/>`,
    `<path d="M${cx} ${cy} L${f(V[4][0])} ${f(V[4][1])} L${f(V[5][0])} ${f(V[5][1])} Z" fill="${c}" opacity="0.75"/>`,
  ].join('');
  return `<g>${facets}<path d="${hex}Z" fill="none" stroke="${c}" stroke-width="6" stroke-linejoin="round"/></g>`;
}

// 4. APERTURE-P — geometric P monogram whose bowl is an aperture opening
function apertureP(c: string) {
  return `
    <rect x="22" y="14" width="13" height="72" rx="3" fill="${c}"/>
    <path d="M35 14 H56 a25 25 0 0 1 0 50 H35 V51 H54 a12 12 0 0 0 0-24 H35 Z" fill="${c}"/>
    <circle cx="53" cy="39" r="6.5" fill="${c}"/>
  `;
}

// 5. PIXEL — rounded tile, aperture dot formed within a subtle pixel grid
function pixel(c: string) {
  const cx = 50,
    cy = 50;
  let g = '';
  for (let i = 1; i < 5; i++) {
    g += `<line x1="${i * 20}" y1="0" x2="${i * 20}" y2="100" stroke="${c}" stroke-opacity="0.12" stroke-width="2"/><line x1="0" y1="${i * 20}" x2="100" y2="${i * 20}" stroke="${c}" stroke-opacity="0.12" stroke-width="2"/>`;
  }
  const blades = [
    [50, 18],
    [82, 50],
    [50, 82],
    [18, 50],
  ]
    .map(
      (p, i) =>
        `<rect x="${p[0] - 11}" y="${p[1] - 11}" width="22" height="22" rx="6" fill="${c}" opacity="${0.55 + 0.15 * (i % 2)}"/>`,
    )
    .join('');
  return `<g>${g}${blades}<circle cx="${cx}" cy="${cy}" r="13" fill="${c}"/></g>`;
}

// 6. BEAM — lens disc sliced by a diagonal beam of light (negative space)
function beam(c: string, c2: string) {
  const id = uid('bc');
  return `
    <defs><clipPath id="${id}"><circle cx="50" cy="50" r="42"/></clipPath></defs>
    <g clip-path="url(#${id})">
      <circle cx="50" cy="50" r="42" fill="${c}"/>
      <path d="M-10 64 L66 -12 L84 6 L8 82 Z" fill="${c2}" opacity="0.92"/>
      <path d="M20 96 L96 20 L106 30 L30 106 Z" fill="${c2}" opacity="0.5"/>
    </g>
  `;
}

// 7. CAMERA — monoline retro-camera badge (old-Instagram genre, reinterpreted)
function camera(c: string) {
  return `
    <rect x="10" y="14" width="80" height="72" rx="22" fill="none" stroke="${c}" stroke-width="7"/>
    <circle cx="50" cy="50" r="19" fill="none" stroke="${c}" stroke-width="7"/>
    <circle cx="50" cy="50" r="6.5" fill="${c}"/>
    <circle cx="72" cy="30" r="4.6" fill="${c}"/>
  `;
}

// 8. LENS TILE — filled app-icon badge; lens + flash knocked out (the photon)
function lensTile(c: string, bg: string) {
  return `
    <rect x="8" y="12" width="84" height="76" rx="24" fill="${c}"/>
    <circle cx="50" cy="50" r="22" fill="${bg}"/>
    <circle cx="50" cy="50" r="12.5" fill="${c}"/>
    <circle cx="73" cy="29" r="5.2" fill="${bg}"/>
  `;
}

// 9. VINTAGE — flat take on the original skeuomorphic camera badge ("Original
// camera"). The lens + flash are TRUE holes (an SVG mask), so the opening shows
// whatever surface is behind it on any theme — `bg` only matters when it's an
// opaque color you actually want painted into the openings (e.g. a white-lens
// favicon); when `bg` is transparent the holes stay transparent.
function vintage(c: string, bg: string) {
  const opaque = bg !== 'transparent' && bg !== 'none' && bg !== '';
  const m = uid('vgm');
  // Body with lens + flash punched out as real holes.
  const body = `
    <defs><mask id="${m}">
      <rect width="100" height="100" fill="#fff"/>
      <circle cx="50" cy="53" r="22" fill="#000"/>
      <circle cx="74" cy="30" r="5" fill="#000"/>
    </mask></defs>
    <g mask="url(#${m})">
      <path d="M34 16 L40 5 L60 5 L66 16 Z" fill="${c}"/>
      <rect x="8" y="14" width="84" height="72" rx="20" fill="${c}"/>
    </g>`;
  // When the caller wants an opaque lens/flash (favicon), paint discs back in.
  const fills = opaque
    ? `<circle cx="50" cy="53" r="22" fill="${bg}"/><circle cx="74" cy="30" r="5" fill="${bg}"/>`
    : '';
  return `${body}${fills}${irisBlades(50, 53, 20, 8.4, c, -26)}`;
}

// 10. VINTAGE LINE — monoline camera with the stripe (lighter, friendlier)
function vintageLine(c: string) {
  return `
    <rect x="10" y="16" width="80" height="68" rx="20" fill="none" stroke="${c}" stroke-width="7"/>
    <circle cx="50" cy="54" r="18" fill="none" stroke="${c}" stroke-width="7"/>
    <circle cx="50" cy="54" r="6" fill="${c}"/>
    <line x1="24" y1="30" x2="44" y2="30" stroke="${c}" stroke-width="6" stroke-linecap="round"/>
    <circle cx="71" cy="30" r="4.4" fill="${c}"/>
  `;
}

const GENERATORS: Record<LogoMark, (c: string, bg: string) => string> = {
  photon: (c) => photon(c),
  iris: (c, bg) => iris(c, bg),
  hexalens: (c) => hexalens(c),
  apertureP: (c) => apertureP(c),
  pixel: (c) => pixel(c),
  beam: (c, bg) => beam(c, bg),
  camera: (c) => camera(c),
  lensTile: (c, bg) => lensTile(c, bg),
  vintage: (c, bg) => vintage(c, bg),
  vintageLine: (c) => vintageLine(c),
};

/** Human labels for each mark (for a picker / design-system card). */
export const LOGO_MARKS: { id: LogoMark; label: string }[] = [
  { id: 'photon', label: 'Photon' },
  { id: 'iris', label: 'Iris' },
  { id: 'hexalens', label: 'Hexalens' },
  { id: 'apertureP', label: 'Aperture-P' },
  { id: 'pixel', label: 'Pixel iris' },
  { id: 'beam', label: 'Beam' },
  { id: 'camera', label: 'Camera' },
  { id: 'lensTile', label: 'Lens tile' },
  { id: 'vintage', label: 'Vintage' },
  { id: 'vintageLine', label: 'Vintage line' },
];

export interface LogoOpts {
  /** Primary mark color. Defaults to `currentColor` so the mark inherits CSS. */
  color?: string;
  /** Secondary / knock-out tint for marks that flip on dark surfaces. */
  bg?: string;
  size?: number;
}

/** Render a complete `<svg>` string for the given mark. */
export function logoSvg(mark: LogoMark = 'photon', opts: LogoOpts = {}): string {
  const { color = 'currentColor', bg = '#0E121B', size } = opts;
  const gen = GENERATORS[mark] ?? GENERATORS.photon;
  const dim = size ? `width="${size}" height="${size}" ` : '';
  return `<svg ${dim}viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">${gen(color, bg)}</svg>`;
}
