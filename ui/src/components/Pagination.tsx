interface Props {
  total: number;
  limit: number;
  offset: number;
  onChange: (offset: number) => void;
}

export function Pagination({ total, limit, offset, onChange }: Props) {
  if (total <= limit) return null;
  const from = offset + 1;
  const to = Math.min(offset + limit, total);
  return (
    <div class="pagination">
      <span>Showing {from}–{to} of {total}</span>
      <div class="pagination-btns">
        <button class="btn btn-sm" disabled={offset === 0}
          onClick={() => onChange(Math.max(0, offset - limit))}>Prev</button>
        <button class="btn btn-sm" disabled={offset + limit >= total}
          onClick={() => onChange(offset + limit)}>Next</button>
      </div>
    </div>
  );
}
