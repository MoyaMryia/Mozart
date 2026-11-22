import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { statusSchema, subtitleSchema } from '../src/contracts';
import { SubtitleHistory, subscribeStream } from '../src/streams';
import { subtitleFixture } from './fixtures';

class Source extends EventTarget {
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  close = vi.fn();
  constructor(public url: string) { super(); }
  emit(name: string, value: unknown, id = '') {
    const event = new MessageEvent(name, { data: typeof value === 'string' ? value : JSON.stringify(value), lastEventId: id });
    if (name === 'error') this.onerror?.(event); else this.dispatchEvent(event);
  }
}

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe('字幕读模型', () => {
  it('按身份更新修订，重复和旧修订不能覆盖译文', () => {
    const history = new SubtitleHistory(); const first = subtitleFixture();
    history.accept(first);
    history.accept(subtitleFixture({ revision: 1, en: 'Hello', translation_status: 'completed' }));
    const records = history.accept(first);
    expect(records).toHaveLength(1); expect(records[0].en).toBe('Hello');
  });
  it('ASR 重启后相同 seq 是不同字幕', () => {
    const history = new SubtitleHistory(); history.accept(subtitleFixture());
    expect(history.accept(subtitleFixture({ utterance_id: 'boot-b:1' }))).toHaveLength(2);
  });
  it('历史窗口有界，窗口外的旧修订或重传不能成为当前字幕', () => {
    const history = new SubtitleHistory();
    for (let i = 1; i <= 51; i++) history.accept(subtitleFixture({ utterance_id: `boot-a:${i}`, seq: i }));
    history.accept(subtitleFixture({ revision: 100, en: 'Late old translation' }));
    const records = history.accept(subtitleFixture());
    expect(records).toHaveLength(50); expect(records[0].seq).toBe(2); expect(records.at(-1)?.seq).toBe(51);
  });
  it('较早的未出现字幕插入原顺序，不覆盖当前句', () => {
    const history = new SubtitleHistory(); history.accept(subtitleFixture());
    history.accept(subtitleFixture({ utterance_id: 'boot-a:3', seq: 3 }));
    const records = history.accept(subtitleFixture({ utterance_id: 'boot-a:2', seq: 2 }));
    expect(records.map(record => record.seq)).toEqual([1, 2, 3]);
  });
  it('旧 ASR 启动的迟到事件不挤到新启动的当前句之后', () => {
    const history = new SubtitleHistory(); history.accept(subtitleFixture());
    history.accept(subtitleFixture({ utterance_id: 'boot-b:1' }));
    const records = history.accept(subtitleFixture({ utterance_id: 'boot-a:2', seq: 2 }));
    expect(records.at(-1)?.utterance_id).toBe('boot-b:1');
    history.clear(); expect(history.accept(subtitleFixture())).toHaveLength(1);
  });
});

describe('命名 SSE', () => {
  it('接收 subtitle 并携带与当前读模型对应的游标重连', () => {
    const sources: Source[] = []; const accept = vi.fn(); const error = vi.fn();
    const stop = subscribeStream({ path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema, accept, state: vi.fn(), error }, url => {
      const source = new Source(url); sources.push(source); return source as unknown as EventSource;
    });
    sources[0].emit('subtitle', subtitleFixture(), 'g1:100');
    expect(accept).toHaveBeenCalledOnce();
    sources[0].onerror?.(new Event('error')); vi.advanceTimersByTime(1000);
    expect(sources[1].url).toBe('/api/v1/events/subtitles?cursor=g1%3A100');
    sources[1].onopen?.(new Event('open')); expect(error).toHaveBeenLastCalledWith('');
    stop();
  });
  it('reset 清空模型和游标，刷新页面也不恢复孤立游标', () => {
    const sources: Source[] = []; const reset = vi.fn();
    const options = { path: '/events/subtitles' as const, event: 'subtitle' as const, schema: subtitleSchema, accept: vi.fn(), state: vi.fn(), error: vi.fn(), reset };
    const factory = (url: string) => { const source = new Source(url); sources.push(source); return source as unknown as EventSource; };
    const stop = subscribeStream(options, factory);
    sources[0].emit('subtitle', subtitleFixture(), 'g1:100');
    sources[0].emit('reset', { generation: 'g2' }); expect(reset).toHaveBeenCalledOnce();
    sources[0].onerror?.(new Event('error')); vi.advanceTimersByTime(1000);
    expect(sources[1].url).toBe('/api/v1/events/subtitles'); stop();
    const stopFresh = subscribeStream(options, factory);
    expect(sources[2].url).toBe('/api/v1/events/subtitles'); stopFresh();
  });
  it('坏 JSON 或无效游标必须报告，不能悄悄跳过', () => {
    const sources: Source[] = []; const accept = vi.fn(); const error = vi.fn(); const state = vi.fn();
    const stop = subscribeStream({ path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema, accept, state, error }, url => {
      const source = new Source(url); sources.push(source); return source as unknown as EventSource;
    });
    sources[0].emit('subtitle', '{broken', 'g:10');
    expect(error).toHaveBeenLastCalledWith('推送正文不是有效 JSON'); expect(state).toHaveBeenLastCalledWith('offline');
    vi.advanceTimersByTime(1000); sources[1].emit('subtitle', subtitleFixture(), 'invalid');
    expect(accept).not.toHaveBeenCalled(); expect(error.mock.calls.at(-1)?.[0]).toContain('响应结构'); stop();
  });
  it('业务错误在重连 open 时保留，直到收到有效数据', () => {
    const sources: Source[] = []; const error = vi.fn(); const state = vi.fn();
    const stop = subscribeStream({ path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema, accept: vi.fn(), state, error }, url => {
      const source = new Source(url); sources.push(source); return source as unknown as EventSource;
    });
    sources[0].emit('error', { status: 'error', code: 'SourceUnavailable', reason: '字幕坏行', data: null });
    vi.advanceTimersByTime(1000); sources[1].onopen?.(new Event('open'));
    expect(error).toHaveBeenLastCalledWith('SourceUnavailable：字幕坏行');
    expect(state).toHaveBeenLastCalledWith('connecting');
    sources[1].emit('subtitle', subtitleFixture(), 'g:100');
    expect(error).toHaveBeenLastCalledWith(''); expect(state).toHaveBeenLastCalledWith('online'); stop();
  });
  it('服务端 error 原因可见，旧连接排队事件不能更新新连接', () => {
    const sources: Source[] = []; const accept = vi.fn(); const error = vi.fn();
    const stop = subscribeStream({ path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema, accept, state: vi.fn(), error }, url => {
      const source = new Source(url); sources.push(source); return source as unknown as EventSource;
    });
    sources[0].emit('error', { status: 'error', code: 'SourceUnavailable', reason: '坏行', data: null });
    expect(error).toHaveBeenLastCalledWith('SourceUnavailable：坏行');
    vi.advanceTimersByTime(1000); sources[0].emit('subtitle', subtitleFixture(), 'g:99');
    expect(accept).not.toHaveBeenCalled(); stop(); vi.advanceTimersByTime(10000); expect(sources).toHaveLength(2);
  });
  it('状态流解析真实状态结构，而不是把 open 当业务成功', () => {
    const state = vi.fn(); const accept = vi.fn(); const error = vi.fn(); let source: Source;
    const stop = subscribeStream({ path: '/events/status', event: 'status', schema: statusSchema, accept, state, error }, url => {
      source = new Source(url); return source as unknown as EventSource;
    });
    source!.emit('status', { fabricated: true });
    expect(accept).not.toHaveBeenCalled(); expect(state).toHaveBeenLastCalledWith('offline'); expect(error).toHaveBeenCalled(); stop();
  });
});
