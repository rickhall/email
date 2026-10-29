import { createEffect, createSignal, For, onCleanup, onMount, Show } from 'solid-js';
import { Editor } from '@tiptap/core';
import StarterKit from '@tiptap/starter-kit';
import Placeholder from '@tiptap/extension-placeholder';
import type { Addr } from '../api';
import { accounts } from '../state';
import { closeCompose, compose, hasContent, isAddress, parseAddresses, send, updateCompose, type ComposeState } from '../lib/compose';
import { MOD } from '../lib/shortcuts';
import { DRAG } from '../lib/platform';
import { confirmDialog } from './Actions';
import { CloseIcon, TrashIcon } from './Icons';

// ------------------------------------------------------------- address field

/** To/Cc/Bcc: typed addresses become chips on Enter, comma, semicolon, Tab or leaving the field. */
function AddressField(props: { label: string; list: Addr[]; onChange: (l: Addr[]) => void; autofocus?: boolean }) {
  let input!: HTMLInputElement;
  const [text, setText] = createSignal('');
  const commit = () => {
    const parsed = parseAddresses(text());
    if (parsed.length) props.onChange([...props.list, ...parsed]);
    setText('');
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
            <span class="addr-chip" classList={{ bad: !isAddress(a.addr) }} title={a.addr}>
              {a.name || a.addr}
              <button
                class="addr-x"
                aria-label={`Remove ${a.addr}`}
                tabIndex={-1}
                onClick={() => props.onChange(props.list.filter((_, j) => j !== i()))}
              >
                <CloseIcon size={9} />
              </button>
            </span>
          )}
        </For>
        <input
          ref={input}
          class="addr-input"
          type="text"
          autocomplete="off"
          spellcheck={false}
          aria-label={props.label}
          value={text()}
          onInput={(e) => setText(e.currentTarget.value)}
          onKeyDown={(e) => {
            if ((e.key === 'Enter' || e.key === ',' || e.key === ';') && text().trim()) {
              e.preventDefault();
              commit();
            } else if (e.key === 'Tab' && text().trim()) {
              commit();
            } else if (e.key === 'Backspace' && !text() && props.list.length) {
              props.onChange(props.list.slice(0, -1));
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
      </div>
    </div>
  );
}

// ------------------------------------------------------------- formatting bar

function Toolbar(props: { editor: () => Editor | undefined; tick: () => number; linking: boolean; setLinking: (v: boolean) => void }) {
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
  const c = () => compose() as ComposeState;

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

  const discard = async () => {
    const cur = compose();
    if (cur && hasContent(cur) && !(await confirmDialog('Discard this message?', 'Discard'))) return;
    closeCompose();
  };

  const onKey = (e: KeyboardEvent) => {
    if (!(e.target instanceof Node) || !root.contains(e.target)) return;
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key === 'Enter') {
      e.preventDefault();
      void send(c());
    } else if (mod && !e.shiftKey && e.key.toLowerCase() === 'k' && editorEl.contains(document.activeElement)) {
      e.preventDefault();
      setLinking(true);
    } else if (e.key === 'Escape' && !(e.target as Element).closest?.('.link-edit') && !document.querySelector('.modal-backdrop')) {
      e.preventDefault();
      void discard();
    }
    // Single-key app shortcuts (e, #, j…) act on the list behind: not while writing here.
    if (!mod) e.stopPropagation();
  };
  // On the document, after the fields' own handlers (Solid delegates those to the document
  // too, registered earlier), but before the app's shortcuts on the window.
  onMount(() => {
    document.addEventListener('keydown', onKey);
    onCleanup(() => document.removeEventListener('keydown', onKey));
  });

  const accountList = () => accounts() ?? [];
  const from = () => accountList().find((a) => a.id === c().accountId);

  return (
    <section class="reader composer" aria-label="New message" ref={root}>
      <header class="reader-band cmp-band" data-tauri-drag-region={DRAG}>
        <h1 class="reader-subject" title={c().subject || 'New message'}>
          {c().subject.trim() || 'New message'}
        </h1>
        <div class="band-meta cmp-actions" data-tauri-drag-region={DRAG}>
          <span class="cmp-hint">{MOD === '⌘' ? '⌘↵' : 'Ctrl+Enter'} to send · Esc to close</span>
          <button class="icon-btn" title="Discard" aria-label="Discard" onClick={() => void discard()}>
            <TrashIcon />
          </button>
          <button class="btn primary cmp-send" onClick={() => void send(c())}>
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
          <AddressField label="To" list={c().to} onChange={(to) => updateCompose({ to })} autofocus={c0.to.length === 0} />
          <Show when={!c().showCc}>
            <button class="link-btn cmp-cc-toggle" onClick={() => updateCompose({ showCc: true })}>
              Cc Bcc
            </button>
          </Show>
        </div>
        <Show when={c().showCc}>
          <AddressField label="Cc" list={c().cc} onChange={(cc) => updateCompose({ cc })} />
          <AddressField label="Bcc" list={c().bcc} onChange={(bcc) => updateCompose({ bcc })} />
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
      <Toolbar editor={editor} tick={tick} linking={linking()} setLinking={setLinking} />
      <div class="cmp-scroll" onMouseDown={(e) => e.target === e.currentTarget && (e.preventDefault(), editor()?.commands.focus('end'))}>
        <div class="cmp-editor" ref={editorEl} />
      </div>
    </section>
  );
}
