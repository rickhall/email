import { createSignal } from 'solid-js';
import { loadPref, savePref } from './format';

/** The reader's head (sender, recipients, attachments) stays at the top while the message
 *  scrolls under it (Settings › General › Appearance); on unless turned off. */
const PREF = 'ui.pinReaderHead';
const [pinReaderHead, setSig] = createSignal<boolean>(loadPref<boolean>(PREF, true) !== false);
export { pinReaderHead };
export function setPinReaderHead(on: boolean): void {
  setSig(on);
  savePref(PREF, on);
}
