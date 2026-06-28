<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import type { Snippet } from 'svelte';

  let {
    title,
    sub = '',
    icon = 'settings',
    onClose,
    children,
    footer,
  }: {
    title: string;
    sub?: string;
    icon?: string;
    onClose: () => void;
    children: Snippet;
    footer?: Snippet;
  } = $props();

  // Escape closes the modal (global listener so it works without focus).
  $effect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

<div
  class="pk-modal-scrim"
  role="button"
  tabindex="-1"
  onclick={(e) => { if (e.target === e.currentTarget) onClose(); }}
  onkeydown={(e) => { if (e.key === 'Escape') onClose(); }}
>
  <div class="pk-modal" role="dialog" aria-modal="true" aria-label={title}>
    <div class="pk-modal-head">
      <Icon name={icon} size={18} />
      <div>
        <div class="pk-modal-title">{title}</div>
        {#if sub}<div class="pk-modal-sub">{sub}</div>{/if}
      </div>
      <button class="pk-iconbtn pk-modal-close" onclick={onClose} aria-label="Close"><Icon name="x" size={17} /></button>
    </div>
    <div class="pk-modal-body">{@render children()}</div>
    {#if footer}<div class="pk-modal-foot">{@render footer()}</div>{/if}
  </div>
</div>
