<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { client } from './api';
import { useConsole } from './useConsole';
import { requestSelect } from './inputs';
import { systemActivity } from './system';
import RefreshIcon from './components/RefreshIcon.vue';
import ResourceStats from './components/ResourceStats.vue';
import ConnectionLog from './components/ConnectionLog.vue';
import SystemPanel from './components/SystemPanel.vue';
import FilesPanel from './components/FilesPanel.vue';
import DevicesPanel from './components/DevicesPanel.vue';
import ParamsPanel from './components/ParamsPanel.vue';
import BroadcastPanel from './components/BroadcastPanel.vue';
import SubtitlesPanel from './components/SubtitlesPanel.vue';
import PendingResult from './components/PendingResult.vue';
import AlertBanner from './components/AlertBanner.vue';
import ToastStack, { type ToastItem } from './components/ToastStack.vue';
import MobileNav, { MOBILE_SECTIONS, type MobileSection } from './components/MobileNav.vue';
import type { OutputMode, OutputPhase } from './contracts';

const MODE_LABELS: Record<OutputMode, string> = {
  direct: '直出',
  rvc: '变声',
  broadcast: '翻译播报',
};
const PHASE_LABELS: Record<Exclude<OutputPhase, 'applied'>, string> = {
  closing_speech: '停止播报中',
  configuring_playout: '切换输出中',
  routing: '切换路由中',
  opening_speech: '开启播报中',
  reattaching_playout: '恢复输出中',
};

const ERROR_GROUPS = {
  files: ['files', 'fileList'],
  devices: ['mute', 'devices', 'deviceList', 'captureBaseline', 'playbackBaseline'],
  voice: ['model', 'models', 'preset'],
} as const;

const ui = useConsole();
const { status, health, ready, adapterOnline, busy, writing, reloading, notice, errors, models, preset, devices, jobs, paused,
  records, subtitleState, subtitleError, subtitleStorageError, statusStreamState, statusStreamError, downloadSupported,
  uploadGeneration, baselines, filesFresh, devicesFresh, modelsFresh, presetFresh, unknowns } = ui;
const selectedModel = ref('');
const activeModelId = computed(() => status.value?.rvc?.model_id ?? models.value.find(item => item.loaded)?.id ?? '');
watch([activeModelId, models], ([next, list], old) => {
  const prev = old?.[0];
  if (!next) return;
  const existsInList = list.some(item => item.id === next && item.exists);
  if (!existsInList) return;
  if (!selectedModel.value || selectedModel.value === prev || !list.some(item => item.id === selectedModel.value)) {
    selectedModel.value = next;
  }
}, { immediate: true });
const theme = ref<'light' | 'dark'>('light');
const activeSection = ref<MobileSection>('live');
const activity = computed(() => systemActivity(status.value, ready.value));
const modeValue = computed<OutputMode | ''>(() => {
  if (!ready.value || !status.value) return '';
  if (status.value.output_mode) return status.value.output_mode.requested;
  return status.value.bridge?.speech_enabled ? 'broadcast' : status.value.route.desired;
});
const modeActual = computed(() => {
  if (!ready.value || !status.value) return { text: '实际：未知', transitioning: false };
  const om = status.value.output_mode;
  if (om) {
    if (om.applied) return { text: `实际：${MODE_LABELS[om.applied]}`, transitioning: false };
    return { text: om.phase === 'applied' ? '同步中' : PHASE_LABELS[om.phase], transitioning: true };
  }
  const base = `实际：${MODE_LABELS[status.value.route.active]}`;
  return { text: status.value.bridge?.speech_enabled ? `${base} + 播报` : base, transitioning: false };
});
const translateEnabled = computed(() => ready.value && !!status.value?.bridge?.translate_enabled);
const broadcastVisible = computed(() => ready.value && !!status.value && (
  status.value.output_mode?.requested === 'broadcast'
  || status.value.output_mode?.applied === 'broadcast'
  || (status.value.output_mode == null && !!status.value.bridge?.speech_enabled)
));
const muted = computed(() => ready.value && !!status.value?.mute);
const systemBlocked = computed(() => !ready.value || !!unknowns.value.system.length);
const joinErrors = (keys: readonly (keyof typeof errors.value)[]) => keys.map(key => errors.value[key]).filter(Boolean).join('\n');
const filesError = computed(() => joinErrors(ERROR_GROUPS.files));
const devicesError = computed(() => joinErrors(ERROR_GROUPS.devices));
const voiceError = computed(() => joinErrors(ERROR_GROUPS.voice));
const subtitleProblem = computed(() => [subtitleError.value, subtitleStorageError.value].filter(Boolean).join('；'));
// 与默认预设的差异同时看服务端标记和当前生效参数，页面外的改动也能及时反映。
const presetDirty = computed(() => !!preset.value && presetFresh.value &&
  (preset.value.dirty || (status.value?.rvc_params ? JSON.stringify(preset.value.preset_values) !== JSON.stringify(status.value.rvc_params) : false)));

const toasts = computed<ToastItem[]>(() => {
  const list: ToastItem[] = [];
  if (errors.value.connection) list.push({ id: 'connection', tone: 'error', message: errors.value.connection });
  if (errors.value.status) list.push({ id: 'status', tone: 'error', message: errors.value.status });
  if (errors.value.lifecycle) list.push({ id: 'lifecycle', tone: 'error', message: errors.value.lifecycle, dismissible: true });
  if (errors.value.theme) list.push({ id: 'theme', tone: 'warn', message: errors.value.theme, dismissible: true });
  if (!adapterOnline.value) list.push({ id: 'offline', tone: 'warn', message: 'HTTP 适配器未连接。当前页面不使用模拟运行状态。' });
  if (health.value?.uploads.retained) list.push({ id: 'retained', tone: 'warn', message: `保留上传 ${health.value.uploads.retained} 项：${health.value.uploads.reasons.join('；')}` });
  if (notice.value) list.push({ id: 'notice', tone: 'info', message: notice.value, dismissible: true });
  return list;
});

const sectionAlerts = computed<Record<MobileSection, boolean>>(() => ({
  live: Boolean(errors.value.route || errors.value.translate || errors.value.speech || unknowns.value.speech.length || unknowns.value.system.length || (ready.value && status.value?.output_mode?.last_error)),
  voice: Boolean(voiceError.value || errors.value.params || devicesError.value || unknowns.value.params.length || unknowns.value.devices.length),
  files: Boolean(filesError.value || unknowns.value.files.length),
  system: Boolean(errors.value.system || (ready.value && status.value?.degraded.length)),
}));

function selectMode(event: Event) { requestSelect<OutputMode>(event, modeValue.value, value => ui.perform('route', () => client.mode(value), { context: '切换路由' })); }
function dismissToast(id: string) {
  if (id === 'notice') ui.dismissNotice();
  else if (id === 'lifecycle') ui.dismissError('lifecycle');
  else if (id === 'theme') ui.dismissError('theme');
}
function dismissErrorGroup(group: keyof typeof ERROR_GROUPS) {
  for (const key of ERROR_GROUPS[group]) ui.dismissError(key);
}

function jumpSection(section: MobileSection) {
  activeSection.value = section;
  const target = document.getElementById(`sec-${section}`);
  target?.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

function resolveInitialTheme(): 'light' | 'dark' {
  if (document.documentElement.dataset.theme === 'dark') return 'dark';
  if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }
  return 'light';
}
function applyTheme() {
  document.documentElement.dataset.theme = theme.value;
  const bg = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim();
  if (bg) document.getElementById('meta-theme-color')?.setAttribute('content', bg);
}
function toggleTheme() {
  theme.value = theme.value === 'light' ? 'dark' : 'light';
  applyTheme();
  try { localStorage.setItem('mozart-theme', theme.value); ui.dismissError('theme'); }
  catch (error) {
    if (!(error instanceof DOMException && error.name === 'SecurityError')) throw error;
    errors.value.theme = '浏览器拒绝保存主题；本次选择仅在当前页面有效。';
  }
}

let sectionObserver: IntersectionObserver | undefined;
onMounted(() => {
  try {
    const saved = localStorage.getItem('mozart-theme');
    if (saved === 'dark' || saved === 'light') theme.value = saved;
    else if (saved !== null) {
      theme.value = resolveInitialTheme();
      errors.value.theme = '已保存的主题值无效，已回退为默认主题。';
    } else {
      theme.value = resolveInitialTheme();
    }
  } catch (error) {
    if (!(error instanceof DOMException && error.name === 'SecurityError')) throw error;
    theme.value = resolveInitialTheme();
    errors.value.theme = '浏览器拒绝读取主题设置。';
  }
  applyTheme();

  if (typeof IntersectionObserver !== 'undefined') {
    const visibleSections = new Set<MobileSection>();
    sectionObserver = new IntersectionObserver(entries => {
      for (const entry of entries) {
        const id = entry.target.id.slice('sec-'.length) as MobileSection;
        if (entry.isIntersecting) visibleSections.add(id);
        else visibleSections.delete(id);
      }
      const next = MOBILE_SECTIONS.find(id => visibleSections.has(id));
      if (next) activeSection.value = next;
    }, { rootMargin: '-12% 0px -65% 0px', threshold: 0 });
    for (const id of MOBILE_SECTIONS) {
      const el = document.getElementById(`sec-${id}`);
      if (el) sectionObserver.observe(el);
    }
  }
});
onUnmounted(() => { sectionObserver?.disconnect(); });
</script>

<template>
  <a href="#content" class="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 skip-link focus:px-4 focus:py-2 focus:z-50 rounded-sm text-xs font-mono shadow-md">跳到控制台</a>

  <div class="app-frame md:flex-1 md:min-h-0">
  <!-- 顶部状态导航条：移动端 3 行清晰分组，桌面端单行居中。 -->
  <header class="app-header grid grid-cols-[1fr_auto_auto] md:flex md:flex-nowrap items-center md:justify-center gap-x-2 gap-y-3.5 md:gap-3 px-4 pt-[max(1rem,env(safe-area-inset-top))] pb-3.5 md:px-8 md:py-3.5 bg-app shrink-0">
    <div class="flex items-center gap-2.5 md:gap-2 min-w-0 pl-2.5 md:pl-0 md:order-1">
      <h1 class="text-2xl md:text-lg font-extrabold tracking-tight text-ink shrink-0" translate="no">MOZART</h1>
      <div class="flex items-center gap-1.5 min-w-0" :title="activity.stateText" :aria-label="activity.stateText" role="img">
        <span class="relative inline-flex items-center justify-center shrink-0">
          <svg v-if="activity.running || activity.starting" :class="['system-state-icon', activity.stateTone, { 'state-pulse': activity.starting }]" aria-hidden="true" fill="currentColor" viewBox="0 0 48 48"><path d="M24 2a22 22 0 1 0 0 44 22 22 0 0 0 0-44Z"/><path fill="#fff" d="m20 15 14 9-14 9V15Z"/></svg>
          <svg v-else :class="['system-state-icon', activity.stateTone]" aria-hidden="true" fill="currentColor" viewBox="0 0 48 48"><path d="M24 2a22 22 0 1 0 0 44 22 22 0 0 0 0-44Z"/><path fill="#fff" d="M16 16h16v16H16V16Z"/></svg>
        </span>
        <span :class="['md:hidden text-xs font-bold truncate', activity.stateTone]">{{ activity.stateText }}</span>
      </div>
    </div>

    <div class="col-span-3 md:col-span-1 md:order-2 transport-control transport-control-2 transport-compact shrink-0" role="group" aria-label="系统运行控制">
      <button type="button" data-state="running" class="transport-button" aria-label="启动系统" title="启动业务服务组" :aria-pressed="activity.running" :class="{ 'is-active': activity.running }" :disabled="busy.lifecycle || !!unknowns.system.length" @click="ui.perform('lifecycle', client.up)">
        <svg class="w-[18px] h-[18px] md:w-4 md:h-4" aria-hidden="true" fill="currentColor" viewBox="0 0 24 24"><path d="M8 5v14l11-7L8 5Z"/></svg>
        <span class="text-sm md:text-tiny font-bold">启动系统</span>
      </button>
      <button type="button" data-state="stopped" class="transport-button" aria-label="停止系统" title="停止业务服务组；daemon 与页面保持在线" :aria-pressed="ready && !!status && !activity.running" :class="{ 'is-active': ready && !!status && !activity.running }" :disabled="busy.lifecycle || systemBlocked || !status" @click="ui.perform('lifecycle', client.down)">
        <svg class="w-[18px] h-[18px] md:w-4 md:h-4" aria-hidden="true" fill="currentColor" viewBox="0 0 24 24"><path d="M6 6h12v12H6V6Z"/></svg>
        <span class="text-sm md:text-tiny font-bold">停止系统</span>
      </button>
    </div>

    <button type="button" class="col-start-2 row-start-1 md:order-3 icon-button shrink-0" aria-label="刷新" title="刷新状态" :disabled="writing || reloading" @click="ui.reload()">
      <RefreshIcon :spinning="reloading" size-class="w-[22px] h-[22px] md:w-4 md:h-4" />
    </button>

    <ResourceStats class="col-span-3 md:col-span-1 md:order-4" :online="adapterOnline" :ready="ready" :status="status" />

    <button type="button" class="col-start-3 row-start-1 md:order-5 icon-button theme-toggle shrink-0" :aria-pressed="theme === 'dark'" aria-label="切换亮暗主题" :title="theme === 'dark' ? '切换到亮色主题' : '切换到暗色主题'" @click="toggleTheme">
      <svg v-if="theme === 'dark'" class="w-[22px] h-[22px] md:w-4 md:h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><circle cx="12" cy="12" r="4"/><path d="M12 2v2m0 16v2M4.93 4.93l1.41 1.41m11.32 11.32 1.41 1.41M2 12h2m16 0h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41"/></svg>
      <svg v-else class="w-[22px] h-[22px] md:w-4 md:h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79Z"/></svg>
    </button>
  </header>

  <!-- 主布局：移动端按实时操作优先级单列排布；桌面端左会话文件 / 右控制设备 -->
  <main id="content" class="flex-1 min-h-0 flex flex-col gap-5 px-4 pt-1.5 pb-[calc(6rem+env(safe-area-inset-bottom))] md:p-0 md:gap-0 md:grid md:grid-cols-[1fr_340px] bg-app overflow-visible md:overflow-hidden">

    <div class="workspace contents md:flex md:flex-col md:flex-1 md:min-h-0 md:gap-3 md:px-5 md:py-4 md:w-full md:overflow-y-auto md:h-full">
      <SubtitlesPanel class="order-2 md:order-none" :records="records" :state="subtitleState" />
      <div id="sec-files" class="order-5 md:order-none scroll-mt-4">
        <FilesPanel :key="uploadGeneration" :jobs="jobs" :paused="paused" :ready="ready && filesFresh" :busy="busy.files" :reloading="reloading" :refresh-blocked="writing"
          :ack-ready="ui.canAcknowledge('files')" :download-supported="downloadSupported" :error="filesError" :unknowns="unknowns.files" @acknowledge="ui.acknowledge('files')"
          @submit="ui.submit" @cancel="jobId => ui.perform('files', () => client.cancel(jobId))"
          @pause="value => ui.perform('files', () => client.pause(value))" @refresh="ui.reload()" @download="ui.download"
          @dismiss-error="dismissErrorGroup('files')" />
      </div>
      <ConnectionLog class="order-7 md:order-none" :status="status" :ready="ready" :status-stream-state="statusStreamState" :status-stream-error="statusStreamError" :subtitle-state="subtitleState" :subtitle-error="subtitleProblem" />
    </div>

    <aside class="right-sidebar contents md:flex md:flex-col md:gap-5 md:p-4 md:border-l md:border-edge-soft bg-app md:overflow-y-auto md:h-full" aria-label="控制与设备">
      <div id="sec-live" class="order-1 md:order-none scroll-mt-4">
        <PendingResult class="mb-3" :messages="unknowns.system" :enabled="ui.canAcknowledge('system')" :busy="busy.system" @acknowledge="ui.acknowledge('system')" />
        <!-- 路由切换与独立翻译开关 -->
        <section class="card p-5 md:p-3" aria-label="路由切换">
          <div class="flex items-center justify-between gap-2 mb-3 md:mb-2 min-w-0">
            <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">路由</h2>
            <span :class="['text-micro font-mono truncate', modeActual.transitioning ? 'state-warn font-bold' : 'text-ink-3']">{{ modeActual.text }}</span>
          </div>
          <AlertBanner :message="errors.route" dismissible class="mb-3 md:mb-2.5" @dismiss="ui.dismissError('route')" />
          <AlertBanner :message="errors.translate" dismissible class="mb-3 md:mb-2.5" @dismiss="ui.dismissError('translate')" />
          <AlertBanner v-if="ready && status?.output_mode?.last_error" :message="status.output_mode.last_error" tone="warn" class="mb-3 md:mb-2.5" />
          <label for="system-route" class="sr-only">路由</label>
          <select id="system-route" class="field w-full min-w-0 min-h-[46px] md:min-h-0 text-base md:text-xs px-3.5 py-2.5 md:px-3 md:py-2" :value="modeValue" :disabled="busy.route || systemBlocked || !status" @change="selectMode">
            <option value="" disabled>未知</option><option value="direct">直出</option><option value="rvc">变声</option><option value="broadcast">翻译播报</option>
          </select>
          <button
            type="button"
            class="translate-toggle mt-3 md:mt-2.5"
            :class="{ 'is-active': translateEnabled }"
            :aria-pressed="translateEnabled"
            :disabled="busy.translate || !!unknowns.speech.length || !ready || !status?.bridge"
            @click="ui.perform('translate', () => client.translate(!translateEnabled), { context: '切换翻译' })"
          >
            <span class="translate-toggle-dot" aria-hidden="true"></span>
            <span>翻译</span>
            <span class="translate-toggle-state">{{ ready && status?.bridge ? (translateEnabled ? '已开启' : '已关闭') : '未知' }}</span>
          </button>
        </section>
        <PendingResult class="mt-3" :messages="unknowns.speech" :enabled="ui.canAcknowledge('speech')" :busy="busy.speech || busy.translate" @acknowledge="ui.acknowledge('speech')" />
        <Transition name="card-reveal">
          <div v-if="broadcastVisible" class="card-reveal-wrap">
            <div class="card-reveal-inner pt-5">
              <BroadcastPanel
                :status="ready ? status : null"
                :ready="ready"
                :busy="busy.speech || !!unknowns.speech.length"
                :error="errors.speech"
                @clear="ui.perform('speech', client.speechClear, { message: '已请求清除播报队列。', context: '清除播报队列' })"
                @dismiss-error="ui.dismissError('speech')"
              />
            </div>
          </div>
        </Transition>
      </div>

      <div class="order-4 md:order-none space-y-3">
        <PendingResult :messages="unknowns.devices" :enabled="ui.canAcknowledge('devices')" :busy="busy.devices" @acknowledge="ui.acknowledge('devices')" />
        <DevicesPanel :devices="devices" :status="ready ? status : null" :baselines="baselines" :ready="ready && devicesFresh" :busy="busy.devices || !!unknowns.devices.length"
          :error="devicesError"
          :muted="muted" :mute-blocked="busy.mute || !!unknowns.system.length"
          @set="(key, value) => ui.setConfig(key, value, 'devices')"
          @mute="value => ui.perform('mute', () => client.mute(value))"
          @dismiss-error="dismissErrorGroup('devices')" />
      </div>

      <!-- 变声音色：模型、default 预设与参数在同一卡片 -->
      <section id="sec-voice" class="card p-5 md:p-3 order-3 md:order-none scroll-mt-4" aria-label="变声音色">
        <div class="flex items-center gap-2 mb-3 md:mb-2 min-w-0">
          <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">变声音色</h2>
          <span class="text-micro font-mono font-bold text-ink-2 bg-surface border border-edge rounded-md px-2 py-0.5 max-w-[55%] truncate ml-auto shrink-0" translate="no">{{ ready ? (status?.rvc?.model_id ?? '未加载') : '未知' }}</span>
        </div>
        <AlertBanner :message="voiceError" dismissible class="mb-3 md:mb-2.5" @dismiss="dismissErrorGroup('voice')" />
        <label for="model-select" class="sr-only">切换模型</label>
        <select id="model-select" v-model="selectedModel" class="field w-full min-w-0 min-h-[46px] md:min-h-0 text-base md:text-xs px-3.5 py-2.5 md:px-3 md:py-2" autocomplete="off" :disabled="busy.model || systemBlocked || !modelsFresh">
          <option value="">选择模型</option>
          <option v-for="item in models" :key="item.id" :value="item.id" :disabled="!item.exists">{{ item.id }}{{ item.loaded ? '（当前）' : '' }}{{ item.exists ? '' : '（缺失）' }}</option>
        </select>
        <button class="btn btn-accent btn-sm w-full mt-3 md:mt-2.5 min-h-[46px] md:min-h-0 text-sm md:text-[11px]" :disabled="busy.model || systemBlocked || !selectedModel || !modelsFresh || !models.some(item => item.id === selectedModel && item.exists)" @click="ui.perform('model', () => client.useModel(selectedModel), { context: '切换模型' })">使用模型</button>
        <PendingResult class="mt-3 md:mt-2.5" :messages="unknowns.params" :enabled="ui.canAcknowledge('params')" :busy="busy.params" @acknowledge="ui.acknowledge('params')" />
        <ParamsPanel embedded :params="ready ? status?.rvc_params ?? null : null" :model-id="ready ? status?.rvc?.model_id ?? null : null" :ready="ready" :blocked="!!unknowns.params.length" :error="errors.params"
          :preset-dirty="presetDirty" :preset-busy="busy.model || busy.system || busy.params || !!unknowns.system.length || !!unknowns.params.length"
          @set="ui.setParam"
          @save="ui.perform('model', client.savePreset, { context: '保存预设' })"
          @restore="ui.perform('model', client.usePreset, { context: '使用预设' })"
          @dismiss-error="ui.dismissError('params')" />
      </section>

      <SystemPanel id="sec-system" class="order-6 md:order-none scroll-mt-4" :status="status" :connected="ready" :busy="busy.system" :write-blocked="!!unknowns.system.length" :error="errors.system"
        @restart="ui.restart" @dismiss-error="ui.dismissError('system')" />
    </aside>
  </main>

  <ToastStack :items="toasts" @dismiss="dismissToast" />
  <MobileNav :active="activeSection" :alerts="sectionAlerts" :has-subtitles="records.length > 0 && subtitleState === 'online'" @select="jumpSection" />
  </div>
</template>
