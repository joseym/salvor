/**
 * Focus an element that a signal write is about to render. Zoneless rendering flushes on the
 * next animation frame, so a synchronous or microtask-time focus() runs before the node exists;
 * this polls frame by frame (bounded) and focuses the first match.
 *
 * Finding the node is NOT enough. A cross-view hand-off (the Runs signpost → an Inbox card) renders
 * the target inside a `<section>` that is still `[hidden]` for a frame or two while the router
 * commits and the shell toggles `.is-active`. `focus()` on a node inside a `display:none` ancestor
 * is a silent no-op — the node exists, the query matches, but focus never lands, and the operator is
 * stranded at the top of the page. So this keeps trying until focus ACTUALLY takes
 * (`document.activeElement === el`), not merely until the node is in the DOM.
 */
export function focusWhenRendered(selector: string, maxFrames = 30): void {
  let frames = 0;
  const tryFocus = (): void => {
    const el = document.querySelector<HTMLElement>(selector);
    if (el) {
      el.focus();
      if (document.activeElement === el) return; // focus genuinely landed
    }
    frames += 1;
    if (frames < maxFrames) requestAnimationFrame(tryFocus);
  };
  requestAnimationFrame(tryFocus);
}
