/**
 * Focus an element that a signal write is about to render. Zoneless rendering flushes on the
 * next animation frame, so a synchronous or microtask-time focus() runs before the node exists;
 * this polls frame by frame (bounded) and focuses the first match.
 */
export function focusWhenRendered(selector: string, maxFrames = 10): void {
  let frames = 0;
  const tryFocus = (): void => {
    const el = document.querySelector<HTMLElement>(selector);
    if (el) {
      el.focus();
      return;
    }
    frames += 1;
    if (frames < maxFrames) requestAnimationFrame(tryFocus);
  };
  requestAnimationFrame(tryFocus);
}
