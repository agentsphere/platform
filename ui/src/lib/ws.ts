export interface WsOptions {
  url: string;
  onMessage: (data: any) => void;
  onOpen?: () => void;
  onClose?: () => void;
  onError?: (err: Event) => void;
  reconnect?: boolean;
  maxRetries?: number;
}

export class ReconnectingWebSocket {
  private ws: WebSocket | null = null;
  private retries = 0;
  private backoff = 1000;
  private closed = false;
  private opts: Required<Pick<WsOptions, 'reconnect' | 'maxRetries'>> & WsOptions;

  constructor(options: WsOptions) {
    this.opts = {
      reconnect: true,
      maxRetries: 5,
      ...options,
    };
  }

  connect(): void {
    if (this.closed) return;

    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = this.opts.url.startsWith('/')
      ? `${protocol}//${window.location.host}${this.opts.url}`
      : this.opts.url;

    this.ws = new WebSocket(url);

    this.ws.onopen = () => {
      this.retries = 0;
      this.backoff = 1000;
      this.opts.onOpen?.();
    };

    this.ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data);
        this.opts.onMessage(data);
      } catch {
        this.opts.onMessage(event.data);
      }
    };

    this.ws.onclose = () => {
      this.opts.onClose?.();
      if (this.opts.reconnect && !this.closed && this.retries < this.opts.maxRetries) {
        this.retries++;
        const delay = Math.min(this.backoff, 30000);
        this.backoff *= 2;
        setTimeout(() => this.connect(), delay);
      }
    };

    this.ws.onerror = (err) => {
      this.opts.onError?.(err);
    };
  }

  send(data: string): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data);
    }
  }

  close(): void {
    this.closed = true;
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }
}

export function createWs(options: WsOptions): ReconnectingWebSocket {
  const ws = new ReconnectingWebSocket(options);
  ws.connect();
  return ws;
}
