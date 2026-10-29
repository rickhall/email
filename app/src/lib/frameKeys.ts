/**
 * Keyboard shortcuts while a message body (sandboxed iframe) has focus: keydowns inside an
 * iframe never reach the app's window listeners, so they're re-dispatched from the frame
 * element. Needs `allow-same-origin` on the frame, and `allow-scripts` for WebKit, which
 * otherwise blocks every listener in the frame, ours included ("Blocked script execution in
 * 'about:srcdoc'"). The email still can't run code: it's sanitised, and the document's CSP has
 * script-src 'none' (see `buildSrcdoc`). Keys that act on the message itself stay with it.
 */

/** Scrolling keys: inside the message they scroll it rather than move between messages. */
const NATIVE = new Set(['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'PageUp', 'PageDown', 'Home', 'End', ' ', 'Tab']);

function typing(t: EventTarget | null): boolean {
  // Not `instanceof HTMLElement`: elements inside the frame belong to its own window.
  const el = t as { isContentEditable?: boolean; tagName?: string } | null;
  return !!el && (el.isContentEditable === true || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName ?? ''));
}

/** Forward shortcut keys from `frame`'s document (call on each load). */
export function forwardFrameKeys(frame: HTMLIFrameElement): void {
  const doc = frame.contentDocument;
  if (!doc) return;
  doc.addEventListener('keydown', (e) => {
    if (NATIVE.has(e.key) || typing(e.target)) return;
    const copy = new KeyboardEvent('keydown', {
      key: e.key,
      code: e.code,
      shiftKey: e.shiftKey,
      ctrlKey: e.ctrlKey,
      metaKey: e.metaKey,
      altKey: e.altKey,
      repeat: e.repeat,
      modifierCapsLock: e.getModifierState('CapsLock'),
      bubbles: true,
      cancelable: true,
    });
    frame.dispatchEvent(copy);
    // A handled shortcut (e.g. ⌘F) shouldn't also do its default inside the frame.
    if (copy.defaultPrevented) e.preventDefault();
  });
}
