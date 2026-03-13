import { useState, useRef } from 'preact/hooks';
import type { IframePanel } from '../lib/types';

interface Props {
  panels: IframePanel[];
}

export function IframePreview({ panels }: Props) {
  const [activeTab, setActiveTab] = useState(0);
  const iframeRef = useRef<HTMLIFrameElement>(null);

  if (panels.length === 0) return null;

  const clamped = Math.min(activeTab, panels.length - 1);
  const active = panels[clamped];

  const refreshIframe = () => {
    if (iframeRef.current) {
      iframeRef.current.src = active.preview_url;
    }
  };

  return (
    <div class="session-preview-panel">
      {panels.length > 1 && (
        <div class="preview-tabs">
          {panels.map((p, i) => (
            <button
              class={`preview-tab ${i === activeTab ? 'active' : ''}`}
              onClick={() => setActiveTab(i)}
            >
              :{p.port}
            </button>
          ))}
        </div>
      )}
      <div class="preview-toolbar">
        <span class="preview-toolbar-label text-xs text-muted">
          {active.service_name}:{active.port}
        </span>
        <div class="preview-toolbar-actions">
          <button class="btn btn-sm" onClick={refreshIframe}>Refresh</button>
          <a class="btn btn-sm" href={active.preview_url} target="_blank" rel="noopener">Open in tab</a>
        </div>
      </div>
      <iframe
        ref={iframeRef}
        class="preview-iframe"
        src={active.preview_url}
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups"
      />
    </div>
  );
}
