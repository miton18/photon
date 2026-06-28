<script lang="ts">
  import Sidebar from './lib/components/Sidebar.svelte';
  import Topbar from './lib/components/Topbar.svelte';
  import Feed from './lib/components/Feed.svelte';
  import StatusBar from './lib/components/StatusBar.svelte';
  import Lightbox from './lib/components/Lightbox.svelte';
  import Toaster from './lib/components/Toaster.svelte';
  import AccountSwitcher from './lib/components/AccountSwitcher.svelte';
  import SharingModal from './lib/components/SharingModal.svelte';
  import StorageModal from './lib/components/StorageModal.svelte';
  import VaultModal from './lib/components/VaultModal.svelte';
  import UploadPanel from './lib/components/UploadPanel.svelte';
  import AdminConsole from './lib/components/AdminConsole.svelte';
  import AddPhotosModal from './lib/components/AddPhotosModal.svelte';
  import AlbumPickerModal from './lib/components/AlbumPickerModal.svelte';
  import PlacesMap from './lib/components/PlacesMap.svelte';
  import Explore from './lib/components/Explore.svelte';
  import LoginScreen from './lib/components/LoginScreen.svelte';
  import PasskeyModal from './lib/components/PasskeyModal.svelte';
  import PeopleStudio from './lib/components/PeopleStudio.svelte';
  import PeopleBrowse from './lib/components/PeopleBrowse.svelte';
  import { getPasskey, passkeysSupported, platformAuthenticatorAvailable } from './lib/passkey';
  import Icon from './lib/icons/Icon.svelte';
  import { api, API, type Album, type Group, type Person, type TimelinePrefs, type User } from './lib/api';
  import { getToken, setToken, authedUrl } from './lib/session';
  import type { Section, UIPhoto } from './lib/media';
  import { toast } from './lib/toast.svelte';
  import { enqueue, configureUploads } from './lib/upload.svelte';
  import { autofocus } from './lib/actions';

  // ---- app state ----
  let theme = $state<'dark' | 'light'>('dark');
  let nav = $state('Timeline');

  // URL hash <-> current view, so a refresh (F5) / bookmark restores the menu.
  // Hash routing needs no server config (works under Vite dev + static serving).
  // Modals (Groups/Shared/Vault) are transient and intentionally NOT routed.
  const ROUTED_VIEWS = [
    'Timeline', 'Albums', 'People', 'Places', 'Explore',
    'Favorites', 'Archive', 'Trash', 'Duplicates',
  ];
  function viewFromHash(): string | null {
    if (typeof window === 'undefined') return null;
    const h = window.location.hash.replace(/^#\/?/, '').toLowerCase();
    return ROUTED_VIEWS.find((v) => v.toLowerCase() === h) ?? null;
  }
  function writeHash(label: string) {
    if (typeof window === 'undefined' || !ROUTED_VIEWS.includes(label)) return;
    const want = '#' + label.toLowerCase();
    if (window.location.hash !== want) {
      // replaceState (not assigning location.hash) avoids a history entry + a
      // redundant hashchange event, but keeps it in the URL for F5/bookmark.
      window.history.replaceState(null, '', window.location.pathname + window.location.search + want);
    }
  }
  let filter = $state('All');
  let search = $state('');
  let density = $state(7);
  let selecting = $state(false);
  let selected = $state<Set<string>>(new Set());
  let hovered = $state<UIPhoto | null>(null);
  let lbIndex = $state(-1);

  // ---- data ----
  let users = $state<User[]>([]);
  let groups = $state<Group[]>([]);
  let albums = $state<Album[]>([]);
  let sections = $state<Section[]>([]);
  let prefs = $state<TimelinePrefs>({ show_shared: true, per_album: {} });
  let allPhotos = $state<UIPhoto[]>([]);
  let archivePhotos = $state<UIPhoto[]>([]);
  let trashPhotos = $state<UIPhoto[]>([]);
  let duplicateGroups = $state<UIPhoto[][]>([]);
  // Face recognition (People): clusters + the currently-open person's photos.
  let people = $state<Person[]>([]);
  let selectedPerson = $state<Person | null>(null);
  let personPhotos = $state<UIPhoto[]>([]);
  let selectedAlbum = $state<string | null>(null);
  // Bulk "add selected photos to an album" picker.
  let pickingAlbum = $state(false);
  let addingToAlbum = $state(false);
  let searchResults = $state<UIPhoto[]>([]);
  let fCamera = $state('');
  let fFrom = $state('');
  let fTo = $state('');
  let fPlace = $state('');
  let showFilters = $state(false);
  let meId = $state('usr_alice');
  let connected = $state(true);
  let loading = $state(true);
  // ---- auth gate ----
  let authed = $state(false);
  let booting = $state(true);

  const me = $derived(users.find((u) => u.id === meId) ?? ({ id: meId, name: meId, email: '' } as User));

  // ---- theme ----
  $effect(() => {
    document.documentElement.classList.toggle('dark', theme === 'dark');
  });

  // ---- loading ----
  async function loadShared() {
    [users, groups, albums] = await Promise.all([api.users(), api.groups(), api.albums()]);
  }
  let storage = $state<{ used_mb: number; total_mb: number } | null>(null);
  // Route plugins surfaced in the sidebar Tools section (empty when plugins off).
  let routePlugins = $state<{ id: string; label: string; ui_path: string | null }[]>([]);
  function openPlugin(p: { id: string; ui_path: string | null }) {
    const url = authedUrl(`${API}/api/plugins/${p.id}${p.ui_path ?? '/ui'}`);
    if (url) window.open(url, '_blank', 'noopener');
  }
  async function loadForUser() {
    [sections, prefs, allPhotos] = await Promise.all([
      api.timeline(meId),
      api.prefs(meId),
      api.allPhotos(meId).catch(() => [] as UIPhoto[]),
    ]);
    api.userStorage(meId).then((s) => (storage = s)).catch(() => (storage = null));
    api.routePlugins().then((p) => (routePlugins = p)).catch(() => (routePlugins = []));
    // Face clusters (People) — for the sidebar count + the People view. Empty
    // offline / when ML is disabled (no faces detected).
    api.people(meId).then((p) => (people = p)).catch(() => (people = []));
  }

  // Lazily (re)load the dataset a non-timeline view needs when nav changes.
  async function loadForNav(label: string) {
    selectedAlbum = null;
    try {
      if (label === 'Archive') archivePhotos = await api.archived(meId);
      else if (label === 'Trash') trashPhotos = await api.trashed(meId);
      else if (label === 'Duplicates') duplicateGroups = (await api.duplicates(meId)).groups;
      else if (label === 'People') {
        selectedPerson = null;
        people = await api.people(meId).catch(() => [] as Person[]);
      }
    } catch {
      toast({ tone: 'error', message: `Could not load ${label}` });
    }
  }
  async function reloadAll() {
    try {
      await loadShared();
      await loadForUser();
      connected = true;
    } catch {
      connected = false;
      toast({
        tone: 'error',
        title: 'Server unreachable',
        message: 'Start the Rust server: cargo run (port 3000)',
        duration: 6000,
      });
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    void meId;
    if (!loading) loadForUser().catch(() => (connected = false));
  });
  $effect(() => {
    configureUploads(meId, loadForUser, addUploadedPhoto);
  });

  // Add a freshly uploaded photo to the timeline immediately — no full reload.
  // Inserts (or replaces, so a companion ARW that adopts a JPG doesn't duplicate)
  // the photo into its day-section, creating the section in date order if needed.
  // A final `loadForUser()` after the whole batch reconciles server grouping.
  function addUploadedPhoto(p: UIPhoto) {
    // De-dup across the dataset: a companion attach / adopt returns an existing id.
    if (!allPhotos.some((q) => q.id === p.id)) allPhotos = [p, ...allPhotos];
    else allPhotos = allPhotos.map((q) => (q.id === p.id ? p : q));

    const day = p.taken_at.slice(0, 10) || '—';
    // Drop any existing copy of this photo from every section first (adopt case).
    for (const s of sections) {
      const k = s.items.findIndex((q) => q.id === p.id);
      if (k >= 0) s.items.splice(k, 1);
    }
    let sec = sections.find((s) => (s.items[0]?.taken_at.slice(0, 10) || s.label) === day);
    if (sec) {
      sec.items = [p, ...sec.items].sort((a, b) => (a.taken_at < b.taken_at ? 1 : -1));
    } else {
      const fresh: Section = { id: 'd' + day, label: day, date: '1 item', items: [p] };
      sections = [...sections, fresh].sort((a, b) => {
        const ka = a.items[0]?.taken_at ?? a.label;
        const kb = b.items[0]?.taken_at ?? b.label;
        return ka < kb ? 1 : -1;
      });
    }
  }

  // Server-backed search (free text + facets), scoped to the user's rights.
  const searching = $derived(!!(search.trim() || fCamera || fFrom || fTo || fPlace));
  // The kind chips + camera/place/date facets only make sense over a PHOTO GRID.
  // Hide the whole filter bar on views that aren't one (People, Tag faces, the
  // Places map, Explore, Groups, and the album LIST) so it isn't shown pointlessly.
  const PHOTO_GRID_VIEWS = ['Timeline', 'Favorites', 'Archive', 'Trash', 'Large files', 'Duplicates'];
  const showFilterBar = $derived(
    searching || PHOTO_GRID_VIEWS.includes(nav) || (nav === 'Albums' && !!selectedAlbum),
  );
  $effect(() => {
    const params = { q: search.trim(), camera: fCamera, from: fFrom, to: fTo, place: fPlace };
    if (!(params.q || params.camera || params.from || params.to || params.place)) {
      searchResults = [];
      return;
    }
    let cancelled = false;
    api
      .search(meId, params)
      .then((r) => {
        if (!cancelled) searchResults = r;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  });

  // ---- auth bootstrap ----
  // Restore an existing session (validate the stored token via /api/me), else
  // fall through to the login screen.
  async function boot() {
    // Capture the routed view from the URL hash BEFORE anything scrubs the URL,
    // so a refresh on e.g. `#people` restores that view after data loads.
    const initialView = viewFromHash();
    // A shared deep link looks like `#photo=<id>` — capture it before the URL is
    // scrubbed so we can open that photo in the lightbox once data has loaded.
    const deepPhoto = (window?.location?.hash?.match(/^#photo=(.+)$/)?.[1]) ?? '';
    // OIDC redirect landing: the callback sends the browser to `/#token=…` (the
    // token is in the URL FRAGMENT so it isn't sent to the server/Referer) or
    // `/?oidc_error=1`. Pick up the token, persist it, and scrub the URL so a
    // refresh/bookmark doesn't carry the credential around.
    if (typeof window !== 'undefined') {
      const params = new URLSearchParams(window.location.search);
      const hash = window.location.hash;
      const oidcToken =
        (hash.startsWith('#token=') ? decodeURIComponent(hash.slice(7)) : '') ||
        params.get('token') ||
        '';
      if (oidcToken) {
        setToken(oidcToken);
      }
      if (oidcToken || params.has('oidc_error')) {
        if (params.get('oidc_error')) {
          toast({ tone: 'error', message: 'OpenID sign-in failed. Please try again.' });
        }
        window.history.replaceState({}, '', window.location.pathname);
      }
    }
    if (!getToken()) {
      booting = false;
      return;
    }
    try {
      const u = await api.me();
      meId = u.id;
      authed = true;
      await reloadAll();
      // Restore the view from the URL hash (F5/bookmark), loading its data.
      if (initialView && initialView !== nav) onNav(initialView);
      // Open a shared `#photo=<id>` deep link against the user's full set.
      if (deepPhoto) {
        const ph = allPhotos.find((x) => x.id === decodeURIComponent(deepPhoto));
        if (ph) openExplorePhoto(ph);
        else toast({ tone: 'error', message: 'That photo is not available' });
        window.history.replaceState({}, '', window.location.pathname);
      }
    } catch {
      setToken(null);
      authed = false;
    } finally {
      booting = false;
    }
  }
  async function doLogin(email: string, password: string, totp: string | undefined = undefined) {
    // Throws on bad credentials, or `TotpRequired`/`TotpInvalid` (from api.ts)
    // when the account is 2FA-enrolled — LoginScreen catches those to prompt.
    const r = await api.login(email, password, totp);
    meId = r.user.id;
    authed = true;
    loading = true;
    await reloadAll();
    // After a PASSWORD login, offer to enable a passkey on this device (once, and
    // only if the browser supports it and the user has none yet).
    offerPasskey(r.user.id);
  }
  // Usernameless passkey sign-in: the browser picks a resident credential, the
  // server resolves it to a user and mints a session (token persisted in api.ts).
  async function doPasskeyLogin() {
    const { handle, options } = await api.passkeyLoginStart();
    const credential = await getPasskey(options);
    const r = await api.passkeyLoginFinish(handle, credential);
    meId = r.user.id;
    authed = true;
    loading = true;
    await reloadAll();
  }
  // Gently propose passkey enrollment after a password login: skip if unsupported,
  // already enrolled, or declined earlier this browser.
  async function offerPasskey(userId: string) {
    if (!passkeysSupported()) return;
    if (localStorage.getItem('photon_passkey_offer_dismissed') === '1') return;
    try {
      const existing = await api.passkeys(userId);
      if (existing.length > 0) return;
    } catch {
      return;
    }
    if (!(await platformAuthenticatorAvailable())) return;
    modal = 'passkeys';
  }
  async function doLogout() {
    await api.logout();
    authed = false;
    nav = 'Timeline';
    lbPhotos = [];
    modal = null;
  }
  boot();

  // Keep the URL hash in sync with the current view once signed in, so F5 stays
  // put. (`writeHash` no-ops for non-routed labels and when already correct.)
  $effect(() => {
    if (authed) writeHash(nav);
  });
  // Honor manual hash edits / browser back-forward.
  $effect(() => {
    if (typeof window === 'undefined') return;
    const onHash = () => {
      const v = viewFromHash();
      if (authed && v && v !== nav) onNav(v);
    };
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  });

  // ---- upload (drag & drop + multi-select) ----
  let dragging = $state(false);
  let dragDepth = 0;
  let fileInput: HTMLInputElement;

  function onDragEnter(e: DragEvent) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    dragDepth++;
    dragging = true;
  }
  function onDragLeave() {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dragging = false;
  }
  function onDrop(e: DragEvent) {
    e.preventDefault();
    dragDepth = 0;
    dragging = false;
    if (!e.dataTransfer?.files?.length) return;
    // Dropping while viewing an album adds the imported photos to THAT album
    // (the server attaches them on finalize); dropping anywhere else imports to
    // the timeline.
    const albumId = nav === 'Albums' && selectedAlbum ? selectedAlbum : undefined;
    enqueue(e.dataTransfer.files, albumId ?? undefined);
  }
  // Drop files directly onto an album card in the grid → import into that album.
  let dropAlbum = $state<string | null>(null);
  function onDropAlbum(e: DragEvent, albumId: string) {
    if (!e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    e.stopPropagation();
    dragDepth = 0;
    dragging = false;
    dropAlbum = null;
    if (e.dataTransfer?.files?.length) enqueue(e.dataTransfer.files, albumId);
  }
  function pickFiles() {
    fileInput?.click();
  }
  function onPicked(e: Event) {
    const t = e.target as HTMLInputElement;
    if (t.files?.length) enqueue(t.files);
    t.value = '';
  }

  // ---- derived view ----
  function matchFilter(p: UIPhoto) {
    switch (filter) {
      case 'Photos': return p.kind === 'photo';
      case 'Videos': return p.kind === 'video';
      case 'RAW': return p.kind === 'raw' || p.companions.some((c) => c.kind === 'raw');
      case 'Geotagged': return !!p.lat;
      case 'Favorites': return p.favorite;
      default: return true;
    }
  }
  function matchSearch(p: UIPhoto) {
    const q = search.trim().toLowerCase();
    if (!q) return true;
    return [p.filename, p.title, p.city, p.country, ...p.tags, ...p.people]
      .filter(Boolean)
      .some((s) => String(s).toLowerCase().includes(q));
  }

  function applyFS(items: UIPhoto[]) {
    return items.filter((p) => matchFilter(p) && matchSearch(p));
  }
  function groupByDay(photos: UIPhoto[]): Section[] {
    const by = new Map<string, UIPhoto[]>();
    for (const p of photos) {
      const d = p.taken_at.slice(0, 10) || '—';
      (by.get(d) ?? by.set(d, []).get(d)!).push(p);
    }
    return [...by.entries()]
      .sort((a, b) => (a[0] < b[0] ? 1 : -1))
      .map(([d, items]) => ({ id: 'd' + d, label: d, date: `${items.length} items`, items }));
  }
  function groupByKey(photos: UIPhoto[], keyFn: (p: UIPhoto) => string | string[] | null): Section[] {
    const by = new Map<string, UIPhoto[]>();
    for (const p of photos) {
      const k = keyFn(p);
      if (!k) continue;
      for (const kk of Array.isArray(k) ? k : [k]) {
        if (!kk) continue;
        (by.get(kk) ?? by.set(kk, []).get(kk)!).push(p);
      }
    }
    return [...by.entries()]
      .sort((a, b) => b[1].length - a[1].length)
      .map(([k, items]) => ({ id: 'g' + k, label: k, date: `${items.length} items`, items }));
  }

  const timelinePhotos = $derived(sections.flatMap((s) => s.items));
  const photoIndex = $derived(new Map(allPhotos.map((p) => [p.id, p])));
  const currentAlbum = $derived(albums.find((a) => a.id === selectedAlbum) ?? null);

  const displaySections = $derived.by((): Section[] => {
    // A query/facet is active → show server search results (already scoped),
    // only re-applying the kind chips (Photos/Videos/RAW/…) client-side.
    if (searching) return groupByDay(searchResults.filter(matchFilter));
    switch (nav) {
      case 'Favorites':
        return groupByDay(applyFS(timelinePhotos.filter((p) => p.favorite)));
      case 'Archive':
        return [{ id: 'archive', label: 'Archive', date: `${archivePhotos.length} items`, items: applyFS(archivePhotos) }].filter((s) => s.items.length);
      case 'Trash':
        return [{ id: 'trash', label: 'Trash', date: 'auto-deletes after retention', items: applyFS(trashPhotos) }].filter((s) => s.items.length);
      case 'People':
        // A selected cluster shows that person's photos. With clusters present
        // (ML on) the grid of face cards is rendered separately (not as sections).
        // With no clusters (offline / no ML) fall back to grouping by people tag.
        if (selectedPerson) {
          const items = applyFS(personPhotos);
          return [{ id: 'person-' + selectedPerson.person_id, label: selectedPerson.name || 'Unnamed person', date: `${items.length} photos`, items }].filter((s) => s.items.length);
        }
        if (people.length) return [];
        return groupByKey(applyFS(timelinePhotos), (p) => p.people);
      case 'Places':
      case 'Explore':
        return groupByKey(applyFS(timelinePhotos), (p) => p.city || null);
      case 'Large files':
        return [{ id: 'large', label: 'Large files', date: 'largest first', items: applyFS([...timelinePhotos].sort((a, b) => b.sizeMB - a.sizeMB)).slice(0, 60) }].filter((s) => s.items.length);
      case 'Duplicates':
        return duplicateGroups
          .map((g, i) => ({ id: 'dup' + i, label: `Duplicate set ${i + 1}`, date: `${g.length} similar`, items: applyFS(g) }))
          .filter((s) => s.items.length);
      case 'Albums': {
        if (!currentAlbum) return [];
        const items = applyFS(currentAlbum.photo_ids.map((id) => photoIndex.get(id)).filter((p): p is UIPhoto => !!p));
        return [{ id: currentAlbum.id, label: currentAlbum.name, date: `${items.length} items`, items }];
      }
      default: // Timeline
        return sections.map((s) => ({ ...s, items: applyFS(s.items) })).filter((s) => s.items.length);
    }
  });
  const visible = $derived(displaySections.flatMap((s) => s.items));

  const counts = $derived({
    albums: albums.length,
    groups: groups.length,
    shared: albums.filter((a) => a.owner_id !== meId).length,
    people: people.length,
    // Real duplicate count (total photos across detected near-dup groups). Lazy:
    // populated when the Duplicates view is opened (daily-job result), so the
    // badge is absent until known rather than showing a fabricated number.
    duplicates: duplicateGroups.reduce((n, g) => n + g.length, 0),
  });

  // ---- photo mutations ----
  function patchLocal(id: string, fn: (p: UIPhoto) => void) {
    for (const s of sections) {
      const p = s.items.find((x) => x.id === id);
      if (p) fn(p);
    }
    sections = [...sections];
  }

  async function toggleFav(id: string) {
    let next = false;
    patchLocal(id, (p) => { p.favorite = !p.favorite; next = p.favorite; });
    try {
      await api.patchMetadata(id, [{ op: 'add', path: '/favorite', value: next }]);
    } catch {
      patchLocal(id, (p) => (p.favorite = !p.favorite));
      toast({ tone: 'error', message: 'Could not update favorite' });
    }
  }
  async function rate(id: string, n: number) {
    patchLocal(id, (p) => (p.rating = n || null));
    try {
      await api.patchMetadata(id, [{ op: 'add', path: '/rating', value: n || null }]);
    } catch {
      /* keep optimistic */
    }
  }
  async function archive(id: string) {
    try {
      await api.archivePhoto(id);
      await loadForUser();
    } catch {
      toast({ tone: 'error', message: 'Archive failed' });
    }
  }
  async function trash(id: string) {
    try {
      await api.trashPhoto(id);
      await loadForUser();
      toast({ tone: 'default', icon: 'trash-2', message: 'Moved to trash', actionLabel: 'Undo', onAction: () => restore(id) });
    } catch {
      toast({ tone: 'error', message: 'Delete failed' });
    }
  }
  function onSaved(updated: UIPhoto) {
    patchLocal(updated.id, (p) => Object.assign(p, updated));
  }

  // ---- selection ----
  function toggleSel(id: string) {
    selecting = true;
    const n = new Set(selected);
    n.has(id) ? n.delete(id) : n.add(id);
    selected = n;
  }
  function selectMany(ids: string[], select: boolean) {
    const n = new Set(selected);
    for (const id of ids) select ? n.add(id) : n.delete(id);
    selected = n;
    selecting = n.size > 0;
  }
  function clearSel() {
    selected = new Set();
    selecting = false;
  }

  async function bulk(action: 'fav' | 'archive' | 'trash') {
    const ids = [...selected];
    for (const id of ids) {
      if (action === 'fav') await toggleFav(id);
      if (action === 'archive') await archive(id);
      if (action === 'trash') await trash(id);
    }
    toast({
      tone: 'success',
      message: `${ids.length} ${action === 'fav' ? 'favorited' : action === 'archive' ? 'archived' : 'trashed'}`,
    });
    clearSel();
  }

  async function addSelectedToAlbum(albumId: string) {
    const ids = [...selected];
    if (!ids.length || addingToAlbum) return;
    addingToAlbum = true;
    try {
      await api.addAlbumPhotos(albumId, ids);
      const name = albums.find((a) => a.id === albumId)?.name ?? 'album';
      pickingAlbum = false;
      clearSel();
      await reloadAll();
      toast({ tone: 'success', icon: 'images', message: `${ids.length} added to ${name}` });
    } catch (e) {
      toast({ tone: 'error', title: 'Could not add to album', message: String(e) });
    } finally {
      addingToAlbum = false;
    }
  }

  // ---- lightbox ----
  let lbPhotos = $state<UIPhoto[]>([]);
  function openPhoto(p: UIPhoto) {
    lbPhotos = visible;
    const i = visible.findIndex((x) => x.id === p.id);
    lbIndex = i >= 0 ? i : 0;
  }
  function openCity(city: string) {
    const ps = timelinePhotos.filter((p) => p.city === city);
    if (!ps.length) return;
    lbPhotos = ps;
    lbIndex = 0;
  }
  // Explore opens a photo against the user's full set (Explore filters from allPhotos).
  function openExplorePhoto(p: UIPhoto) {
    lbPhotos = allPhotos;
    const i = allPhotos.findIndex((x) => x.id === p.id);
    lbIndex = i >= 0 ? i : 0;
  }

  // ---- album creation / add photos ----
  let creatingAlbum = $state(false);
  let newAlbumName = $state('');
  let addingPhotos = $state(false);
  // candidates = the user's own already-uploaded photos not already in the album
  const addCandidates = $derived(
    currentAlbum
      ? allPhotos.filter((p) => p.owner_id === meId && !currentAlbum.photo_ids.includes(p.id))
      : [],
  );
  async function createAlbum() {
    const name = newAlbumName.trim();
    if (!name) return;
    try {
      await api.createAlbum({ name, owner_id: meId, photo_ids: [] });
      newAlbumName = '';
      creatingAlbum = false;
      await reloadAll();
      toast({ tone: 'success', message: `Album “${name}” created` });
    } catch (e) {
      toast({ tone: 'error', title: 'Create failed', message: String(e) });
    }
  }

  // ---- people (face clusters) ----
  async function openPerson(p: Person) {
    selectedPerson = p;
    try {
      personPhotos = await api.personPhotos(p.person_id, meId);
    } catch {
      personPhotos = [];
      toast({ tone: 'error', message: 'Could not load this person’s photos' });
    }
  }

  // ---- modals ----
  let modal = $state<'account' | 'sharing' | 'storage' | 'vault' | 'admin' | 'passkeys' | null>(null);
  function onNav(label: string) {
    if (label === 'Groups' || label === 'Shared') {
      modal = 'sharing';
      return;
    }
    if (label === 'Vault') {
      modal = 'vault';
      return;
    }
    // 'Tag faces' is the People Studio — a full inline view (see the render switch).
    nav = label;
    clearSel();
    loadForNav(label);
  }

  const viewTitle = $derived(nav === 'Albums' && currentAlbum ? currentAlbum.name : nav);

  async function unarchive(id: string) {
    try {
      await api.unarchivePhoto(id);
      archivePhotos = archivePhotos.filter((p) => p.id !== id);
      await loadForUser();
      toast({ tone: 'success', message: 'Restored from archive' });
    } catch {
      toast({ tone: 'error', message: 'Failed' });
    }
  }
  async function restore(id: string) {
    try {
      await api.restorePhoto(id);
      trashPhotos = trashPhotos.filter((p) => p.id !== id);
      await loadForUser();
      toast({ tone: 'success', message: 'Restored' });
    } catch {
      toast({ tone: 'error', message: 'Failed' });
    }
  }

  const FILTERS: [string, string | null][] = [
    ['All', null],
    ['Photos', 'image'],
    ['Videos', 'video'],
    ['RAW', 'camera'],
    ['Geotagged', 'map-pin'],
    ['Favorites', 'star'],
  ];
  // Tile density (visualization) options for the filter bar — L / M / S grids.
  const DENSITIES: [string, string, number][] = [
    ['rows-3', 'L', 5],
    ['layout-grid', 'M', 7],
    ['grip', 'S', 10],
  ];
</script>

{#if booting}
  <!-- brief splash while validating a stored session -->
  <div class="pk-boot"></div>
{:else if !authed}
  <LoginScreen onLogin={doLogin} onPasskeyLogin={doPasskeyLogin} />
{:else}
<div
  class={'pk-app' + (selecting ? ' is-selecting' : '')}
  role="application"
  ondragenter={onDragEnter}
  ondragover={(e) => e.preventDefault()}
  ondragleave={onDragLeave}
  ondrop={onDrop}
>
  <input
    bind:this={fileInput}
    type="file"
    multiple
    accept="image/*,video/*,.arw,.raf,.cr2,.cr3,.nef,.dng,.orf,.rw2"
    style="display:none"
    onchange={onPicked}
  />
  <Sidebar
    active={nav}
    {theme}
    user={me}
    {counts}
    {onNav}
    onToggleTheme={() => (theme = theme === 'dark' ? 'light' : 'dark')}
    onSwitchAccount={() => (modal = 'account')}
    onOpenSettings={() => (modal = 'storage')}
    onOpenAdmin={() => (modal = 'admin')}
    onOpenSecurity={() => (modal = 'passkeys')}
    plugins={routePlugins}
    onOpenPlugin={openPlugin}
    {storage}
  />

  <div class="pk-main">
    <Topbar
      title={viewTitle}
      bind:search
      onUpload={pickFiles}
    />

    {#if showFilterBar}
      <div class="pk-filterbar">
        <div class="pk-chips">
          {#each FILTERS as [label, icon] (label)}
            <button class={'pk-chip' + (filter === label ? ' is-active' : '')} onclick={() => (filter = label)}>
              {#if icon}<Icon name={icon} size={13} />{/if}{label}
            </button>
          {/each}
        </div>
        <div class="pk-filter-right">
          {#if !connected}
            <span class="pk-chip" style="color:var(--danger)"><Icon name="circle-alert" size={13} />offline</span>
          {/if}
          <button
            class={'pk-meta-link' + (showFilters || fCamera || fFrom || fTo || fPlace ? ' is-on' : '')}
            onclick={() => (showFilters = !showFilters)}
          >
            <Icon name="sliders-horizontal" size={15} />Filters
          </button>
          <button class="pk-meta-link" onclick={() => { selecting = !selecting; if (!selecting) selected = new Set(); }}>
            <Icon name={selecting ? 'x' : 'check-square'} size={15} />{selecting ? 'Cancel' : 'Select'}
          </button>
          <div class="pk-segmented">
            {#each DENSITIES as [ic, key, cols] (key)}
              <button class={density === cols ? 'is-on' : ''} onclick={() => (density = cols)} title={`${key} tiles`}>
                <Icon name={ic} size={14} />
              </button>
            {/each}
          </div>
          <div class="pk-divider-v"></div>
          <button class="pk-meta-link" onclick={() => (modal = 'sharing')}>
            <Icon name="share-2" size={15} />Sharing &amp; groups
          </button>
        </div>
      </div>

      {#if showFilters}
        <div class="pk-facets">
          <label>Camera<input bind:value={fCamera} placeholder="e.g. Sony, Leica" /></label>
          <label>Place<input bind:value={fPlace} placeholder="city or country" /></label>
          <label>From<input type="date" bind:value={fFrom} /></label>
          <label>To<input type="date" bind:value={fTo} /></label>
          {#if fCamera || fFrom || fTo || fPlace}
            <button class="pk-meta-link" onclick={() => { fCamera = ''; fFrom = ''; fTo = ''; fPlace = ''; }}>
              <Icon name="x" size={14} />Clear
            </button>
          {/if}
        </div>
      {/if}
    {/if}

    {#if searching}
      <div class="pk-viewbar">
        <span class="pk-viewbar-hint"><Icon name="search" size={14} />{visible.length} result{visible.length === 1 ? '' : 's'} across your photos &amp; shared albums</span>
      </div>
      <Feed
        sections={displaySections}
        {density}
        {selected}
        {selecting}
        emptyLabel="No matches."
        onOpen={openPhoto}
        onToggleSel={toggleSel}
        onSelectMany={selectMany}
        onHover={(p) => (hovered = p)}
        onFav={toggleFav}
      />
    {:else if nav === 'Tag faces'}
      <PeopleStudio userId={meId} />
    {:else if nav === 'People' && !selectedPerson}
      <PeopleBrowse {people} onOpenPerson={openPerson} onOpenStudio={() => onNav('Tag faces')} />
    {:else if nav === 'Places'}
      <PlacesMap photos={timelinePhotos} onOpenCity={openCity} />
    {:else if nav === 'Explore'}
      <Explore photos={allPhotos} {people} onOpen={openExplorePhoto} />

    {:else if nav === 'Albums' && !selectedAlbum}
      <div class="pk-viewbar">
        {#if creatingAlbum}
          <input
            class="pk-newalbum-input"
            placeholder="Album name…"
            use:autofocus
            bind:value={newAlbumName}
            onkeydown={(e) => {
              if (e.key === 'Enter') createAlbum();
              else if (e.key === 'Escape') { creatingAlbum = false; newAlbumName = ''; }
            }}
          />
          <button class="pk-btn pk-btn-primary" onclick={createAlbum} disabled={!newAlbumName.trim()}>
            <Icon name="check" size={15} />Create
          </button>
          <button class="pk-meta-link" onclick={() => { creatingAlbum = false; newAlbumName = ''; }}>Cancel</button>
        {:else}
          <button class="pk-btn pk-btn-primary" onclick={() => (creatingAlbum = true)}>
            <Icon name="plus" size={15} />New album
          </button>
          <span class="pk-viewbar-hint">{albums.length} album{albums.length === 1 ? '' : 's'}</span>
        {/if}
      </div>
      <div class="pk-albumgrid">
        {#if albums.length === 0}
          <div class="pk-view-empty"><Icon name="images" size={26} /><span>No albums yet — create one.</span></div>
        {/if}
        {#each albums as a (a.id)}
          {@const cover = a.cover_seed ?? photoIndex.get(a.photo_ids[0])?.seed ?? 100}
          <button
            class="pk-albumcard"
            class:pk-albumcard-drop={dropAlbum === a.id}
            onclick={() => (selectedAlbum = a.id)}
            ondragover={(e) => { if (e.dataTransfer?.types.includes('Files')) { e.preventDefault(); e.stopPropagation(); dropAlbum = a.id; } }}
            ondragleave={() => { if (dropAlbum === a.id) dropAlbum = null; }}
            ondrop={(e) => onDropAlbum(e, a.id)}
          >
            <img src={`https://picsum.photos/seed/ph${cover}/600/400`} alt={a.name} loading="lazy" />
            <div class="pk-albumcard-grad"></div>
            <div class="pk-albumcard-meta">
              <div class="pk-albumcard-name">{a.name}</div>
              <div class="pk-albumcard-sub">
                {a.photo_ids.length} photos{a.owner_id !== meId ? ' · shared' : ''}{(a.shares ?? []).length ? ` · ${(a.shares ?? []).length} shares` : ''}
              </div>
            </div>
          </button>
        {/each}
      </div>
    {:else}
      {#if nav === 'People' && selectedPerson}
        <div class="pk-viewbar">
          <button class="pk-meta-link" onclick={() => (selectedPerson = null)}><Icon name="chevron-left" size={15} />All people</button>
          <span class="pk-viewbar-hint" style="margin-left:auto"><Icon name="user" size={15} />{selectedPerson.name}</span>
        </div>
      {/if}
      {#if (nav === 'Albums' && selectedAlbum) || nav === 'Archive' || nav === 'Trash'}
        <div class="pk-viewbar">
          {#if nav === 'Albums'}
            <button class="pk-meta-link" onclick={() => (selectedAlbum = null)}><Icon name="chevron-left" size={15} />All albums</button>
            <button class="pk-btn pk-btn-primary" style="margin-left:auto" onclick={() => (addingPhotos = true)}>
              <Icon name="image-plus" size={15} />Add photos
            </button>
          {:else}
            <span class="pk-viewbar-hint">
              <Icon name={nav === 'Trash' ? 'trash-2' : 'archive'} size={14} />
              {nav === 'Trash' ? 'Items here are deleted after the retention period.' : 'Archived items are hidden from the timeline and search.'}
            </span>
          {/if}
        </div>
      {/if}
      <Feed
        sections={displaySections}
        {density}
        {selected}
        {selecting}
        emptyLabel={!connected
          ? 'Start the Rust server (cargo run) to load your library.'
          : nav === 'Duplicates'
            ? 'No duplicates found (scanned daily).'
            : 'Nothing here.'}
        onOpen={openPhoto}
        onToggleSel={toggleSel}
        onSelectMany={selectMany}
        onHover={(p) => (hovered = p)}
        onFav={toggleFav}
      />
    {/if}

    <StatusBar
      {selecting}
      selCount={selected.size}
      onClearSel={clearSel}
      onShare={() => (modal = 'sharing')}
      onAddAlbum={() => (pickingAlbum = true)}
      onFavorite={() => bulk('fav')}
      onArchive={() => bulk('archive')}
      onDelete={() => bulk('trash')}
    />
  </div>

  {#if lbIndex >= 0 && lbPhotos[lbIndex]}
    <Lightbox
      photos={lbPhotos}
      index={lbIndex}
      onClose={() => (lbIndex = -1)}
      onSetIndex={(i) => (lbIndex = i)}
      onFav={toggleFav}
      onRate={rate}
      onArchive={archive}
      onDelete={trash}
      {onSaved}
      onRestore={nav === 'Trash' ? restore : nav === 'Archive' ? unarchive : undefined}
      restoreLabel={nav === 'Trash' ? 'Restore from trash' : 'Restore from archive'}
    />
  {/if}

  {#if modal === 'account'}
    <AccountSwitcher {users} current={meId} onPick={(id) => (meId = id)} onLogout={doLogout} onClose={() => (modal = null)} />
  {/if}
  {#if modal === 'sharing'}
    <SharingModal {me} {users} {groups} {albums} {prefs} onClose={() => (modal = null)} onChanged={reloadAll} />
  {/if}
  {#if modal === 'storage'}
    <StorageModal onClose={() => (modal = null)} />
  {/if}
  {#if modal === 'vault'}
    <VaultModal {me} onClose={() => (modal = null)} />
  {/if}
  {#if modal === 'admin'}
    <AdminConsole
      {me}
      {users}
      onClose={() => (modal = null)}
      onChanged={reloadAll}
      onOpenAlbum={(id) => { modal = null; onNav('Albums'); selectedAlbum = id; }}
    />
  {/if}
  {#if modal === 'passkeys'}
    <PasskeyModal
      userId={meId}
      onClose={() => {
        // Remember a dismissal so we don't re-offer on every password login.
        try { localStorage.setItem('photon_passkey_offer_dismissed', '1'); } catch {}
        modal = null;
      }}
    />
  {/if}
  {#if pickingAlbum}
    <AlbumPickerModal
      albums={albums.filter((a) => a.owner_id === meId)}
      count={selected.size}
      busy={addingToAlbum}
      onPick={addSelectedToAlbum}
      onClose={() => (pickingAlbum = false)}
    />
  {/if}
  {#if addingPhotos && currentAlbum}
    <AddPhotosModal
      albumId={currentAlbum.id}
      albumName={currentAlbum.name}
      photos={addCandidates}
      onClose={() => (addingPhotos = false)}
      onAdded={reloadAll}
    />
  {/if}

  {#if dragging}
    <div class="pk-dropzone">
      <div class="pk-dropzone-card">
        <Icon name="upload-cloud" size={40} />
        <div class="pk-dropzone-title">Drop photos &amp; videos to upload</div>
        <div class="pk-dropzone-sub">JPG + RAW pairs are grouped automatically</div>
      </div>
    </div>
  {/if}

  <UploadPanel />
  <Toaster />
</div>
{/if}

<style>
  .pk-boot { position: fixed; inset: 0; background: var(--bg, #0b0b0d); }
  .pk-dropzone {
    position: fixed;
    inset: 0;
    z-index: 90;
    display: grid;
    place-items: center;
    background: var(--scrim);
    backdrop-filter: var(--blur-sm);
    pointer-events: none;
  }
  .pk-dropzone-card {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
    padding: 48px 64px;
    border: 2px dashed var(--accent);
    border-radius: var(--radius-xl);
    background: var(--surface);
    color: var(--accent-text);
    box-shadow: var(--shadow-xl);
  }
  .pk-dropzone-title { font-size: var(--text-lg); font-weight: var(--fw-semibold); color: var(--text); }
  .pk-dropzone-sub { font-size: var(--text-xs); color: var(--text-muted); }

  .pk-albumgrid {
    flex: 1;
    overflow-y: auto;
    padding: 16px 18px 28px;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 14px;
    align-content: start;
  }
  .pk-albumcard-drop { outline: 2px solid var(--accent); outline-offset: 2px; }
  .pk-albumcard {
    position: relative;
    height: 150px;
    border-radius: var(--radius-lg);
    overflow: hidden;
    border: 0;
    padding: 0;
    cursor: pointer;
  }
  .pk-albumcard img { width: 100%; height: 100%; object-fit: cover; transition: transform var(--dur-slow) var(--ease-out); }
  .pk-albumcard:hover img { transform: scale(1.05); }
  .pk-albumcard-grad { position: absolute; inset: 0; background: linear-gradient(to top, rgba(7, 9, 16, 0.82), transparent 60%); }
  .pk-albumcard-meta { position: absolute; left: 13px; right: 13px; bottom: 11px; text-align: left; }
  .pk-albumcard-name { font-size: var(--text-md); font-weight: var(--fw-semibold); color: #fff; }
  .pk-albumcard-sub { font-size: var(--text-2xs); color: rgba(255, 255, 255, 0.8); }
  .pk-facets { display: flex; flex-wrap: wrap; align-items: flex-end; gap: 10px; padding: 0 16px 10px; flex: none; }
  .pk-facets label { display: flex; flex-direction: column; gap: 4px; font-size: var(--text-2xs); color: var(--text-muted); font-weight: var(--fw-medium); }
  .pk-facets input { height: var(--control-h-sm); padding: 0 9px; background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); color: var(--text); font: inherit; font-size: var(--text-xs); outline: none; }
  .pk-facets input:focus { border-color: var(--accent-soft-bd); }
  .pk-meta-link.is-on { background: var(--accent-soft); color: var(--accent-text); }
  .pk-viewbar { display: flex; align-items: center; gap: 10px; padding: 8px 18px 0; flex: none; }
  .pk-viewbar-hint { display: inline-flex; align-items: center; gap: 7px; font-size: var(--text-xs); color: var(--text-faint); }
  .pk-newalbum-input { height: var(--control-h); padding: 0 11px; background: var(--bg-subtle); border: 1px solid var(--border); border-radius: var(--radius-md); color: var(--text); font: inherit; font-size: var(--text-sm); outline: none; min-width: 220px; }
  .pk-newalbum-input:focus { border-color: var(--accent-soft-bd); }
  .pk-view-empty {
    grid-column: 1 / -1;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    padding: 60px;
    color: var(--text-faint);
  }

  /* People (face clusters) grid */
  .pk-peoplegrid {
    flex: 1;
    overflow-y: auto;
    padding: 16px 18px 28px;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(120px, 1fr));
    gap: 16px;
    align-content: start;
  }
  .pk-personcard { display: flex; flex-direction: column; align-items: center; gap: 7px; }
  .pk-person-avatar {
    width: 96px; height: 96px; padding: 0; border: 0; cursor: pointer;
    border-radius: var(--radius-pill); overflow: hidden;
    background: var(--bg-subtle); display: grid; place-items: center;
    color: var(--text-faint);
  }
  .pk-person-img { display: block; width: 100%; height: 100%; background-repeat: no-repeat; }
  .pk-person-avatar:hover { outline: 2px solid var(--accent); }
  .pk-person-name {
    border: 0; background: none; cursor: pointer; padding: 0;
    font-size: var(--text-xs); font-weight: var(--fw-medium); color: var(--text);
    max-width: 110px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .pk-person-name:hover { color: var(--accent-text); }
  .pk-person-name-input {
    width: 104px; height: var(--control-h-sm); padding: 0 7px; text-align: center;
    background: var(--bg-subtle); border: 1px solid var(--accent-soft-bd);
    border-radius: var(--radius-md); color: var(--text); font: inherit; font-size: var(--text-xs); outline: none;
  }
  .pk-person-count { font-size: var(--text-2xs); color: var(--text-faint); }
  /* kinship */
  .pk-person-rels {
    display: flex; flex-wrap: wrap; gap: 4px; justify-content: center; max-width: 130px;
  }
  .pk-rel-chip {
    display: inline-flex; align-items: center; gap: 4px;
    padding: 2px 4px 2px 7px; border-radius: var(--radius-pill);
    background: var(--bg-subtle); border: 1px solid var(--border);
    font-size: var(--text-2xs); color: var(--text-muted); max-width: 130px;
  }
  .pk-rel-kind { color: var(--accent-text); font-weight: var(--fw-medium); }
  .pk-rel-who { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
