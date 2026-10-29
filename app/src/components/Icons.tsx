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
export const ReplyIcon = (p: P) => (
  <Svg {...p}>
    <path d="M6 3.5 2.5 7 6 10.5M2.5 7H9a4.5 4.5 0 0 1 4.5 4.5v1" />
  </Svg>
);
export const ForwardIcon = (p: P) => (
  <Svg {...p}>
    <path d="M10 3.5 13.5 7 10 10.5M13.5 7H7a4.5 4.5 0 0 0-4.5 4.5v1" />
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
export const HelpIcon = (p: P) => (
  <Svg {...p}>
    <circle cx="8" cy="8" r="6" />
    <path d="M6.3 6.2a1.8 1.8 0 0 1 3.5.5c0 1.2-1.8 1.5-1.8 2.6" />
    <path d="M8 11.4h.01" />
  </Svg>
);
export const TagIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 3.5v4.1l6.4 6.4 5.1-5.1-6.4-6.4H3.5a1 1 0 0 0-1 1z" />
    <circle cx="5.5" cy="5.5" r="1" fill="currentColor" stroke="none" />
  </Svg>
);
export const SidebarIcon = (p: P) => (
  <Svg {...p}>
    <rect x="2" y="2.75" width="12" height="10.5" rx="1.75" />
    <path d="M6 2.75v10.5" />
  </Svg>
);
export const FoldersIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 3.5h4l1.2 1.3h5.8v7.7h-11z" />
    <path d="M5 8h6M5 10.5h4" />
  </Svg>
);
export const MoveIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2 4.5A1.5 1.5 0 0 1 3.5 3h2.8l1.4 1.5h4.8A1.5 1.5 0 0 1 14 6v5.5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5z" />
    <path d="M6 9h4.5M9 7.25 10.75 9 9 10.75" />
  </Svg>
);
export const StarFilledIcon = (p: P) => (
  <Svg {...p} fill>
    <path d="m8 1.6 1.95 4.05 4.45.55-3.27 3.1.83 4.4L8 11.55 4.04 13.7l.83-4.4L1.6 6.2l4.45-.55z" />
  </Svg>
);
export const UnreadIcon = (p: P) => (
  <Svg {...p}>
    <rect x="1.75" y="3.75" width="10.5" height="8.5" rx="1.5" />
    <path d="m2.5 5 4.75 3.25L12 5" />
    <circle cx="13" cy="3.5" r="2" fill="currentColor" stroke="none" />
  </Svg>
);
export const ReadIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2 6.5 8 2.5l6 4V12a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 12z" />
    <path d="m2.5 7 5.5 3.75L13.5 7" />
  </Svg>
);
export const MoreIcon = (p: P) => (
  <Svg {...p} fill>
    <circle cx="3.5" cy="8" r="1.25" />
    <circle cx="8" cy="8" r="1.25" />
    <circle cx="12.5" cy="8" r="1.25" />
  </Svg>
);
export const DeleteForeverIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 4h11" />
    <path d="M6 4V2.5h4V4" />
    <path d="M3.8 4l.7 9a1 1 0 0 0 1 .9h5a1 1 0 0 0 1-.9l.7-9" />
    <path d="m6.5 7 3 3.5M9.5 7l-3 3.5" />
  </Svg>
);
export const ExternalIcon = (p: P) => (
  <Svg {...p}>
    <path d="M9 2.75h4.25V7" />
    <path d="M13.25 2.75 7.5 8.5" />
    <path d="M11.5 9.5v2.75a1 1 0 0 1-1 1h-6.75a1 1 0 0 1-1-1V5.5a1 1 0 0 1 1-1H6.5" />
  </Svg>
);
export const DownloadIcon = (p: P) => (
  <Svg {...p}>
    <path d="M8 2.5v7.5" />
    <path d="m4.75 7 3.25 3.25L11.25 7" />
    <path d="M3 13h10" />
  </Svg>
);
export const CloudIcon = (p: P) => (
  <Svg {...p}>
    <path d="M4.5 12.5h7a2.75 2.75 0 0 0 .4-5.47A4 4 0 0 0 4.2 7.6 2.5 2.5 0 0 0 4.5 12.5z" />
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
    {/* Back bubble (upper left), drawn only where the front bubble doesn't cover it. */}
    <path d="M10.5 6V4a1.75 1.75 0 0 0-1.75-1.75h-5A1.75 1.75 0 0 0 2 4v2.5a1.75 1.75 0 0 0 1.75 1.75H4V10l1.75-1.75" />
    {/* Front bubble (lower right) with its tail. */}
    <path d="M7.25 6h5A1.75 1.75 0 0 1 14 7.75v2.5A1.75 1.75 0 0 1 12.25 12H12v1.75L10.25 12h-3A1.75 1.75 0 0 1 5.5 10.25v-2.5A1.75 1.75 0 0 1 7.25 6z" />
  </Svg>
);

export const SlidersIcon = (p: P) => (
  <Svg {...p}>
    <path d="M2.5 5h5.75M11.75 5h1.75M2.5 11h1.75M7.75 11h5.75" />
    <circle cx="10" cy="5" r="1.75" />
    <circle cx="6" cy="11" r="1.75" />
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
