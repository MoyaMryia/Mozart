import { computed, onMounted, onUnmounted, ref, type Ref } from 'vue';
import { ApiError, client, deviceNow, downloadResult } from './api';
import { statusSchema, subtitleSchema, type ConnectionState, type DeviceEntry, type FileJob, type Health, type ModelEntry, type Service, type Status } from './contracts';
import { SubtitleStore, subscribeStream } from './streams';
import type { SubtitleRecord } from './contracts';

const REFRESH_MS = 3000;
const NOTICE_MS = 5000;
/** 打开页面前这段时间内收到的字幕仍算当前内容，保留正在说的那句。 */
const LIVE_CONTEXT_S = 10;
/** 修订在句子说出后不久到达（桥的复核期限为 30 s）；更早的句子不会再有修订，不必记住。 */
const REVISION_WINDOW_S = 60;
export type ActionArea = 'system' | 'files' | 'devices' | 'params' | 'speech';
/** 写锁的粒度：同一控件的写入串行，其余控件照常可用；区域之间的冲突由 daemon 仲裁。 */
export type Lock = ActionArea | 'mute' | 'route' | 'model' | 'lifecycle';
/** 每个写锁归属的结果区域：未知结果按区域锁定。 */
const AREA_OF_LOCK: Record<Lock, ActionArea> = { system: 'system', files: 'files', devices: 'devices', params: 'params', speech: 'speech', mute: 'system', route: 'system', model: 'system', lifecycle: 'system' };
export type ErrorKey = Lock | 'connection' | 'status' | 'models' | 'preset' | 'deviceList' | 'fileList' | 'captureBaseline' | 'playbackBaseline' | 'theme';
/** 状态与健康之外，按需重读的数据。 */
type Resource = 'files' | 'models' | 'preset' | 'devices' | 'baselines';
/**
 * poll：定时补查，状态过期才重读。
 * write：写入后确认结果；总是经 HTTP 重读状态并保留当前显示，同一控件的重复提交由写锁拦住。
 * full：手动刷新；先作废当前状态再重读，并重读全部资源。
 */
type Mode = 'poll' | 'write' | 'full';
interface Scope { mode: Mode; resources: readonly Resource[] }
const POLL: Scope = { mode: 'poll', resources: ['files'] };
const FULL: Scope = { mode: 'full', resources: ['files', 'models', 'preset', 'devices', 'baselines'] };
/** 写入后除状态外还要重读的资源：预设的当前模型与差异标记，模型列表的已加载标记。 */
const REREAD: Partial<Record<Lock, readonly Resource[]>> = { lifecycle: ['preset', 'models'], model: ['preset', 'models'], params: ['preset'] };

// 就地更新：只写入变化的字段，依赖未变字段的组件不重渲染。
function patch(target: Record<string, unknown>, next: Record<string, unknown>) {
  const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
  for (const key of Object.keys(next)) {
    const value = next[key]; const current = target[key];
    if (isRecord(current) && isRecord(value)) patch(current, value);
    else if (!(Array.isArray(current) && Array.isArray(value) && current.length === value.length && value.every((item, index) => item === current[index]))) target[key] = value;
  }
  for (const key of Object.keys(target)) if (!(key in next)) delete target[key];
}
function update<T extends object>(slot: Ref<T | null>, value: T) {
  if (slot.value) patch(slot.value as unknown as Record<string, unknown>, value as unknown as Record<string, unknown>);
  else slot.value = value;
}

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
  // 新打开的页面只显示打开之后收到的字幕，存档回放只推进游标；页面重建时恢复已保存的字幕与游标。
  const subtitles = new SubtitleStore();
  const restored = subtitles.restore();
  const fresh = restored === null;
  // 打开时刻的设备时钟（秒）：undefined 表示尚未探测，null 表示设备未提供。
  let openedAt: number | null | undefined;
  const replayQueue: Array<[SubtitleRecord, string | undefined]> = [];
  // 每次修订都会重写接收时间，被过滤的句子要按编号记住，它的后续修订同样不显示。
  const archivedIds = new Set<string>();
  const records = ref<SubtitleRecord[]>(restored ?? []);
  const unknowns = ref<Record<ActionArea, string[]>>({ system: [], files: [], devices: [], params: [], speech: [] });
  const busy = ref<Record<Lock, boolean>>({ system: false, files: false, devices: false, params: false, speech: false, mute: false, route: false, model: false, lifecycle: false });
  const writing = computed(() => Object.values(busy.value).some(Boolean));
  const reloading = ref(false);
  const notice = ref('');
  const errors = ref<Record<ErrorKey, string>>({
    system: '', lifecycle: '', route: '', mute: '', model: '', files: '', devices: '', params: '', speech: '',
    connection: '', status: '', models: '', preset: '', deviceList: '', fileList: '', captureBaseline: '', playbackBaseline: '', theme: '',
  });
  const subtitleState = ref<ConnectionState>('connecting');
  const statusStreamState = ref<ConnectionState>('connecting');
  const subtitleError = ref('');
  const subtitleStorageError = ref('');
  const statusStreamError = ref('');
  const filesFresh = ref(false);
  const devicesFresh = ref(false);
  const modelsFresh = ref(false);
  const presetFresh = ref(false);
  const uploadGeneration = ref(0);
  const baselines = ref<Partial<Record<'capture' | 'playback', string>>>({});
  const ready = computed(() => adapterOnline.value && statusFresh.value);
  const downloadSupported = computed(() => status.value?.api_capabilities.includes('file_fetch_safe_errors_v1') ?? false);
  let disposed = false;
  let statusRevision = 0;
  let invalidated = 0;
  let filesRevision = 0;
  let baselineRevision = 0;
  const baselineFresh = ref({ capture: false, playback: false });
  const requiredBaselines = ref<Array<'capture' | 'playback'>>([]);
  let lastStatusAt = 0;
  let inFlight: Promise<void> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;
  let stopStatus: (() => void) | undefined;
  let stopSubtitles: (() => void) | undefined;
  const reads = new AbortController();

  function setNotice(message: string) {
    clearTimeout(noticeTimer);
    noticeTimer = undefined;
    notice.value = message;
    if (message && !disposed) {
      noticeTimer = setTimeout(() => { notice.value = ''; noticeTimer = undefined; }, NOTICE_MS);
    }
  }
  function dismissNotice() { setNotice(''); }
  function dismissError(key: ErrorKey) { errors.value[key] = ''; }

  const acceptStatus = (value: Status) => {
    update(status, value);
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

  // 适配器不可达时整体离线，本轮不再读取其他数据。
  async function readHealth() {
    try {
      const value = await client.health(reads.signal);
      if (disposed) return false;
      update(health, value);
      adapterOnline.value = true;
      errors.value.connection = '';
      if (fresh && openedAt === undefined) { openedAt = deviceNow(); for (const queued of replayQueue.splice(0)) acceptSubtitle(...queued); }
      return true;
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      if (disposed) return false;
      adapterOnline.value = false;
      statusFresh.value = false;
      filesFresh.value = false;
      devicesFresh.value = false;
      modelsFresh.value = false;
      presetFresh.value = false;
      health.value = null;
      errors.value.connection = describe(error);
      return false;
    }
  }

  function refresh(scope: Scope): Promise<void> {
    if (inFlight) return inFlight;
    inFlight = (async () => {
      if (!await readHealth()) return;
      const { mode, resources } = scope;
      const stale = statusStreamState.value !== 'online' || !statusFresh.value || Date.now() - lastStatusAt >= REFRESH_MS;
      if (mode !== 'poll' || stale) {
        if (mode !== 'write') statusFresh.value = false;
        // 其他区域的写入结果未知会作废在途读取；写入后的确认读取重新发起，不静默丢弃。
        for (let attempt = 0; ; attempt++) {
          const revision = statusRevision; const epoch = invalidated;
          try {
            const value = await client.status(reads.signal);
            if (disposed) return;
            // 到上限仍被打断时明确报错，不把未确认的状态当作已确认。
            if (mode === 'write' && epoch !== invalidated) {
              if (attempt < 4) continue;
              if (!disposed) errors.value.status = '写入后的状态确认连续被打断，请刷新状态。';
              break;
            }
            if (statusRevision === revision) {
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
          break;
        }
      }
      const fileRevision = filesRevision;
      if (resources.includes('files')) {
        try {
          const value = await client.files(reads.signal);
          if (!disposed && fileRevision === filesRevision) {
            // 队列内容未变时保留原数组，文件面板不因轮询重渲染。
            if (JSON.stringify(jobs.value) !== JSON.stringify(value.jobs)) jobs.value = value.jobs;
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
      }
      if (resources.length) {
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
          resources.includes('models') ? read(() => client.models(reads.signal), value => { models.value = value.models; }, 'models') : undefined,
          resources.includes('preset') ? read(() => client.preset(reads.signal), value => { preset.value = value; }, 'preset') : undefined,
          resources.includes('devices') ? read(() => client.devices(reads.signal), value => { devices.value = value.devices; }, 'deviceList') : undefined,
          resources.includes('baselines') ? readBaseline('capture') : undefined,
          resources.includes('baselines') ? readBaseline('playback') : undefined,
        ]);
        if (resources.includes('models')) modelsFresh.value = !errors.value.models;
        if (resources.includes('preset')) presetFresh.value = !errors.value.preset;
        if (resources.includes('devices')) devicesFresh.value = !errors.value.deviceList;
      }
    })().finally(() => { inFlight = undefined; });
    return inFlight;
  }

  // 写入后的读取必须等在途读取结束，不能复用写入前的快照。
  async function settle(scope: Scope) {
    if (inFlight) await inFlight;
    await refresh(scope);
  }
  let reloadPending = 0;
  async function reload() {
    reloadPending++;
    reloading.value = true;
    try { await settle(FULL); }
    finally {
      reloadPending--;
      reloading.value = reloadPending > 0;
    }
  }

  async function perform(lock: Lock, operation: () => Promise<unknown>, options: { message?: string; context?: string; baseline?: 'capture' | 'playback'; resources?: readonly Resource[] } = {}) {
    const area = AREA_OF_LOCK[lock];
    const { message = '请求已受理；实际状态以下次查询为准。', context = '本次操作' } = options;
    if (busy.value[lock] || unknowns.value[area].length) return false;
    busy.value[lock] = true;
    setNotice('');
    errors.value[lock] = '';
    try {
      await operation();
      if (disposed) return false;
      if (message) setNotice(message);
      const resources: Resource[] = area === 'files' ? ['files'] : [];
      resources.push(...(REREAD[lock] ?? []), ...(options.resources ?? []));
      await settle({ mode: 'write', resources });
      return true;
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      if (!disposed) {
        if (error.resultUnknown) {
          unknowns.value[area].push(`${context}：${describe(error)}`);
          if (area === 'files') { filesRevision++; filesFresh.value = false; }
          else {
            statusRevision++;
            invalidated++;
            statusFresh.value = false;
            statusReadFresh.value = false;
            if (area === 'devices') {
              baselineRevision++;
              baselineFresh.value = { capture: false, playback: false };
              requiredBaselines.value = options.baseline ? [options.baseline] : ['capture', 'playback'];
            }
          }
        } else errors.value[lock] = describe(error);
      }
      return false;
    } finally { busy.value[lock] = false; }
  }

  // 重启推理进程会改变模型列表里的已加载标记。
  const restart = (service: Service) => perform('system', () => client.restart(service), { resources: service === 'rvc' ? ['models'] : undefined });

  async function setConfig(key: string, value: string, area: 'devices' | 'params') {
    const baseline = key === 'pre.capture_device' ? 'capture' : key === 'pre.playback_device' ? 'playback' : undefined;
    await perform(area, async () => {
      const effect = await client.setConfig(key, value);
      if (baseline) baselines.value[baseline] = value;
      if (effect.restart_required && effect.affected.length) {
        for (const service of effect.affected) await client.restart(service as Service);
        setNotice(`已写入基线并重启：${effect.affected.join('、')}`);
      } else if (area === 'devices') setNotice('基线已写入；文件任务使用各自的参数快照。');
    }, { message: '', baseline });
  }

  // 连续调节参数时按顺序写入，同一参数只写最新值；前一次还在途时排队，不丢弃。
  const pendingParams = new Map<string, string>();
  let paramWrites = Promise.resolve();
  function setParam(key: string, value: string) {
    pendingParams.set(key, value);
    // 上一条写入意外失败不能阻塞后续参数写入。
    paramWrites = paramWrites.catch(() => undefined).then(async () => {
      const latest = pendingParams.get(key);
      if (latest === undefined) return;
      pendingParams.delete(key);
      await setConfig(key, latest, 'params');
    });
  }

  async function submit(file: File, modelId?: string) {
    const accepted = await perform('files', async () => {
      const result = await client.submit(file, modelId);
      setNotice(`已受理 ${result.job_id} · ${result.state}，转换尚未完成。`);
    }, { message: '', context: `提交 ${file.name}` });
    if (accepted) uploadGeneration.value++;
  }

  async function download(jobId: string) {
    if (busy.value.files) return;
    try {
      const { blob, name } = await downloadResult(jobId);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url; anchor.download = name; anchor.click();
      setTimeout(() => URL.revokeObjectURL(url), 0);
      setNotice('下载已交给浏览器。');
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
      errors.value.files = describe(error);
    }
  }

  // 游标与读模型作为同一快照保存；存储被浏览器拒绝时提示，刷新后退回只显示新字幕。
  function persistSubtitles(cursor?: string) {
    // 推送每收到一条记录都会清空推送错误，存储提示单独保存。
    subtitleStorageError.value = subtitles.save(cursor, records.value) ? '' : '浏览器拒绝保存字幕进度；刷新后只显示新字幕。';
  }
  function acceptSubtitle(value: SubtitleRecord, cursor?: string) {
    if (fresh) {
      // 设备时钟尚未探测到时先排队，探测后再判断新旧。
      if (openedAt === undefined) { replayQueue.push([value, cursor]); return; }
      // 存档回放只推进游标，不进入显示。
      const archived = openedAt !== null && value.received_at < openedAt - LIVE_CONTEXT_S;
      if (archived && openedAt! - value.received_at <= REVISION_WINDOW_S) archivedIds.add(value.utterance_id);
      if (archived || archivedIds.has(value.utterance_id)) { persistSubtitles(cursor); return; }
    }
    records.value = subtitles.accept(value);
    persistSubtitles(cursor);
  }

  async function tick() {
    await refresh(POLL);
    if (!disposed) timer = setTimeout(() => void tick(), REFRESH_MS);
  }
  onMounted(() => {
    void refresh(FULL).then(() => { if (!disposed) timer = setTimeout(() => void tick(), REFRESH_MS); });
    stopStatus = subscribeStream({
      path: '/events/status', event: 'status', schema: statusSchema, accept: acceptStatus,
      state: state => { statusStreamState.value = state; if (state === 'offline') statusFresh.value = false; },
      error: reason => { statusStreamError.value = reason; },
    });
    stopSubtitles = subscribeStream({
      path: '/events/subtitles', event: 'subtitle', schema: subtitleSchema, cursor: subtitles.current,
      accept: acceptSubtitle,
      reset: () => { subtitles.clear(); records.value = []; replayQueue.length = 0; archivedIds.clear(); },
      state: state => { subtitleState.value = state; }, error: reason => { subtitleError.value = reason; },
    });
  });
  onUnmounted(() => {
    disposed = true;
    clearTimeout(timer);
    clearTimeout(noticeTimer);
    reads.abort();
    stopStatus?.(); stopSubtitles?.();
  });

  function canAcknowledge(area: ActionArea) {
    if (busy.value[area] || !unknowns.value[area].length) return false;
    if (area === 'files') return filesFresh.value && ready.value;
    return statusReadFresh.value && ready.value && (area !== 'devices' || requiredBaselines.value.every(key => baselineFresh.value[key]));
  }

  return {
    status, health, adapterOnline, ready, jobs, paused, models, preset, devices, records, busy, writing, reloading, errors, notice,
    subtitleState, subtitleError, subtitleStorageError, statusStreamState, statusStreamError, downloadSupported,
    uploadGeneration, baselines, filesFresh, devicesFresh, modelsFresh, presetFresh, unknowns,
    canAcknowledge,
    acknowledge: (area: ActionArea) => { if (canAcknowledge(area)) unknowns.value[area] = []; },
    dismissNotice, dismissError,
    reload, perform, restart, setConfig, setParam, submit, download,
  };
}
