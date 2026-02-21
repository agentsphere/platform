import { useState } from 'preact/hooks';

interface Props {
  code: string;
  language?: string;
  showLineNumbers?: boolean;
}

export function CodeBlock({ code, showLineNumbers }: Props) {
  const [copied, setCopied] = useState(false);
  const lines = code.split('\n');

  const copy = () => {
    navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  };

  return (
    <div class="code-block">
      <div class="code-block-header">
        <button class="btn btn-ghost btn-sm" onClick={copy}>
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <pre class="code-block-content">
        {lines.map((line, i) => (
          <div key={i} class="code-line">
            {showLineNumbers !== false && (
              <span class="line-number">{i + 1}</span>
            )}
            <span class="line-content">{line}</span>
          </div>
        ))}
      </pre>
    </div>
  );
}
