<script lang="ts">
  import Icon from '../icons/Icon.svelte';
  import { toasts, dismiss, type Tone } from '../toast.svelte';

  const TONES: Record<Tone, { icon: string; color: string }> = {
    default: { icon: 'bell', color: 'var(--text-muted)' },
    success: { icon: 'circle-check', color: 'var(--success)' },
    error: { icon: 'circle-alert', color: 'var(--danger)' },
    info: { icon: 'info', color: 'var(--info)' },
    loading: { icon: 'loader-circle', color: 'var(--accent)' },
  };
</script>

<div class="pk-toast-stack">
  {#each toasts as t (t.id)}
    <div class={'pk-toast' + (t.leaving ? ' is-leaving' : '')} role="status">
      <Icon
        name={t.icon || TONES[t.tone].icon}
        size={18}
        spin={t.tone === 'loading'}
        color={TONES[t.tone].color}
        class="pk-toast-lead"
      />
      <div class="pk-toast-body">
        {#if t.title}<div class="pk-toast-title">{t.title}</div>{/if}
        {#if t.message}<div class="pk-toast-msg">{t.message}</div>{/if}
      </div>
      {#if t.actionLabel}
        <button class="pk-toast-action" onclick={() => { t.onAction?.(); dismiss(t.id); }}>
          {t.actionLabel}
        </button>
      {/if}
      <button class="pk-toast-close" aria-label="Dismiss" onclick={() => dismiss(t.id)}>
        <Icon name="x" size={15} />
      </button>
    </div>
  {/each}
</div>

<style>
  :global(.pk-toast-lead) { flex: none; margin-top: 1px; }
</style>
