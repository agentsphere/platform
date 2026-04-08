import { useState, useEffect, useRef } from 'preact/hooks';

/**
 * C4 → flowchart transpiler (from docs/viewer).
 * Mermaid's C4 layout causes edge crossings; dagre flowcharts route properly.
 */
function isC4(def: string): boolean {
  return /^C4(Context|Container|Component|Deployment)\b/.test(def.trim().split('\n')[0].trim());
}

function transpileC4(definition: string): string {
  if (!isC4(definition)) return definition;
  const lines = definition.split('\n');
  const firstLine = lines[0].trim();
  const nodes: any[] = [];
  const rels: any[] = [];
  const boundaries: any[] = [];
  const boundaryLabels = new Map<string, string>();
  let title = '';

  const currentBoundary = () => boundaries.length > 0 ? boundaries[boundaries.length - 1].id : null;

  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith('%%')) continue;
    const titleMatch = line.match(/^\s*title\s+(.+)$/);
    if (titleMatch) { title = titleMatch[1]; continue; }
    if (/^C4(Context|Container|Component|Deployment)\b/.test(line)) continue;
    if (line.startsWith('UpdateLayoutConfig') || line.startsWith('UpdateRelStyle')) continue;
    const bm = line.match(/^(Deployment_Node|Enterprise_Boundary|System_Boundary|Container_Boundary|Boundary)\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)\s*\{?\s*$/);
    if (bm) { boundaries.push({ id: bm[2], label: bm[3] }); boundaryLabels.set(bm[2], bm[3]); continue; }
    if (line === '}') { boundaries.pop(); continue; }
    const pm = line.match(/^Person\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)/);
    if (pm) { nodes.push({ id: pm[1], label: pm[2], desc: pm[3] || '', type: 'person', boundary: currentBoundary() }); continue; }
    const sem = line.match(/^System_Ext\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)/);
    if (sem) { nodes.push({ id: sem[1], label: sem[2], desc: sem[3] || '', type: 'external', boundary: currentBoundary() }); continue; }
    const sdm = line.match(/^SystemDb\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)/);
    if (sdm) { nodes.push({ id: sdm[1], label: sdm[2], desc: sdm[3] || '', type: 'database', boundary: currentBoundary() }); continue; }
    const sm = line.match(/^System\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)/);
    if (sm) { nodes.push({ id: sm[1], label: sm[2], desc: sm[3] || '', type: 'system', boundary: currentBoundary() }); continue; }
    const cdbm = line.match(/^ContainerDb\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?(?:,\s*"([^"]*)")?\)/);
    if (cdbm) { nodes.push({ id: cdbm[1], label: cdbm[2], tech: cdbm[3] || '', desc: cdbm[4] || cdbm[3] || '', type: 'database', boundary: currentBoundary() }); continue; }
    const cm = line.match(/^Container\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?(?:,\s*"([^"]*)")?\)/);
    if (cm) { nodes.push({ id: cm[1], label: cm[2], tech: cm[3] || '', desc: cm[4] || '', type: 'container', boundary: currentBoundary() }); continue; }
    const comp = line.match(/^Component\((\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?(?:,\s*"([^"]*)")?\)/);
    if (comp) { nodes.push({ id: comp[1], label: comp[2], tech: comp[3] || '', desc: comp[4] || '', type: 'component', boundary: currentBoundary() }); continue; }
    const rm = line.match(/^Rel\((\w+),\s*(\w+),\s*"([^"]*)"(?:,\s*"([^"]*)")?\)/);
    if (rm) { rels.push({ from: rm[1], to: rm[2], label: rm[3], protocol: rm[4] || '' }); continue; }
  }

  const buildLabel = (n: any) => {
    let l = n.label;
    if (n.tech) l += `<br/><i>${n.tech}</i>`;
    if (n.desc && n.desc !== n.tech) l += `<br/><small>${n.desc}</small>`;
    return l;
  };
  const renderNode = (n: any) => {
    const l = buildLabel(n);
    switch (n.type) {
      case 'person':   return `${n.id}(["${l}"])`;
      case 'database': return `${n.id}[("${l}")]`;
      case 'external': return `${n.id}[/"${l}"\\]`;
      default:         return `${n.id}["${l}"]`;
    }
  };

  let dsl = `flowchart TD\n`;
  const byBoundary = new Map<string, any[]>();
  for (const n of nodes) {
    const key = n.boundary || '__root__';
    if (!byBoundary.has(key)) byBoundary.set(key, []);
    byBoundary.get(key)!.push(n);
  }
  for (const [bId, members] of byBoundary) {
    if (bId === '__root__') {
      for (const n of members) dsl += `  ${renderNode(n)}\n`;
    } else {
      dsl += `  subgraph ${bId}["${boundaryLabels.get(bId) || bId}"]\n`;
      for (const n of members) dsl += `    ${renderNode(n)}\n`;
      dsl += '  end\n';
    }
  }
  for (const r of rels) {
    const l = r.protocol ? `${r.label}\\n${r.protocol}` : r.label;
    dsl += `  ${r.from} -->|"${l}"| ${r.to}\n`;
  }
  dsl += '\n';
  for (const n of nodes) dsl += `  class ${n.id} ${n.type}\n`;
  dsl += `
  classDef person fill:#08427b,stroke:#073b6f,color:#fff,stroke-width:2px
  classDef system fill:#1168bd,stroke:#0b4884,color:#fff,stroke-width:2px
  classDef external fill:#999,stroke:#6b6b6b,color:#fff
  classDef database fill:#438dd5,stroke:#2e6295,color:#fff
  classDef container fill:#438dd5,stroke:#2e6295,color:#fff
  classDef component fill:#85bbf0,stroke:#5a9bd5,color:#000`;
  return dsl;
}

// Serialized mermaid render queue — only one render at a time
let mermaidMod: any = null;
let initDone = false;
let counter = 0;
let queue = Promise.resolve();

async function getMermaid() {
  if (!mermaidMod) {
    mermaidMod = (await import('mermaid')).default;
  }
  if (!initDone) {
    const isDark = document.documentElement.getAttribute('data-theme') !== 'light';
    mermaidMod.initialize({
      startOnLoad: false,
      suppressErrorRendering: true,
      theme: isDark ? 'dark' : 'default',
      themeVariables: isDark ? {
        darkMode: true, background: '#1a1a2e', primaryColor: '#1168bd',
        primaryTextColor: '#e0e0e0', primaryBorderColor: '#0b4884',
        lineColor: '#8892b0', secondaryColor: '#438dd5', tertiaryColor: '#2d2d4e',
        fontFamily: '"Inter", system-ui, sans-serif', fontSize: '14px',
      } : {
        darkMode: false, background: '#ffffff', primaryColor: '#1168bd',
        primaryTextColor: '#1a1a2e', lineColor: '#4a5568',
        fontFamily: '"Inter", system-ui, sans-serif', fontSize: '14px',
      },
      flowchart: { htmlLabels: true, curve: 'basis', padding: 16 },
      sequence: { mirrorActors: false, messageMargin: 40, useMaxWidth: false },
      state: { useMaxWidth: false },
    });
    initDone = true;
  }
  return mermaidMod;
}

function renderMermaid(definition: string): Promise<{ svg: string }> {
  const id = `mmd-${++counter}`;
  const finalDef = transpileC4(definition);
  const result = queue.then(async () => {
    const mm = await getMermaid();
    return mm.render(id, finalDef);
  });
  queue = result.then(() => {}, () => {});
  return result;
}

interface MermaidBlockProps {
  code: string;
  onClick?: () => void;
}

export function MermaidBlock({ code, onClick }: MermaidBlockProps) {
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setSvg(null);
    setError(null);
    renderMermaid(code)
      .then(result => setSvg(result.svg))
      .catch(err => setError(String(err)));
  }, [code]);

  if (error) {
    return (
      <div class="mermaid-error">
        <pre class="text-xs" style="color:var(--danger);white-space:pre-wrap">{error}</pre>
        <details class="mt-sm">
          <summary class="text-xs text-muted" style="cursor:pointer">Source</summary>
          <pre class="text-xs" style="max-height:200px;overflow:auto">{code}</pre>
        </details>
      </div>
    );
  }

  if (!svg) {
    return <div class="mermaid-loading text-muted text-sm" style="padding:2rem;text-align:center">Rendering diagram...</div>;
  }

  return (
    <div class={`mermaid-rendered${onClick ? ' mermaid-clickable' : ''}`}
      onClick={onClick}
      dangerouslySetInnerHTML={{ __html: svg }} />
  );
}

/** Zoomable + pannable diagram in an overlay. Wheel to zoom, drag to pan. */
export function ZoomableDiagram({ svg }: { svg: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [transform, setTransform] = useState({ x: 0, y: 0, scale: 1 });
  const dragging = useRef(false);
  const dragStart = useRef({ x: 0, y: 0 });

  const handleWheel = (e: WheelEvent) => {
    e.preventDefault();
    const delta = e.deltaY > 0 ? 0.9 : 1.1;
    setTransform(t => {
      const newScale = Math.min(Math.max(t.scale * delta, 0.2), 5);
      // Zoom toward cursor
      const rect = containerRef.current?.getBoundingClientRect();
      if (!rect) return { ...t, scale: newScale };
      const cx = e.clientX - rect.left;
      const cy = e.clientY - rect.top;
      const dx = cx - t.x;
      const dy = cy - t.y;
      const ratio = newScale / t.scale;
      return { x: cx - dx * ratio, y: cy - dy * ratio, scale: newScale };
    });
  };

  const handleMouseDown = (e: MouseEvent) => {
    if (e.button !== 0) return;
    dragging.current = true;
    dragStart.current = { x: e.clientX - transform.x, y: e.clientY - transform.y };
  };
  const handleMouseMove = (e: MouseEvent) => {
    if (!dragging.current) return;
    setTransform(t => ({ ...t, x: e.clientX - dragStart.current.x, y: e.clientY - dragStart.current.y }));
  };
  const handleMouseUp = () => { dragging.current = false; };

  const resetZoom = () => setTransform({ x: 0, y: 0, scale: 1 });

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    el.addEventListener('wheel', handleWheel, { passive: false });
    return () => el.removeEventListener('wheel', handleWheel);
  }, []);

  return (
    <div>
      <div class="flex-between mb-sm">
        <span class="text-xs text-muted">Scroll to zoom, drag to pan</span>
        <button class="btn btn-sm" onClick={resetZoom}>Reset</button>
      </div>
      <div ref={containerRef} class="diagram-zoom-container"
        onMouseDown={handleMouseDown} onMouseMove={handleMouseMove}
        onMouseUp={handleMouseUp} onMouseLeave={handleMouseUp}>
        <div style={`transform: translate(${transform.x}px, ${transform.y}px) scale(${transform.scale}); transform-origin: 0 0; transition: ${dragging.current ? 'none' : 'transform 0.1s'}`}
          dangerouslySetInnerHTML={{ __html: svg }} />
      </div>
    </div>
  );
}

/** Render mermaid and return the SVG string (for use in overlays). */
export { renderMermaid };
