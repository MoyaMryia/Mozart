import { computed, onMounted, onUnmounted, ref } from 'vue';
import { ApiError, client, downloadResult } from './api';
import { statusSchema, subtitleSchema, type ConnectionState, type DeviceEntry, type FileJob, type Health, type ModelEntry, type Status } from './contracts';
import { SubtitleHistory, subscribeStream } from './streams';
import type { SubtitleRecord } from './contracts';

const REFRESH_MS = 3000;
export type ActionArea = 'system' | 'files' | 'devices' | 'params' | 'speech';

export function useConsole() {
  const status = ref<Status | null>(null);
  const health = ref<Health | null>(null);
  const adapterOnline = ref(false);
  const statusFresh = ref(false);
  const statusReadFresh = ref(false);
  const jobs = ref<FileJob[]>([]);
  const paused = ref(false);
  const models = ref<ModelEntry[]>([]);
  const preset = ref<Awaited<ReturnType<typeof client.preset>> | null>(null);
  const devices = ref<DeviceEntry[]>([]);
  const records = ref<SubtitleRecord[]>([]);
  const subtitleHistory = new SubtitleHistory();
  const unknowns = ref<Record<ActionArea, string[]>>({ system: [], files: [], devices: [], params: [], speech: [] });
  const busy = ref(false);
  const notice = ref('');
  const errors = ref<Record<ActionArea | 'connection' | 'status' | 'models' | 'preset' | 'deviceList' | 'fileList' | 'captureBaseline' | 'playbackBaseline', string>>({
    system: '', files: '', devices: '', params: '', speech: '', connection: '', status: '', models: '', preset: '', deviceList: '', fileList: '', captureBaseline: '', playbackBaseline: '',
  });
  const subtitleState = ref<ConnectionState>('connecting');
  const statusStreamState = ref<ConnectionState>('connecting');
  const subtitleError = ref('');
  const statusStreamError = ref('');
  const filesFresh = ref(false);
  const devicesFresh = ref(false);
  const uploadGeneration = ref(0);
  const baselines = ref<Partial<Record<'capture' | 'playback', string>>>({});
  const ready = computed(() => adapterOnline.value && statusFresh.value);
  const downloadSupported = computed(() => status.value?.api_capabilities.includes('file_fetch_safe_errors_v1') ?? false);
  let disposed = false;
  let statusRevision = 0;
  let filesRevision = 0;
  let baselineRevision = 0;
  const baselineFresh = ref({ capture: false, playback: false });
  const requiredBaselines = ref<Array<'capture' | 'playback'>>([]);
  let lastStatusAt = 0;
  let inFlight: Promise<void> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopStatus: (() => void) | undefined;
  let stopSubtitles: (() => void) | undefined;
  const reads = new AbortController();

  const acceptStatus = (value: Status) => {
    status.value = value;
    statusFresh.value = true;
    adapterOnline.value = true;
    errors.value.status = '';
    statusRevision++;
    lastStatusAt = Date.now();
  };
  const describe = (error: ApiError) => `${error.code}：${error.message}${error.resultUnknown ? '；执行结果未知，请先刷新状态，不要重复提交。' : ''}`;

  async function read<T>(load: () => Promise<T>, accept: (value: T) => void, area: keyof typeof errors.value) {
    try {
      const value = await load();
      if (disposed) return;
      accept(value);
      errors.value[area] = '';
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      if (!disposed) errors.value[area] = describe(error);
    }
  }

  function refresh(resources = false): Promise<void> {
    if (inFlight) return inFlight;
    inFlight = (async () => {
      try {
        const value = await client.health(reads.signal);
        if (disposed) return;
        health.value = value;
        adapterOnline.value = true;
        errors.value.connection = '';
      } catch (error) {
        if (!(error instanceof ApiError)) throw error;
        if (disposed) return;
        adapterOnline.value = false;
        statusFresh.value = false;
        filesFresh.value = false;
        devicesFresh.value = false;
        health.value = null;
        errors.value.connection = describe(error);
        return;
      }
      const revision = statusRevision;
      if (resources || statusStreamState.value !== 'online' || !statusFresh.value || Date.now() - lastStatusAt >= REFRESH_MS) {
        statusFresh.value = false;
        try {
          const value = await client.status(reads.signal);
          if (!disposed && statusRevision === revision) {
            acceptStatus(value);
            statusReadFresh.value = true;
          }
        } catch (error) {
          if (!(error instanceof ApiError)) throw error;
          if (!disposed && statusRevision === revision) {
            errors.value.status = describe(error);
            statusFresh.value = false;
          }
        }
      }
      const fileRevision = filesRevision;
      try {
        const value = await client.files(reads.signal);
        if (!disposed && fileRevision === filesRevision) {
          jobs.value = value.jobs;
          paused.value = value.paused;
          filesFresh.value = true;
          errors.value.fileList = '';
        }
      } catch (error) {
        if (!(error instanceof ApiError)) throw error;
        if (!disposed && fileRevision === filesRevision) {
          errors.value.fileList = describe(error);
          filesFresh.value = false;
        }
      }
      if (resources) {
        const revision = baselineRevision;
        const readBaseline = async (key: 'capture' | 'playback') => {
          const area = key === 'capture' ? 'captureBaseline' : 'playbackBaseline';
          try {
            const value = await client.deviceConfig(`pre.${key}_device`);
            if (!disposed && revision === baselineRevision) {
              baselines.value[key] = value.value;
              baselineFresh.value[key] = true;
              errors.value[area] = '';
            }
          } catch (error) {
            if (!(error instanceof ApiError)) throw error;
            if (!disposed && revision === baselineRevision) {
              baselineFresh.value[key] = false;
              errors.value[area] = describe(error);
            }
          }
        };
        await Promise.all([
          read(() => client.models(reads.signal), value => { models.value = value.models; }, 'models'),
          read(() => client.preset(reads.signal), value => { preset.value = value; }, 'preset'),
          read(() => client.devices(reads.signal), value => { devices.value = value.devices; }, 'deviceList'),
          readBaseline('capture'),
          readBaseline('playback'),
        ]);
        devicesFresh.value = !errors.value.deviceList;
      }
    })().finally(() => { inFlight = undefined; });
    return inFlight;
  }

  // 写入后的刷新必须在当前在途读取结束后重新查询，不能复用写入前的快照。
  async function reload() {
    if (inFlight) await inFlight;
    await refresh(true);
  }

  async function perform(area: ActionArea, operation: () => Promise<unknown>, message = '请求已受理；实际状态以下次查询为准。', context = '本次操作', baselineKey?: 'capture' | 'playback') {
    if (busy.value || unknowns.value[area].length) return false;
    busy.value = true;
    notice.value = '';
    errors.value[area] = '';
    try {
      await operation();
      if (disposed) return false;
      if (message) notice.value = message;
      await reload();
      return true;
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      if (!disposed) {
        if (error.resultUnknown) {
          unknowns.value[area].push(`${context}：${describe(error)}`);
          if (area === 'files') { filesRevision++; filesFresh.value = false; }
          else {
            statusRevision++;
            statusFresh.value = false;
            statusReadFresh.value = false;
            if (area === 'devices') {
              baselineRevision++;
              baselineFresh.value = { capture: false, playback: false };
              requiredBaselines.value = baselineKey ? [baselineKey] : ['capture', 'playback'];
            }
          }
        } else errors.value[area] = describe(error);
      }
      return false;
    } finally { busy.value = false; }
  }

  async function setConfig(key: string, value: string, area: 'devices' | 'params') {
    await perform(area, async () => {
      const effect = await client.setConfig(key, value);
      if (key === 'pre.capture_device') baselines.value.capture = value;
      if (key === 'pre.playback_device') baselines.value.playback = value;
      notice.value = effect.restart_required ? `已写入基线，待重启：${effect.affected.join('、')}` : '基线已写入；文件任务使用各自的参数快照。';
    }, '', '本次操作', key === 'pre.capture_device' ? 'capture' : key === 'pre.playback_device' ? 'playback' : undefined);
  }

  async function submit(file: File, modelId?: string) {
    const accepted = await perform('files', async () => {
      const result = await client.submit(file, modelId);
      notice.value = `已受理 ${result.job_id} · ${result.state}，转换尚未完成。`;
    }, '', `提交 ${file.name}`);
    if (accepted) uploadGeneration.value++;
  }

  async function download(jobId: string) {
    if (busy.value) return;
    try {
      const { blob, name } = await downloadResult(jobId);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url; anchor.download = name; anchor.click();
      setTimeout(() => URL.revokeObjectURL(url), 0);
      notice.value = '下载已交给浏览器。';
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      errors.value.files = describe(error);
    }
  }

  async function tick() {
    await refresh();
    if (!disposed) timer = setTimeout(() => void tick(), REFRESH_MS);
  }
  onMounted(() => {
    void refresh(true).then(() => { if (!disposed) timer = setTimeout(() => void tick(), REFRESH_MS); });
    stopStatus = subscribeStream({
      path: '/events/status', event: 'status', schema: statusSchema, accept: acceptStatus,
      state: state => { statusStreamState.value = state; if (state === 'offline') statusFresh.value = false; },
      error: reason => { statusStreamError.value = reason; },
    });
    stopSubtitles = subscribeStream({
      path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema,
      accept: value => { records.value = subtitleHistory.accept(value); },
      reset: () => { subtitleHistory.clear(); records.value = []; },
      state: state => { subtitleState.value = state; }, error: reason => { subtitleError.value = reason; },
    });
  });
  onUnmounted(() => {
    disposed = true;
    clearTimeout(timer);
    reads.abort();
    stopStatus?.(); stopSubtitles?.();
  });

  function canAcknowledge(area: ActionArea) {
    if (busy.value || !unknowns.value[area].length) return false;
    if (area === 'files') return filesFresh.value && ready.value;
    return statusReadFresh.value && ready.value && (area !== 'devices' || requiredBaselines.value.every(key => baselineFresh.value[key]));
  }

  return {
    status, health, adapterOnline, ready, jobs, paused, models, preset, devices, records, busy, errors, notice,
    subtitleState, subtitleError, statusStreamState, statusStreamError, downloadSupported,
    uploadGeneration, baselines, filesFresh, devicesFresh, unknowns,
    canAcknowledge,
    acknowledge: (area: ActionArea) => { if (canAcknowledge(area)) unknowns.value[area] = []; },
    reload, perform, setConfig, submit, download,
  };
}
