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
