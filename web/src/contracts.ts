import { z } from 'zod';

// 镜像 core/src/ctl/schema.rs 中 UI 消费的字段；只在 HTTP/SSE 入口解析。
// passthrough 保留上游扩展，未消费的字段不在组件里重复校验。
export const services = ['enhance', 'pre', 'rvc', 'asr', 'tts', 'bridge', 'llama'] as const;
export type Service = typeof services[number];
export const routeSchema = z.enum(['direct', 'rvc']);
export type Route = z.infer<typeof routeSchema>;
export const outputModeSchema = z.enum(['direct', 'rvc', 'broadcast']);
export type OutputMode = z.infer<typeof outputModeSchema>;
export const outputPhaseSchema = z.enum([
  'closing_speech',
  'configuring_playout',
  'routing',
  'opening_speech',
  'reattaching_playout',
  'applied',
]);
export type OutputPhase = z.infer<typeof outputPhaseSchema>;
export const outputModeStatusSchema = z.object({
  requested: outputModeSchema,
  applied: outputModeSchema.nullable(),
  phase: outputPhaseSchema,
  last_error: z.string().nullable(),
}).passthrough();
export type OutputModeStatus = z.infer<typeof outputModeStatusSchema>;
export type ConnectionState = 'connecting' | 'online' | 'offline';

const processSchema = z.object({
  state: z.enum(['stopped', 'starting', 'running', 'restarting']),
  pid: z.number().nullable(), restarts: z.number(), last_error: z.string().nullable(),
}).passthrough();
export const paramsSchema = z.object({
  pitch_shift: z.number(), index_rate: z.number(), rms_mix_rate: z.number(),
  protect: z.number(), f0_method: z.string(),
}).passthrough();
export type RvcParams = z.infer<typeof paramsSchema>;
export const statusSchema = z.object({
  daemon: z.object({ pid: z.number(), version: z.string(), uptime_s: z.number(), poll_interval_ms: z.number().optional() }).passthrough(),
  route: z.object({ desired: routeSchema, active: routeSchema }).passthrough(),
  output_mode: outputModeStatusSchema.nullable().optional().default(null),
  mute: z.boolean(),
  arbiter: z.object({ gpu: z.object({ owner: z.enum(['realtime', 'file']).nullable() }).passthrough() }).passthrough(),
  processes: z.object({ enhance: processSchema.optional(), pre: processSchema, rvc: processSchema, asr: processSchema, tts: processSchema, bridge: processSchema, llama: processSchema }).passthrough(),
  enhance: z.object({ backend: z.string(), frames_in: z.number(), frames_out: z.number(), sessions: z.number(), algorithm_delay_samples: z.number(), last_error: z.string().nullable() }).passthrough().nullable().optional(),
  pre: z.object({
    capture_device: z.string(),
    playback_device: z.string(),
    speaker_state: z.string(),
    last_error: z.string().nullable(),
    aec: z.object({
      enabled: z.boolean(),
      backend: z.string(),
      active: z.boolean(),
      delay_ms: z.number().nullable(),
      erle_db: z.number().nullable(),
      ref_dropped: z.number(),
      ref_underruns: z.number(),
      resets: z.number(),
      capture_clips: z.number(),
      last_error: z.string().nullable(),
    }).passthrough().nullable().optional(),
  }).passthrough().nullable(),
  rvc: z.object({
    state: z.string(), model_id: z.string().nullable(), frames_in: z.number(), frames_out: z.number(),
    frames_dropped: z.number(), inference_ms_avg: z.number().nullable(), inference_ms_p95: z.number().nullable(),
    last_error: z.string().nullable(),
  }).passthrough().nullable(),
  rvc_params: paramsSchema.nullable().optional(),
  bridge: z.object({
    translate_enabled: z.boolean(), speech_enabled: z.boolean(), llama_connected: z.boolean(),
    tts_connected: z.boolean(), translated: z.number(), translate_failed: z.number(),
    translate_queue_depth: z.number(), speech_queue_depth: z.number(), degraded: z.array(z.string()),
  }).passthrough().nullable().optional(),
  hw: z.object({
    cpu_percent: z.number().nullable(), memory: z.object({ total_bytes: z.number(), used_bytes: z.number() }).passthrough(),
    gpu: z.object({ available: z.boolean(), load_percent: z.number().nullable() }).passthrough(),
  }).passthrough().nullable(),
  degraded: z.array(z.string()), pending_restart: z.array(z.string()).optional().default([]),
  pending_restart_keys: z.array(z.string()).optional().default([]),
  file_limits: z.object({ max_upload_bytes: z.number() }).passthrough().optional(),
  api_capabilities: z.array(z.string()).optional().default([]),
}).passthrough();
export type Status = z.infer<typeof statusSchema>;

export const modelSchema = z.object({ id: z.string(), exists: z.boolean(), loaded: z.boolean() }).passthrough();
export type ModelEntry = z.infer<typeof modelSchema>;
export const deviceSchema = z.object({ id: z.string(), label: z.string(), kind: z.enum(['capture', 'playback']) }).passthrough();
export type DeviceEntry = z.infer<typeof deviceSchema>;
export const fileJobSchema = z.object({
  job_id: z.string(), name: z.string(), model_id: z.string(),
  state: z.enum(['queued', 'processing', 'cancelling', 'completed', 'failed', 'cancelled']),
  progress: z.object({ stage: z.string(), percent: z.number(), blocks_done: z.number().nullable(), blocks_total: z.number().nullable() }).passthrough(),
  error: z.string().nullable(), result_ready: z.boolean(), created_unix_ms: z.number(),
}).passthrough();
export type FileJob = z.infer<typeof fileJobSchema>;
export const subtitleSchema = z.object({
  utterance_id: z.string().regex(/^.+:\d+$/), seq: z.number().int().nonnegative(), revision: z.number().int().nonnegative(),
  zh: z.string(), en: z.string(), final: z.boolean(),
  translation_status: z.enum(['recognizing', 'pending', 'completed', 'failed', 'skipped']),
  received_at: z.number(), ts: z.string(), asr_engine: z.string(), online_text: z.string(), final_decode_ms: z.number(),
  translation_error: z.string().optional(), refinement_status: z.string().optional(),
}).passthrough();
export type SubtitleRecord = z.infer<typeof subtitleSchema>;

export const healthSchema = z.object({
  adapter: z.literal('running'),
  uploads: z.object({ active: z.number(), retained: z.number(), reasons: z.array(z.string()) }).passthrough(),
}).passthrough();
export type Health = z.infer<typeof healthSchema>;
export const configEffectSchema = z.object({ key: z.string(), value: z.string(), restart_required: z.boolean(), affected: z.array(z.string()) }).passthrough();
export type ConfigEffect = z.infer<typeof configEffectSchema>;
export const configSchema = z.object({ value: z.unknown() }).passthrough();
export const fileListSchema = z.object({ jobs: z.array(fileJobSchema), paused: z.boolean() }).passthrough();
export const modelListSchema = z.object({ models: z.array(modelSchema) }).passthrough();
export const deviceListSchema = z.object({ devices: z.array(deviceSchema) }).passthrough();
export const submitSchema = z.object({ job_id: z.string(), state: fileJobSchema.shape.state, queue_position: z.number() }).passthrough();
export const lifecycleSchema = z.object({ state: z.enum(['running', 'already-running', 'stopped', 'not-running']) }).passthrough();
export const envelopeSchema = z.discriminatedUnion('status', [
  z.object({ status: z.literal('ok'), code: z.literal('Ok'), reason: z.literal(''), data: z.unknown() }),
  z.object({ status: z.literal('error'), code: z.string().min(1), reason: z.string().min(1), data: z.null() }),
]);
