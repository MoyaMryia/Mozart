import { createApp, defineComponent, h, nextTick, reactive, type App as VueApp } from 'vue';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.vue';
import { useConsole } from '../src/useConsole';
import { ApiError } from '../src/api';
import DevicesPanel from '../src/components/DevicesPanel.vue';
import SystemPanel from '../src/components/SystemPanel.vue';
import { statusSchema } from '../src/contracts';
import { statusFixture, healthFixture, jobFixture, subtitleFixture, jsonResponse, errorResponse, envelope } from './fixtures';
import type { FileJob, SubtitleRecord } from '../src/contracts';

const mounts: Array<{ app: VueApp; root: HTMLElement }> = [];
const sources: Source[] = [];
class Source extends EventTarget {
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  constructor(public url: string) { super(); sources.push(this); }
  close() {}
}
function mount(component = App, props = {}) {
  const root = document.createElement('div'); document.body.append(root);
  const app = createApp(component, props); app.mount(root); mounts.push({ app, root }); return root;
}
async function flush() { for (let i = 0; i < 60; i++) await Promise.resolve(); await nextTick(); }
function button(root: ParentNode, text: string) {
  const found = [...root.querySelectorAll('button')].find(item => item.textContent?.trim() === text);
  if (!found) throw new Error(`没有按钮：${text}`); return found;
}
function iconButton(root: ParentNode, label: string) {
  const found = root.querySelector<HTMLButtonElement>(`[aria-label="${label}"]`);
  if (!found) throw new Error(`没有图标按钮：${label}`); return found;
}
function section(root: HTMLElement, title: string) {
  const found = [...root.querySelectorAll('section')].find(item => item.querySelector('h2')?.textContent === title);
  if (!found) throw new Error(`没有卡片：${title}`); return found;
}

function mockBackend() {
  const snapshot = statusFixture();
  let running = true;
  let jobs: FileJob[] = [];
  let submitFailure = false;
  let presetDirty = false;
  const baseline = { capture: 'mic-a', playback: 'speaker-a' };
  const fetcher = vi.fn(async (path: string, init: RequestInit = {}) => {
    const method = init.method ?? 'GET'; const body = typeof init.body === 'string' ? JSON.parse(init.body) : undefined;
    if (path === '/api/v1/health') return jsonResponse(healthFixture);
    if (path === '/api/v1/status') return running ? jsonResponse(snapshot) : errorResponse('UpstreamUnavailable', 'daemon 未运行', 503);
    if (path === '/api/v1/system/up') { running = true; return jsonResponse({ state: 'running' }); }
    if (path === '/api/v1/system/down') { running = false; return jsonResponse({ state: 'stopped' }); }
    if (path === '/api/v1/route') { snapshot.route.desired = body.route; return jsonResponse({ route: body.route }); }
    if (path === '/api/v1/mode') {
      const phase = body.mode === 'broadcast' ? 'opening_speech' : 'routing';
      snapshot.output_mode = { requested: body.mode, applied: null, phase, last_error: null };
      snapshot.bridge!.speech_enabled = body.mode === 'broadcast';
      return jsonResponse(snapshot.output_mode);
    }
    if (path === '/api/v1/translate') {
      snapshot.bridge!.translate_enabled = body.enabled;
      return jsonResponse({ translate_enabled: body.enabled });
    }
    if (path === '/api/v1/speech/clear') {
      snapshot.bridge!.speech_queue_depth = 0;
      return jsonResponse({});
    }
    if (path === '/api/v1/mute') { snapshot.mute = body.mute; return jsonResponse({ mute: body.mute }); }
    if (path === '/api/v1/models') return jsonResponse({ models: [{ id: 'model-a', exists: true, loaded: true }] });
    if (path === '/api/v1/preset') return jsonResponse({ model_id: 'model-a', preset: 'default', preset_values: snapshot.rvc_params, live_values: snapshot.rvc_params, dirty: presetDirty });
    if (path === '/api/v1/model') return jsonResponse({ model_id: body.model_id, previous_model_id: 'model-a' });
    if (path === '/api/v1/preset/save' || path === '/api/v1/preset/use') { presetDirty = false; return jsonResponse({ model_id: 'model-a', preset: 'default', values: snapshot.rvc_params }); }
    if (path === '/api/v1/devices') return jsonResponse({ devices: [{ id: 'mic-a', label: '麦克风 A', kind: 'capture' }, { id: 'mic-b', label: '麦克风 B', kind: 'capture' }, { id: 'speaker-a', label: '扬声器 A', kind: 'playback' }] });
    if (path === '/api/v1/config?key=pre.capture_device') return jsonResponse({ value: baseline.capture });
    if (path === '/api/v1/config?key=pre.playback_device') return jsonResponse({ value: baseline.playback });
    if (path === '/api/v1/config' && method === 'PUT') {
      if (body.key === 'pre.capture_device') { baseline.capture = body.value; snapshot.pending_restart = ['pre']; }
      if (body.key.startsWith('rvc.')) { const params = snapshot.rvc_params! as unknown as Record<string, unknown>; params[body.key.slice(4)] = Number(body.value); }
      return jsonResponse({ ...body, restart_required: body.key.startsWith('pre.'), affected: body.key.startsWith('pre.') ? ['pre'] : [] });
    }
    if (path === '/api/v1/files' && method === 'POST') {
      if (submitFailure) throw new TypeError('connection lost');
      jobs = [jobFixture({ job_id: 'job-new' })];
      return jsonResponse({ job_id: 'job-new', state: 'queued', queue_position: 1 }, 202);
    }
    if (path === '/api/v1/files') return jsonResponse({ jobs, paused: false });
    if (path.startsWith('/api/v1/logs')) { const name = new URLSearchParams(path.split('?')[1] ?? '').get('name') ?? 'rvc'; return jsonResponse({ name, tail: `${name} 日志第一行\n${name} 日志第二行` }); }
    return jsonResponse({});
  });
  vi.stubGlobal('fetch', fetcher);
  return { snapshot, fetcher, failSubmit: () => { submitFailure = true; }, markPresetDirty: () => { presetDirty = true; } };
}

beforeEach(() => { vi.useFakeTimers(); sources.length = 0; vi.stubGlobal('EventSource', Source); localStorage.clear(); sessionStorage.clear(); });
afterEach(() => { for (const { app, root } of mounts.splice(0)) { app.unmount(); root.remove(); } delete document.documentElement.dataset.theme; vi.unstubAllGlobals(); vi.useRealTimers(); });

// 适配器的真实响应带 Date 头，页面用它得到设备时钟。
function withDeviceClock(backend: ReturnType<typeof mockBackend>) {
  const original = backend.fetcher.getMockImplementation()!;
  backend.fetcher.mockImplementation((path, init) => path === '/api/v1/health'
    ? Promise.resolve(new Response(JSON.stringify(envelope(healthFixture)), { headers: { 'Content-Type': 'application/json', Date: new Date().toISOString() } }))
    : original(path, init));
}
const subtitleStream = () => sources.filter(item => item.url.includes('/events/subtitles')).at(-1)!;
const sendSubtitle = (id: string, record: Partial<SubtitleRecord>) => subtitleStream().dispatchEvent(new MessageEvent('subtitle', { data: JSON.stringify(subtitleFixture(record)), lastEventId: id }));
const subtitlesOf = (root: HTMLElement) => root.querySelector('section[aria-label="实时字幕"]')?.textContent ?? '';
function pickFile(root: ParentNode, name: string) {
  const input = root.querySelector<HTMLInputElement>('input[type=file]')!;
  Object.defineProperty(input, 'files', { value: [new File(['sound'], name)], configurable: true });
  input.dispatchEvent(new Event('change', { bubbles: true }));
}

describe('控制台', () => {
  it('新打开的页面只显示打开之后收到的字幕，存档回放不显示', async () => {
    const backend = mockBackend(); withDeviceClock(backend); const root = mount(); await flush();
    const now = Date.now() / 1000;
    expect(subtitleStream().url).not.toContain('cursor=');
    subtitleStream().dispatchEvent(new MessageEvent('reset', { data: JSON.stringify({ generation: 'g1' }) }));
    sendSubtitle('g1:100', { utterance_id: 'old:1', received_at: now - 3600, zh: '上次的句子', en: 'old', translation_status: 'completed' });
    sendSubtitle('g1:200', { utterance_id: 'old:2', seq: 2, received_at: now - 3600, zh: '上次的第二句', en: 'old 2', translation_status: 'completed' });
    await nextTick();
    expect(root.querySelector('section[aria-label="实时字幕"]')).toBeNull(); expect(subtitlesOf(root)).not.toContain('上次');
    sendSubtitle('g1:300', { received_at: now - 3, zh: '在途句子', en: 'in flight', translation_status: 'completed' });
    await nextTick();
    expect(subtitlesOf(root)).toContain('在途句子');
    sendSubtitle('g1:400', { utterance_id: 'boot-b:1', received_at: now, zh: '新句子', en: 'new', translation_status: 'completed' });
    await nextTick();
    // 最新一句高亮；上一句保留在可滚动的历史里。
    expect(subtitlesOf(root)).toContain('新句子'); expect(subtitlesOf(root)).toContain('在途句子'); expect(subtitlesOf(root)).not.toContain('上次');
    const saved = JSON.parse(sessionStorage.getItem('mozart-subtitles')!);
    expect(saved.cursor).toBe('g1:400'); expect(saved.records).toHaveLength(2);
    // 打开前 20 秒说的那句被过滤；它的翻译修订带着新时间戳到达，同样不显示。
    sendSubtitle('g1:250', { utterance_id: 'old:3', seq: 3, received_at: now - 20, zh: '打开前 20 秒的句子', translation_status: 'pending' });
    await nextTick(); expect(subtitlesOf(root)).not.toContain('打开前 20 秒');
    sendSubtitle('g1:500', { utterance_id: 'old:3', seq: 3, revision: 1, received_at: now + 2, zh: '打开前 20 秒的句子', en: 'late', translation_status: 'completed' });
    sendSubtitle('g1:600', { utterance_id: 'boot-b:2', seq: 2, received_at: now + 2, zh: '之后的句子', en: 'later', translation_status: 'completed' });
    await nextTick();
    expect(subtitlesOf(root)).not.toContain('打开前 20 秒'); expect(subtitlesOf(root)).toContain('之后的句子');
  });
  it('INFO 展开后尾随服务日志，收起后停止轮询', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const details = root.querySelector('details')!;
    const calls = () => backend.fetcher.mock.calls.filter(([path]) => String(path).startsWith('/api/v1/logs'));
    expect(calls()).toHaveLength(0);
    details.open = true; details.dispatchEvent(new Event('toggle')); await flush();
    expect(calls()).toHaveLength(1);
    expect(details.querySelector('pre')!.textContent).toContain('rvc 日志第一行');
    await vi.advanceTimersByTimeAsync(2000); await flush();
    expect(calls()).toHaveLength(2);
    const select = details.querySelector<HTMLSelectElement>('#log-service')!;
    select.value = 'pre'; select.dispatchEvent(new Event('change')); await flush();
    expect(String(calls().at(-1)![0])).toContain('name=pre');
    details.open = false; details.dispatchEvent(new Event('toggle')); await flush();
    const settled = calls().length;
    await vi.advanceTimersByTimeAsync(4000); await flush();
    expect(calls()).toHaveLength(settled);
  });
  it('浏览器拒绝保存字幕进度时给出提示', async () => {
    const backend = mockBackend(); withDeviceClock(backend);
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new DOMException('denied', 'SecurityError'); });
    const root = mount(); await flush();
    sendSubtitle('g1:100', { received_at: Date.now() / 1000, translation_status: 'completed' }); await nextTick();
    expect(root.querySelector('details')!.textContent).toContain('浏览器拒绝保存字幕进度');
    expect(subtitlesOf(root)).toContain('你好');
  });
  it('页面重建后恢复字幕并从游标接续，reset 清空', async () => {
    const backend = mockBackend(); withDeviceClock(backend); mount(); await flush();
    const now = Date.now() / 1000;
    sendSubtitle('g1:100', { received_at: now, zh: '刷新前的句子', en: 'before', translation_status: 'completed' });
    await nextTick();
    const first = mounts.pop()!; first.app.unmount(); first.root.remove();
    const reloaded = mount(); await flush();
    expect(subtitleStream().url).toContain('?cursor=g1%3A100');
    expect(subtitlesOf(reloaded)).toContain('刷新前的句子');
    sendSubtitle('g1:200', { utterance_id: 'boot-a:2', seq: 2, received_at: now, zh: '刷新后的句子', en: 'after', translation_status: 'completed' });
    await nextTick();
    expect(subtitlesOf(reloaded)).toContain('刷新后的句子'); expect(subtitlesOf(reloaded)).toContain('刷新前的句子');
    subtitleStream().dispatchEvent(new MessageEvent('reset', { data: JSON.stringify({ generation: 'g2' }) })); await nextTick();
    expect(reloaded.querySelector('section[aria-label="实时字幕"]')).toBeNull(); expect(sessionStorage.getItem('mozart-subtitles')).toBeNull();
  });
  it('模型和预设读取失败后禁止使用旧数据，关闭告警也不解除，恢复读取后才解除', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    const root = mount(); await flush();
    const select = [...root.querySelectorAll('select')].find(item => item.textContent?.includes('选择模型'))!;
    select.value = 'model-a'; select.dispatchEvent(new Event('change')); await flush();
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/models' || path === '/api/v1/preset' ? Promise.resolve(errorResponse('Unavailable', '读取失败', 503)) : original(path, init));
    iconButton(root, '刷新').click(); await flush();
    expect(root.textContent).toContain('读取失败');
    expect(button(root, '使用模型').disabled).toBe(true);
    expect(root.querySelector('.preset-actions.is-visible')).toBeNull();
    iconButton(section(root, '变声音色'), '关闭提示').click();
    await vi.advanceTimersByTimeAsync(300); await flush();
    expect(button(root, '使用模型').disabled).toBe(true);
    expect(root.querySelector('.preset-actions.is-visible')).toBeNull();
    backend.fetcher.mockImplementation(original); iconButton(root, '刷新').click(); await flush();
    expect(button(root, '使用模型').disabled).toBe(false);
  });
  it('未知结果刷新后仍需确认，边界拒绝重复写入且不自动重试', async () => {
    mockBackend(); let ui!: ReturnType<typeof useConsole>;
    mount(defineComponent({ setup() { ui = useConsole(); return () => h('div'); } })); await flush();
    const operation = vi.fn().mockRejectedValue(new ApiError('断开', 'Network', 0, true));
    await ui.perform('system', operation); await ui.reload();
    expect(ui.ready.value).toBe(true);
    await ui.perform('system', operation);
    expect(operation).toHaveBeenCalledTimes(1);
    ui.acknowledge('system'); await ui.perform('system', operation);
    expect(operation).toHaveBeenCalledTimes(2);
  });
  it('未知结果使在途状态查询失效', async () => {
    const backend = mockBackend(); let ui!: ReturnType<typeof useConsole>;
    mount(defineComponent({ setup() { ui = useConsole(); return () => h('div'); } })); await flush();
    const original = backend.fetcher.getMockImplementation()!; let complete!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/status'
      ? new Promise<Response>(resolve => { complete = resolve; }) : original(path, init));
    const reload = ui.reload(); await flush();
    await ui.perform('system', () => Promise.reject(new ApiError('断开', 'Network', 0, true)));
    complete(jsonResponse(statusFixture())); await reload;
    expect(ui.canAcknowledge('system')).toBe(false);
    ui.acknowledge('system'); expect(ui.unknowns.value.system).toHaveLength(1);
  });
  it('未知结果后 SSE 不能替代成功的 HTTP 状态查询', async () => {
    const backend = mockBackend(); let ui!: ReturnType<typeof useConsole>;
    mount(defineComponent({ setup() { ui = useConsole(); return () => h('div'); } })); await flush();
    const stream = sources.find(item => item.url.endsWith('/events/status'))!;
    const sendStatus = () => stream.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(statusFixture()) }));
    sendStatus();
    await ui.perform('system', () => Promise.reject(new ApiError('断开', 'Network', 0, true)));
    sendStatus();
    expect(ui.ready.value).toBe(true);
    expect(ui.canAcknowledge('system')).toBe(false);
    ui.acknowledge('system'); expect(ui.unknowns.value.system).toHaveLength(1);
    const original = backend.fetcher.getMockImplementation()!; let complete!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/status'
      ? new Promise<Response>(resolve => { complete = resolve; }) : original(path, init));
    const reload = ui.reload(); await flush();
        sendStatus(); complete(jsonResponse(statusFixture())); await reload;
    expect(ui.canAcknowledge('system')).toBe(false);
    backend.fetcher.mockImplementation(original); await ui.reload();
    expect(ui.canAcknowledge('system')).toBe(true);
    ui.acknowledge('system'); expect(ui.unknowns.value.system).toHaveLength(0);
  });
  it('设备未知结果后 SSE 和旧基线不能代替重新读取', async () => {
    const backend = mockBackend(); let ui!: ReturnType<typeof useConsole>;
    mount(defineComponent({ setup() { ui = useConsole(); return () => h('div'); } })); await flush();
    const original = backend.fetcher.getMockImplementation()!; let complete!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/config?key=pre.capture_device'
      ? new Promise<Response>(resolve => { complete = resolve; }) : original(path, init));
    const reload = ui.reload(); await flush();
    await ui.perform('devices', () => Promise.reject(new ApiError('断开', 'Network', 0, true)));
    sources.find(item => item.url.endsWith('/events/status'))!.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(statusFixture()) }));
    complete(jsonResponse({ value: 'old' })); await reload;
    expect(ui.canAcknowledge('devices')).toBe(false);
    ui.acknowledge('devices'); expect(ui.unknowns.value.devices).toHaveLength(1);
    backend.fetcher.mockImplementation(original); await ui.reload();
    expect(ui.canAcknowledge('devices')).toBe(true);
  });
  it('与默认不同时才显示保存与恢复按钮，操作后隐藏', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const actions = () => root.querySelector('.preset-actions')!;
    expect(actions().classList.contains('is-visible')).toBe(false);
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(true);
    const select = [...root.querySelectorAll('select')].find(item => item.textContent?.includes('选择模型'))!;
    select.value = 'model-a'; select.dispatchEvent(new Event('change')); await flush();
    button(root, '使用模型').click(); await flush();
    expect(backend.fetcher.mock.calls.some(([path, init]) => path === '/api/v1/model' && init?.method === 'PUT')).toBe(true);
    backend.markPresetDirty(); iconButton(root, '刷新').click(); await flush();
    expect(actions().classList.contains('is-visible')).toBe(true);
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(false);
    expect(iconButton(root, '恢复 default 预设').disabled).toBe(false);
    iconButton(root, '保存为 default 预设').click(); await flush();
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/preset/save')).toBe(true);
    expect(actions().classList.contains('is-visible')).toBe(false);
    backend.markPresetDirty(); iconButton(root, '刷新').click(); await flush();
    iconButton(root, '恢复 default 预设').click(); await flush();
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/preset/use')).toBe(true);
    expect(actions().classList.contains('is-visible')).toBe(false);
  });
  it('显示增强状态并请求重启增强进程', async () => {
    const restart = vi.fn();
    const base = statusFixture();
    const status = statusSchema.parse({ ...base, processes: { ...base.processes, enhance: { ...base.processes.pre, state: 'running' } }, enhance: { backend: 'test', frames_in: 3, frames_out: 2, sessions: 1, algorithm_delay_samples: 64, last_error: '故障' } });
    const root = mount(defineComponent({ render: () => h(SystemPanel, { status, connected: true, busy: false, onRestart: restart }) }));
    expect(root.textContent).toContain('增强后端：test');
    expect(root.textContent).toContain('故障');
    root.querySelector<HTMLButtonElement>('[aria-label="重启 enhance"]')!.click();
    expect(restart).toHaveBeenCalledWith('enhance');
  });
  it('实际路由与请求意图分开显示，不在浏览器编排播放设备', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const route = root.querySelector<HTMLSelectElement>('#system-route')!;
    route.value = 'rvc'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    expect(backend.snapshot.output_mode?.requested).toBe('rvc'); expect(backend.snapshot.output_mode?.applied).toBeNull();
    expect(route.value).toBe('rvc');
    const routeCardText = section(root, '路由').textContent ?? '';
    expect(routeCardText).toContain('切换路由中');
    expect(routeCardText).not.toContain('实际：变声');
    const writes = backend.fetcher.mock.calls.filter(([, init]) => init?.method && init.method !== 'GET');
    expect(writes).toHaveLength(1); expect(writes[0][0]).toBe('/api/v1/mode');
  });
  it('路由被拒绝后选择器恢复后端已确认值', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/mode'
      ? Promise.resolve(errorResponse('NotReady', '模型未就绪', 409)) : original(path, init));
    const root = mount(); await flush(); const route = root.querySelector<HTMLSelectElement>('#system-route')!;
    route.value = 'rvc'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    expect(root.textContent).toContain('NotReady：模型未就绪'); expect(route.value).toBe('direct');
  });
  it('迟到的状态查询错误不能覆盖较新的 SSE 成功状态', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let complete!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/status'
      ? new Promise<Response>(resolve => { complete = resolve; }) : original(path, init));
    const root = mount(); await flush();
    const source = sources.find(item => item.url.endsWith('/events/status'))!;
    source.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(statusFixture()) })); await nextTick();
    complete(errorResponse('UpstreamUnavailable', '过期的查询错误', 503)); await flush();
    expect(root.textContent).not.toContain('过期的查询错误'); expect(root.querySelector('[data-service="pre"][data-state="running"]')).not.toBeNull();
    expect(root.querySelector<HTMLSelectElement>('#system-route')!.disabled).toBe(false);
  });
  it('静音被拒绝后复选框仍显示后端实际值', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/mute'
      ? Promise.resolve(errorResponse('NotReady', '处理器不可达', 409)) : original(path, init));
    const root = mount(); await flush();
    const mute = section(root, '设备').querySelector<HTMLInputElement>('input[type=checkbox]')!;
    mute.click(); await flush();
    expect(root.textContent).toContain('NotReady：处理器不可达');
    expect(mute.checked).toBe(false); expect(backend.snapshot.mute).toBe(false);
  });
  it('静音后只经 HTTP 确认状态，不重拉资源列表，也不闪成未知', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const services = section(root, '服务');
    const flashes: unknown[] = [];
    const observer = new MutationObserver(() => { flashes.push(services.textContent); });
    observer.observe(services, { subtree: true, childList: true, characterData: true });
    backend.fetcher.mockClear();
    const mute = section(root, '设备').querySelector<HTMLInputElement>('input[type=checkbox]')!;
    mute.click(); await flush(); observer.disconnect();
    expect(backend.fetcher.mock.calls.map(([path]) => path)).toEqual(['/api/v1/mute', '/api/v1/health', '/api/v1/status']);
    expect(mute.checked).toBe(true);
    expect(flashes).toEqual([]);
  });
  it('参数写入后重读状态与预设，不重拉模型与设备列表', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    input.value = '3'; input.dispatchEvent(new Event('input', { bubbles: true }));
    backend.fetcher.mockClear();
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(backend.fetcher.mock.calls.map(([path]) => path)).toEqual(['/api/v1/config', '/api/v1/health', '/api/v1/status', '/api/v1/preset']);
    expect(input.value).toBe('3');
  });
  it('写入只锁本控件：设备设置进行中，其他控件的写入照常可用', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let finish!: () => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/config' && init?.method === 'PUT'
      ? new Promise<Response>(resolve => { finish = () => resolve(original(path, init)); }) : original(path, init));
    const root = mount(); await flush();
    const select = root.querySelector<HTMLSelectElement>('#device-capture')!;
    select.value = 'mic-b'; select.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    iconButton(section(root, '设备'), '设置输入设备').click(); await flush();
    expect(section(root, '设备').querySelectorAll<HTMLButtonElement>('button:disabled')).toHaveLength(2);
    expect(root.querySelector<HTMLSelectElement>('#device-playback')!.disabled).toBe(true);
    const others = [button(root, '启动系统'), iconButton(root, '暂停队列'), root.querySelector<HTMLSelectElement>('#system-route')!,
      root.querySelector<HTMLInputElement>('#param-pitch_shift')!, section(root, '设备').querySelector<HTMLInputElement>('input[type=checkbox]')!];
    expect(others.map(item => (item as HTMLButtonElement).disabled)).toEqual([false, false, false, false, false]);
    finish(); await flush();
    expect(section(root, '设备').querySelectorAll('button:disabled')).toHaveLength(0);
  });
  it('其他控件的写入结果未知时，重新发起本控件的确认读取', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let holdStatus = false; let finish!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => {
      if (path === '/api/v1/status' && holdStatus) { holdStatus = false; return new Promise<Response>(resolve => { finish = resolve; }); }
      if (path === '/api/v1/mute') return Promise.resolve(errorResponse('GatewayTimeout', '网关超时', 504));
      return original(path, init);
    });
    const root = mount(); await flush(); backend.fetcher.mockClear(); holdStatus = true;
    const select = root.querySelector<HTMLSelectElement>('#device-capture')!;
    select.value = 'mic-b'; select.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    iconButton(section(root, '设备'), '设置输入设备').click(); await flush();
    section(root, '设备').querySelector<HTMLInputElement>('input[type=checkbox]')!.click(); await flush();
    finish(jsonResponse(statusFixture())); await flush();
    expect(backend.fetcher.mock.calls.filter(([path]) => path === '/api/v1/status')).toHaveLength(2);
    expect(root.textContent).toContain('执行结果未知');
  });
  it('内容相同的状态推送不改动 DOM，数值变化只改对应文本', async () => {
    mockBackend(); const root = mount(); await flush();
    const source = sources.find(item => item.url.endsWith('/events/status'))!;
    // 首个事件把推送状态从 connecting 切到 online，属于连接状态变化，不计入。
    source.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(statusFixture()) })); await nextTick();
    // 在变动发生时记录元素与内容，便于断言“只有对应文本变化”。
    // 整体替换状态对象的回归只会表现为表单控件被改写 value 属性，不产生文本变化，所以 valueWrites 是主要的捕获点。
    const touched: string[] = []; const valueWrites: string[] = [];
    const observer = new MutationObserver(items => {
      for (const item of items) {
        const target = item.target as Element;
        const text = (item.target.textContent ?? '').trim().slice(0, 40);
        if (item.type === 'attributes' && item.attributeName === 'value') valueWrites.push(target.parentElement?.id ?? target.tagName);
        else if (item.type === 'attributes') touched.push(`${target.tagName}.${item.attributeName}`);
        else if (item.type === 'characterData') touched.push(`${target.parentElement?.tagName}:${text}`);
        else {
          const text = (node: Node) => node.nodeType === 8 ? '' : (node.textContent ?? '').trim().slice(0, 40);
          for (const node of item.addedNodes) if (text(node)) touched.push(`+${text(node)}`);
          for (const node of item.removedNodes) if (text(node)) touched.push(`-${text(node)}`);
        }
      }
    });
    observer.observe(root, { subtree: true, attributes: true, childList: true, characterData: true });
    const push = (value: ReturnType<typeof statusFixture>) => source.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(value) }));
    const base = statusFixture();
    push(statusFixture()); await nextTick();
    expect([touched, valueWrites]).toEqual([[], []]);
    push({ ...base, hw: { ...base.hw!, cpu_percent: 99 } }); await nextTick();
    expect(touched.splice(0)).toEqual(['+99%', '-12%']);
    push({ ...base, hw: { ...base.hw!, cpu_percent: 99 }, rvc: { ...base.rvc!, frames_in: 7, inference_ms_p95: 12.34 } }); await nextTick();
    observer.disconnect();
    expect(touched).toEqual(['P:RVC model-a · idle · 7/0 帧 · 丢弃 0', '+> 推理 P95：12.3 ms']);
    expect(valueWrites).toEqual([]);
  });
  it('daemon 停止后页面和启动操作仍在，停止不伪装成健康', async () => {
    mockBackend(); const root = mount(); await flush();
    button(root, '停止系统').click(); await flush();
    expect(root.textContent).toContain('daemon 未运行'); expect(root.textContent).toContain('API 在线');
    expect(button(root, '停止系统').disabled).toBe(true);
    button(root, '启动系统').click(); await flush(); expect(root.querySelector('[data-service="pre"][data-state="running"]')).not.toBeNull();
  });
  it('选择文件后直接开始转换，不要求切换模式，受理不显示完成', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    pickFile(section(root, '文件转换'), 'sample.wav'); await flush();
    const files = section(root, '文件转换');
    expect(root.textContent).toContain('已受理 job-new'); expect(root.textContent).toContain('转换尚未完成');
    expect(files.textContent).toContain('排队中'); expect(files.textContent).not.toContain('已完成');
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/route' || path === '/api/v1/mode')).toBe(false);
    expect(files.querySelector<HTMLInputElement>('input[type=file]')!.files!.length).toBe(0);
  });
  it('提交结果未知时不重复提交，核对后才可继续', async () => {
    const backend = mockBackend(); backend.failSubmit(); const root = mount(); await flush();
    const files = section(root, '文件转换');
    pickFile(files, 'sample.wav'); await flush();
    expect(files.textContent).toContain('执行结果未知');
    await vi.advanceTimersByTimeAsync(3000); await flush();
    expect(files.textContent).toContain('执行结果未知');
    expect(iconButton(files, '暂停队列').disabled).toBe(true);
    // 结果未知期间再次选择文件不再提交
    pickFile(files, 'second.wav'); await flush();
    expect(backend.fetcher.mock.calls.filter(([path, init]) => path === '/api/v1/files' && init?.method === 'POST')).toHaveLength(1);
    button(files, '已核对队列').click(); await nextTick();
    expect(iconButton(files, '暂停队列').disabled).toBe(false);
    pickFile(files, 'third.wav'); await flush();
    expect(backend.fetcher.mock.calls.filter(([path, init]) => path === '/api/v1/files' && init?.method === 'POST')).toHaveLength(2);
  });
  it('提交未知前发出的队列查询不能解锁核对按钮', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const original = backend.fetcher.getMockImplementation()!; let delayRead = true;
    let finishOldRead!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => {
      if (path === '/api/v1/files' && (init?.method ?? 'GET') === 'GET' && delayRead) {
        delayRead = false;
        return new Promise<Response>(resolve => { finishOldRead = resolve; });
      }
      return original(path, init);
    });
    const files = section(root, '文件转换');
    iconButton(files, '刷新').click(); await flush(); expect(finishOldRead).toBeTypeOf('function');
    backend.failSubmit(); pickFile(files, 'sample.wav'); await flush();
    expect(button(files, '已核对队列').disabled).toBe(true);
    finishOldRead(jsonResponse({ jobs: [], paused: false })); await flush();
    expect(button(files, '已核对队列').disabled).toBe(true);
    await vi.advanceTimersByTimeAsync(3000); await flush();
    expect(button(files, '已核对队列').disabled).toBe(false);
    expect(iconButton(files, '暂停队列').disabled).toBe(true);
  });
  it('调节参数后自动应用，合并连点，写入在途时输入不被状态推送覆盖', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let finish!: () => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/config' && init?.method === 'PUT'
      ? new Promise<Response>(resolve => { finish = () => resolve(original(path, init)); }) : original(path, init));
    const root = mount(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    const configured = () => backend.fetcher.mock.calls.filter(([path, init]) => path === '/api/v1/config' && init?.method === 'PUT');
    input.value = '2'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(80);
    input.value = '3'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(configured()).toHaveLength(1);
    expect(JSON.parse(configured()[0][1]!.body as string)).toEqual({ key: 'rvc.pitch_shift', value: '3' });
    sources.find(item => item.url.endsWith('/events/status'))!.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(statusFixture()) })); await nextTick();
    expect(input.value).toBe('3');
    finish(); await flush();
    expect(input.value).toBe('3'); expect(backend.snapshot.rvc_params!.pitch_shift).toBe(3);
  });
  it('参数确认前禁用预设操作，确认后恢复可用', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    backend.markPresetDirty(); iconButton(root, '刷新').click(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    input.value = '3'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(true);
    expect(iconButton(root, '恢复 default 预设').disabled).toBe(true);
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(false);
    expect(iconButton(root, '恢复 default 预设').disabled).toBe(false);
    expect(backend.snapshot.rvc_params!.pitch_shift).toBe(3);
  });
  it('参数写入被拒绝只回滚该项，不影响其他尚未提交的输入', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/config' && init?.method === 'PUT' && String(init.body).includes('pitch_shift')
      ? Promise.resolve(errorResponse('InvalidArgument', '音高超出范围', 400)) : original(path, init));
    const root = mount(); await flush();
    const pitch = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    const index = root.querySelector<HTMLInputElement>('#param-index_rate')!;
    pitch.value = '99'; pitch.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(80);
    index.value = '0.9'; index.dispatchEvent(new Event('input', { bubbles: true }));
    // 只让音高那次写入发生并被拒绝；索引比例仍在挂起，错误横幅可见。
    await vi.advanceTimersByTimeAsync(130); await flush();
    expect(section(root, '参数').textContent).toContain('InvalidArgument：音高超出范围');
    expect(pitch.value).toBe('0');
    expect(index.value).toBe('0.9');
    await vi.advanceTimersByTimeAsync(150); await flush();
    expect(backend.snapshot.rvc_params!.index_rate).toBe(0.9);
  });
  it('设备写入后自动重启 pre，并显示基线与实际差异', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const select = root.querySelector<HTMLSelectElement>('#device-capture')!;
    select.value = 'mic-b'; select.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    iconButton(section(root, '设备'), '设置输入设备').click(); await flush();
    expect(section(root, '设备').textContent).toContain('实际：mic-a · 基线：mic-b');
    expect(section(root, '设备').textContent).toContain('待重启');
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/services/pre/restart')).toBe(true);
  });
  it('状态推送中断立即禁止业务写入，保留查询错误', async () => {
    mockBackend(); const root = mount(); await flush();
    const source = sources.find(item => item.url.endsWith('/events/status'))!;
    source.onerror?.(new Event('error')); await nextTick();
    expect(root.querySelector<HTMLSelectElement>('#system-route')!.disabled).toBe(true);
    expect(section(root, '服务').textContent).toContain('未连接');
  });
  it('没有适配器时显示未连接，不填入模拟状态', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('<html>Vite</html>')));
    const root = mount(); await flush(); expect(root.textContent).toContain('HTTP 适配器未连接');
    expect(root.textContent).toContain('CPU —'); expect(root.querySelector('[data-service="pre"]')?.getAttribute('data-state')).toBe('unknown');
  });
  it('错误按控件归属展示在对应卡片内，且可手动关闭', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => {
      if (path === '/api/v1/mute') return Promise.resolve(errorResponse('NotReady', '静音拒绝', 409));
      if (path === '/api/v1/model') return Promise.resolve(errorResponse('NotLoaded', '模型损坏', 409));
      return original(path, init);
    });
    const root = mount(); await flush();
    section(root, '设备').querySelector<HTMLInputElement>('input[type=checkbox]')!.click(); await flush();
    expect(section(root, '设备').textContent).toContain('NotReady：静音拒绝');
    expect(section(root, '路由').textContent).not.toContain('静音拒绝');
    const select = root.querySelector<HTMLSelectElement>('#model-select')!;
    select.value = 'model-a'; select.dispatchEvent(new Event('change')); await nextTick();
    button(root, '使用模型').click(); await flush();
    expect(section(root, '变声音色').textContent).toContain('NotLoaded：模型损坏');
    expect(section(root, '路由').textContent).not.toContain('模型损坏');
    iconButton(section(root, '变声音色'), '关闭提示').click();
    await vi.advanceTimersByTimeAsync(300); await flush();
    expect(section(root, '变声音色').textContent).not.toContain('模型损坏');
  });
  it('参数失焦立即提交待写值，改回原值清除脏状态并恢复预设按钮', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    backend.markPresetDirty(); iconButton(root, '刷新').click(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    input.value = '4'; input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new Event('blur')); await flush();
    expect(backend.snapshot.rvc_params!.pitch_shift).toBe(4);
    input.value = '6'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(true);
    input.value = '4'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(iconButton(root, '保存为 default 预设').disabled).toBe(false);
    iconButton(root, '增大音高增益').click();
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(backend.snapshot.rvc_params!.pitch_shift).toBe(5);
  });
  it('参数输入框使用各字段步进与边界，步进按钮不越界', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const protect = root.querySelector<HTMLInputElement>('#param-protect')!;
    const index = root.querySelector<HTMLInputElement>('#param-index_rate')!;
    const pitch = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    expect(protect.step).toBe('0.05'); expect(protect.min).toBe('0'); expect(protect.max).toBe('0.5');
    expect(index.step).toBe('0.05'); expect(index.min).toBe('0'); expect(index.max).toBe('1');
    expect(pitch.step).toBe('1'); expect(pitch.min).toBe('-12'); expect(pitch.max).toBe('12');
    for (let i = 0; i < 5; i += 1) iconButton(root, '增大清辅音保护').click();
    await vi.advanceTimersByTimeAsync(250); await flush();
    expect(backend.snapshot.rvc_params!.protect).toBe(0.5);
  });
  it('模型选框初始回显当前加载模型，刷新时不覆盖手动待选模型', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/models'
      ? Promise.resolve(jsonResponse({ models: [{ id: 'model-a', exists: true, loaded: true }, { id: 'model-b', exists: true, loaded: false }] }))
      : original(path, init));
    const root = mount(); await flush();
    const select = root.querySelector<HTMLSelectElement>('#model-select')!;
    expect(select.value).toBe('model-a');
    select.value = 'model-b'; select.dispatchEvent(new Event('change')); await nextTick();
    iconButton(root, '刷新').click(); await flush();
    expect(select.value).toBe('model-b');
  });
  it('日志快速切换服务时忽略过期的旧请求结果', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let resolveRvc!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => {
      if (path === '/api/v1/logs?name=rvc') return new Promise<Response>(resolve => { resolveRvc = resolve; });
      return original(path, init);
    });
    const root = mount(); await flush();
    const details = root.querySelector('details')!;
    details.open = true; details.dispatchEvent(new Event('toggle')); await flush();
    const select = details.querySelector<HTMLSelectElement>('#log-service')!;
    select.value = 'pre'; select.dispatchEvent(new Event('change')); await flush();
    expect(details.querySelector('pre')!.textContent).toContain('pre 日志第一行');
    resolveRvc(jsonResponse({ name: 'rvc', tail: '迟到的 rvc 日志' })); await flush();
    expect(details.querySelector('pre')!.textContent).toContain('pre 日志第一行');
    expect(details.querySelector('pre')!.textContent).not.toContain('迟到的 rvc 日志');
  });
  it('操作通知支持自动消隐与手动关闭', async () => {
    mockBackend(); const root = mount(); await flush();
    const route = root.querySelector<HTMLSelectElement>('#system-route')!;
    route.value = 'rvc'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    expect(root.textContent).toContain('请求已受理');
    await vi.advanceTimersByTimeAsync(5300); await flush();
    expect(root.textContent).not.toContain('请求已受理');
    route.value = 'direct'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    expect(root.textContent).toContain('请求已受理');
    iconButton(root, '关闭通知').click();
    await vi.advanceTimersByTimeAsync(300); await flush();
    expect(root.textContent).not.toContain('请求已受理');
  });
  it('无本地主题存储时保留首屏检测到的暗色主题', async () => {
    mockBackend();
    document.documentElement.dataset.theme = 'dark';
    mount(); await flush();
    expect(document.documentElement.dataset.theme).toBe('dark');
  });
  it('参数前次写入在途时改回原值仍排队写回，不被在途结果覆盖', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let finishFirst!: () => void;
    let firstHeld = true;
    backend.fetcher.mockImplementation((path, init) => {
      if (path === '/api/v1/config' && init?.method === 'PUT' && firstHeld) {
        firstHeld = false;
        return new Promise<Response>(resolve => { finishFirst = () => resolve(original(path, init)); });
      }
      return original(path, init);
    });
    const root = mount(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    input.value = '4'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(250); await flush();
    input.value = '0'; input.dispatchEvent(new Event('input', { bubbles: true }));
    await vi.advanceTimersByTimeAsync(250); await flush();
    finishFirst(); await flush();
    expect(backend.snapshot.rvc_params!.pitch_shift).toBe(0);
    expect(input.value).toBe('0');
  });
  it('日志切换服务期间先清空旧服务内容，不继续展示过期日志', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    let resolvePre!: (value: Response) => void;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/logs?name=pre'
      ? new Promise<Response>(resolve => { resolvePre = resolve; }) : original(path, init));
    const root = mount(); await flush();
    const details = root.querySelector('details')!;
    details.open = true; details.dispatchEvent(new Event('toggle')); await flush();
    expect(details.querySelector('pre')!.textContent).toContain('rvc 日志第一行');
    const select = details.querySelector<HTMLSelectElement>('#log-service')!;
    select.value = 'pre'; select.dispatchEvent(new Event('change')); await nextTick();
    expect(details.querySelector('pre')!.textContent).toContain('正在读取日志');
    expect(details.querySelector('pre')!.textContent).not.toContain('rvc 日志第一行');
    resolvePre(jsonResponse({ name: 'pre', tail: 'pre 最新日志' })); await flush();
    expect(details.querySelector('pre')!.textContent).toContain('pre 最新日志');
  });
  it('无左侧翻译播报卡片，播报设置仅在翻译播报时显示且无占位文案', async () => {
    mockBackend(); const root = mount(); await flush();
    expect(root.querySelector('section[aria-label="翻译播报"]')).toBeNull();
    expect(root.querySelector('section[aria-label="播报设置"]')).toBeNull();
    const route = root.querySelector<HTMLSelectElement>('#system-route')!;
    route.value = 'rvc'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    expect(root.querySelector('section[aria-label="播报设置"]')).toBeNull();
    route.value = 'broadcast'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    const panel = section(root, '播报设置');
    expect(panel.textContent).toContain('开启中');
    expect(panel.textContent).toContain('清除播报队列');
    expect(panel.textContent).toContain('播报队列');
    expect(panel.textContent).not.toMatch(/敬请期待|即将|暂未/);
    const stream = sources.find(item => item.url.endsWith('/events/status'))!;
    const applied = statusFixture();
    applied.output_mode = { requested: 'broadcast', applied: 'broadcast', phase: 'applied', last_error: null };
    applied.bridge!.speech_enabled = true;
    stream.dispatchEvent(new MessageEvent('status', { data: JSON.stringify(applied) })); await nextTick();
    expect(section(root, '播报设置').textContent).toContain('播报中');
    route.value = 'direct'; route.dispatchEvent(new Event('change', { bubbles: true })); await flush();
    await vi.advanceTimersByTimeAsync(300); await flush();
    expect(root.querySelector('section[aria-label="播报设置"]')).toBeNull();
  });
  it('清除播报队列调用 POST /api/v1/speech/clear 并保持播报与托管模式', async () => {
    const backend = mockBackend();
    backend.snapshot.output_mode = { requested: 'broadcast', applied: 'broadcast', phase: 'applied', last_error: null };
    backend.snapshot.bridge!.speech_enabled = true;
    backend.snapshot.bridge!.speech_queue_depth = 3;
    const root = mount(); await flush();
    expect(section(root, '播报设置').textContent).toContain('播报队列 3');
    button(section(root, '播报设置'), '清除播报队列').click(); await flush();
    const writes = backend.fetcher.mock.calls.filter(([, init]) => init?.method && init.method !== 'GET');
    expect(writes.map(([path]) => path)).toContain('/api/v1/speech/clear');
    expect(writes.some(([path]) => path === '/api/v1/speech/stop' || path === '/api/v1/speech')).toBe(false);
    expect(backend.snapshot.bridge!.speech_enabled).toBe(true);
    expect(backend.snapshot.bridge!.speech_queue_depth).toBe(0);
    expect(root.querySelector('section[aria-label="播报设置"]')).not.toBeNull();
  });
  it('路由卡内独立翻译开关切换 PUT /api/v1/translate 并在播报设置提示状态', async () => {
    const backend = mockBackend();
    backend.snapshot.output_mode = { requested: 'broadcast', applied: 'broadcast', phase: 'applied', last_error: null };
    backend.snapshot.bridge!.speech_enabled = true;
    const root = mount(); await flush();
    const toggle = section(root, '路由').querySelector<HTMLButtonElement>('.translate-toggle')!;
    expect(toggle.getAttribute('aria-pressed')).toBe('true');
    toggle.click(); await flush();
    const writes = backend.fetcher.mock.calls.filter(([path, init]) => path === '/api/v1/translate' && init?.method === 'PUT');
    expect(writes).toHaveLength(1);
    expect(writes[0][1]!.body).toBe('{"enabled":false}');
    expect(toggle.getAttribute('aria-pressed')).toBe('false');
    expect(toggle.textContent).toContain('已关闭');
    expect(section(root, '播报设置').textContent).toContain('翻译已关闭，当前不会触发播报。');
    toggle.click(); await flush();
    expect(toggle.getAttribute('aria-pressed')).toBe('true');
    expect(section(root, '播报设置').textContent).not.toContain('翻译已关闭，当前不会触发播报。');
  });
  it('托管模式切换错误 output_mode.last_error 在路由卡内显示', async () => {
    const backend = mockBackend();
    backend.snapshot.output_mode = { requested: 'rvc', applied: null, phase: 'routing', last_error: 'rvc stream failed' };
    const root = mount(); await flush();
    expect(section(root, '路由').textContent).toContain('rvc stream failed');
  });
});

describe('设备草稿', () => {
  it('其他设备状态更新不覆盖用户未应用的选择', async () => {
    const status = reactive(statusFixture()); const baselines = reactive({ capture: 'mic-a', playback: 'speaker-a' });
    const root = document.createElement('div'); document.body.append(root);
    const app = createApp({ render: () => h(DevicesPanel, { status, baselines, devices: [], ready: true, busy: false }) });
    app.mount(root); mounts.push({ app, root });
    const input = root.querySelector<HTMLSelectElement>('#device-capture')!;
    // 为真实外部设备变更提供新选项。
    const option = document.createElement('option'); option.value = 'mic-b'; input.append(option);
    input.value = 'mic-b'; input.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    status.pre!.playback_device = 'speaker-b'; await nextTick(); expect(input.value).toBe('mic-b');
  });

  it('回声消除 (AEC) 按钮按当前开关状态切换 pre.aec.enabled', async () => {
    const status = reactive(statusFixture());
    (status.pre as NonNullable<typeof status.pre> & { aec?: unknown }).aec = {
      enabled: false,
      backend: 'webrtc-aec3',
      active: false,
      delay_ms: null,
      erle_db: null,
      ref_dropped: 0,
      ref_underruns: 0,
      resets: 0,
      capture_clips: 0,
      last_error: null,
    };
    const emitted: Array<[string, string]> = [];
    const root = document.createElement('div');
    document.body.append(root);
    const app = createApp({
      render: () => h(DevicesPanel, {
        status,
        baselines: { capture: 'mic-a', playback: 'speaker-a' },
        devices: [],
        ready: true,
        busy: false,
        onSet: (key: string, value: string) => emitted.push([key, value]),
      }),
    });
    app.mount(root);
    mounts.push({ app, root });
    await nextTick();
    const button = Array.from(root.querySelectorAll('button')).find(el => el.textContent?.includes('回声消除 (AEC)'));
    expect(button).toBeDefined();
    expect(button!.getAttribute('aria-pressed')).toBe('false');
    button!.click();
    expect(emitted[0]).toEqual(['pre.aec.enabled', 'true']);

    (status.pre as NonNullable<typeof status.pre> & { aec?: { enabled: boolean; active: boolean } }).aec!.enabled = true;
    (status.pre as NonNullable<typeof status.pre> & { aec?: { enabled: boolean; active: boolean } }).aec!.active = true;
    await nextTick();
    expect(button!.getAttribute('aria-pressed')).toBe('true');
    button!.click();
    expect(emitted[1]).toEqual(['pre.aec.enabled', 'false']);
  });
});
