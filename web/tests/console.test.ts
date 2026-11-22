import { createApp, defineComponent, h, nextTick, reactive, type App as VueApp } from 'vue';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.vue';
import { useConsole } from '../src/useConsole';
import { ApiError } from '../src/api';
import DevicesPanel from '../src/components/DevicesPanel.vue';
import SystemPanel from '../src/components/SystemPanel.vue';
import { statusSchema } from '../src/contracts';
import { statusFixture, healthFixture, jobFixture, jsonResponse, errorResponse } from './fixtures';
import type { FileJob } from '../src/contracts';

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
function section(root: HTMLElement, title: string) {
  const found = [...root.querySelectorAll('section')].find(item => item.querySelector('h2')?.textContent === title);
  if (!found) throw new Error(`没有卡片：${title}`); return found;
}

function mockBackend() {
  const snapshot = statusFixture();
  let running = true;
  let jobs: FileJob[] = [];
  let submitFailure = false;
  const baseline = { capture: 'mic-a', playback: 'speaker-a' };
  const fetcher = vi.fn(async (path: string, init: RequestInit = {}) => {
    const method = init.method ?? 'GET'; const body = typeof init.body === 'string' ? JSON.parse(init.body) : undefined;
    if (path === '/api/v1/health') return jsonResponse(healthFixture);
    if (path === '/api/v1/status') return running ? jsonResponse(snapshot) : errorResponse('UpstreamUnavailable', 'daemon 未运行', 503);
    if (path === '/api/v1/system/up') { running = true; return jsonResponse({ state: 'running' }); }
    if (path === '/api/v1/system/down') { running = false; return jsonResponse({ state: 'stopped' }); }
    if (path === '/api/v1/route') { snapshot.route.desired = body.route; return jsonResponse({ route: body.route }); }
    if (path === '/api/v1/models') return jsonResponse({ models: [{ id: 'model-a', exists: true, loaded: true }] });
    if (path === '/api/v1/preset') return jsonResponse({ model_id: 'model-a', preset: 'default', preset_values: snapshot.rvc_params, live_values: snapshot.rvc_params, dirty: false });
    if (path === '/api/v1/model') return jsonResponse({ model_id: body.model_id, previous_model_id: 'model-a' });
    if (path === '/api/v1/preset/save' || path === '/api/v1/preset/use') return jsonResponse({ model_id: 'model-a', preset: 'default', values: snapshot.rvc_params });
    if (path === '/api/v1/devices') return jsonResponse({ devices: [{ id: 'mic-a', label: '麦克风 A', kind: 'capture' }, { id: 'mic-b', label: '麦克风 B', kind: 'capture' }, { id: 'speaker-a', label: '扬声器 A', kind: 'playback' }] });
    if (path === '/api/v1/config?key=pre.capture_device') return jsonResponse({ value: baseline.capture });
    if (path === '/api/v1/config?key=pre.playback_device') return jsonResponse({ value: baseline.playback });
    if (path === '/api/v1/config' && method === 'PUT') {
      if (body.key === 'pre.capture_device') { baseline.capture = body.value; snapshot.pending_restart = ['pre']; }
      if (body.key === 'rvc.pitch_shift') snapshot.rvc_params!.pitch_shift = Number(body.value);
      return jsonResponse({ ...body, restart_required: body.key.startsWith('pre.'), affected: body.key.startsWith('pre.') ? ['pre'] : [] });
    }
    if (path === '/api/v1/files' && method === 'POST') {
      if (submitFailure) throw new TypeError('connection lost');
      jobs = [jobFixture({ job_id: 'job-new' })];
      return jsonResponse({ job_id: 'job-new', state: 'queued', queue_position: 1 }, 202);
    }
    if (path === '/api/v1/files') return jsonResponse({ jobs, paused: false });
    return jsonResponse({});
  });
  vi.stubGlobal('fetch', fetcher);
  return { snapshot, fetcher, failSubmit: () => { submitFailure = true; } };
}

beforeEach(() => { vi.useFakeTimers(); sources.length = 0; vi.stubGlobal('EventSource', Source); localStorage.clear(); });
afterEach(() => { for (const { app, root } of mounts.splice(0)) { app.unmount(); root.remove(); } vi.unstubAllGlobals(); vi.useRealTimers(); });

describe('控制台', () => {
  it('模型和预设读取失败后禁止使用旧数据，恢复读取后解除', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    const root = mount(); await flush();
    const select = [...root.querySelectorAll('select')].find(item => item.textContent?.includes('选择模型'))!;
    select.value = 'model-a'; select.dispatchEvent(new Event('change')); await flush();
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/models' || path === '/api/v1/preset' ? Promise.resolve(errorResponse('Unavailable', '读取失败', 503)) : original(path, init));
    button(root, '刷新').click(); await flush();
    expect(root.textContent).toContain('读取失败');
    expect(button(root, '使用模型').disabled).toBe(true);
    expect(button(root, '使用 default 预设').disabled).toBe(true);
    backend.fetcher.mockImplementation(original); button(root, '刷新').click(); await flush();
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
  it('选择模型并保存和使用 default 预设', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    expect(root.textContent).toContain('与当前值一致');
    const select = [...root.querySelectorAll('select')].find(item => item.textContent?.includes('选择模型'))!;
    select.value = 'model-a'; select.dispatchEvent(new Event('change')); await flush();
    button(root, '使用模型').click(); await flush();
    button(root, '保存 default 预设').click(); await flush();
    button(root, '使用 default 预设').click(); await flush();
    expect(backend.fetcher.mock.calls.some(([path, init]) => path === '/api/v1/model' && init?.method === 'PUT')).toBe(true);
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/preset/save')).toBe(true);
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/preset/use')).toBe(true);
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
    expect(backend.snapshot.route.desired).toBe('rvc'); expect(backend.snapshot.route.active).toBe('direct');
    expect(route.value).toBe('rvc'); expect(section(root, '系统').textContent).toContain('实际：direct');
    const writes = backend.fetcher.mock.calls.filter(([, init]) => init?.method && init.method !== 'GET');
    expect(writes).toHaveLength(1); expect(writes[0][0]).toBe('/api/v1/route');
  });
  it('路由被拒绝后选择器恢复后端已确认值', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/route'
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
    expect(root.textContent).not.toContain('过期的查询错误'); expect(section(root, '系统').textContent).toContain('已连接');
    expect(root.querySelector<HTMLSelectElement>('#system-route')!.disabled).toBe(false);
  });
  it('静音被拒绝后复选框仍显示后端实际值', async () => {
    const backend = mockBackend(); const original = backend.fetcher.getMockImplementation()!;
    backend.fetcher.mockImplementation((path, init) => path === '/api/v1/mute'
      ? Promise.resolve(errorResponse('NotReady', '处理器不可达', 409)) : original(path, init));
    const root = mount(); await flush();
    const mute = section(root, '系统').querySelector<HTMLInputElement>('input[type=checkbox]')!;
    mute.click(); await flush();
    expect(root.textContent).toContain('NotReady：处理器不可达');
    expect(mute.checked).toBe(false); expect(backend.snapshot.mute).toBe(false);
  });
  it('daemon 停止后页面和启动操作仍在，停止不伪装成健康', async () => {
    mockBackend(); const root = mount(); await flush();
    button(root, '停止系统').click(); await flush();
    expect(root.textContent).toContain('daemon 未运行'); expect(root.textContent).toContain('API 在线');
    expect(button(root, '停止系统').disabled).toBe(true);
    button(root, '启动系统').click(); await flush(); expect(section(root, '系统').textContent).toContain('已连接');
  });
  it('文件直接入队，不要求切换 file 模式，受理不显示完成', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    let files = section(root, '文件转换'); const input = files.querySelector<HTMLInputElement>('input[type=file]')!;
    Object.defineProperty(input, 'files', { value: [new File(['sound'], 'sample.wav')] }); input.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    files.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
    files = section(root, '文件转换');
    expect(root.textContent).toContain('已受理 job-new'); expect(root.textContent).toContain('转换尚未完成');
    expect(files.textContent).toContain('排队中'); expect(files.textContent).not.toContain('已完成');
    expect(backend.fetcher.mock.calls.some(([path]) => path === '/api/v1/route')).toBe(false);
    expect(files.querySelector<HTMLInputElement>('input[type=file]')!.files!.length).toBe(0);
  });
  it('提交断连保留文件草稿和未知结果，后台查询不能清掉错误或重发', async () => {
    const backend = mockBackend(); backend.failSubmit(); const root = mount(); await flush();
    const files = section(root, '文件转换'); const input = files.querySelector<HTMLInputElement>('input[type=file]')!;
    Object.defineProperty(input, 'files', { value: [new File(['sound'], 'sample.wav')] }); input.dispatchEvent(new Event('change')); await nextTick();
    files.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
    expect(files.textContent).toContain('执行结果未知');
    await vi.advanceTimersByTimeAsync(3000); await flush();
    expect(files.textContent).toContain('执行结果未知'); expect(input.files!.length).toBe(1);
    button(files, '暂停队列').click(); await flush();
    expect(files.textContent).toContain('执行结果未知'); expect(button(files, '加入队列').disabled).toBe(true);
    files.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
    expect(backend.fetcher.mock.calls.filter(([path, init]) => path === '/api/v1/files' && init?.method === 'POST')).toHaveLength(1);
    button(files, '已核对队列').click(); await nextTick(); expect(button(files, '加入队列').disabled).toBe(false);
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
    const files = section(root, '文件转换'); const input = files.querySelector<HTMLInputElement>('input[type=file]')!;
    Object.defineProperty(input, 'files', { value: [new File(['sound'], 'sample.wav')] }); input.dispatchEvent(new Event('change')); await nextTick();
    button(files, '刷新').click(); await flush(); expect(finishOldRead).toBeTypeOf('function');
    backend.failSubmit(); files.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
    expect(button(files, '已核对队列').disabled).toBe(true);
    finishOldRead(jsonResponse({ jobs: [], paused: false })); await flush();
    expect(button(files, '已核对队列').disabled).toBe(true);
    await vi.advanceTimersByTimeAsync(3000); await flush();
    expect(button(files, '已核对队列').disabled).toBe(false);
    expect(button(files, '加入队列').disabled).toBe(true);
  });
  it('参数草稿不被轮询覆盖，只提交显式点击的一项', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const input = root.querySelector<HTMLInputElement>('#param-pitch_shift')!;
    input.value = '3'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
    await vi.advanceTimersByTimeAsync(3000); await flush(); expect(input.value).toBe('3');
    input.closest('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
    const config = backend.fetcher.mock.calls.find(([path, init]) => path === '/api/v1/config' && init?.method === 'PUT');
    expect(JSON.parse(config![1]!.body as string)).toEqual({ key: 'rvc.pitch_shift', value: '3' });
    expect(section(root, '参数').textContent).toContain('当前：3');
    expect(backend.fetcher.mock.calls.some(([path]) => path.includes('stream') || path.includes('mode/switch'))).toBe(false);
  });
  it('设备写入显示基线与实际差异，不自动重启', async () => {
    const backend = mockBackend(); const root = mount(); await flush();
    const select = root.querySelector<HTMLSelectElement>('#device-capture')!;
    select.value = 'mic-b'; select.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
    button(section(root, '设备'), '设置').click(); await flush();
    expect(section(root, '设备').textContent).toContain('实际：mic-a · 基线：mic-b');
    expect(section(root, '设备').textContent).toContain('待重启');
    expect(backend.fetcher.mock.calls.some(([path]) => path.endsWith('/restart'))).toBe(false);
  });
  it('状态推送中断立即禁止业务写入，保留查询错误', async () => {
    mockBackend(); const root = mount(); await flush();
    const source = sources.find(item => item.url.endsWith('/events/status'))!;
    source.onerror?.(new Event('error')); await nextTick();
    expect(root.querySelector<HTMLSelectElement>('#system-route')!.disabled).toBe(true);
    expect(section(root, '系统').textContent).toContain('状态未知');
  });
  it('没有适配器时显示未连接，不填入模拟状态', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('<html>Vite</html>')));
    const root = mount(); await flush(); expect(root.textContent).toContain('HTTP 适配器未连接');
    expect(root.textContent).toContain('CPU —'); expect(root.textContent).not.toContain('已连接');
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
});
