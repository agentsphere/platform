import { useState } from 'preact/hooks';

export interface Column<T> {
  key: string;
  label: string;
  sortable?: boolean;
  render?: (item: T) => any;
  class?: string;
}

interface Props<T> {
  columns: Column<T>[];
  data: T[];
  loading?: boolean;
  emptyMessage?: string;
  onRowClick?: (item: T) => void;
  keyFn?: (item: T) => string;
}

export function Table<T extends Record<string, any>>({ columns, data, loading, emptyMessage, onRowClick, keyFn }: Props<T>) {
  const [sortKey, setSortKey] = useState<string | null>(null);
  const [sortAsc, setSortAsc] = useState(true);

  const toggleSort = (key: string) => {
    if (sortKey === key) {
      setSortAsc(!sortAsc);
    } else {
      setSortKey(key);
      setSortAsc(true);
    }
  };

  const sorted = sortKey
    ? [...data].sort((a, b) => {
        const av = a[sortKey], bv = b[sortKey];
        const cmp = av < bv ? -1 : av > bv ? 1 : 0;
        return sortAsc ? cmp : -cmp;
      })
    : data;

  if (loading) {
    return (
      <div class="table-skeleton">
        {[0, 1, 2, 3, 4].map(i => (
          <div key={i} class="skeleton-row" />
        ))}
      </div>
    );
  }

  if (data.length === 0) {
    return <div class="empty-state">{emptyMessage || 'No data'}</div>;
  }

  return (
    <table class="table">
      <thead>
        <tr>
          {columns.map(col => (
            <th key={col.key}
              class={col.sortable ? 'sortable-th' : ''}
              onClick={col.sortable ? () => toggleSort(col.key) : undefined}>
              {col.label}
              {col.sortable && sortKey === col.key && (
                <span class="sort-indicator">{sortAsc ? ' ^' : ' v'}</span>
              )}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {sorted.map((item, idx) => (
          <tr key={keyFn ? keyFn(item) : idx}
            class={onRowClick ? 'table-link' : ''}
            onClick={onRowClick ? () => onRowClick(item) : undefined}>
            {columns.map(col => (
              <td key={col.key} class={col.class || ''}>
                {col.render ? col.render(item) : item[col.key]}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}
