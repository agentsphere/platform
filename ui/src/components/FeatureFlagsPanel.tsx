import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';

interface Flag {
  id: string;
  key: string;
  default_value: boolean;
  description: string;
  flag_type: string;
  environment: string | null;
}

interface Props {
  projectId: string;
  projectName: string;
}

export function FeatureFlagsPanel({ projectId }: Props) {
  const [flags, setFlags] = useState<Flag[]>([]);
  const [toggling, setToggling] = useState<string | null>(null);
  const [unlocked, setUnlocked] = useState(false);

  useEffect(() => {
    api.get<ListResponse<Flag>>(`/api/projects/${projectId}/flags?limit=10`)
      .then(r => setFlags(r.items))
      .catch(() => {});
  }, [projectId]);

  if (flags.length === 0) return null;

  const toggle = async (flag: Flag, e: Event) => {
    e.stopPropagation();
    if (!unlocked) return;
    setToggling(flag.id);
    try {
      await api.post(`/api/projects/${projectId}/flags/${flag.key}/toggle`, {});
      setFlags(prev =>
        prev.map(f => f.id === flag.id ? { ...f, default_value: !f.default_value } : f)
      );
    } catch (err) {
      console.warn('toggle flag:', err);
    } finally {
      setToggling(null);
    }
  };

  return (
    <div class="panel">
      <div class="panel-header">
        <span>Flags</span>
        <button
          class={`flag-lock-btn ${unlocked ? 'unlocked' : ''}`}
          onClick={(e: Event) => { e.stopPropagation(); setUnlocked(!unlocked); }}
          title={unlocked ? 'Lock flags' : 'Unlock to edit'}
        >
          {unlocked ? '\uD83D\uDD13' : '\uD83D\uDD12'}
        </button>
      </div>
      <div class="panel-body">
        {flags.map(f => (
          <div key={f.id} class="flag-row">
            <div class="flag-info">
              <span class="flag-key" title={f.description || f.key}>{f.key}</span>
            </div>
            <button
              class={`flag-toggle ${f.default_value ? 'on' : 'off'} ${!unlocked ? 'locked' : ''}`}
              onClick={(e) => toggle(f, e)}
              disabled={toggling === f.id || !unlocked}
              title={!unlocked ? 'Unlock to toggle' : f.default_value ? 'Enabled' : 'Disabled'}
            >
              <span class="flag-toggle-knob" />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
