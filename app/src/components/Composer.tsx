import { createEffect, createSelector, createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { Editor } from '@tiptap/core';
import StarterKit from '@tiptap/starter-kit';
import Placeholder from '@tiptap/extension-placeholder';
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
import { api } from '../api';
import { composeDraftId, onCloseRequested, onFileDrop, pickFiles, setWindowTitle } from '../lib/windows';
import { GMAIL_LIMIT } from '../lib/compose';
import { MOD } from '../lib/shortcuts';
import { DRAG } from '../lib/platform';
import { confirmDialog } from './Actions';
import { CloseIcon, PaperclipIcon, TrashIcon } from './Icons';
import { AutoHeightFrame } from './Conversation';
import { buildSrcdoc } from './Reader';
import { fmtSize } from '../lib/format';
import { showToast } from './Toast';

/** The quoted original under the text: read-only, at the composer's margins. */
const QUOTE_FRAME_CSS = 'html{overflow-y:hidden}body{padding:0 0 12px}';

// ------------------------------------------------------------- address field

/** A chip as text for editing: "Name <addr>" or the address. */
const chipText = (a: Addr) => (a.name ? `${a.name} <${a.addr}>` : a.addr);

/** To/Cc/Bcc: typed addresses become chips on Enter, comma, semicolon, Tab or leaving the field;
 *  people from your mail are suggested as you type (↑/↓, Enter or Tab to pick, Esc to close). */
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
  const B = (p: { label: string; key: string; name?: string; onClick: () => void; children: string }) => (
    <button
      class="fmt-btn"
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
        <B label="Bold" key={`${MOD}B`} name="bold" onClick={run((e) => e.chain().focus().toggleBold().run())}>
          B
        </B>
        <B label="Italic" key={`${MOD}I`} name="italic" onClick={run((e) => e.chain().focus().toggleItalic().run())}>
          I
        </B>
        <B label="Underline" key={`${MOD}U`} name="underline" onClick={run((e) => e.chain().focus().toggleUnderline().run())}>
          U
        </B>
        <B label="Link" key={`${MOD}K`} name="link" onClick={openLink}>
          🔗
        </B>
        <span class="fmt-sep" />
        <B label="Bulleted list" key="" name="bulletList" onClick={run((e) => e.chain().focus().toggleBulletList().run())}>
          •
        </B>
        <B label="Numbered list" key="" name="orderedList" onClick={run((e) => e.chain().focus().toggleOrderedList().run())}>
          1.
        </B>
        <B label="Quote" key="" name="blockquote" onClick={run((e) => e.chain().focus().toggleBlockquote().run())}>
          ❝
        </B>
        <span class="fmt-fill" />
        <button class="fmt-btn fmt-attach" title="Attach files" aria-label="Attach files" onClick={() => props.onAttach()}>
          <PaperclipIcon size={15} />
        </button>
      </Show>
    </div>
  );
}

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
          codeBlock: false,
          horizontalRule: false,
          link: { openOnClick: false, autolink: true, defaultProtocol: 'https' },
        }),
        Placeholder.configure({ placeholder: 'Write your message…' }),
      ],
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
    } else if (mod && !e.shiftKey && e.key.toLowerCase() === 'w') {
      e.preventDefault();
      void close();
    } else if (
      e.key === 'Escape' &&
      !(e.target as Element).closest?.('.link-edit') &&
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

  return (
    <section class="reader composer" aria-label="New message" ref={root}>
      {/* Compact: the subject is in the window title and the field below, not repeated here. */}
      <header class="cmp-bar" data-tauri-drag-region={DRAG}>
        <span class="cmp-kind">{{ new: 'New message', reply: 'Reply', replyAll: 'Reply all', forward: 'Forward' }[c().mode]}</span>
        <div class="cmp-actions" data-tauri-drag-region={DRAG}>
          <span class="cmp-hint">{MOD === '⌘' ? '⌘↵' : 'Ctrl+Enter'} to send · Esc to close</span>
          <Show when={asking()}>
            <div class="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && answer('cancel')}>
              <div class="card modal confirm cmp-close" role="dialog" aria-modal="true" aria-label="Close message">
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
              </div>
            </div>
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
            when={accountList().length > 1}
            fallback={<span class="cmp-from">{from()?.email ?? ''}</span>}
          >
            <select
              class="cmp-select"
              aria-label="From"
              value={String(c().accountId)}
              onChange={(e) => updateCompose({ accountId: Number(e.currentTarget.value) })}
            >
              <For each={accountList()}>{(a) => <option value={String(a.id)}>{a.email}</option>}</For>
            </select>
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
      <Toolbar editor={editor} tick={tick} linking={linking()} setLinking={setLinking} onAttach={() => void pickFiles().then(addFiles)} />
      <Show when={dropping()}>
        <div class="cmp-drop" aria-hidden="true">
          <span>Drop files to attach</span>
        </div>
      </Show>
      <div class="cmp-scroll" onMouseDown={(e) => e.target === e.currentTarget && (e.preventDefault(), editor()?.commands.focus('end'))}>
        <div class="cmp-editor" classList={{ 'has-quote': !!c().quoteHtml || c().fwdAttachments.length > 0 }} ref={editorEl} />
        <Show when={c().fwdAttachments.length + (c().files?.length ?? 0) > 0}>
          <div class="cmp-atts" aria-label="Attachments">
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
