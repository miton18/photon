<script lang="ts">
  /* Photon icon — renders a Lucide icon by name (Lucide is the brand icon set:
     1.75–2px stroke, round caps/joins). Mirrors the design-kit Icon component. */
  import { icons } from 'lucide';

  let {
    name,
    size = 16,
    strokeWidth = 2,
    fill = 'none',
    color = undefined,
    class: klass = '',
    spin = false,
  }: {
    name: string;
    size?: number;
    strokeWidth?: number;
    fill?: string;
    color?: string;
    class?: string;
    spin?: boolean;
  } = $props();

  function pascal(n: string) {
    return n
      .split(/[-_]/)
      .map((s) => s.charAt(0).toUpperCase() + s.slice(1))
      .join('');
  }

  const node = $derived((icons as Record<string, any>)[pascal(name)] ?? (icons as any)[name]);
  const kids = $derived(Array.isArray(node) ? node : (node?.iconNode ?? []));
</script>

<svg
  class={'pk-ic' + (spin ? ' pk-spin' : '') + (klass ? ' ' + klass : '')}
  width={size}
  height={size}
  viewBox="0 0 24 24"
  {fill}
  stroke="currentColor"
  stroke-width={strokeWidth}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  style={color ? `color:${color}` : undefined}
>
  {#each kids as [tag, attrs] (tag + JSON.stringify(attrs))}
    <svelte:element this={tag} {...attrs} />
  {/each}
</svg>
