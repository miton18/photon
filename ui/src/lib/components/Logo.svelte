<script lang="ts">
  /* Photon brand mark. Renders any of the ten logo directions (see `lib/logo.ts`),
     defaulting to the `photon` hero. Inherits `currentColor` unless `color` is
     given, so a placement controls the mark color via CSS and it adapts to theme. */
  import { logoSvg, type LogoMark } from '../logo';

  let {
    mark = 'vintage' as LogoMark,
    size = 24,
    color = undefined as string | undefined,
    // Knock-out tint for filled marks (lens/flash). Defaults to `transparent` so a
    // self-contained badge like `vintage` shows the surface behind it — theme-proof.
    bg = 'transparent',
    title = 'Photon',
  }: {
    mark?: LogoMark;
    size?: number;
    color?: string;
    bg?: string;
    title?: string;
  } = $props();

  const svg = $derived(logoSvg(mark, { color, bg, size }));
</script>

<span
  class="pk-logo"
  style={`width:${size}px;height:${size}px`}
  role="img"
  aria-label={title}
>{@html svg}</span>

<style>
  .pk-logo {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 0;
    flex: none;
  }
  .pk-logo :global(svg) {
    width: 100%;
    height: 100%;
    display: block;
  }
</style>
