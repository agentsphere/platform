import { marked } from 'marked';

export function Markdown({ content }: { content: string }) {
  const html = marked.parse(content, { async: false }) as string;
  return <div class="md-body" dangerouslySetInnerHTML={{ __html: html }} />;
}
