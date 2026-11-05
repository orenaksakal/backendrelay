export interface EventSourceType {
  readonly url: string;
  readonly readyState: number;

  onopen: ((this: EventSourceType, ev: Event) => any) | null;
  onmessage: ((this: EventSourceType, ev: MessageEvent) => any) | null;
  onerror: ((this: EventSourceType, ev: Event) => any) | null;

  close(): void;
}

export interface EventSourceConstructor {
  OPEN: number;
  CONNECTING: number;
  CLOSED: number;
  new (url: string): EventSourceType;
}

let _connId = 0;

type Conn = EventSourceType | WebSocket;

type OpenEvent<T extends Conn> = {
  target: Connection<T>;
};

type MessageData = {
  op: string;
  [key: string]: any;
};

type SendMessageData = {
  'client-event-id': string;
  [key: string]: any;
};

type MsgEvent<T extends Conn> = {
  target: Connection<T>;
  message: MessageData | MessageData[];
};

type CloseEvent<T extends Conn> = {
  target: Connection<T>;
  /**
   * WebSocket close code, when the transport reports one. Used to recognise
   * an operator restart (1012) so reconnection can start immediately.
   */
  code?: number;
};

interface ErrorEvent<T extends Conn> {
  target: Connection<T>;
}

export type TransportType = 'ws' | 'sse';

export interface Connection<T extends Conn> {
  conn: T;
  type: 'ws' | 'sse';
  id: string;
  close(): void;
  isOpen(): boolean;
  isConnecting(): boolean;
  send(msg: SendMessageData): void;
  onopen: ((event: OpenEvent<T>) => void) | null;
  onmessage: ((event: MsgEvent<T>) => void) | null;
  onclose: ((event: CloseEvent<T>) => void) | null;
  onerror: ((event: ErrorEvent<T>) => void) | null;
}

export class WSConnection implements Connection<WebSocket> {
  type: TransportType = 'ws';
  conn: WebSocket;
  id: string;
  onopen: (event: OpenEvent<WebSocket>) => void;
  onmessage: (event: MsgEvent<WebSocket>) => void;
  onclose: (event: CloseEvent<WebSocket>) => void;
  onerror: (event: ErrorEvent<WebSocket>) => void;
  constructor(url: string) {
    this.id = `${this.type}_${_connId++}`;
    this.conn = new WebSocket(url);
    this.conn.onopen = (_e) => {
      if (this.onopen) {
        this.onopen({ target: this });
      }
    };
    this.conn.onmessage = (e) => {
      if (this.onmessage) {
        this.onmessage({
          target: this,
          message: JSON.parse(e.data.toString()),
        });
      }
    };
    this.conn.onclose = (e) => {
      if (this.onclose) {
        this.onclose({ target: this, code: e.code });
      }
    };
    this.conn.onerror = (_e) => {
      if (this.onerror) {
        this.onerror({ target: this });
      }
    };
  }

  close() {
    this.conn.close();
  }

  isOpen(): boolean {
    return this.conn.readyState === (WebSocket.OPEN ?? 1);
  }

  isConnecting(): boolean {
    return this.conn.readyState === (WebSocket.CONNECTING ?? 0);
  }

  send(msg: SendMessageData) {
    return this.conn.send(JSON.stringify(msg));
  }
}

type SSEInitParams = {
  machineId: string;
  sessionId: string;
  sseToken: string;
};

const SSE_PUSH_FORMAT_HEADER = 'X-Instant-SSE-Push-Format';
const SSE_PUSH_NDJSON = 'ndjson';
const SSE_PUSH_BATCH_SIZE = 100;
const SSE_PUSH_TIMEOUT_MS = 10_000;

export class SSEConnection implements Connection<EventSourceType> {
  type: TransportType = 'sse';
  private initParams: SSEInitParams | null = null;
  private sendQueue: SendMessageData[] = [];
  private sendPromise: Promise<void> | null;
  private flushScheduled = false;
  private supportsNdjson = false;
  private closeFired: boolean = false;
  private sseInitTimeout: ReturnType<typeof setTimeout> | undefined = undefined;
  private pushTimeout: ReturnType<typeof setTimeout> | undefined;
  private pushController: AbortController | null = null;
  private ES: EventSourceConstructor;
  private messageUrl: string;
  conn: EventSourceType;
  url: string;
  id: string;
  onopen: (event: OpenEvent<EventSourceType>) => void;
  onmessage: (event: MsgEvent<EventSourceType>) => void;
  onclose: (event: CloseEvent<EventSourceType>) => void;
  onerror: (event: ErrorEvent<EventSourceType>) => void;

  constructor(ES: EventSourceConstructor, url: string, messageUrl?: string) {
    this.id = `${this.type}_${_connId++}`;
    this.url = url;
    this.messageUrl = messageUrl || this.url;
    this.ES = ES;
    this.conn = new ES(url);

    // Close the connection if we didn't get an init within 10 seconds
    this.sseInitTimeout = setTimeout(() => {
      if (!this.initParams) {
        this.handleError();
      }
    }, 10000);

    this.conn.onmessage = (e) => {
      const message = JSON.parse(e.data);
      if (Array.isArray(message)) {
        for (const msg of message) {
          this.handleMessage(msg);
        }
      } else {
        this.handleMessage(message);
      }
    };

    this.conn.onerror = (e) => {
      this.handleError();
    };
  }

  private handleMessage(msg: MessageData) {
    if (this.closeFired) return;
    if (msg.op === 'sse-init') {
      this.initParams = {
        machineId: msg['machine-id'],
        sessionId: msg['session-id'],
        sseToken: msg['sse-token'],
      };
      if (this.onopen) {
        this.onopen({ target: this });
      }
      clearTimeout(this.sseInitTimeout);
      return;
    }
    if (this.onmessage) {
      this.onmessage({
        target: this,
        message: msg,
      });
    }
  }

  // Runs the onerror and closes the connection
  private handleError() {
    if (this.closeFired) return;
    try {
      if (this.onerror) {
        this.onerror({ target: this });
      }
    } finally {
      this.handleClose();
    }
  }

  private handleClose() {
    if (this.closeFired) return;
    this.closeFired = true;
    clearTimeout(this.sseInitTimeout);
    clearTimeout(this.pushTimeout);
    this.sendQueue = [];
    this.pushController?.abort();
    this.pushController = null;
    this.conn.close();
    if (this.onclose) {
      this.onclose({ target: this });
    }
  }

  private post(
    messages: SendMessageData[],
    ndjson: boolean,
    signal: AbortSignal,
  ) {
    const initParams = this.initParams;
    const body = ndjson
      ? [
          JSON.stringify({
            machine_id: initParams?.machineId,
            session_id: initParams?.sessionId,
            sse_token: initParams?.sseToken,
          }),
          ...messages.map((message) => JSON.stringify(message)),
          '',
        ].join('\n')
      : JSON.stringify({
          machine_id: initParams?.machineId,
          session_id: initParams?.sessionId,
          sse_token: initParams?.sseToken,
          messages,
        });
    return fetch(this.messageUrl, {
      method: 'POST',
      headers: {
        Accept: 'application/x-ndjson, application/json',
        'Content-Type': ndjson ? 'application/x-ndjson' : 'application/json',
      },
      body,
      signal,
    });
  }

  private async postMessages(messages: SendMessageData[]): Promise<void> {
    const controller = new AbortController();
    this.pushController = controller;
    // Delivery may already have happened: close on timeout, never replay.
    this.pushTimeout = setTimeout(
      () => this.handleError(),
      SSE_PUSH_TIMEOUT_MS,
    );
    try {
      let resp = await this.post(
        messages,
        this.supportsNdjson,
        controller.signal,
      );
      if (this.closeFired) return;
      // An older endpoint cannot parse NDJSON. Retrying only an explicit
      // format rejection avoids replaying a batch after an ambiguous failure.
      if (this.supportsNdjson && (resp.status === 406 || resp.status === 415)) {
        this.supportsNdjson = false;
        resp = await this.post(messages, false, controller.signal);
        if (this.closeFired) return;
      }
      if (!resp.ok) {
        this.handleError();
        return;
      }
      this.supportsNdjson =
        resp.headers.get(SSE_PUSH_FORMAT_HEADER) === SSE_PUSH_NDJSON;
    } catch (e) {
      this.handleError();
    } finally {
      clearTimeout(this.pushTimeout);
      this.pushController = null;
    }
  }

  private async flushQueue() {
    if (!this.isOpen() || this.sendPromise || !this.sendQueue.length) return;

    const messages = this.sendQueue.splice(0, SSE_PUSH_BATCH_SIZE);
    const sendPromise = this.postMessages(messages);
    this.sendPromise = sendPromise;
    sendPromise.then(() => {
      this.sendPromise = null;
      if (this.isOpen()) {
        this.flushQueue();
      }
    });
  }

  private scheduleFlush() {
    if (this.flushScheduled) return;
    this.flushScheduled = true;
    Promise.resolve().then(() => {
      this.flushScheduled = false;
      this.flushQueue();
    });
  }

  send(msg: SendMessageData) {
    if (!this.isOpen() || !this.initParams) {
      if (this.isConnecting()) {
        throw new Error(
          `Failed to execute 'send' on 'EventSource': Still in CONNECTING state.`,
        );
      }
      if (this.conn.readyState === this.ES.CLOSED) {
        throw new Error(`EventSource is already in CLOSING or CLOSED state.`);
      }
      throw new Error(`EventSource is in invalid state.`);
    }
    this.sendQueue.push(msg);
    this.scheduleFlush();
  }

  isOpen(): boolean {
    return (
      !this.closeFired &&
      this.conn.readyState === this.ES.OPEN &&
      this.initParams !== null
    );
  }

  isConnecting(): boolean {
    return (
      !this.closeFired &&
      (this.conn.readyState === this.ES.CONNECTING ||
        (this.conn.readyState === this.ES.OPEN && this.initParams === null))
    );
  }

  close() {
    this.handleClose();
  }
}
