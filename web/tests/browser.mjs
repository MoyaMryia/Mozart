import assert from 'node:assert/strict';
import { EventEmitter, once } from 'node:events';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';
import { preview } from 'vite';
import { statusFixture, healthFixture, jobFixture, subtitleFixture, envelope } from './fixtures.ts';

// 仅测试进程提供契约夹具；生产入口没有模拟数据或模拟模式开关。
const root = fileURLToPath(new URL('..', import.meta.url));
const evidence = process.env.MOZART_WEB_EVIDENCE_DIR ?? await mkdtemp(join(tmpdir(), 'mozart-web-browser-'));
const snapshot = statusFixture();
const presetValues = { ...snapshot.rvc_params };
const writes = [];
let running = true;
let jobs = [];
let queuePaused = false;
let baselineCapture = 'mic-a';
let subtitleReconnectGate;
const statusStreams = new Set();
const subtitleStreams = new Set();
const events = new EventEmitter();
// 新打开的页面只显示打开之后的字幕：回放记录在页面加载前就标记为刚收到。
const live = { received_at: Date.now() / 1000 };
const subtitleHistory = [{ id: 'browser-1:100', record: subtitleFixture({ en: 'Hello', revision: 1, translation_status: 'completed', received_at: live.received_at }) }];
const wav = Buffer.alloc(46);
wav.write('RIFF'); wav.writeUInt32LE(38, 4); wav.write('WAVEfmt ', 8); wav.writeUInt32LE(16, 16);
wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22); wav.writeUInt32LE(48000, 24);
wav.writeUInt32LE(96000, 28); wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34); wav.write('data', 36); wav.writeUInt32LE(2, 40);

const sendEvent = (res, name, data, id) => res.write(`${id ? `id: ${id}\n` : ''}event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
const broadcastStatus = () => { for (const res of statusStreams) sendEvent(res, 'status', snapshot); };
const response = (res, data, status = 200) => { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store', Date: new Date().toUTCString() }); res.end(JSON.stringify(envelope(data))); };
const failure = (res, code, reason, status) => { res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ status: 'error', code, reason, data: null })); };

async function handleApi(req, res) {
  const url = new URL(req.url, 'http://localhost'); const path = url.pathname.slice('/api/v1'.length);
  const method = req.method;
  if (method === 'GET') {
    if (path === '/health') return response(res, healthFixture);
    if (path === '/status') return running ? response(res, snapshot) : failure(res, 'UpstreamUnavailable', 'daemon 未运行', 503);
    if (path === '/models') return response(res, { models: [{ id: 'model-a', exists: true, loaded: true }] });
    if (path === '/preset') return response(res, { model_id: 'model-a', preset: 'default', preset_values: presetValues, live_values: snapshot.rvc_params, dirty: JSON.stringify(presetValues) !== JSON.stringify(snapshot.rvc_params) });
    if (path === '/devices') return response(res, { devices: [{ id: 'mic-a', label: '麦克风 A', kind: 'capture' }, { id: 'mic-b', label: '麦克风 B', kind: 'capture' }, { id: 'speaker-a', label: '扬声器 A', kind: 'playback' }] });
    if (path === '/config') return response(res, { value: url.searchParams.get('key') === 'pre.capture_device' ? baselineCapture : 'speaker-a' });
    if (path === '/files') return response(res, { jobs, paused: queuePaused });
    if (path === '/logs') return response(res, { name: url.searchParams.get('name') ?? 'rvc', tail: '[mock] frame=100\n[mock] frame=200' });
    if (path === '/files/job-new/result') {
      res.writeHead(200, { 'Content-Type': 'audio/wav', 'Content-Disposition': "attachment; filename*=UTF-8''result.wav", 'Content-Length': wav.length }); res.end(wav); return;
    }
    if (path === '/events/status' || path === '/events/subtitles') {
      if (!running && path.endsWith('/status')) return failure(res, 'UpstreamUnavailable', 'daemon 未运行', 503);
      if (path.endsWith('/subtitles') && subtitleReconnectGate) await subtitleReconnectGate;
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
      res.write(': connected\n\n');
      const streams = path.endsWith('/status') ? statusStreams : subtitleStreams;
      streams.add(res); res.on('close', () => streams.delete(res));
      if (path.endsWith('/status')) sendEvent(res, 'status', snapshot);
      else {
        const cursor = url.searchParams.get('cursor');
        events.emit('subtitle-connection', { cursor });
        if (!cursor) sendEvent(res, 'reset', { generation: 'browser-1' });
        const offset = cursor ? Number(cursor.split(':').at(-1)) : 0;
        for (const item of subtitleHistory) if (Number(item.id.split(':').at(-1)) > offset) sendEvent(res, 'subtitle', item.record, item.id);
      }
      return;
    }
    return failure(res, 'NotFound', `未定义测试端点 ${path}`, 404);
  }
  const chunks = []; for await (const chunk of req) chunks.push(chunk);
  const raw = Buffer.concat(chunks).toString('utf8');
  const body = req.headers['content-type']?.startsWith('application/json') ? JSON.parse(raw) : null;
  writes.push({ path, method, body });
  if (path === '/system/down') {
    running = false;
    for (const stream of statusStreams) { sendEvent(stream, 'error', { status: 'error', code: 'UpstreamUnavailable', reason: 'daemon 未运行', data: null }); stream.end(); }
    return response(res, { state: 'stopped' });
  }
  if (path === '/system/up') { running = true; return response(res, { state: 'running' }); }
  if (path === '/services/pre/restart') { snapshot.pending_restart = []; broadcastStatus(); return response(res, { service: 'pre' }); }
  if (path === '/route') { snapshot.route.desired = body.route; broadcastStatus(); return response(res, { route: body.route }); }
  if (path === '/mode') {
    const phase = body.mode === 'broadcast' ? 'opening_speech' : 'routing';
    snapshot.output_mode = { requested: body.mode, applied: null, phase, last_error: null };
    snapshot.bridge.speech_enabled = body.mode === 'broadcast';
    broadcastStatus();
    return response(res, snapshot.output_mode);
  }
  if (path === '/mute') { snapshot.mute = body.mute; broadcastStatus(); return response(res, { mute: body.mute }); }
  if (path === '/preset/save') { Object.assign(presetValues, snapshot.rvc_params); return response(res, { model_id: 'model-a', preset: 'default', values: presetValues }); }
  if (path === '/preset/use') { Object.assign(snapshot.rvc_params, presetValues); broadcastStatus(); return response(res, { model_id: 'model-a', preset: 'default', values: presetValues }); }
  if (path === '/translate') { snapshot.bridge.translate_enabled = body.enabled; broadcastStatus(); return response(res, body); }
  if (path === '/speech') { snapshot.bridge.speech_enabled = body.enabled; broadcastStatus(); return response(res, body); }
  if (path === '/speech/stop') { snapshot.bridge.speech_enabled = false; broadcastStatus(); return response(res, {}); }
  if (path === '/speech/clear') { snapshot.bridge.speech_queue_depth = 0; broadcastStatus(); return response(res, {}); }
  if (path === '/files/pause' || path === '/files/resume') { queuePaused = path.endsWith('/pause'); return response(res, { paused: queuePaused }); }
  if (path === '/files/job-new/cancel') { jobs[0].state = 'cancelled'; return response(res, { job_id: 'job-new', state: 'cancelled' }); }
  if (path === '/files') {
    assert.match(raw, /name="file"/); assert.doesNotMatch(raw, /name="audio_file"/);
    jobs = [jobFixture({ job_id: 'job-new' })]; return response(res, { job_id: 'job-new', state: 'queued', queue_position: 1 }, 202);
  }
  if (path === '/config') {
    if (body.key === 'rvc.pitch_shift' && body.value === '13') return failure(res, 'InvalidArgument', '音高超出范围', 400);
    const restart = body.key.startsWith('pre.');
    if (body.key === 'pre.capture_device') { baselineCapture = body.value; snapshot.pending_restart = ['pre']; }
    if (body.key === 'rvc.pitch_shift') snapshot.rvc_params.pitch_shift = Number(body.value);
    broadcastStatus(); return response(res, { ...body, restart_required: restart, affected: restart ? ['pre'] : [] });
  }
  return failure(res, 'Unsupported', `未接入 ${path}`, 501);
}

const server = await preview({ root, configFile: false, preview: { host: '127.0.0.1', port: 0, strictPort: true }, plugins: [{
  name: 'browser-contract-fixture',
  configurePreviewServer(instance) {
    instance.middlewares.use((req, res, next) => {
      if (!req.url?.startsWith('/api/v1/')) return next();
      handleApi(req, res).catch(error => { res.destroy(error); throw error; });
    });
  },
}] });
const address = server.httpServer.address();
let browser;
const failures = [];
try {
  browser = await chromium.launch({ headless: true, executablePath: process.env.MOZART_WEB_BROWSER_EXECUTABLE || undefined });
  const context = await browser.newContext({ viewport: { width: 1366, height: 900 }, acceptDownloads: true });
  const page = await context.newPage(); page.on('pageerror', error => failures.push(error.message));
  const card = title => page.locator('section').filter({ has: page.getByRole('heading', { name: title, exact: true }) });
  await page.goto(`http://127.0.0.1:${address.port}`);
  await page.getByText('API 在线', { exact: true }).waitFor(); await page.getByText('Hello', { exact: true }).first().waitFor();
  assert.equal(await card('翻译播报').count(), 0);
  assert.equal(await card('播报设置').count(), 0);
  await page.getByLabel('路由', { exact: true }).selectOption('rvc');
  await page.getByText('请求已受理；实际状态以下次查询为准。', { exact: true }).waitFor();
  await card('路由').getByText('切换路由中', { exact: true }).waitFor();
  assert.equal(snapshot.output_mode.requested, 'rvc'); assert.equal(snapshot.output_mode.applied, null);
  assert.equal(writes.filter(item => item.path === '/mode').length, 1);
  assert.equal(await card('播报设置').count(), 0);
  assert.equal(writes.some(item => item.path.startsWith('/preprocessor') || item.path.includes('mode/switch')), false);

  const broadcastMode = page.waitForResponse(res => res.url().endsWith('/api/v1/mode') && res.request().method() === 'PUT');
  await page.getByLabel('路由', { exact: true }).selectOption('broadcast');
  await broadcastMode;
  await card('播报设置').waitFor({ state: 'visible' });
  await card('播报设置').getByRole('button', { name: '清除播报队列', exact: true }).click();
  await page.getByText('已请求清除播报队列。', { exact: true }).waitFor();
  assert.equal(writes.some(item => item.path === '/speech/clear'), true);
  assert.equal(snapshot.bridge.speech_enabled, true);
  assert.equal(await card('播报设置').isVisible(), true);

  await card('路由').locator('.translate-toggle').click();
  await card('播报设置').getByText('翻译已关闭，当前不会触发播报。', { exact: true }).waitFor();
  await card('路由').locator('.translate-toggle').click();
  await page.getByLabel('路由', { exact: true }).selectOption('direct');
  await card('播报设置').waitFor({ state: 'detached' });

  // 调节即应用：被拒绝后回到已确认值；有效值写入后出现保存与恢复按钮。
  await page.getByLabel('音高增益', { exact: true }).fill('13');
  await page.getByText('InvalidArgument：音高超出范围', { exact: true }).waitFor(); assert.equal(snapshot.rvc_params.pitch_shift, 0);
  assert.equal(await page.getByLabel('音高增益', { exact: true }).inputValue(), '0');
  await page.getByLabel('音高增益', { exact: true }).fill('3');
  await page.locator('.preset-actions.is-visible').waitFor();
  assert.equal(await page.getByLabel('音高增益', { exact: true }).inputValue(), '3');
  assert.equal(snapshot.rvc_params.pitch_shift, 3);
  await page.getByRole('button', { name: '保存为 default 预设', exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.preset-actions.is-visible'));
  await page.getByLabel('输入', { exact: true }).selectOption('mic-b'); await card('设备').getByRole('button', { name: '设置输入设备', exact: true }).click();
  await page.getByText('实际：mic-a · 基线：mic-b', { exact: true }).waitFor();
  assert.equal(writes.some(item => item.path === '/services/pre/restart'), true);

  await card('文件转换').locator('input[type=file]').setInputFiles({ name: 'sample.wav', mimeType: 'audio/wav', buffer: wav });
  await page.getByText('已受理 job-new · queued，转换尚未完成。', { exact: true }).waitFor();
  await card('文件转换').getByText('排队中', { exact: true }).waitFor();
  jobs[0] = jobFixture({ job_id: 'job-new', state: 'completed', result_ready: true, progress: { stage: 'publish', percent: 100, blocks_done: 1, blocks_total: 1 } });
  await card('文件转换').getByRole('button', { name: '刷新', exact: true }).click();
  const [download] = await Promise.all([page.waitForEvent('download'), card('文件转换').getByRole('button', { name: '下载', exact: true }).click()]);
  assert.equal(download.suggestedFilename(), 'result.wav'); assert.deepEqual(await readFile(await download.path()), wav);

  subtitleHistory.push({ id: 'browser-1:200', record: subtitleFixture({ revision: 2, en: 'Updated translation', translation_status: 'completed', received_at: live.received_at }) });
  let resumeSubtitles;
  subtitleReconnectGate = new Promise(resolve => { resumeSubtitles = resolve; });
  for (const res of subtitleStreams) { sendEvent(res, 'subtitle', subtitleHistory.at(-1).record, 'browser-1:200'); res.end(); }
  await page.getByText('Updated translation', { exact: true }).first().waitFor();
  assert.equal(await card('SUB').locator('.subtitle-item').count(), 1);
  subtitleHistory.push({ id: 'browser-1:300', record: subtitleFixture({ utterance_id: 'boot-a:2', seq: 2, zh: '断线期间的新句', en: 'Recovered while offline', translation_status: 'completed', received_at: live.received_at }) });
  const reconnected = once(events, 'subtitle-connection', { signal: AbortSignal.timeout(5000) });
  resumeSubtitles(); subtitleReconnectGate = undefined;
  const [resume] = await reconnected; assert.equal(resume.cursor, 'browser-1:200');
  await page.getByText('Recovered while offline', { exact: true }).waitFor();
  await page.waitForFunction(() => document.querySelector('section[aria-label="实时字幕"]')?.getAttribute('data-online') === 'true');
  assert.equal(await page.getByText('推送连接已断开，正在重连', { exact: true }).count(), 0);
  assert.equal(await page.getByText('Updated translation', { exact: true }).count(), 1);
  assert.equal(await card('SUB').locator('.subtitle-item').count(), 2);
  assert.equal(await card('SUB').getByText('Recovered while offline', { exact: true }).count(), 1);
  assert.equal(await card('SUB').getByText('Hello', { exact: true }).count(), 0);
  // 新字幕到达后歌词自动滚到底部。
  assert.equal(await page.evaluate(() => { const box = document.querySelector('.subtitle-scroll'); return box.scrollTop + box.clientHeight >= box.scrollHeight - 1; }), true);
  // INFO 展开后尾随服务日志。
  await page.getByText('INFO', { exact: true }).click();
  await page.getByText('frame=200', { exact: false }).waitFor();
  await page.screenshot({ path: join(evidence, 'desktop.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(evidence, 'mobile.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await page.getByRole('button', { name: '切换亮暗主题' }).click(); assert.equal(await page.locator('html').getAttribute('data-theme'), 'dark');
  await page.screenshot({ path: join(evidence, 'mobile-dark.png'), fullPage: true });

  await page.getByRole('button', { name: '停止系统', exact: true }).click(); await page.getByText('UpstreamUnavailable：daemon 未运行', { exact: true }).first().waitFor();
  assert.equal(await page.getByRole('button', { name: '停止系统', exact: true }).isDisabled(), true);
  await page.getByRole('button', { name: '启动系统', exact: true }).click(); await page.locator('[data-service="pre"][data-state="running"]').waitFor();
  assert.deepEqual(failures, []);
  await writeFile(join(evidence, 'result.json'), JSON.stringify({ mock_api: true, browser: await browser.version(), viewports: ['1366x900', '390x844'], checks: ['route-intent', 'config-rejection', 'draft', 'pending-restart', 'file-queue', 'download-bytes', 'subtitle-reconnect', 'layout', 'theme', 'down-up'], writes, failures }, null, 2));
  console.log(`浏览器契约检查通过；证据：${evidence}`);
} finally {
  await browser?.close();
  for (const res of [...statusStreams, ...subtitleStreams]) res.destroy();
  server.httpServer.closeAllConnections();
  await new Promise((resolve, reject) => server.httpServer.close(error => error ? reject(error) : resolve()));
}
