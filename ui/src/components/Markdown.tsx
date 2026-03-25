import { marked } from 'marked';
import DOMPurify from 'dompurify';

export function Markdown({ content }: { content: string }) {
  const raw = marked.parse(content, { async: false }) as string;
  const html = DOMPurify.sanitize(raw);
  return <div class="md-body" dangerouslySetInnerHTML={{ __html: html }} />;
}
