/** Thin vertical drag handle. Reports absolute width = startWidth + dx. */
export function Resizer(props: {
  width: () => number;
  onResize: (w: number) => void;
  onDone?: () => void;
  label: string;
}) {
  let startX = 0;
  let startW = 0;
  let dragging = false;

  const onPointerDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    dragging = true;
    startX = e.clientX;
    startW = props.width();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    document.body.classList.add('resizing');
  };
  const onPointerMove = (e: PointerEvent) => {
    if (!dragging) return;
    props.onResize(startW + (e.clientX - startX));
  };
  const end = (e: PointerEvent) => {
    if (!dragging) return;
    dragging = false;
    const el = e.currentTarget as HTMLElement;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
    document.body.classList.remove('resizing');
    props.onDone?.();
  };
  const onKeyDown = (e: KeyboardEvent) => {
    const step = e.shiftKey ? 40 : 10;
    if (e.key === 'ArrowLeft') props.onResize(props.width() - step);
    else if (e.key === 'ArrowRight') props.onResize(props.width() + step);
    else return;
    e.preventDefault();
    e.stopPropagation();
    props.onDone?.();
  };

  return (
    <div
      class="resizer"
      role="separator"
      aria-orientation="vertical"
      aria-label={props.label}
      aria-valuenow={Math.round(props.width())}
      tabIndex={0}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onKeyDown={onKeyDown}
    />
  );
}
