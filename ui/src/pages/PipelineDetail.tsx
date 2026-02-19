import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { PipelineDetail as PipelineDetailType, PipelineStep, Artifact } from '../lib/types';
import { timeAgo, duration } from '../lib/format';
import { Badge } from '../components/Badge';

interface Props { id?: string; pipelineId?: string; }

export function PipelineDetail({ id: projectId, pipelineId }: Props) {
  const [pipeline, setPipeline] = useState<PipelineDetailType | null>(null);
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [selectedStep, setSelectedStep] = useState<PipelineStep | null>(null);
  const [logs, setLogs] = useState('');

  useEffect(() => {
    if (!projectId || !pipelineId) return;
    api.get<PipelineDetailType>(`/api/projects/${projectId}/pipelines/${pipelineId}`)
      .then(setPipeline).catch(() => {});
    api.get<ListResponse<Artifact>>(`/api/projects/${projectId}/pipelines/${pipelineId}/artifacts`)
      .then(r => setArtifacts(r.items)).catch(() => {});
  }, [projectId, pipelineId]);

  const viewLogs = async (step: PipelineStep) => {
    setSelectedStep(step);
    setLogs('Loading...');
    try {
      const res = await fetch(`/api/projects/${projectId}/pipelines/${pipelineId}/steps/${step.id}/logs`, {
        credentials: 'include',
      });
      setLogs(await res.text());
    } catch {
      setLogs('Failed to load logs');
    }
  };

  const cancel = async () => {
    try {
      await api.post(`/api/projects/${projectId}/pipelines/${pipelineId}/cancel`);
      const updated = await api.get<PipelineDetailType>(`/api/projects/${projectId}/pipelines/${pipelineId}`);
      setPipeline(updated);
    } catch { /* ignore */ }
  };

  if (!pipeline) return <div class="empty-state">Loading...</div>;

  return (
    <div>
      <div class="mb-md">
        <a href={`/projects/${projectId}/builds`} class="text-sm text-muted">Back to pipelines</a>
      </div>
      <div class="flex-between mb-md">
        <h2>Pipeline <span class="mono text-sm">{pipeline.git_ref}</span></h2>
        <div class="flex gap-sm">
          <Badge status={pipeline.status} />
          {(pipeline.status === 'pending' || pipeline.status === 'running') && (
            <button class="btn btn-danger btn-sm" onClick={cancel}>Cancel</button>
          )}
        </div>
      </div>

      <div class="card mb-md">
        <div class="text-sm">
          <span class="text-muted">Trigger:</span> {pipeline.trigger}
          {pipeline.commit_sha && (
            <span> · <span class="mono">{pipeline.commit_sha.substring(0, 8)}</span></span>
          )}
          <span class="text-muted"> · {timeAgo(pipeline.created_at)}</span>
          {pipeline.started_at && pipeline.finished_at && (
            <span class="text-muted"> · {duration(new Date(pipeline.finished_at).getTime() - new Date(pipeline.started_at).getTime())}</span>
          )}
        </div>
      </div>

      <h3 style="font-size:1rem" class="mb-md">Steps</h3>
      <div class="card">
        <table class="table">
          <thead><tr><th>#</th><th>Name</th><th>Image</th><th>Status</th><th>Duration</th><th></th></tr></thead>
          <tbody>
            {pipeline.steps.map(s => (
              <tr key={s.id}>
                <td class="text-muted">{s.step_order}</td>
                <td>{s.name}</td>
                <td class="mono text-xs">{s.image}</td>
                <td><Badge status={s.status} /> {s.exit_code != null && <span class="text-xs text-muted">exit {s.exit_code}</span>}</td>
                <td class="text-sm">{s.duration_ms != null ? duration(s.duration_ms) : '—'}</td>
                <td><button class="btn btn-ghost btn-sm" onClick={() => viewLogs(s)}>Logs</button></td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {selectedStep && (
        <div class="mt-md">
          <div class="flex-between mb-md">
            <h3 style="font-size:1rem">Logs: {selectedStep.name}</h3>
            <button class="btn btn-sm" onClick={() => { setSelectedStep(null); setLogs(''); }}>Close</button>
          </div>
          <div class="log-viewer">{logs}</div>
        </div>
      )}

      {artifacts.length > 0 && (
        <div class="mt-md">
          <h3 style="font-size:1rem" class="mb-md">Artifacts</h3>
          <div class="card">
            <table class="table">
              <thead><tr><th>Name</th><th>Type</th><th>Size</th><th></th></tr></thead>
              <tbody>
                {artifacts.map(a => (
                  <tr key={a.id}>
                    <td>{a.name}</td>
                    <td class="text-xs text-muted">{a.content_type || '—'}</td>
                    <td class="text-sm">{a.size_bytes != null ? `${a.size_bytes} B` : '—'}</td>
                    <td>
                      <a class="btn btn-sm" href={`/api/projects/${projectId}/pipelines/${pipelineId}/artifacts/${a.id}/download`}>
                        Download
                      </a>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
