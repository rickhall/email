import { createMemo, createSignal, For, onMount, Show } from 'solid-js';
import { Portal } from 'solid-js/web';
import { api, errorMessage, type TagDef } from '../api';
import { refreshTags, sortTags, tagInfo, tagList, tagTint } from '../lib/tags';
import { confirmDialog, placeNear, useDismiss } from './Actions';
import { setSearchQuery } from '../state';
import { CheckIcon, CloseIcon } from './Icons';

/** Defined tags in position order: the ones 1–9 and the menu act on. */
export const definedTags = () => tagList().filter((t) => t.defined);

// ------------------------------------------------------------- indicators

/** Up to 4 coloured dots then "+N" (list rows, card headers). */
export function TagDots(props: { tags: readonly string[] }) {
  const sorted = createMemo(() => sortTags(props.tags));
  const title = () => sorted().map((k) => tagInfo(k).name).join(', ');
  return (
    <Show when={sorted().length > 0}>
      <span class="tag-dots" title={title()} aria-label={`Tags: ${title()}`}>
        <For each={sorted().slice(0, 4)}>{(k) => <span class="tag-dot" style={{ background: tagInfo(k).color }} />}</For>
        <Show when={sorted().length > 4}>
          <span class="tag-more">+{sorted().length - 4}</span>
        </Show>
      </span>
    </Show>
  );
}

/** CSS vars that tint the sender like Thunderbird (first tag), readable in both themes. */
export function tintStyle(tags: readonly string[]): Record<string, string> | undefined {
  if (tags.length === 0) return undefined;
  const t = tagTint(tagInfo(sortTags(tags)[0]!).color);
  return { '--tint-l': t.light, '--tint-d': t.dark };
}

/** Reader chips: dot + name, × removes, click runs a tag search. One line, "+N" when crowded. */
export function TagChips(props: { tags: readonly string[]; onRemove: (k: string) => void; onOpen?: (k: string) => void }) {
  const sorted = createMemo(() => sortTags(props.tags));
  const MAX = 3;
  return (
    <Show when={sorted().length > 0}>
      <span class="tag-chips">
        <For each={sorted().slice(0, MAX)}>
          {(k) => (
            <span class="tag-chip" title={tagInfo(k).defined ? tagInfo(k).name : `${k} (not defined here)`}>
              <button class="tag-chip-main" onClick={() => props.onOpen?.(k)} aria-label={`Search tag ${tagInfo(k).name}`}>
                <span class="tag-dot" style={{ background: tagInfo(k).color }} />
                <span class="tag-chip-name">{tagInfo(k).name}</span>
              </button>
              <button class="tag-chip-x" aria-label={`Remove tag ${tagInfo(k).name}`} title="Remove tag" onClick={() => props.onRemove(k)}>
                <CloseIcon size={8} />
              </button>
            </span>
          )}
        </For>
        <Show when={sorted().length > MAX}>
          <span class="tag-more" title={sorted().slice(MAX).map((k) => tagInfo(k).name).join(', ')}>
            +{sorted().length - MAX}
          </span>
        </Show>
      </span>
    </Show>
  );
}

/** Clicking a reader chip: search for that tag. */
export function searchTag(keyword: string): void {
  const name = tagInfo(keyword).defined ? tagInfo(keyword).name : keyword;
  setSearchQuery(`tag:"${name}"`);
}

// ------------------------------------------------------------- tag menu

const MENU_W = 240;

/**
 * Tags with checkmarks for the viewed messages (✓ all have it, – some do). Clicking toggles;
 * 1–9 work while it's open too. Footer: Remove all tags (0), Manage tags….
 */
export function TagMenu(props: {
  anchor: HTMLElement;
  tagSets: string[][];
  onToggle: (keyword: string, on: boolean) => void;
  onClear: () => void;
  onClose: (refocus: boolean) => void;
}) {
  let el: HTMLDivElement | undefined;
  const [pos, setPos] = createSignal(placeNear(props.anchor, MENU_W, 260));
  const reposition = () => setPos(placeNear(props.anchor, MENU_W, el?.offsetHeight || 260));
  useDismiss(() => el, props.anchor, () => props.onClose(false), reposition);
  onMount(() =>
    queueMicrotask(() => {
      reposition();
      el?.querySelector<HTMLElement>('.vm-item')?.focus();
    }),
  );
  const state = (k: string): 'all' | 'some' | 'none' => {
    const n = props.tagSets.filter((s) => s.includes(k)).length;
    return n === 0 ? 'none' : n === props.tagSets.length ? 'all' : 'some';
  };
  const toggle = (k: string) => props.onToggle(k, state(k) !== 'all');
  return (
    <Portal>
      <div
        ref={el}
        class="view-menu tag-menu"
        role="menu"
        aria-label="Tags"
        style={{ left: `${pos().left}px`, top: `${pos().top}px`, right: 'auto', width: `${MENU_W}px` }}
        onKeyDown={(e) => {
          e.stopPropagation();
          const list = [...(el?.querySelectorAll<HTMLElement>('.vm-item') ?? [])];
          const i = list.indexOf(document.activeElement as HTMLElement);
          if (e.key === 'Escape') {
            e.preventDefault();
            props.onClose(true);
          } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            list[(i + (e.key === 'ArrowDown' ? 1 : -1) + list.length) % list.length]?.focus();
          } else if (/^[1-9]$/.test(e.key)) {
            e.preventDefault();
            const t = definedTags()[Number(e.key) - 1];
            if (t) toggle(t.keyword);
          } else if (e.key === '0') {
            e.preventDefault();
            props.onClear();
          }
        }}
      >
        <For each={tagList()}>
          {(t, i) => (
            <button
              role="menuitemcheckbox"
              aria-checked={state(t.keyword) === 'all' ? 'true' : state(t.keyword) === 'some' ? 'mixed' : 'false'}
              class="vm-item"
              onClick={() => toggle(t.keyword)}
            >
              <span class="vm-check" classList={{ on: state(t.keyword) !== 'none' }} aria-hidden="true">
                <Show when={state(t.keyword) === 'all'} fallback={<Show when={state(t.keyword) === 'some'}>–</Show>}>
                  <CheckIcon size={11} />
                </Show>
              </span>
              <span class="tag-dot" style={{ background: t.color }} />
              <span class="vm-label" classList={{ muted: !t.defined }}>
                {t.name}
              </span>
              <Show when={t.defined && i() < 9}>
                <kbd>{i() + 1}</kbd>
              </Show>
            </button>
          )}
        </For>
        <div class="vm-sep" />
        <button role="menuitem" class="vm-item" onClick={() => props.onClear()}>
          <span class="vm-check" aria-hidden="true" />
          <span class="vm-label">Remove all tags</span>
          <kbd>0</kbd>
        </button>
        <button
          role="menuitem"
          class="vm-item"
          onClick={() => {
            props.onClose(false);
            openManageTags();
          }}
        >
          <span class="vm-check" aria-hidden="true" />
          <span class="vm-label">Manage tags…</span>
        </button>
      </div>
    </Portal>
  );
}

// ------------------------------------------------------------- manage dialog

const PRESETS = ['#FF0000', '#FF9900', '#FFCC00', '#009900', '#00A3A3', '#3333FF', '#007AFF', '#993399', '#FF2D96', '#8E5A2B', '#5F6B7A', '#000000'];
const [manageOpen, setManageOpen] = createSignal(false);
export const openManageTags = () => setManageOpen(true);

function ColorPicker(props: { value: string; onPick: (c: string) => void }) {
  return (
    <div class="tag-palette">
      <For each={PRESETS}>
        {(c) => (
          <button
            class="tag-swatch"
            classList={{ on: c.toLowerCase() === props.value.toLowerCase() }}
            style={{ background: c }}
            aria-label={`Colour ${c}`}
            onClick={() => props.onPick(c)}
          />
        )}
      </For>
      <label class="tag-custom" title="Custom colour">
        <input type="color" value={props.value} onInput={(e) => props.onPick(e.currentTarget.value.toUpperCase())} />
      </label>
    </div>
  );
}

function TagRow(props: { t: TagDef; onError: (s: string | null) => void }) {
  const [name, setName] = createSignal(props.t.name);
  const [color, setColor] = createSignal(props.t.color);
  const [palette, setPalette] = createSignal(false);
  const save = async (c = color(), n = name()) => {
    props.onError(null);
    try {
      await api.setTag(props.t.keyword, n.trim(), c);
      await refreshTags();
    } catch (e) {
      props.onError(errorMessage(e));
      setName(props.t.name);
      setColor(props.t.color);
    }
  };
  return (
    <div class="tag-row" classList={{ undefined: !props.t.defined }}>
      <div class="tag-row-main">
        <button class="tag-swatch big" style={{ background: color() }} aria-label="Change colour" onClick={() => setPalette((v) => !v)} />
        <input
          class="tag-name"
          value={name()}
          aria-label="Tag name"
          onInput={(e) => setName(e.currentTarget.value)}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === 'Enter') e.currentTarget.blur();
          }}
          onBlur={() => props.t.defined && name().trim() !== props.t.name && void save()}
        />
        <span class="tag-kw" title="IMAP keyword">
          {props.t.keyword}
        </span>
        <Show
          when={props.t.defined}
          fallback={
            <>
              <span class="tag-from">from another app</span>
              <button class="btn secondary tag-btn" onClick={() => void save()}>
                Define
              </button>
            </>
          }
        >
          <button
            class="act-btn danger"
            aria-label={`Delete tag ${props.t.name}`}
            title="Delete tag"
            onClick={async () => {
              const ok = await confirmDialog(
                `Delete tag "${props.t.name}"? Messages keep the tag on the server; it will show as '${props.t.keyword}' until defined again.`,
                'Delete tag',
              );
              if (!ok) return;
              try {
                await api.deleteTag(props.t.keyword);
                await refreshTags();
              } catch (e) {
                props.onError(errorMessage(e));
              }
            }}
          >
            <CloseIcon size={11} />
          </button>
        </Show>
      </div>
      <Show when={palette()}>
        <ColorPicker
          value={color()}
          onPick={(c) => {
            setColor(c);
            if (props.t.defined) void save(c);
          }}
        />
      </Show>
    </div>
  );
}

function ManageTags() {
  const [err, setErr] = createSignal<string | null>(null);
  const [newName, setNewName] = createSignal('');
  const [newColor, setNewColor] = createSignal(PRESETS[6]!);
  const [newPalette, setNewPalette] = createSignal(false);
  const add = async () => {
    if (!newName().trim()) return;
    setErr(null);
    try {
      await api.setTag(null, newName().trim(), newColor());
      setNewName('');
      await refreshTags();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  onMount(() => void refreshTags());
  return (
    <div
      class="modal-backdrop"
      onMouseDown={(e) => e.target === e.currentTarget && setManageOpen(false)}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === 'Escape' && !document.querySelector('.modal.confirm')) setManageOpen(false);
      }}
    >
      <div class="card modal manage-tags" role="dialog" aria-modal="true" aria-label="Manage tags">
        <div class="modal-head">
          <h2>Tags</h2>
          <button class="icon-btn" onClick={() => setManageOpen(false)} aria-label="Close">
            <CloseIcon />
          </button>
        </div>
        <div class="tag-rows">
          <For each={tagList()}>{(t) => <TagRow t={t} onError={setErr} />}</For>
        </div>
        <div class="tag-new">
          <button class="tag-swatch big" style={{ background: newColor() }} aria-label="Colour for the new tag" onClick={() => setNewPalette((v) => !v)} />
          <input
            class="tag-name"
            placeholder="New tag"
            value={newName()}
            onInput={(e) => setNewName(e.currentTarget.value)}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === 'Enter') void add();
            }}
          />
          <button class="btn primary tag-btn" disabled={!newName().trim()} onClick={() => void add()}>
            Add
          </button>
        </div>
        <Show when={newPalette()}>
          <ColorPicker value={newColor()} onPick={setNewColor} />
        </Show>
        <Show when={err()}>
          <div class="form-error" role="alert">
            {err()}
          </div>
        </Show>
      </div>
    </div>
  );
}

export function ManageTagsHost() {
  return (
    <Show when={manageOpen()}>
      <ManageTags />
    </Show>
  );
}
