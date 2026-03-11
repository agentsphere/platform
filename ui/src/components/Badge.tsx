export function Badge({ status, variant, children }: { status?: string; variant?: string; children?: any }) {
  const label = children ?? status ?? '';
  const cls = (variant ?? status ?? 'default').toLowerCase().replace(/\s+/g, '_');
  return <span class={`badge badge-${cls}`}>{label}</span>;
}
