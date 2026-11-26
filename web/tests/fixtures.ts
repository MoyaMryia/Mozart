import type { Status, SubtitleRecord, FileJob } from '../src/contracts';

export function statusFixture(): Status {
  const process = () => ({ state: 'running' as const, pid: 1, restarts: 0, last_error: null });
  return {
    daemon: { pid: 100, version: '0.1.0', uptime_s: 10, poll_interval_ms: 1000 },
    route: { desired: 'direct', active: 'direct' },
    output_mode: { requested: 'direct', applied: 'direct', phase: 'applied', last_error: null },
    mute: false, arbiter: { gpu: { owner: null } },
    processes: { pre: process(), rvc: process(), asr: process(), tts: process(), bridge: process(), llama: process() },
    pre: { capture_device: 'mic-a', playback_device: 'speaker-a', speaker_state: 'realtime', last_error: null },
    rvc: { state: 'idle', model_id: 'model-a', frames_in: 0, frames_out: 0, frames_dropped: 0, inference_ms_avg: null, inference_ms_p95: null, last_error: null },
    rvc_params: { pitch_shift: 0, index_rate: 0, rms_mix_rate: 1, protect: 0.33, f0_method: 'rmvpe' },
    bridge: { translate_enabled: true, speech_enabled: false, llama_connected: true, tts_connected: true, translated: 1, translate_failed: 0, translate_queue_depth: 0, speech_queue_depth: 0, degraded: [] },
    hw: { cpu_percent: 12, memory: { total_bytes: 8589934592, used_bytes: 2147483648 }, gpu: { available: true, load_percent: 0 } },
    degraded: [], pending_restart: [], pending_restart_keys: [], api_capabilities: ['file_fetch_safe_errors_v1'], file_limits: { max_upload_bytes: 104857600 },
  };
}
export const healthFixture = { adapter: 'running', uploads: { active: 0, retained: 0, reasons: [] } };
export function subtitleFixture(overrides: Partial<SubtitleRecord> = {}): SubtitleRecord {
  return { utterance_id: 'boot-a:1', seq: 1, revision: 0, zh: '你好', en: '', final: true, translation_status: 'pending',
    received_at: 1, ts: '12:00:00', asr_engine: 'zipformer', online_text: '你好', final_decode_ms: 8, ...overrides };
}
export function jobFixture(overrides: Partial<FileJob> = {}): FileJob {
  return { job_id: 'job-1', name: 'sample.wav', model_id: 'model-a', state: 'queued',
    progress: { stage: 'decode', percent: 0, blocks_done: null, blocks_total: null }, error: null, result_ready: false, created_unix_ms: 1, ...overrides };
}
export const envelope = (data: unknown) => ({ status: 'ok', code: 'Ok', reason: '', data });
export const jsonResponse = (data: unknown, status = 200) => new Response(JSON.stringify(envelope(data)), { status, headers: { 'Content-Type': 'application/json' } });
export const errorResponse = (code: string, reason: string, status: number) => new Response(JSON.stringify({ status: 'error', code, reason, data: null }), { status, headers: { 'Content-Type': 'application/json' } });
