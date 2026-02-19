export function Badge({ status }: { status: string }) {
  const cls = status.toLowerCase().replace(/\s+/g, '_');
  return <span class={`badge badge-${cls}`}>{status}</span>;
}
