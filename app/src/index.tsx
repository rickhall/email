/* @refresh reload */
import { render } from 'solid-js/web';
import './lib/platform';
import { App } from './App';
import { isComposeWindow } from './lib/windows';
import './styles.css';

const root = document.getElementById('root');
if (!root) throw new Error('#root missing');
// A compose window shows only the composer (lib/windows.ts); the main window, the mail app. The
// composer's editor is loaded on demand, so the main window starts without it.
if (isComposeWindow) {
  void import('./components/ComposeWindow').then(({ ComposeWindow }) => render(() => <ComposeWindow />, root));
} else {
  render(() => <App />, root);
}
