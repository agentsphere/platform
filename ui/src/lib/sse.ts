export interface SseOptions {
  url: string;
  event?: string;       // SSE event name to listen for (default: "progress")
  onMessage: (data: any) => void;
  onOpen?: () => void;
  onError?: (err: Event) => void;
}

export class EventSourceClient {
  private source: EventSource | null = null;
  private closed = false;
  constructor(private opts: SseOptions) {}

  connect(): void {
    if (this.closed) return;
    this.source = new EventSource(this.opts.url);  // sends cookies automatically
    this.source.onopen = () => this.opts.onOpen?.();
    this.source.addEventListener(this.opts.event || 'progress', (e: MessageEvent) => {
      try { this.opts.onMessage(JSON.parse(e.data)); }
      catch { this.opts.onMessage(e.data); }
    });
    this.source.onerror = (err) => this.opts.onError?.(err);
    // EventSource has built-in auto-reconnect — no manual retry logic
  }

  close(): void {
    this.closed = true;
    this.source?.close();
    this.source = null;
  }
}

export function createSse(opts: SseOptions): EventSourceClient {
  const sse = new EventSourceClient(opts);
  sse.connect();
  return sse;
}
