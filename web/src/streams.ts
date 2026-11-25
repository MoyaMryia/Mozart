import { z } from 'zod';
import { API_ROOT, ApiError, parsePayload } from './api';
import { envelopeSchema, subtitleSchema, type ConnectionState, type SubtitleRecord } from './contracts';

const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10000;
export const SUBTITLE_HISTORY_LIMIT = 50;
const resetSchema = z.object({ generation: z.string().min(1) });
const cursorSchema = z.string().regex(/^[^:\s]+:\d+$/);

// 每个 ASR 启动只保留顺序与淘汰水位，不为所有历史句子保留正文或修订表。
export class SubtitleHistory {
  private sessions = new Map<string, { order: number; evictedThrough: number }>();
  private records: SubtitleRecord[] = [];
  private session(record: SubtitleRecord) { return record.utterance_id.slice(0, record.utterance_id.lastIndexOf(':')); }

  accept(record: SubtitleRecord): SubtitleRecord[] {
    const sessionId = this.session(record);
    let session = this.sessions.get(sessionId);
    if (!session) {
      session = { order: this.sessions.size, evictedThrough: -1 };
      this.sessions.set(sessionId, session);
    }
    const index = this.records.findIndex(item => item.utterance_id === record.utterance_id);
    if (index >= 0) {
      if (this.records[index].revision < record.revision) {
        this.records = this.records.map((item, at) => at === index ? record : item);
      }
    } else if (record.seq > session.evictedThrough) {
      this.records = [...this.records, record].sort((left, right) => {
        const order = this.sessions.get(this.session(left))!.order - this.sessions.get(this.session(right))!.order;
        return order || left.seq - right.seq;
      });
      if (this.records.length > SUBTITLE_HISTORY_LIMIT) {
        const evicted = this.records.shift()!;
        const owner = this.sessions.get(this.session(evicted))!;
        owner.evictedThrough = Math.max(owner.evictedThrough, evicted.seq);
      }
    }
    return this.records;
  }

  clear() { this.records = []; this.sessions.clear(); }
}

const SNAPSHOT_KEY = 'mozart-subtitles';
const snapshotSchema = z.object({ cursor: cursorSchema, records: z.array(subtitleSchema) });
const storageDenied = (error: unknown) => error instanceof DOMException && (error.name === 'SecurityError' || error.name === 'QuotaExceededError');

/**
 * 读模型与游标作为同一快照存入 sessionStorage：页面重建时接续，新打开的标签页从头回放。
 * 存储不可用时放弃持久化，由调用方按返回值提示。
 */
export class SubtitleStore {
  private history = new SubtitleHistory();
  private cursor?: string;

  restore(): SubtitleRecord[] | null {
    let saved: unknown;
    try { saved = JSON.parse(sessionStorage.getItem(SNAPSHOT_KEY) ?? 'null'); }
    catch (error) { if (error instanceof SyntaxError || storageDenied(error)) return null; throw error; }
    const parsed = snapshotSchema.safeParse(saved);
    if (!parsed.success) return null;
    this.cursor = parsed.data.cursor;
    for (const record of parsed.data.records) this.history.accept(record);
    return parsed.data.records;
  }
  get current() { return this.cursor; }
  accept(record: SubtitleRecord) { return this.history.accept(record); }
  // 没有游标就无法接续，不保存。
  save(cursor: string | undefined, records: SubtitleRecord[]): boolean {
    if (cursor) this.cursor = cursor;
    if (!this.cursor) return true;
    try { sessionStorage.setItem(SNAPSHOT_KEY, JSON.stringify({ cursor: this.cursor, records })); return true; }
    catch (error) { if (!storageDenied(error)) throw error; return false; }
  }
  clear() {
    this.cursor = undefined; this.history.clear();
    try { sessionStorage.removeItem(SNAPSHOT_KEY); }
    catch (error) { if (!storageDenied(error)) throw error; }
  }
}

export interface StreamOptions<T> {
  path: '/events/status' | '/events/subtitles';
  event: 'status' | 'subtitle';
  schema: z.ZodType<T>;
  /** 已保存的游标：有则从断点接续，没有则由服务端从头回放。 */
  cursor?: string;
  accept(value: T, cursor?: string): void;
  state(value: ConnectionState): void;
  error(reason: string): void;
  reset?(): void;
}

// 游标与读模型由 SubtitleStore 作为同一快照保存；没有可恢复的快照时，服务端从头回放。
export function subscribeStream<T>(options: StreamOptions<T>, factory = (url: string) => new EventSource(url)) {
  let source: EventSource | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let cursor = options.cursor;
  let dataError: string | undefined;
  let reconnectDelay = RECONNECT_MIN_MS;

  const disconnect = () => {
    if (source) {
      source.close();
      source = null;
    }
  };
  const fail = (message: string, fromData = false) => {
    if (fromData) dataError = message;
    disconnect();
    options.state('offline');
    options.error(dataError ?? message);
    const delay = reconnectDelay;
    reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
    timer = setTimeout(connect, delay);
  };
  const decode = <V>(event: MessageEvent, schema: z.ZodType<V>): V => {
    let value: unknown;
    try { value = JSON.parse(event.data); }
    catch (error) {
      if (!(error instanceof SyntaxError)) throw error;
      throw new ApiError('推送正文不是有效 JSON', 'InvalidResponse');
    }
    return parsePayload(schema, value);
  };
  const connect = () => {
    options.state('connecting');
    const suffix = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
    const next = factory(`${API_ROOT}${options.path}${suffix}`);
    source = next;
    next.onopen = () => {
      if (source === next) {
        if (!dataError) {
          reconnectDelay = RECONNECT_MIN_MS;
          options.error('');
        }
        options.state(dataError ? 'connecting' : 'online');
      }
    };
    next.addEventListener(options.event, event => {
      if (source !== next) return;
      let value: T;
      let nextCursor: string | undefined;
      try {
        value = decode(event as MessageEvent, options.schema);
        if (options.event === 'subtitle') nextCursor = parsePayload(cursorSchema, (event as MessageEvent).lastEventId);
      } catch (error) {
        if (!(error instanceof ApiError)) throw error;
        fail(error.message, true);
        return;
      }
      options.accept(value, nextCursor);
      dataError = undefined;
      reconnectDelay = RECONNECT_MIN_MS;
      cursor = nextCursor;
      options.error('');
      options.state('online');
    });
    next.addEventListener('reset', event => {
      if (source !== next) return;
      try { decode(event as MessageEvent, resetSchema); }
      catch (error) {
        if (!(error instanceof ApiError)) throw error;
        fail(error.message, true);
        return;
      }
      cursor = undefined;
      options.reset?.();
    });
    next.onerror = event => {
      if (source !== next) return;
      if (event instanceof MessageEvent) {
        let message: string;
        try {
          const envelope = decode(event, envelopeSchema);
          if (envelope.status !== 'error') throw new ApiError('error 事件没有携带错误信封', 'InvalidResponse');
          message = `${envelope.code}：${envelope.reason}`;
        } catch (error) {
          if (!(error instanceof ApiError)) throw error;
          message = error.message;
        }
        fail(message, true);
      } else {
        fail('推送连接已断开，正在重连');
      }
    };
  };
  connect();
  return () => {
    clearTimeout(timer);
    disconnect();
  };
}
