import { Select } from './Select';
import { createEffect, createSelector, createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { Editor } from '@tiptap/core';
import StarterKit from '@tiptap/starter-kit';
import Placeholder from '@tiptap/extension-placeholder';
import Image from '@tiptap/extension-image';
import CodeBlock from '@tiptap/extension-code-block';
import Heading from '@tiptap/extension-heading';
import { Color, TextStyle } from '@tiptap/extension-text-style';
import Highlight from '@tiptap/extension-highlight';
import { mergeAttributes } from '@tiptap/core';
import { TextSelection } from '@tiptap/pm/state';
import { pastedPictures, scalePicture } from '../lib/pictures';
import type { Addr } from '../api';
import { accounts } from '../state';
import {
  addFiles,
  attachedBytes,
  compose,
  discardAndClose,
  hasContent,
  isAddress,
  parseAddresses,
  saveAndClose,
  saveNow,
  send,
  updateCompose,
  type ComposeState,
} from '../lib/compose';
import { switchRecipients, type ReplyRecipients } from '../lib/recipients';
import { api } from '../api';
import { composeDraftId, onCloseRequested, onFileDrop, pickFiles, setWindowTitle } from '../lib/windows';
import { GMAIL_LIMIT } from '../lib/compose';
import { MOD } from '../lib/shortcuts';
import { DRAG, platform } from '../lib/platform';
import { confirmDialog } from './Actions';
import { CloseIcon, ForwardIcon, MailIcon, PaperclipIcon, ReplyAllIcon, ReplyIcon, TrashIcon } from './Icons';
import { AutoHeightFrame } from './AutoHeightFrame';
import { buildSrcdoc } from './Reader';
import { fmtSize } from '../lib/format';
import { showToast } from './Toast';
import { defaultSignature, loadSignatures, signatureById, sigs } from '../lib/signatures';
import { identities, identityById, loadIdentities } from '../lib/identities';
import { AiPanel } from './AiPanel';
import { Modal } from './Modal';

/** The quoted original under the text: read-only, at the composer's margins. */
const QUOTE_FRAME_CSS = 'html{overflow-y:hidden}body{padding:0 0 12px}';

// ------------------------------------------------------------- address field

/** A chip as text for editing: "Name <addr>" or the address. */
const chipText = (a: Addr) => (a.name ? `${a.name} <${a.addr}>` : a.addr);

/** To/Cc/Bcc: typed addresses become chips on Enter, comma, semicolon, Tab or leaving the field;
 *  people from your mail are suggested as you type (↑/↓, Enter or Tab to pick, Esc to close,
 *  Shift+Delete or × to stop suggesting one). */
function AddressField(props: {
  label: string;
  list: Addr[];
  onChange: (l: Addr[]) => void;
  autofocus?: boolean;
  /** what's typed but not (yet) a chip, for Send to check */
  onTyping?: (text: string, focus: () => void) => void;
}) {
  let input!: HTMLInputElement;
  const [text, setTextRaw] = createSignal('');
  const setText = (t: string) => {
    setTextRaw(t);
    props.onTyping?.(t, () => input.focus());
  };
  /** Left with text that isn't an address: shown in red, still editable. */
  const [invalid, setInvalid] = createSignal(false);
  /** Where the text goes back in: the chip being edited keeps its place (null = at the end).
   *  The input moves there with CSS `order`, so it's the same element and keeps focus. */
  const [at, setAt] = createSignal<number | null>(null);
  const insertAt = () => Math.min(at() ?? props.list.length, props.list.length);
  const [sugg, setSugg] = createSignal<Addr[]>([]);
  const [hi, setHi] = createSignal(0);
  /** ↑/↓ used since the last keystroke: Enter then means "this suggestion". */
  let navigated = false;
  // Only the row losing and the one gaining the highlight update (not every row).
  const isHi = createSelector(hi);
  const listId = `sugg-${props.label.toLowerCase()}`;
  let seq = 0;
  const closeSugg = () => {
    seq++;
    setSugg([]);
  };
  const lookup = (q: string) => {
    const my = ++seq;
    if (!q.trim() || /[,;<]/.test(q)) return setSugg([]);
    void api.suggestAddresses(q.trim(), 8).then(
      (found) => {
        if (my !== seq) return; // a newer keystroke's answer wins
        const taken = new Set(props.list.map((a) => a.addr.toLowerCase()));
        setSugg(found.filter((a) => !taken.has(a.addr.toLowerCase())));
        setHi(0);
      },
      () => {},
    );
  };
  /** Take suggestion `i` out of the suggestions, for good (Shift+Delete, or its ×). */
  const forget = (i: number) => {
    const a = sugg()[i];
    if (!a) return;
    void api.forgetAddress(a.addr).catch(() => {});
    const rest = sugg().filter((_, j) => j !== i);
    setSugg(rest);
    setHi(Math.min(hi(), Math.max(0, rest.length - 1)));
  };
  /** Complete addresses become chips; anything else stays as text to fix (never a chip that
   *  can only be deleted). */
  const commit = () => {
    closeSugg();
    const parsed = parseAddresses(text());
    const good = parsed.filter((a) => isAddress(a.addr));
    const bad = parsed.filter((a) => !isAddress(a.addr));
    if (good.length) {
      const i = insertAt();
      props.onChange([...props.list.slice(0, i), ...good, ...props.list.slice(i)]);
    }
    setText(bad.map(chipText).join(', '));
    setInvalid(bad.length > 0);
    // done editing in place: further typing goes at the end again
    if (!bad.length) setAt(null);
  };
  /** A chip back to text, in its place, to edit it (click on it). */
  const edit = (i: number) => {
    const a = props.list[i];
    if (!a) return;
    if (text().trim()) commit(); // text already being typed goes in first
    const list = props.list;
    const k = list.indexOf(a);
    if (k < 0) return;
    props.onChange(list.filter((_, j) => j !== k));
    setAt(k);
    setText(chipText(a));
    setInvalid(false);
    queueMicrotask(() => {
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);
    });
  };
  const pick = (a: Addr) => {
    closeSugg();
    props.onChange([...props.list, a]);
    setText('');
    input.focus();
  };
  onMount(() => {
    if (props.autofocus) input.focus();
  });
  return (
    <div class="cmp-row" onMouseDown={(e) => e.target === e.currentTarget && (e.preventDefault(), input.focus())}>
      <label class="cmp-label">{props.label}</label>
      <div class="addr-field" onMouseDown={(e) => e.target === e.currentTarget && (e.preventDefault(), input.focus())}>
        <For each={props.list}>
          {(a, i) => (
            <span
              class="addr-chip"
              style={{ order: i() < insertAt() ? i() * 2 : i() * 2 + 2 }}
              classList={{ bad: !isAddress(a.addr) }}
              title={`${a.addr} · click to edit`}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => edit(i())}
            >
              {a.name || a.addr}
              <button
                class="addr-x"
                aria-label={`Remove ${a.addr}`}
                tabIndex={-1}
                onClick={(e) => {
                  e.stopPropagation();
                  props.onChange(props.list.filter((_, j) => j !== i()));
                }}
              >
                <CloseIcon size={9} />
              </button>
            </span>
          )}
        </For>
        <input
          ref={input}
          class="addr-input"
          style={{ order: insertAt() * 2 + 1 }}
          classList={{ invalid: invalid(), 'in-place': at() != null }}
          size={at() != null ? Math.max(6, text().length + 1) : undefined}
          type="text"
          autocomplete="off"
          spellcheck={false}
          aria-label={props.label}
          role="combobox"
          aria-autocomplete="list"
          aria-expanded={sugg().length > 0}
          aria-controls={listId}
          aria-activedescendant={sugg().length > 0 ? `${listId}-${hi()}` : undefined}
          value={text()}
          onInput={(e) => {
            navigated = false;
            setText(e.currentTarget.value);
            setInvalid(false);
            lookup(e.currentTarget.value);
          }}
          onKeyDown={(e) => {
            const n = sugg().length;
            if (n > 0) {
              if (e.key === 'Delete' && e.shiftKey) {
                e.preventDefault();
                forget(hi());
                return;
              }
              if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                e.preventDefault();
                navigated = true;
                setHi((hi() + (e.key === 'ArrowDown' ? 1 : n - 1)) % n);
                return;
              }
              // A complete address typed out is taken as typed, unless a suggestion was chosen
              // with ↑/↓.
              if ((e.key === 'Enter' || e.key === 'Tab') && (navigated || !isAddress(text()))) {
                e.preventDefault();
                pick(sugg()[hi()]!);
                return;
              }
            }
            // With text typed, Esc is about the suggestions (shown, or still on their way):
            // it closes them, never the window (the composer leaves this Esc alone).
            if (e.key === 'Escape' && (n > 0 || text().trim())) {
              e.preventDefault();
              closeSugg();
              return;
            }
            if ((e.key === 'Enter' || e.key === ',' || e.key === ';') && text().trim()) {
              e.preventDefault();
              commit();
            } else if (e.key === 'Tab' && text().trim()) {
              commit();
            } else if (e.key === 'Backspace' && !text() && insertAt() > 0) {
              // quick removal of the address before the cursor; clicking a chip edits it
              const i = insertAt() - 1;
              props.onChange(props.list.filter((_, j) => j !== i));
              if (at() != null) setAt(i);
            }
          }}
          onBlur={commit}
          onPaste={(e) => {
            const t = e.clipboardData?.getData('text') ?? '';
            if (/[,;\n]/.test(t)) {
              e.preventDefault();
              setText(text() + t);
              commit();
            }
          }}
        />
        {/* Over the rows below (no layout shift); mousedown keeps focus in the input. */}
        <Show when={sugg().length > 0}>
          <div class="addr-sugg" id={listId} role="listbox" aria-label="Suggestions">
            <For each={sugg()}>
              {(a, i) => (
                <div
                  id={`${listId}-${i()}`}
                  role="option"
                  class="addr-sugg-row"
                  classList={{ on: isHi(i()) }}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pick(a);
                  }}
                  onMouseEnter={() => setHi(i())}
                >
                  <span class="sugg-name">{a.name || a.addr}</span>
                  <Show when={a.name}>
                    <span class="sugg-addr">{a.addr}</span>
                  </Show>
                  <button
                    class="sugg-forget"
                    tabIndex={-1}
                    title="Don't suggest this address (Shift+Delete)"
                    aria-label={`Don't suggest ${a.addr}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      forget(i());
                    }}
                  >
                    <CloseIcon size={12} />
                  </button>
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </div>
  );
}

// ------------------------------------------------------------- formatting bar

function Toolbar(props: {
  editor: () => Editor | undefined;
  tick: () => number;
  linking: boolean;
  setLinking: (v: boolean) => void;
  onAttach: () => void;
  ai: boolean;
  onAi: () => void;
}) {
  const linking = () => props.linking;
  const setLinking = props.setLinking;
  let linkInput: HTMLInputElement | undefined;
  const active = (name: string) => {
    props.tick();
    return props.editor()?.isActive(name) ?? false;
  };
  const run = (f: (e: Editor) => void) => () => {
    const ed = props.editor();
    if (ed) f(ed);
  };
  // The colour palettes (text, highlight): one open at a time, under its button.
  const [palette, setPalette] = createSignal<'color' | 'highlight' | null>(null);
  let paletteEl: HTMLDivElement | undefined;
  createEffect(() => {
    if (!palette()) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element;
      if (!paletteEl?.contains(t) && !t.closest?.('.fmt-color-btn')) setPalette(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        setPalette(null);
        props.editor()?.commands.focus();
      }
    };
    document.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    onCleanup(() => {
      document.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey, true);
    });
  });
  const current = (which: 'color' | 'highlight'): string | null => {
    props.tick();
    const ed = props.editor();
    if (!ed) return null;
    return (which === 'color' ? ed.getAttributes('textStyle').color : ed.getAttributes('highlight').color) ?? null;
  };
  const pick = (which: 'color' | 'highlight', value: string | null) => {
    setPalette(null);
    const ed = props.editor();
    if (!ed) return;
    const chain = ed.chain().focus();
    if (which === 'color') (value ? chain.setColor(value) : chain.unsetColor()).run();
    else (value ? chain.setHighlight({ color: value }) : chain.unsetHighlight()).run();
  };
  const ColorButton = (p: { which: 'color' | 'highlight'; label: string; key?: string; glyph: string }) => (
    <span class="fmt-color">
      <button
        class="fmt-btn fmt-color-btn"
        tabIndex={-1}
        classList={{ on: palette() === p.which }}
        title={`${p.label}${p.key ? ` (${p.key})` : ''}`}
        aria-label={p.label}
        aria-haspopup="menu"
        aria-expanded={palette() === p.which}
        onMouseDown={(e) => e.preventDefault()}
        onClick={() => setPalette(palette() === p.which ? null : p.which)}
      >
        <span class="fmt-color-glyph">{p.glyph}</span>
        {/* the colour now: under the letter, or the marker's tip */}
        <span class="fmt-color-bar" style={{ background: current(p.which) ?? (p.which === 'color' ? 'currentColor' : 'transparent') }} />
      </button>
      <Show when={palette() === p.which}>
        <div ref={paletteEl} class="fmt-palette" role="menu" aria-label={p.label}>
          <For each={p.which === 'color' ? TEXT_COLORS : HIGHLIGHTS}>
            {(c) => (
              <button
                class="fmt-swatch"
                role="menuitemradio"
                aria-checked={current(p.which) === c.value}
                aria-label={c.name}
                title={c.name}
                classList={{ none: !c.value }}
                style={c.value ? { background: c.value } : undefined}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => pick(p.which, c.value)}
              />
            )}
          </For>
        </div>
      </Show>
    </span>
  );
  const openLink = () => setLinking(true);
  // Opened from the button or ⌘K: start from the link under the cursor, if any.
  createEffect(() => {
    if (!linking()) return;
    queueMicrotask(() => {
      if (!linkInput) return;
      linkInput.value = (props.editor()?.getAttributes('link').href as string | undefined) ?? '';
      linkInput.focus();
      linkInput.select();
    });
  });
  const applyLink = () => {
    const ed = props.editor();
    const url = linkInput?.value.trim() ?? '';
    setLinking(false);
    if (!ed) return;
    const chain = ed.chain().focus().extendMarkRange('link');
    if (url) chain.setLink({ href: /^[a-z][a-z0-9+.-]*:/i.test(url) ? url : `https://${url}` }).run();
    else chain.unsetLink().run();
  };
  // Out of the Tab order (Tab from the subject goes straight to the text); the buttons stay
  // clickable and their shortcuts (⌘B…) work in the text.
  const B = (p: { label: string; key: string; name?: string; onClick: () => void; children: string }) => (
    <button
      class="fmt-btn"
      tabIndex={-1}
      classList={{ on: !!p.name && active(p.name) }}
      title={`${p.label}${p.key ? ` (${p.key})` : ''}`}
      aria-label={p.label}
      aria-pressed={p.name ? active(p.name) : undefined}
      onMouseDown={(e) => e.preventDefault()}
      onClick={p.onClick}
    >
      {p.children}
    </button>
  );
  return (
    <div class="cmp-toolbar" role="toolbar" aria-label="Formatting">
      <Show
        when={!linking()}
        fallback={
          <span class="link-edit">
            <input
              ref={linkInput}
              class="link-input"
              type="url"
              placeholder="https://"
              aria-label="Link address"
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  applyLink();
                } else if (e.key === 'Escape') {
                  e.preventDefault();
                  e.stopPropagation();
                  setLinking(false);
                  props.editor()?.commands.focus();
                }
              }}
            />
            <button class="link-btn" onClick={applyLink}>
              Apply
            </button>
          </span>
        }
      >
        <B label="Bold" key={keyText('B')} name="bold" onClick={run((e) => e.chain().focus().toggleBold().run())}>
          B
        </B>
        <B label="Italic" key={keyText('I')} name="italic" onClick={run((e) => e.chain().focus().toggleItalic().run())}>
          I
        </B>
        <B label="Underline" key={keyText('U')} name="underline" onClick={run((e) => e.chain().focus().toggleUnderline().run())}>
          U
        </B>
        <ColorButton which="color" label="Text colour" glyph="A" />
        <ColorButton which="highlight" label="Highlight" key={keyText('H', { shift: true })} glyph="🖍" />
        <B label="Link" key={keyText('K')} name="link" onClick={openLink}>
          🔗
        </B>
        <B label="Code" key={keyText('E')} name="code" onClick={run((e) => e.chain().focus().toggleCode().run())}>
          {'</>'}
        </B>
        <span class="fmt-sep" />
        <B label="Heading" key={keyText('1', { alt: true })} name="heading" onClick={run((e) => e.chain().focus().toggleHeading({ level: 1 }).run())}>
          H
        </B>
        <B label="Bulleted list" key={keyText('8', { shift: true })} name="bulletList" onClick={run((e) => e.chain().focus().toggleBulletList().run())}>
          •
        </B>
        <B label="Numbered list" key={keyText('7', { shift: true })} name="orderedList" onClick={run((e) => e.chain().focus().toggleOrderedList().run())}>
          1.
        </B>
        <B label="Quote" key={keyText('B', { shift: true })} name="blockquote" onClick={run((e) => e.chain().focus().toggleBlockquote().run())}>
          ❝
        </B>
        <B label="Code block" key={keyText('C', { alt: true })} name="codeBlock" onClick={run((e) => e.chain().focus().toggleCodeBlock().run())}>
          {'{ }'}
        </B>
        <span class="fmt-fill" />
        <button
          class="fmt-btn fmt-ai"
          tabIndex={-1}
          classList={{ on: props.ai }}
          title={`Writing help (${MOD}J)`}
          aria-label="Writing help"
          aria-pressed={props.ai}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => props.onAi()}
        >
          ✨ AI
        </button>
        <button class="fmt-btn fmt-attach" tabIndex={-1} title="Attach files" aria-label="Attach files" onClick={() => props.onAttach()}>
          <PaperclipIcon size={15} />
        </button>
      </Show>
    </div>
  );
}

/** How code looks in the message sent (and in the editor, adjusted to the theme there). */
const MONO = "ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace";
const RULE_STYLE = 'border:0;border-top:1px solid #d0d7de;margin:14px 0';

/** Code blocks that take several lines as one: with lines (paragraphs, list items…) selected,
 *  the toolbar's button or ⌥⌘C makes one block of them all, each a line of it, as Slack and
 *  GitHub do (TipTap's own makes a block of each). In a code block it turns back into text,
 *  a paragraph per line. */
const CodeBlockLines = CodeBlock.extend({
  addCommands() {
    return {
      ...this.parent?.(),
      toggleCodeBlock:
        (attributes) =>
        ({ state, dispatch, commands }) => {
          const { $from, $to } = state.selection;
          const type = state.schema.nodes[this.name]!;
          const para = state.schema.nodes.paragraph!;
          // In a code block: its lines back as paragraphs.
          if ($from.parent.type === type && $from.sameParent($to)) {
            const block = $from.parent;
            const start = $from.before();
            const lines = block.textContent.split('\n');
            if (dispatch) {
              const nodes = lines.map((l) => para.create(null, l ? state.schema.text(l) : null));
              const tr = state.tr.replaceWith(start, start + block.nodeSize, nodes);
              tr.setSelection(TextSelection.near(tr.doc.resolve(start + 1)));
              dispatch(tr.scrollIntoView());
            }
            return true;
          }
          // Within one line: as TipTap does.
          if ($from.sameParent($to)) return commands.toggleNode(this.name, 'paragraph', attributes);
          // Several: the top-level blocks they're in, as one block, a line each.
          const from = $from.before(1);
          const to = $to.after(1);
          const text = state.doc.textBetween(from, to, '\n', '\n');
          if (dispatch) {
            const tr = state.tr.replaceWith(from, to, type.create(attributes, text ? state.schema.text(text) : null));
            tr.setSelection(TextSelection.near(tr.doc.resolve(from + 1)));
            dispatch(tr.scrollIntoView());
          }
          return true;
        },
    };
  },
});

/** Headings styled inline per level (the recipient's mail app has none of the app's CSS):
 *  sized for a message, not a web page. */
const HEADING_STYLES: Record<number, string> = {
  1: 'font-size:20px;line-height:1.3;font-weight:600;margin:16px 0 8px',
  2: 'font-size:17px;line-height:1.35;font-weight:600;margin:14px 0 6px',
  3: 'font-size:15px;line-height:1.4;font-weight:600;margin:12px 0 4px',
};
const SizedHeading = Heading.extend({
  renderHTML({ node, HTMLAttributes }) {
    const level = this.options.levels.includes(node.attrs.level) ? node.attrs.level : this.options.levels[0];
    return [`h${level}`, mergeAttributes(this.options.HTMLAttributes, HTMLAttributes, { style: HEADING_STYLES[level] ?? '' }), 0];
  },
});

/** The colours offered: readable on the white a message is read on (and the theme's own). */
const TEXT_COLORS: { name: string; value: string | null }[] = [
  { name: 'Default', value: null },
  { name: 'Gray', value: '#6b7280' },
  { name: 'Red', value: '#d1242f' },
  { name: 'Orange', value: '#c2570c' },
  { name: 'Green', value: '#1a7f37' },
  { name: 'Blue', value: '#0969da' },
  { name: 'Purple', value: '#8250df' },
  { name: 'Pink', value: '#bf3989' },
];
const HIGHLIGHTS: { name: string; value: string | null }[] = [
  { name: 'None', value: null },
  { name: 'Yellow', value: '#fff3a3' },
  { name: 'Green', value: '#c6f0c2' },
  { name: 'Blue', value: '#cfe3ff' },
  { name: 'Pink', value: '#ffd6e8' },
  { name: 'Orange', value: '#ffdcb8' },
  { name: 'Purple', value: '#e4d8ff' },
  { name: 'Gray', value: '#e5e7eb' },
];

/** A shortcut as the platform writes it: ⌥⇧⌘8 on a Mac, Ctrl+Shift+8 elsewhere. */
function keyText(key: string, mods: { shift?: boolean; alt?: boolean } = {}): string {
  if (platform === 'macos') return `${mods.alt ? '⌥' : ''}${mods.shift ? '⇧' : ''}⌘${key}`;
  return ['Ctrl', mods.alt && 'Alt', mods.shift && 'Shift', key].filter(Boolean).join('+');
}

const CODE_STYLE = `font-family:${MONO};font-size:90%;background:#f3f4f6;border:1px solid #e1e4e8;border-radius:4px;padding:1px 4px`;
const CODE_BLOCK_STYLE = `font-family:${MONO};font-size:90%;line-height:1.45;background:#f6f8fa;border:1px solid #e1e4e8;border-radius:6px;padding:10px 12px;white-space:pre-wrap;overflow-wrap:anywhere`;

// ------------------------------------------------------------- composer

export function Composer() {
  // Mounted per message (keyed in App): the editor starts from this state's HTML.
  const c0 = compose()!;
  let editorEl!: HTMLDivElement;
  let root!: HTMLElement;
  const [editor, setEditor] = createSignal<Editor>();
  const [tick, setTick] = createSignal(0);
  const [linking, setLinking] = createSignal(false);
  const [showQuote, setShowQuote] = createSignal(false);
  const [aiOpen, setAiOpen] = createSignal(false);
  const [dropping, setDropping] = createSignal(false);
  onMount(() => onFileDrop(setDropping, addFiles));
  const c = () => compose() as ComposeState;

  // Text still in an address field (not a chip): checked on Send. A complete address is
  // added and sent; anything else stops the send there, with the cursor in that field.
  const typing: Partial<Record<'to' | 'cc' | 'bcc', { text: string; focus: () => void }>> = {};
  const track = (field: 'to' | 'cc' | 'bcc') => (text: string, focus: () => void) => {
    typing[field] = { text, focus };
  };
  const trySend = () => {
    let cur = c();
    for (const field of ['to', 'cc', 'bcc'] as const) {
      const t = typing[field];
      if (!t?.text.trim()) continue;
      const parsed = parseAddresses(t.text);
      const bad = parsed.find((a) => !isAddress(a.addr));
      if (bad) {
        showToast({ kind: 'error', text: `“${bad.addr}” isn't a complete email address.` });
        t.focus();
        return;
      }
      cur = { ...cur, [field]: [...cur[field], ...parsed] };
    }
    void send(cur);
  };

  onMount(() => {
    const ed = new Editor({
      element: editorEl,
      extensions: [
        StarterKit.configure({
          heading: false,
          // Code (`text`, ⌘E) and code blocks (```, ⌥⌘C): styled inline, as sent, since the
          // recipient's mail app has none of the app's CSS.
          code: { HTMLAttributes: { style: CODE_STYLE } },
          codeBlock: false,
          // `---`: a line across.
          horizontalRule: { HTMLAttributes: { style: RULE_STYLE } },
          link: { openOnClick: false, autolink: true, defaultProtocol: 'https' },
        }),
        CodeBlockLines.configure({ HTMLAttributes: { style: CODE_BLOCK_STYLE } }),
        // Text and highlight colours, as inline styles (span color, mark background); `==text==`
        // and ⇧⌘H highlight in yellow.
        TextStyle,
        Color,
        Highlight.configure({ multicolor: true }),
        // `# `, `## `, `### ` (⌥⌘1–3): headings, sized for a message, not a page.
        SizedHeading.configure({ levels: [1, 2, 3] }),
        Placeholder.configure({ placeholder: 'Write your message…' }),
        // Pictures pasted in sit in the text (sized by dragging a corner); sent as inline parts.
        Image.configure({
          inline: true,
          allowBase64: true,
          resize: { enabled: true, directions: ['top-left', 'top-right', 'bottom-left', 'bottom-right'], alwaysPreserveAspectRatio: true, minWidth: 24, minHeight: 24 },
        }),
      ],
      editorProps: {
        // A picture on the clipboard (a screenshot, an image copied from a page): into the
        // message where the cursor is, not attached. Files dropped on the window still attach.
        handlePaste: (_view, event) => {
          const files = pastedPictures(event.clipboardData);
          if (!files.length) return false;
          void (async () => {
            for (const f of files) {
              try {
                const p = await scalePicture(f, { max: 1600, budget: 900_000, showMax: 640 });
                const ed = editor();
                ed?.chain().focus().insertContentAt(ed.state.selection.to, { type: 'image', attrs: p }).run();
              } catch {
                showToast({ kind: 'error', text: 'Couldn’t read the picture.' });
              }
            }
          })();
          return true;
        },
      },
      content: c0.html,
      onUpdate: ({ editor: e }) => updateCompose({ html: e.isEmpty ? '' : e.getHTML() }),
      onTransaction: () => setTick((t) => t + 1),
    });
    setEditor(ed);
    // Reopened (undo, edit after a failure) or a reply with recipients: straight to the text.
    if (c0.to.length > 0) ed.commands.focus('end');
    onCleanup(() => ed.destroy());
  });

  // Closing (Esc, ⌘W, the window's close button): keep it as a draft, or not.
  const [asking, setAsking] = createSignal<((d: 'save' | 'discard' | 'cancel') => void) | null>(null);
  const decide = (): Promise<'save' | 'discard' | 'cancel'> => {
    const cur = compose();
    if (!cur || !hasContent(cur)) return Promise.resolve('discard');
    return new Promise((resolve) => setAsking(() => resolve));
  };
  const answer = (d: 'save' | 'discard' | 'cancel') => {
    asking()?.(d);
    setAsking(null);
  };
  const close = async () => {
    const d = await decide();
    if (d === 'save') await saveAndClose();
    else if (d === 'discard') await discardAndClose();
  };
  onCloseRequested(async () => {
    const d = await decide();
    if (d === 'cancel') return false;
    if (d === 'save') await saveNow();
    else if (composeDraftId != null) await api.deleteDraft(composeDraftId).catch(() => {});
    return true;
  });
  const discard = async () => {
    const cur = compose();
    if (cur && hasContent(cur) && !(await confirmDialog('Discard this message?', 'Discard'))) return;
    await discardAndClose();
  };

  createEffect(() => {
    const cur = c();
    setWindowTitle(cur.subject.trim() || { new: 'New Message', reply: 'Reply', replyAll: 'Reply All', forward: 'Forward' }[cur.mode]);
  });


  const onKey = (e: KeyboardEvent) => {
    if (!(e.target instanceof Node) || !root.contains(e.target)) return;
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key === 'Enter') {
      e.preventDefault();
      trySend();
    } else if (mod && !e.shiftKey && e.key.toLowerCase() === 'k' && editorEl.contains(document.activeElement)) {
      e.preventDefault();
      setLinking(true);
    } else if (mod && !e.shiftKey && e.key.toLowerCase() === 'j') {
      e.preventDefault();
      setAiOpen(!aiOpen());
    } else if (mod && !e.shiftKey && e.key.toLowerCase() === 'w') {
      e.preventDefault();
      void close();
    } else if (
      e.key === 'Escape' &&
      !(e.target as Element).closest?.('.link-edit') &&
      // writing help handles its own Esc (stop, then close the panel)
      !(e.target as Element).closest?.('.ai-panel') &&
      // an address field's Esc that closed its suggestions
      !(e.defaultPrevented && (e.target as Element).closest?.('.addr-field')) &&
      !document.querySelector('.modal-backdrop')
    ) {
      e.preventDefault();
      void close();
    }
  };
  // On the document, after the fields' own handlers (Solid delegates those to the document
  // too, registered earlier): the link input's Esc is handled there first.
  onMount(() => {
    document.addEventListener('keydown', onKey);
    onCleanup(() => document.removeEventListener('keydown', onKey));
  });

  const accountList = () => accounts() ?? [];
  const from = () => accountList().find((a) => a.id === c().accountId);
  onMount(() => void loadIdentities().catch(() => {}));
  // Every address mail can go out from: each account's own, then its others.
  const fromKey = (accountId: number, identity: number | null | undefined) => `${accountId}:${identity ?? ''}`;
  // Shown as it goes out: the name with the address (the account's name where the address
  // has none of its own; an account without a name is stored with its address as the name).
  const asSent = (name: string | undefined, email: string) => (name && name.trim() && name.trim() !== email ? `${name.trim()} <${email}>` : email);
  const fromOptions = () =>
    accountList().flatMap((a) => [
      { value: fromKey(a.id, null), label: asSent(a.displayName, a.email) },
      ...identities()
        .filter((i) => i.accountId === a.id)
        .map((i) => ({ value: fromKey(a.id, i.id), label: asSent(i.name || a.displayName, i.email) })),
    ]);

  // Signatures: the one picked shows under the text (it isn't part of the editor, so switching
  // never touches what's written). Another account: its default, unless one was picked by hand.
  onMount(() => void loadSignatures().catch(() => {}));
  const pickSignature = (id: number | null) => {
    const s = signatureById(id);
    updateCompose({ signatureId: s?.id ?? null, signatureHtml: s?.html ?? null });
  };
  /** Another address to send from (perhaps another account's): its default signature, unless
   *  one was picked by hand. */
  const changeFrom = (key: string) => {
    const [a, i] = key.split(':');
    const accountId = Number(a);
    const fromIdentity = i ? Number(i) : null;
    const cur = c();
    const reply = cur.mode !== 'new';
    const was = defaultSignature(sigs(), cur.accountId, reply, identityById(cur.fromIdentity));
    if ((cur.signatureId ?? null) !== (was?.id ?? null)) return updateCompose({ accountId, fromIdentity });
    const next = defaultSignature(sigs(), accountId, reply, identityById(fromIdentity));
    updateCompose({ accountId, fromIdentity, signatureId: next?.id ?? null, signatureHtml: next?.html ?? null });
  };

  // Reply ⇄ Reply all: the recipients each would fill in, to switch between them.
  const [replyCtx, setReplyCtx] = createSignal<{ reply: ReplyRecipients; replyAll: ReplyRecipients } | null>(null);
  onMount(() => {
    const id = c().replyingTo;
    if (id == null || (c().mode !== 'reply' && c().mode !== 'replyAll')) return;
    void Promise.all([api.replyContext(id, 'reply'), api.replyContext(id, 'replyAll')])
      .then(([r, all]) => setReplyCtx({ reply: { to: r.to, cc: r.cc }, replyAll: { to: all.to, cc: all.cc } }))
      .catch(() => {});
  });
  /** Reply all would add no one: nothing to switch to. */
  const nobodyElse = () => {
    const k = replyCtx();
    if (!k) return false;
    const set = (r: ReplyRecipients) => [...r.to, ...r.cc].map((a) => a.addr.toLowerCase()).sort().join(' ');
    return set(k.reply) === set(k.replyAll);
  };
  function switchMode(mode: 'reply' | 'replyAll') {
    const cur = c();
    const k = replyCtx();
    if (!k || cur.mode === mode || (cur.mode !== 'reply' && cur.mode !== 'replyAll')) return;
    const next = switchRecipients(cur, k[cur.mode], k[mode]);
    updateCompose({ mode, ...next, showCc: cur.showCc || next.cc.length > 0 });
  }

  return (
    <section class="reader composer" aria-label="New message" ref={root}>
      {/* Compact: the subject is in the window title and the field below, not repeated here. */}
      <header class="cmp-bar" data-tauri-drag-region={DRAG}>
        <Show
          when={(c().mode === 'reply' || c().mode === 'replyAll') && c().replyingTo != null}
          fallback={
            <span class="cmp-kind">
              <Show when={c().mode === 'forward'}>
                <ForwardIcon size={14} />
              </Show>
              {c().mode === 'forward' ? 'Forward' : 'New message'}
            </span>
          }
        >
          {/* Which reply this is, at a glance, and the other one a click away. */}
          <div class="cmp-mode" role="radiogroup" aria-label="Reply to">
            <button
              role="radio"
              aria-checked={c().mode === 'reply'}
              classList={{ on: c().mode === 'reply' }}
              title="Reply to the sender only"
              onClick={() => switchMode('reply')}
            >
              <ReplyIcon size={14} />
              Reply
            </button>
            <button
              role="radio"
              aria-checked={c().mode === 'replyAll'}
              classList={{ on: c().mode === 'replyAll' }}
              title={nobodyElse() ? 'No one else was on the original' : 'Reply to everyone on the original'}
              disabled={nobodyElse() && c().mode !== 'replyAll'}
              onClick={() => switchMode('replyAll')}
            >
              <ReplyAllIcon size={14} />
              Reply all
            </button>
          </div>
        </Show>
        <div class="cmp-actions" data-tauri-drag-region={DRAG}>
          <span class="cmp-hint">{MOD === '⌘' ? '⌘↵' : 'Ctrl+Enter'} to send · Esc to close</span>
          <Show when={asking()}>
            <Modal label="Close message" class="confirm cmp-close" onClose={() => answer('cancel')}>
              <p>Keep this message as a draft?</p>
              <div class="cmp-close-btns">
                <button class="btn secondary" onClick={() => answer('discard')}>
                  Discard
                </button>
                <span class="cmp-close-gap" />
                <button class="btn secondary" onClick={() => answer('cancel')}>
                  Cancel
                </button>
                <button class="btn primary" ref={(el) => queueMicrotask(() => el.focus())} onClick={() => answer('save')}>
                  Save draft
                </button>
              </div>
            </Modal>
          </Show>
          <button class="icon-btn" title="Discard" aria-label="Discard" onClick={() => void discard()}>
            <TrashIcon />
          </button>
          <button class="btn primary cmp-send" onClick={trySend}>
            Send
          </button>
        </div>
      </header>
      <div class="cmp-fields">
        <div class="cmp-row">
          <label class="cmp-label">From</label>
          <Show
            when={fromOptions().length > 1}
            fallback={<span class="cmp-from">{from() ? asSent(from()!.displayName, from()!.email) : ''}</span>}
          >
            <Select
              class="cmp-select"
              variant="plain"
              label="From"
              value={fromKey(c().accountId, c().fromIdentity)}
              options={fromOptions()}
              onChange={(key) => changeFrom(key)}
            />
          </Show>
        </div>
        <div class="cmp-to">
          <AddressField label="To" list={c().to} onChange={(to) => updateCompose({ to })} onTyping={track('to')} autofocus={c0.to.length === 0} />
          <Show when={!c().showCc}>
            <button class="link-btn cmp-cc-toggle" onClick={() => updateCompose({ showCc: true })}>
              Cc Bcc
            </button>
          </Show>
        </div>
        <Show when={c().showCc}>
          <AddressField label="Cc" list={c().cc} onChange={(cc) => updateCompose({ cc })} onTyping={track('cc')} />
          <AddressField label="Bcc" list={c().bcc} onChange={(bcc) => updateCompose({ bcc })} onTyping={track('bcc')} />
        </Show>
        <div class="cmp-row">
          <label class="cmp-label" for="cmp-subject">
            Subject
          </label>
          <input
            id="cmp-subject"
            class="cmp-subject"
            type="text"
            value={c().subject}
            onInput={(e) => updateCompose({ subject: e.currentTarget.value })}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                // Synchronously (TipTap's focus command waits a frame): the next keys land in the text.
                const ed = editor();
                ed?.view.focus();
                ed?.commands.setTextSelection(0);
              }
            }}
          />
        </div>
      </div>
      <Toolbar
        editor={editor}
        tick={tick}
        linking={linking()}
        setLinking={setLinking}
        onAttach={() => void pickFiles().then(addFiles)}
        ai={aiOpen()}
        onAi={() => setAiOpen(!aiOpen())}
      />
      <Show when={aiOpen()}>
        <AiPanel
          mode={c().mode}
          context={() => {
            const cur = c();
            const a = from();
            // The writing help writes as whoever the mail is from.
            const i = identityById(cur.fromIdentity);
            const name = i?.name || a?.displayName;
            const email = i?.email ?? a?.email;
            return {
              me: email ? (name ? `${name} <${email}>` : email) : '',
              subject: cur.subject,
              to: [...cur.to, ...cur.cc].map((x) => (x.name ? `${x.name} <${x.addr}>` : x.addr)),
              html: cur.html,
              quoteHtml: cur.quoteHtml,
            };
          }}
          onUse={(html) => {
            const ed = editor();
            if (!ed) return;
            ed.commands.setContent(html, { emitUpdate: true });
            updateCompose({ html: ed.isEmpty ? '' : ed.getHTML() });
            ed.commands.focus('end');
          }}
          onClose={() => {
            setAiOpen(false);
            editor()?.commands.focus();
          }}
        />
      </Show>
      <Show when={dropping()}>
        <div class="cmp-drop" aria-hidden="true">
          <span>Drop files to attach</span>
        </div>
      </Show>
      <div class="cmp-scroll" onMouseDown={(e) => e.target === e.currentTarget && (e.preventDefault(), editor()?.commands.focus('end'))}>
        <div class="cmp-editor" classList={{ 'has-quote': !!c().quoteHtml || c().fwdAttachments.length > 0 }} ref={editorEl} />
        <Show when={sigs().signatures.length > 0 || c().signatureHtml}>
          <div class="cmp-sig" onMouseDown={(e) => e.stopPropagation()}>
            <Select
              class="cmp-sig-select"
              variant="plain"
              label="Signature"
              value={c().signatureId ?? null}
              options={[{ value: null, label: 'No signature' }, ...sigs().signatures.map((s) => ({ value: s.id as number | null, label: s.name }))]}
              onChange={(id) => pickSignature(id)}
            />
            <Show when={c().signatureHtml}>
              {/* Sanitized by the core when it was saved. */}
              <div class="cmp-sig-body" innerHTML={c().signatureHtml!} />
            </Show>
          </div>
        </Show>
        <Show when={c().fwdAttachments.length + (c().files?.length ?? 0) + (c().fwdMessages?.length ?? 0) > 0}>
          <div class="cmp-atts" aria-label="Attachments">
            <For each={c().fwdMessages ?? []}>
              {(m) => (
                <span class="cmp-att cmp-att-msg" title={`${m.subject || '(no subject)'} — sent as an attachment`}>
                  <MailIcon size={12} />
                  <span class="cmp-att-name">{m.subject || '(no subject)'}.eml</span>
                  <span class="cmp-att-size">{fmtSize(m.size)}</span>
                  <button
                    class="addr-x"
                    aria-label={`Don't send ${m.subject || 'this message'}`}
                    title="Remove"
                    onClick={() => updateCompose({ fwdMessages: (c().fwdMessages ?? []).filter((x) => x.id !== m.id) })}
                  >
                    <CloseIcon size={9} />
                  </button>
                </span>
              )}
            </For>
            <For each={c().files ?? []}>
              {(f) => (
                <span class="cmp-att" title={f.path}>
                  <PaperclipIcon size={12} />
                  <span class="cmp-att-name">{f.name}</span>
                  <span class="cmp-att-size">{fmtSize(f.size)}</span>
                  <button
                    class="addr-x"
                    aria-label={`Remove ${f.name}`}
                    title="Remove"
                    onClick={() => updateCompose({ files: (c().files ?? []).filter((x) => x.path !== f.path) })}
                  >
                    <CloseIcon size={9} />
                  </button>
                </span>
              )}
            </For>
            <For each={c().fwdAttachments}>
              {(a) => (
                <span class="cmp-att" title={a.name}>
                  <PaperclipIcon size={12} />
                  <span class="cmp-att-name">{a.name}</span>
                  <span class="cmp-att-size">{fmtSize(a.size)}</span>
                  <button
                    class="addr-x"
                    aria-label={`Don't send ${a.name}`}
                    title="Remove"
                    onClick={() => updateCompose({ fwdAttachments: c().fwdAttachments.filter((x) => x.index !== a.index) })}
                  >
                    <CloseIcon size={9} />
                  </button>
                </span>
              )}
            </For>
          </div>
          <Show when={attachedBytes(c()) > GMAIL_LIMIT}>
            <p class="cmp-att-warn">
              Attachments total {fmtSize(attachedBytes(c()))}: over the 25 MB most servers (Gmail included) accept.
            </p>
          </Show>
        </Show>
        <Show when={c().quoteHtml}>
          {(q) => (
            // A reply's quote is folded (it can be a whole conversation); a forward's is the content.
            <Show
              when={c().mode === 'forward' || showQuote()}
              fallback={
                <button class="quote-pill cmp-quote-pill" title="Show quoted text" aria-label="Show quoted text" onClick={() => setShowQuote(true)}>
                  •••
                </button>
              }
            >
              <div class="cmp-quote" aria-label={c().mode === 'forward' ? 'Forwarded message' : 'Quoted message'}>
                <AutoHeightFrame doc={buildSrcdoc(q(), false, QUOTE_FRAME_CSS)} />
              </div>
            </Show>
          )}
        </Show>
      </div>
    </section>
  );
}
