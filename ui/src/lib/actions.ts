/* Photon — small reusable Svelte actions. */

/** `use:autofocus` — focus the node as soon as it's mounted (and select any text).
 *
 *  RULE: when a user action reveals an input/field (e.g. clicking "New album"
 *  swaps a button for a text input), that field MUST take focus so the user can
 *  type immediately without a second click. Put `use:autofocus` on the revealed
 *  input. The native `autofocus` attribute is unreliable for elements added
 *  dynamically after initial page load, so we focus explicitly on mount.
 */
export function autofocus(node: HTMLElement) {
  // Defer to the next frame so the element is laid out before we focus it
  // (avoids scroll jumps when it mounts inside a transition).
  requestAnimationFrame(() => {
    node.focus();
    if (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement) {
      node.select();
    }
  });
}
