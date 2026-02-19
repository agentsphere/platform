import { useEffect } from 'preact/hooks';

interface Props {
  open: boolean;
  onClose: () => void;
  title: string;
  children: any;
}

export function Modal({ open, onClose, title, children }: Props) {
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div class="modal-overlay" onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div class="modal">
        <div class="modal-title">{title}</div>
        {children}
      </div>
    </div>
  );
}
