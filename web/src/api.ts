import { z } from 'zod';
import {
  configEffectSchema, configSchema, deviceListSchema, envelopeSchema, fileListSchema,
  healthSchema, lifecycleSchema, modelListSchema, outputModeStatusSchema, statusSchema, submitSchema, paramsSchema,
  type OutputMode, type Route, type Service,
} from './contracts';

export const API_ROOT = '/api/v1';
let deviceClock: number | null = null;
/** 最近一次响应时设备时钟的秒数，取自 Date 头；响应不带该头时为 null。 */
export const deviceNow = () => deviceClock;
const objectSchema = z.object({}).passthrough();

export class ApiError extends Error {
  constructor(message: string, public code: string, public httpStatus = 0, public resultUnknown = false) {
    super(message);
    this.name = 'ApiError';
  }
}

export function parsePayload<T>(schema: z.ZodType<T>, value: unknown): T {
  const parsed = schema.safeParse(value);
  if (!parsed.success) throw new ApiError(`响应结构不符合接口契约：${parsed.error.issues.map(issue => issue.path.join('.')).join('、')}`, 'InvalidResponse');
  return parsed.data;
}

function isTransportFailure(error: unknown): error is TypeError | DOMException {
  return error instanceof TypeError || (error instanceof DOMException && error.name === 'AbortError');
}

async function readEnvelope(response: Response, write: boolean) {
  if (response.headers.get('Content-Type')?.split(';')[0].trim() !== 'application/json') {
    throw new ApiError(`服务没有返回 JSON 响应（HTTP ${response.status}）`, 'InvalidResponse', response.status, write);
  }
  let value: unknown;
  try { value = await response.json(); }
  catch (error) {
    if (error instanceof SyntaxError) throw new ApiError(`服务没有返回有效 JSON（HTTP ${response.status}）`, 'InvalidResponse', response.status, write);
    if (isTransportFailure(error)) throw new ApiError('响应传输已中断', 'NetworkUnavailable', response.status, write);
    throw error;
  }
  let envelope;
  try { envelope = parsePayload(envelopeSchema, value); }
  catch (error) {
    if (!(error instanceof ApiError)) throw error;
    throw new ApiError(error.message, error.code, response.status, write);
  }
  if (!response.ok && envelope.status === 'ok') {
    throw new ApiError('HTTP 状态与响应信封不一致', 'InvalidResponse', response.status, write);
  }
  if (envelope.status === 'error') {
    throw new ApiError(envelope.reason, envelope.code, response.status,
      write && (response.status >= 502 || envelope.code === 'GatewayTimeout'));
  }
  return envelope.data;
}

async function send(path: string, init: RequestInit) {
  const write = init.method !== undefined && init.method !== 'GET';
  let response: Response;
  try { response = await fetch(`${API_ROOT}${path}`, { ...init, cache: 'no-store' }); }
  catch (error) {
    if (!isTransportFailure(error)) throw error;
    throw new ApiError(error.name === 'AbortError' ? '请求已中断' : '无法连接 HTTP 适配器', 'NetworkUnavailable', 0, write);
  }
  const date = Date.parse(response.headers.get('date') ?? '');
  if (!Number.isNaN(date)) deviceClock = date / 1000;
  return response;
}

export async function request<T>(path: string, schema: z.ZodType<T>, init: RequestInit = {}): Promise<T> {
  const write = init.method !== undefined && init.method !== 'GET';
  const response = await send(path, init);
  const payload = await readEnvelope(response, write);
  try { return parsePayload(schema, payload); }
  catch (error) {
    if (!(error instanceof ApiError)) throw error;
    throw new ApiError(error.message, error.code, response.status, write);
  }
}

const json = (method: string, value: unknown): RequestInit => ({
  method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(value),
});

export const client = {
  health: (signal?: AbortSignal) => request('/health', healthSchema, { signal }),
  status: (signal?: AbortSignal) => request('/status', statusSchema, { signal }),
  models: (signal?: AbortSignal) => request('/models', modelListSchema, { signal }),
  preset: (signal?: AbortSignal) => request('/preset', z.object({ model_id: z.string(), preset: z.literal('default'), preset_values: paramsSchema, live_values: paramsSchema, dirty: z.boolean() }), { signal }),
  useModel: (model_id: string) => request('/model', z.object({ model_id: z.string(), previous_model_id: z.string().nullable() }), json('PUT', { model_id })),
  savePreset: () => request('/preset/save', z.object({ model_id: z.string(), preset: z.literal('default'), values: paramsSchema }), json('POST', {})),
  usePreset: () => request('/preset/use', z.object({ model_id: z.string(), preset: z.literal('default'), values: paramsSchema }), json('POST', {})),
  devices: (signal?: AbortSignal) => request('/devices', deviceListSchema, { signal }),
  files: (signal?: AbortSignal) => request('/files', fileListSchema, { signal }),
  config: (key?: string) => request(`/config${key ? `?key=${encodeURIComponent(key)}` : ''}`, configSchema),
  deviceConfig: (key: 'pre.capture_device' | 'pre.playback_device') => request(`/config?key=${key}`, z.object({ value: z.string() })),
  logTail: (name: string) => request(`/logs?name=${encodeURIComponent(name)}`, z.object({ name: z.string(), tail: z.string() })),
  up: () => request('/system/up', lifecycleSchema, json('POST', {})),
  down: () => request('/system/down', lifecycleSchema, json('POST', {})),
  route: (route: Route) => request('/route', objectSchema, json('PUT', { route })),
  mode: (mode: OutputMode) => request('/mode', outputModeStatusSchema, json('PUT', { mode })),
  mute: (mute: boolean) => request('/mute', objectSchema, json('PUT', { mute })),
  translate: (enabled: boolean) => request('/translate', objectSchema, json('PUT', { enabled })),
  speech: (enabled: boolean) => request('/speech', objectSchema, json('PUT', { enabled })),
  speechStop: () => request('/speech/stop', objectSchema, json('POST', {})),
  speechClear: () => request('/speech/clear', objectSchema, json('POST', {})),
  restart: (service: Service) => request(`/services/${service}/restart`, objectSchema, json('POST', {})),
  setConfig: (key: string, value: string) => request('/config', configEffectSchema, json('PUT', { key, value })),
  setMicMixer: (payload: { volume_percent?: number; agc_enabled?: boolean }) =>
    request('/devices/capture-mixer', objectSchema, json('PUT', payload)),
  submit: (file: File, modelId?: string) => {
    const body = new FormData();
    body.append('file', file);
    if (modelId !== undefined) body.append('model_id', modelId);
    return request('/files', submitSchema, { method: 'POST', body });
  },
  cancel: (jobId: string) => request(`/files/${encodeURIComponent(jobId)}/cancel`, objectSchema, json('POST', {})),
  pause: (paused: boolean) => request(`/files/${paused ? 'pause' : 'resume'}`, objectSchema, json('POST', {})),
};

export async function downloadResult(jobId: string) {
  const response = await send(`/files/${encodeURIComponent(jobId)}/result`, {});
  if (!response.ok) { await readEnvelope(response, false); }
  if (response.headers.get('Content-Type')?.split(';')[0].trim() !== 'audio/wav') {
    throw new ApiError('下载响应不是 WAV 文件', 'InvalidResponse', response.status);
  }
  const disposition = response.headers.get('Content-Disposition') ?? '';
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(disposition)?.[1];
  let name = /filename="([^"\r\n]+)"/i.exec(disposition)?.[1] ?? `${jobId}.wav`;
  if (encoded) {
    try { name = decodeURIComponent(encoded); }
    catch (error) {
      if (!(error instanceof URIError)) throw error;
      throw new ApiError('下载文件名编码无效', 'InvalidResponse', response.status);
    }
  }
  try { return { blob: await response.blob(), name }; }
  catch (error) {
    if (!isTransportFailure(error)) throw error;
    throw new ApiError('WAV 下载传输中断', 'NetworkUnavailable', response.status);
  }
}
