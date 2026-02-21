interface Props {
  status: string;
  label?: string;
}

const STATUS_COLORS: Record<string, string> = {
  healthy: 'var(--success)',
  success: 'var(--success)',
  running: 'var(--accent)',
  active: 'var(--success)',
  completed: 'var(--success)',
  syncing: 'var(--warning)',
  pending: 'var(--warning)',
  warning: 'var(--warning)',
  degraded: 'var(--warning)',
  failed: 'var(--danger)',
  failure: 'var(--danger)',
  error: 'var(--danger)',
  stopped: 'var(--text-muted)',
  cancelled: 'var(--text-muted)',
  inactive: 'var(--text-muted)',
};

export function StatusDot({ status, label }: Props) {
  const color = STATUS_COLORS[status.toLowerCase()] || 'var(--text-muted)';
  return (
    <span class="status-dot-wrapper">
      <span class="status-dot" style={{ backgroundColor: color }} />
      {label && <span class="status-dot-label">{label}</span>}
    </span>
  );
}
