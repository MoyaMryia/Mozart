// Explicit opt-in test for a dedicated real runtime. It stops and restarts daemon.
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createHash } from 'node:crypto';
import { chromium } from 'playwright-core';
const url = process.env.MOZART_WEB_LIVE_URL;
assert.ok(url && process.env.MOZART_WEB_LIVE_INPUT && process.env.MOZART_WEB_LIVE_AUDIO_GATE && process.env.MOZART_WEB_LIVE_MODEL, 'provide a dedicated runtime URL, model ID, 2 s WAV, and audio injection gate');
const evidence = await mkdtemp(join(tmpdir(), 'mozart-web-live-'));
const input = await readFile(process.env.MOZART_WEB_LIVE_INPUT);
const filename = `web-live-${Date.now()}.wav`;
const executablePath = process.env.MOZART_WEB_BROWSER_EXECUTABLE;
const browser = await chromium.launch({ executablePath, headless: true, args: ['--no-sandbox'] });
const context = await browser.newContext({ viewport: { width: 1366, height: 900 }, acceptDownloads: true });
const page = await context.newPage();
await page.addInitScript(() => {
  window.__liveSubtitles = [];
  const Original = window.EventSource;
  window.EventSource = class extends Original {
    constructor(url, options) { super(url, options); this.addEventListener('subtitle', event => window.__liveSubtitles.push(JSON.parse(event.data))); }
  };
});
const failures = []; page.on('pageerror', error => failures.push(error.message));
const card = title => page.locator('section.card').filter({ has: page.getByRole('heading', { name: title, exact: true }) });
async function waitFor(check, timeout = 90000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { const result = await check(); if (result) return result; await new Promise(resolve => setTimeout(resolve, 100)); }
  throw new Error('live condition timed out');
}
let expectedDaemonPid = null;
async function api(path) {
  const response = await fetch(`${url}/api/v1/${path}`); const body = await response.json();
  assert.equal(body.status, 'ok', JSON.stringify(body));
  if (path === 'status' && expectedDaemonPid !== null) assert.equal(body.data.daemon.pid, expectedDaemonPid, 'the dedicated runtime was replaced during the live test');
  return body.data;
}
async function observeSubtitles() {
  const controller = new AbortController();
  const response = await fetch(`${url}/api/v1/events/subtitles`, { signal: controller.signal });
  assert.equal(response.status, 200);
  const state = { records: new Map(), ready: false, error: null };
  const task = (async () => {
    let pending = ''; const decoder = new TextDecoder();
    for await (const chunk of response.body) {
      pending += decoder.decode(chunk, { stream: true });
      let boundary;
      while ((boundary = pending.indexOf('\n\n')) >= 0) {
        const frame = pending.slice(0, boundary); pending = pending.slice(boundary + 2);
        if (frame.startsWith(':')) { state.ready = true; continue; }
        const name = frame.split('\n').find(line => line.startsWith('event: '))?.slice(7);
        const data = frame.split('\n').find(line => line.startsWith('data: '))?.slice(6);
        if (name === 'reset') state.records.clear();
        if (name === 'error') throw new Error(`observer SSE error: ${data}`);
        if (name === 'subtitle') { const record = JSON.parse(data); state.records.set(record.utterance_id, record); }
      }
    }
    if (!controller.signal.aborted) throw new Error('observer stream ended unexpectedly');
  })().catch(error => { if (!controller.signal.aborted) state.error = error; });
  return { state, check() { if (state.error) throw state.error; }, async close() { controller.abort(); await task; if (state.error) throw state.error; } };
}
function wav(bytes) {
  assert.equal(bytes.toString('ascii', 0, 4), 'RIFF'); assert.equal(bytes.toString('ascii', 8, 12), 'WAVE');
  let format, data;
  for (let offset = 12; offset + 8 <= bytes.length;) {
    const size = bytes.readUInt32LE(offset + 4); const name = bytes.toString('ascii', offset, offset + 4);
    if (name === 'fmt ') format = { codec: bytes.readUInt16LE(offset + 8), channels: bytes.readUInt16LE(offset + 10), sampleRate: bytes.readUInt32LE(offset + 12), bits: bytes.readUInt16LE(offset + 22) };
    if (name === 'data') data = bytes.subarray(offset + 8, offset + 8 + size);
    offset += 8 + size + size % 2;
  }
  assert.ok(format && data); assert.equal(format.codec, 3); assert.equal(format.channels, 1); assert.equal(format.bits, 32); assert.equal(format.sampleRate, 48000);
  assert.equal(data.length / (format.bits / 8 * format.channels), format.sampleRate * 2, 'output must conserve the fixed 2 s input duration');
  let peak = 0;
  for (let offset = 0; offset < data.length; offset += 4) { const sample = data.readFloatLE(offset); assert.ok(Number.isFinite(sample)); peak = Math.max(peak, Math.abs(sample)); }
  assert.ok(peak > 0, 'inference output must not be all zero');
  return { ...format, frames: data.length / 4, bytes: bytes.length, peak };
}
let result, observer;
try {
  await page.goto(url);
  await card('系统').getByText('已连接', { exact: true }).waitFor({ timeout: 30000 });
  const initial = await api('status');
  expectedDaemonPid = initial.daemon.pid;
  assert.equal(initial.daemon.poll_interval_ms, 1000);
  assert.ok(initial.api_capabilities.includes('file_fetch_safe_errors_v1'));
  await waitFor(async () => { const state = await api('status'); return ['pre', 'rvc', 'asr', 'tts', 'bridge', 'llama'].every(name => state.processes[name].state === 'running') && state.asr && state.tts && state.bridge; });
  const mute = card('系统').getByRole('checkbox', { name: '静音', exact: true });
  await mute.click(); await waitFor(() => mute.isChecked());
  await mute.click(); await waitFor(async () => !await mute.isChecked());
  await card('文件转换').locator('select').selectOption(process.env.MOZART_WEB_LIVE_MODEL);
  await card('文件转换').getByLabel('待转换音频').setInputFiles({ name: filename, mimeType: 'audio/wav', buffer: input });
  const submitResponse = page.waitForResponse(response => response.url().endsWith('/api/v1/files') && response.request().method() === 'POST');
  await card('文件转换').getByRole('button', { name: '加入队列', exact: true }).click();
  const accepted = await submitResponse; assert.equal(accepted.status(), 202, await accepted.text());
  const job = card('文件转换').locator('li').filter({ has: page.getByText(filename, { exact: true }) });
  await job.getByText('已完成', { exact: true }).waitFor({ timeout: 90000 });
  const jobs = await api('files'); const submitted = jobs.jobs.find(job => job.name === filename);
  assert.equal(submitted.state, 'completed');
  assert.equal(submitted.model_id, process.env.MOZART_WEB_LIVE_MODEL);
  const downloadPromise = page.waitForEvent('download'); await job.getByRole('button', { name: '下载', exact: true }).click();
  const download = await downloadPromise; await download.saveAs(join(evidence, 'converted.wav'));
  const bytes = await readFile(join(evidence, 'converted.wav')); const output = wav(bytes);
  const fileStatus = await api('status'); assert.equal(fileStatus.rvc.frames_in, 100); assert.equal(fileStatus.rvc.frames_out, 100); assert.equal(fileStatus.rvc.frames_dropped, 0);
  // Enable speech after file-model setup, and verify the provider state rather than a transient DOM value.
  const speech = card('翻译播报').getByRole('checkbox', { name: '播报', exact: true });
  await waitFor(async () => (await api('status')).bridge.speech_enabled === await speech.isChecked());
  if (await speech.isChecked()) {
    await speech.click();
    await waitFor(async () => !(await api('status')).bridge.speech_enabled && !await speech.isChecked());
  }
  const speechResponse = page.waitForResponse(response => response.url().endsWith('/api/v1/speech') && response.request().method() === 'PUT');
  await speech.click();
  assert.equal((await speechResponse).status(), 200);
  await waitFor(async () => (await api('status')).bridge.speech_enabled && await speech.isChecked());
  // Observe the real server independently; old history cannot satisfy recovery.
  observer = await observeSubtitles();
  await waitFor(() => { observer.check(); return observer.state.ready; });
  const knownIds = new Set(observer.state.records.keys());
  const beforeLanguage = await api('status');
  await context.setOffline(true);
  await writeFile(process.env.MOZART_WEB_LIVE_AUDIO_GATE, 'inject\n');
  await new Promise(resolve => setTimeout(resolve, 22000));
  const language = await waitFor(async () => {
    observer.check(); const state = await api('status');
    assert.ok(state.asr && state.tts && state.bridge && state.pre, `language services unavailable: ${JSON.stringify(state.degraded)}`);
    return state.asr.finals > beforeLanguage.asr.finals && state.bridge.translated > beforeLanguage.bridge.translated && state.tts.synthesized > beforeLanguage.tts.synthesized && state.pre.tts_frames_played > beforeLanguage.pre.tts_frames_played ? state : null;
  });
  const expected = [...observer.state.records.values()].filter(record => !knownIds.has(record.utterance_id) && record.final && record.translation_status === 'completed');
  assert.ok(expected.length > 0, 'no new completed subtitles were observed while the browser was offline');
  await context.setOffline(false);
  await waitFor(async () => {
    observer.check(); const received = await page.evaluate(() => window.__liveSubtitles);
    return expected.every(record => received.some(item => item.utterance_id === record.utterance_id && item.revision >= record.revision && item.zh === record.zh && item.en === record.en));
  });
  await waitFor(() => page.evaluate(records => records.every(record => {
    const node = [...document.querySelectorAll('.subtitle-item')].find(item => item.dataset.utteranceId === record.utterance_id);
    return node && Number(node.dataset.revision) >= record.revision && node.querySelector('p').textContent === record.zh && node.querySelector('.subtitle-translation').textContent === record.en;
  }), expected));
  await observer.close(); observer = null;
  await page.screenshot({ path: join(evidence, 'desktop.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(evidence, 'mobile.png'), fullPage: true });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await api('status');
  expectedDaemonPid = null;
  await page.getByRole('button', { name: '停止系统', exact: true }).click();
  await waitFor(async () => !(await card('系统').getByRole('button', { name: '停止系统', exact: true }).isEnabled()));
  assert.equal((await api('health')).adapter, 'running');
  await page.reload(); await card('系统').getByRole('button', { name: '启动系统', exact: true }).waitFor();
  await page.getByRole('button', { name: '启动系统', exact: true }).click();
  await card('系统').getByText('已连接', { exact: true }).waitFor({ timeout: 60000 });
  const restored = await api('status'); assert.notEqual(restored.daemon.pid, initial.daemon.pid);
  assert.deepEqual(failures, []);
  result = { real_api: true, real_inference: true, browser: await browser.version(), job_id: submitted.job_id, model: submitted.model_id, output,
    sha256: createHash('sha256').update(bytes).digest('hex'), language: { finals: language.asr.finals - beforeLanguage.asr.finals, translated: language.bridge.translated - beforeLanguage.bridge.translated, synthesized: language.tts.synthesized - beforeLanguage.tts.synthesized, played: language.pre.tts_frames_played - beforeLanguage.pre.tts_frames_played },
    recovered: expected.map(record => ({ utterance_id: record.utterance_id, revision: record.revision, zh: record.zh, en: record.en })),
    prior_utterances: knownIds.size,
    checks: ['status', 'mute', 'speech-toggle', 'upload', 'rvc-100-frames', 'download', 'real-subtitle-reconnect', 'real-translation-tts', 'layout', 'down-page-reload-up'], failures };
  await writeFile(join(evidence, 'result.json'), JSON.stringify(result, null, 2));
  console.log(`真实服务浏览器联调通过；证据：${evidence}`);
} catch (error) {
  await page.screenshot({ path: join(evidence, 'failure.png'), fullPage: true }).catch(() => {});
  await writeFile(join(evidence, 'failure.txt'), `${error.stack}\npage errors: ${JSON.stringify(failures)}\n`);
  console.error(`真实服务浏览器联调失败；证据：${evidence}`); throw error;
} finally { try { if (observer) await observer.close(); } finally { await browser.close(); } }
