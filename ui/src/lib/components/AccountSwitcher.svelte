<script lang="ts">
  import Modal from './Modal.svelte';
  import Icon from '../icons/Icon.svelte';
  import type { User } from '../api';

  let {
    users,
    current,
    onPick,
    onLogout,
    onClose,
  }: {
    users: User[];
    current: string;
    onPick: (id: string) => void;
    onLogout: () => void;
    onClose: () => void;
  } = $props();
</script>

<Modal title="Account" sub="Admins can view the library as another user; everyone can sign out" icon="user-round" {onClose}>
  <div class="pk-list">
    {#each users as u (u.id)}
      <button class="pk-listrow" onclick={() => { onPick(u.id); onClose(); }}>
        <img src={u.avatar_url || 'https://i.pravatar.cc/64?img=12'} alt={u.name} />
        <div class="pk-listrow-main">
          <div class="pk-listrow-name">{u.name}</div>
          <div class="pk-listrow-sub">{u.email}</div>
        </div>
        {#if u.id === current}
          <span class="pk-pill is-on pk-listrow-act"><Icon name="check" size={12} />Current</span>
        {/if}
      </button>
    {/each}
  </div>
  <button class="pk-btn pk-btn-ghost" style="width:100%;margin-top:12px;justify-content:center" onclick={() => { onClose(); onLogout(); }}>
    <Icon name="log-out" size={15} />Sign out
  </button>
</Modal>
