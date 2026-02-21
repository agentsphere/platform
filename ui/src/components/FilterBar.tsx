export interface FilterDef {
  key: string;
  label: string;
  type: 'select' | 'text';
  options?: { value: string; label: string }[];
  placeholder?: string;
}

interface Props {
  filters: FilterDef[];
  values: Record<string, string>;
  onChange: (values: Record<string, string>) => void;
  onApply: () => void;
}

export function FilterBar({ filters, values, onChange, onApply }: Props) {
  const setValue = (key: string, value: string) => {
    onChange({ ...values, [key]: value });
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Enter') onApply();
  };

  return (
    <div class="filter-bar">
      {filters.map(f => (
        <div key={f.key} class="filter-item">
          <label class="filter-label">{f.label}</label>
          {f.type === 'select' ? (
            <select class="input filter-input" value={values[f.key] || ''}
              onChange={(e) => setValue(f.key, (e.target as HTMLSelectElement).value)}>
              {f.options?.map(o => (
                <option key={o.value} value={o.value}>{o.label}</option>
              ))}
            </select>
          ) : (
            <input class="input filter-input" value={values[f.key] || ''}
              placeholder={f.placeholder || ''}
              onInput={(e) => setValue(f.key, (e.target as HTMLInputElement).value)}
              onKeyDown={handleKeyDown} />
          )}
        </div>
      ))}
      <div class="filter-item filter-actions">
        <button class="btn btn-primary btn-sm" onClick={onApply}>Search</button>
      </div>
    </div>
  );
}
