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
const writes = [];
let running = true;
let jobs = [];
let queuePaused = false;
let baselineCapture = 'mic-a';
let subtitleReconnectGate;
const statusStreams = new Set();
const subtitleStreams = new Set();
const events = new EventEmitter();
const subtitleHistory = [{ id: 'browser-1:100', record: subtitleFixture({ en: 'Hello', revision: 1, translation_status: 'completed' }) }];
const wav = Buffer.alloc(46);
wav.write('RIFF'); wav.writeUInt32LE(38, 4); wav.write('WAVEfmt ', 8); wav.writeUInt32LE(16, 16);
wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22); wav.writeUInt32LE(48000, 24);
wav.writeUInt32LE(96000, 28); wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34); wav.write('data', 36); wav.writeUInt32LE(2, 40);

const sendEvent = (res, name, data, id) => res.write(`${id ? `id: ${id}\n` : ''}event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
const broadcastStatus = () => { for (const res of statusStreams) sendEvent(res, 'status', snapshot); };
const response = (res, data, status = 200) => { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(envelope(data))); };
const failure = (res, code, reason, status) => { res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ status: 'error', code, reason, data: null })); };

async function handleApi(req, res) {
  const url = new URL(req.url, 'http://localhost'); const path = url.pathname.slice('/api/v1'.length);
  const method = req.method;
  if (method === 'GET') {
    if (path === '/health') return response(res, healthFixture);
    if (path === '/status') return running ? response(res, snapshot) : failure(res, 'UpstreamUnavailable', 'daemon 未运行', 503);
    if (path === '/models') return response(res, { models: [{ id: 'model-a', exists: true, loaded: true }] });
    if (path === '/devices') return response(res, { devices: [{ id: 'mic-a', label: '麦克风 A', kind: 'capture' }, { id: 'mic-b', label: '麦克风 B', kind: 'capture' }, { id: 'speaker-a', label: '扬声器 A', kind: 'playback' }] });
    if (path === '/config') return response(res, { value: url.searchParams.get('key') === 'pre.capture_device' ? baselineCapture : 'speaker-a' });
    if (path === '/files') return response(res, { jobs, paused: queuePaused });
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
  if (path === '/route') { snapshot.route.desired = body.route; broadcastStatus(); return response(res, { route: body.route }); }
  if (path === '/mute') { snapshot.mute = body.mute; broadcastStatus(); return response(res, { mute: body.mute }); }
  if (path === '/translate') { snapshot.bridge.translate_enabled = body.enabled; broadcastStatus(); return response(res, body); }
  if (path === '/speech') { snapshot.bridge.speech_enabled = body.enabled; broadcastStatus(); return response(res, body); }
  if (path === '/speech/stop') { snapshot.bridge.speech_enabled = false; broadcastStatus(); return response(res, {}); }
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
  await page.getByLabel('路由', { exact: true }).selectOption('rvc');
  await page.getByText('请求已受理；实际状态以下次查询为准。', { exact: true }).waitFor();
  assert.equal(snapshot.route.active, 'direct'); assert.equal(snapshot.route.desired, 'rvc');
  assert.equal(writes.filter(item => item.path === '/route').length, 1);
  assert.equal(writes.some(item => item.path.startsWith('/preprocessor') || item.path.includes('mode/switch')), false);

  await page.getByLabel('音高', { exact: true }).fill('13'); await card('参数').getByRole('button', { name: '应用', exact: true }).first().click();
  await page.getByText('InvalidArgument：音高超出范围', { exact: true }).waitFor(); assert.equal(snapshot.rvc_params.pitch_shift, 0);
  await page.getByLabel('音高', { exact: true }).fill('3');
  // 较新的 SSE 会合并首轮读取；等待实际发生的后台查询，不用固定睡眠代替。
  await page.waitForResponse(response => new URL(response.url()).pathname === '/api/v1/status', { timeout: 10000 });
  await page.waitForFunction(() => document.querySelector('#param-pitch_shift')?.disabled === false);
  assert.equal(await page.getByLabel('音高', { exact: true }).inputValue(), '3');
  assert.equal(snapshot.rvc_params.pitch_shift, 0);
  await card('参数').getByRole('button', { name: '应用', exact: true }).first().click();
  await page.getByText('当前：3', { exact: true }).waitFor();
  await page.getByLabel('输入', { exact: true }).selectOption('mic-b'); await card('设备').getByRole('button', { name: '设置', exact: true }).first().click();
  await page.getByText('实际：mic-a · 基线：mic-b', { exact: true }).waitFor();
  assert.equal(writes.some(item => item.path.endsWith('/restart')), false);

  await card('文件转换').locator('input[type=file]').setInputFiles({ name: 'sample.wav', mimeType: 'audio/wav', buffer: wav });
  await card('文件转换').getByRole('button', { name: '加入队列', exact: true }).click();
  await page.getByText('已受理 job-new · queued，转换尚未完成。', { exact: true }).waitFor();
  await card('文件转换').getByText('排队中', { exact: true }).waitFor();
  jobs[0] = jobFixture({ job_id: 'job-new', state: 'completed', result_ready: true, progress: { stage: 'publish', percent: 100, blocks_done: 1, blocks_total: 1 } });
  await card('文件转换').getByRole('button', { name: '刷新', exact: true }).click();
  const [download] = await Promise.all([page.waitForEvent('download'), card('文件转换').getByRole('button', { name: '下载', exact: true }).click()]);
  assert.equal(download.suggestedFilename(), 'result.wav'); assert.deepEqual(await readFile(await download.path()), wav);

  subtitleHistory.push({ id: 'browser-1:200', record: subtitleFixture({ revision: 2, en: 'Updated translation', translation_status: 'completed' }) });
  let resumeSubtitles;
  subtitleReconnectGate = new Promise(resolve => { resumeSubtitles = resolve; });
  for (const res of subtitleStreams) { sendEvent(res, 'subtitle', subtitleHistory.at(-1).record, 'browser-1:200'); res.end(); }
  await page.getByText('Updated translation', { exact: true }).first().waitFor();
  assert.equal(await card('字幕').locator('.subtitle-item').count(), 1);
  subtitleHistory.push({ id: 'browser-1:300', record: subtitleFixture({ utterance_id: 'boot-a:2', seq: 2, zh: '断线期间的新句', en: 'Recovered while offline', translation_status: 'completed' }) });
  const reconnected = once(events, 'subtitle-connection', { signal: AbortSignal.timeout(5000) });
  resumeSubtitles(); subtitleReconnectGate = undefined;
  const [resume] = await reconnected; assert.equal(resume.cursor, 'browser-1:200');
  await page.getByText('Recovered while offline', { exact: true }).waitFor();
  await card('字幕').getByText('已连接', { exact: true }).waitFor();
  assert.equal(await card('字幕').getByText('推送连接已断开，正在重连', { exact: true }).count(), 0);
  assert.equal(await page.getByText('Updated translation', { exact: true }).count(), 1);
  assert.equal(await card('字幕').locator('.subtitle-item').count(), 2);
  assert.equal(await card('字幕').getByText('Hello', { exact: true }).count(), 0);
  await page.screenshot({ path: join(evidence, 'desktop.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(evidence, 'mobile.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await page.getByRole('button', { name: '切换亮暗主题' }).click(); assert.equal(await page.locator('html').getAttribute('data-theme'), 'dark');
  await page.screenshot({ path: join(evidence, 'mobile-dark.png'), fullPage: true });

  await page.getByRole('button', { name: '停止系统', exact: true }).click(); await page.getByText('UpstreamUnavailable：daemon 未运行', { exact: true }).first().waitFor();
  assert.equal(await page.getByRole('button', { name: '停止系统', exact: true }).isDisabled(), true);
  await page.getByRole('button', { name: '启动系统', exact: true }).click(); await card('系统').getByText('已连接', { exact: true }).waitFor();
  assert.deepEqual(failures, []);
  await writeFile(join(evidence, 'result.json'), JSON.stringify({ mock_api: true, browser: await browser.version(), viewports: ['1366x900', '390x844'], checks: ['route-intent', 'config-rejection', 'draft', 'pending-restart', 'file-queue', 'download-bytes', 'subtitle-reconnect', 'layout', 'theme', 'down-up'], writes, failures }, null, 2));
  console.log(`浏览器契约检查通过；证据：${evidence}`);
} finally {
  await browser?.close();
  for (const res of [...statusStreams, ...subtitleStreams]) res.destroy();
  server.httpServer.closeAllConnections();
  await new Promise((resolve, reject) => server.httpServer.close(error => error ? reject(error) : resolve()));
}
