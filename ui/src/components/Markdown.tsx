import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { MermaidBlock } from './MermaidBlock';

interface Segment {
  type: 'html' | 'mermaid';
  content: string;
}

/** Split markdown into text segments and mermaid code blocks. */
function splitMermaid(md: string): Segment[] {
  const segments: Segment[] = [];
  // Match ```mermaid ... ``` blocks (with optional <!-- mermaid:... --> prefix)
  const pattern = /(?:<!--\s*mermaid:[^\n]*-->\s*\n)?```mermaid\n([\s\S]*?)```(?:\s*\n<!--\s*\/mermaid\s*-->)?/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = pattern.exec(md)) !== null) {
    // Text before this mermaid block
    if (match.index > lastIndex) {
      const text = md.slice(lastIndex, match.index);
      if (text.trim()) {
        const raw = marked.parse(text, { async: false }) as string;
        segments.push({ type: 'html', content: DOMPurify.sanitize(raw) });
      }
    }
    segments.push({ type: 'mermaid', content: match[1].trim() });
    lastIndex = match.index + match[0].length;
  }

  // Remaining text
  if (lastIndex < md.length) {
    const text = md.slice(lastIndex);
    if (text.trim()) {
      const raw = marked.parse(text, { async: false }) as string;
      segments.push({ type: 'html', content: DOMPurify.sanitize(raw) });
    }
  }

  return segments;
}

export function Markdown({ content }: { content: string }) {
  const segments = splitMermaid(content);

  // Fast path: no mermaid blocks
  if (segments.length === 1 && segments[0].type === 'html') {
    return <div class="md-body" dangerouslySetInnerHTML={{ __html: segments[0].content }} />;
  }
  if (segments.length === 0) {
    const raw = marked.parse(content, { async: false }) as string;
    const html = DOMPurify.sanitize(raw);
    return <div class="md-body" dangerouslySetInnerHTML={{ __html: html }} />;
  }

  return (
    <div class="md-body">
      {segments.map((seg, i) =>
        seg.type === 'mermaid' ? (
          <div key={i} class="mermaid-block">
            <MermaidBlock code={seg.content} />
          </div>
        ) : (
          <div key={i} dangerouslySetInnerHTML={{ __html: seg.content }} />
        )
      )}
    </div>
  );
}
