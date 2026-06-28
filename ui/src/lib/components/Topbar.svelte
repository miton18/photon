<script lang="ts">
  import Icon from '../icons/Icon.svelte';

  let {
    title = 'Photos',
    search = $bindable(''),
    onUpload,
  }: {
    title?: string;
    search?: string;
    onUpload: () => void;
  } = $props();
</script>

<header class="pk-topbar">
  <span class="pk-title">{title}</span>
  <div class="pk-search">
    <Icon name="search" size={16} />
    <input placeholder="Search people, places, things…" bind:value={search} />
    <kbd>/</kbd>
  </div>
  <div class="pk-top-actions">
    <button class="pk-btn pk-btn-primary" onclick={onUpload}>
      <Icon name="upload" size={16} /> Upload
    </button>
  </div>
</header>
