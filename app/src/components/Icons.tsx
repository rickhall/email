import type { JSX } from 'solid-js';
import type { FolderRole } from '../api';

type P = { class?: string; size?: number };

function Svg(props: P & { children: JSX.Element; fill?: boolean }) {
  return (
    <svg
      class={props.class ?? 'icon'}
      width={props.size ?? 16}
      height={props.size ?? 16}
      viewBox="0 0 16 16"
      fill={props.fill ? 'currentColor' : 'none'}
      stroke={props.fill ? 'none' : 'currentColor'}
      stroke-width="1.4"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      {props.children}
    </svg>
  );
}

export const InboxIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2 9.5 3.6 3.3A1 1 0 0 1 4.6 2.5h6.8a1 1 0 0 1 1 .8L14 9.5V12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 12z" />
    <path d="M2 9.5h3.2l.8 1.5h4l.8-1.5H14" />
  </Svg>
);
export const StarIcon = (p: P) => (
  <Svg {...p}>
    <path d="m8 2 1.8 3.8 4.1.5-3 2.9.8 4.1L8 11.3l-3.7 2 .8-4.1-3-2.9 4.1-.5z" />
  </Svg>
);
export const ImportantIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 4h8.2L13.5 8l-2.8 4H2.5L5 8z" />
  </Svg>
);
export const SentIcon = (p: P) => (
  <Svg {...p}>
    <path d="M14 2 7 9" />
    <path d="M14 2 9.5 14 7 9 2 6.5z" />
  </Svg>
);
export const DraftIcon = (p: P) => (
  <Svg {...p}>
    <path d="M10.5 2.5 13.5 5.5 6 13H3v-3z" />
    <path d="m9 4 3 3" />
  </Svg>
);
export const ArchiveIcon = (p: P) => (
  <Svg {...p}>
    <rect x="2" y="2.5" width="12" height="3.5" rx="1" />
    <path d="M3 6v6.5a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V6" />
    <path d="M6.5 8.5h3" />
  </Svg>
);
export const AllMailIcon = (p: P) => (
  <Svg {...p}>
    <rect x="2" y="4.5" width="12" height="9" rx="1.5" />
    <path d="M4 2.5h8" />
    <path d="m2.5 6 5.5 3.8L13.5 6" />
  </Svg>
);
export const JunkIcon = (p: P) => (
  <Svg {...p}>
    <path d="M5.5 2h5L14 5.5v5L10.5 14h-5L2 10.5v-5z" />
    <path d="M8 5v3.5" />
    <path d="M8 11h.01" />
  </Svg>
);
export const TrashIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 4h11" />
    <path d="M6 4V2.5h4V4" />
    <path d="M3.8 4l.7 9a1 1 0 0 0 1 .9h5a1 1 0 0 0 1-.9l.7-9" />
  </Svg>
);
export const FolderIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2 4.5A1.5 1.5 0 0 1 3.5 3h2.8l1.4 1.5h4.8A1.5 1.5 0 0 1 14 6v5.5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5z" />
  </Svg>
);
export const ChevronIcon = (p: P) => (
  <Svg {...p}>
    <path d="m6 4 4 4-4 4" />
  </Svg>
);
export const PaperclipIcon = (p: P) => (
  <Svg {...p}>
    <path d="M13 7.5 8 12.5a3 3 0 0 1-4.3-4.3l5.3-5.3a2 2 0 0 1 2.8 2.8L6.6 11a1 1 0 0 1-1.4-1.4L10 4.8" />
  </Svg>
);
export const FlagIcon = (p: P) => (
  <Svg {...p} fill>
    <path d="M3.5 1.75a.75.75 0 0 1 .75.75v.3c1.1-.5 2.3-.6 3.7-.1 1.4.5 2.6.5 3.9-.1a.6.6 0 0 1 .85.55v6.6a.7.7 0 0 1-.4.6c-1.6.8-3.1.8-4.7.2-1.3-.5-2.3-.4-3.35.15v4.8a.75.75 0 0 1-1.5 0V2.5a.75.75 0 0 1 .75-.75z" />
  </Svg>
);
export const PlusIcon = (p: P) => (
  <Svg {...p}>
    <path d="M8 3v10M3 8h10" />
  </Svg>
);
export const ImageIcon = (p: P) => (
  <Svg {...p}>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <circle cx="6" cy="6.5" r="1.2" />
    <path d="m2.5 12 3.5-3.5 2.5 2.5 2-2 3 3" />
  </Svg>
);
export const FileIcon = (p: P) => (
  <Svg {...p}>
    <path d="M9 2H4.5A1.5 1.5 0 0 0 3 3.5v9A1.5 1.5 0 0 0 4.5 14h7a1.5 1.5 0 0 0 1.5-1.5V6z" />
    <path d="M9 2v4h4" />
  </Svg>
);
export const MailIcon = (p: P) => (
  <Svg {...p}>
    <rect x="1.75" y="3.25" width="12.5" height="9.5" rx="1.75" />
    <path d="m2.5 4.5 5.5 4 5.5-4" />
  </Svg>
);
export const CheckIcon = (p: P) => (
  <Svg {...p}>
    <path d="m3.5 8.5 3 3 6-7" />
  </Svg>
);
export const SearchIcon = (p: P) => (
  <Svg {...p}>
    <circle cx="7" cy="7" r="4.25" />
    <path d="m10.25 10.25 3.25 3.25" />
  </Svg>
);
export const CloseIcon = (p: P) => (
  <Svg {...p}>
    <path d="m4 4 8 8M12 4l-8 8" />
  </Svg>
);
export const LockIcon = (p: P) => (
  <Svg {...p}>
    <rect x="3" y="7" width="10" height="7" rx="1.5" />
    <path d="M5.25 7V5a2.75 2.75 0 0 1 5.5 0v2" />
  </Svg>
);
export const AlertIcon = (p: P) => (
  <Svg {...p}>
    <circle cx="8" cy="8" r="6" />
    <path d="M8 5v3.5M8 11h.01" />
  </Svg>
);

export const ThreadIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 4.25A1.75 1.75 0 0 1 4.25 2.5h5.5a1.75 1.75 0 0 1 1.75 1.75v3a1.75 1.75 0 0 1-1.75 1.75H6L3.5 11V9H4.25" />
    <path d="M11.5 5.5h.25a1.75 1.75 0 0 1 1.75 1.75v3A1.75 1.75 0 0 1 11.75 12H11v1.75L8.75 12h-2.5A1.75 1.75 0 0 1 4.6 10.9" />
  </Svg>
);

export function RoleIcon(props: { role: FolderRole } & P) {
  switch (props.role) {
    case 'inbox':
      return <InboxIcon {...props} />;
    case 'flagged':
      return <StarIcon {...props} />;
    case 'important':
      return <ImportantIcon {...props} />;
    case 'sent':
      return <SentIcon {...props} />;
    case 'drafts':
      return <DraftIcon {...props} />;
    case 'archive':
      return <ArchiveIcon {...props} />;
    case 'all':
      return <AllMailIcon {...props} />;
    case 'junk':
      return <JunkIcon {...props} />;
    case 'trash':
      return <TrashIcon {...props} />;
    default:
      return <FolderIcon {...props} />;
  }
}
