/** Platform class on <html> ("macos" | "windows" | "linux") for platform-specific chrome.
 *  `?mac` forces macOS (browser mock / design checks). */
export type Platform = 'macos' | 'windows' | 'linux';

function detect(): Platform {
  try {
    if (new URLSearchParams(location.search).has('mac')) return 'macos';
  } catch {
    /* ignore */
  }
  const ua = navigator.userAgent;
  const plat = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform ?? navigator.platform ?? '';
  if (/Mac OS X|Macintosh/i.test(ua) || /^Mac/i.test(plat)) return 'macos';
  if (/Windows/i.test(ua) || /^Win/i.test(plat)) return 'windows';
  return 'linux';
}

export const platform: Platform = detect();
document.documentElement.classList.add(platform);

/**
 * Value for `data-tauri-drag-region`. Only macOS drags the window from pane tops (its title
 * bar is an overlay); elsewhere the native title bar does that, and "false" stops Tauri's drag
 * script from swallowing mouse presses (and with them text selection) in those areas.
 */
export const DRAG: 'true' | 'false' = platform === 'macos' ? 'true' : 'false';

// A window in the background doesn't animate: its continuous animations (sync spinner and
// progress bar, loading shimmer) pause while another window, like a compose window, has
// focus. In WebKitGTK every window draws on one thread, so they'd steal the frames the focused
// window needs; it also saves battery.
const setActive = (active: boolean) => document.documentElement.classList.toggle('inactive', !active);
window.addEventListener('focus', () => setActive(true));
window.addEventListener('blur', () => setActive(false));
setActive(document.hasFocus());
