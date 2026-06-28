/* Photon — toast store. Svelte 5 runes-based singleton. */
export type Tone = 'default' | 'success' | 'error' | 'info' | 'loading';

export interface Toast {
  id: string;
  tone: Tone;
  icon?: string;
  title?: string;
  message?: string;
  actionLabel?: string;
  onAction?: () => void;
  duration?: number;
  leaving?: boolean;
}

let seq = 0;
const timers = new Map<string, ReturnType<typeof setTimeout>>();

export const toasts = $state<Toast[]>([]);

export function dismiss(id: string) {
  const t = toasts.find((x) => x.id === id);
  if (t) t.leaving = true;
  const timer = timers.get(id);
  if (timer) clearTimeout(timer);
  timers.delete(id);
  setTimeout(() => {
    const i = toasts.findIndex((x) => x.id === id);
    if (i >= 0) toasts.splice(i, 1);
  }, 220);
}

export function toast(opts: Partial<Toast> = {}): string {
  const id = `${++seq}_${Math.round(performance.now())}`;
  const item: Toast = { id, tone: 'default', duration: 3400, ...opts };
  toasts.push(item);
  if (toasts.length > 4) toasts.shift();
  if (item.duration) timers.set(id, setTimeout(() => dismiss(id), item.duration));
  return id;
}
