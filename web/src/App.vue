<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
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
import SpeechPanel from './components/SpeechPanel.vue';
import SubtitlesPanel from './components/SubtitlesPanel.vue';
import PendingResult from './components/PendingResult.vue';
import type { Route } from './contracts';

const ui = useConsole();
const { status, health, ready, adapterOnline, busy, writing, notice, errors, models, preset, devices, jobs, paused,
  records, subtitleState, subtitleError, subtitleStorageError, statusStreamState, statusStreamError, downloadSupported,
  uploadGeneration, baselines, filesFresh, devicesFresh, unknowns } = ui;
const selectedModel = ref('');
const theme = ref<'light' | 'dark'>('light');
const activity = computed(() => systemActivity(status.value, ready.value));
const routeValue = computed(() => ready.value && status.value ? status.value.route.desired : '');
const muted = computed(() => ready.value && !!status.value?.mute);
const systemBlocked = computed(() => !ready.value || !!unknowns.value.system.length);
const filesError = computed(() => [errors.value.files, errors.value.fileList].filter(Boolean).join('\n'));
const subtitleProblem = computed(() => [subtitleError.value, subtitleStorageError.value].filter(Boolean).join('；'));
// 与默认预设的差异同时看服务端标记和当前生效参数，页面外的改动也能及时反映。
const presetDirty = computed(() => !!preset.value && !errors.value.preset &&
  (preset.value.dirty || (status.value?.rvc_params ? JSON.stringify(preset.value.preset_values) !== JSON.stringify(status.value.rvc_params) : false)));
function selectRoute(event: Event) { requestSelect<Route>(event, routeValue.value, value => ui.perform('route', () => client.route(value))); }

function applyTheme() {
  document.documentElement.dataset.theme = theme.value;
  const bg = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim();
  if (bg) document.getElementById('meta-theme-color')?.setAttribute('content', bg);
}
function toggleTheme() {
  theme.value = theme.value === 'light' ? 'dark' : 'light';
  applyTheme();
  try { localStorage.setItem('mozart-theme', theme.value); }
  catch (error) {
    if (!(error instanceof DOMException && error.name === 'SecurityError')) throw error;
    errors.value.system = '浏览器拒绝保存主题；本次选择仅在当前页面有效。';
  }
}
onMounted(() => {
  try {
    const saved = localStorage.getItem('mozart-theme');
    if (saved === 'dark' || saved === 'light') theme.value = saved;
    else if (saved !== null) errors.value.system = '已保存的主题值无效，使用亮色。';
  } catch (error) {
    if (!(error instanceof DOMException && error.name === 'SecurityError')) throw error;
    errors.value.system = '浏览器拒绝读取主题设置。';
  }
  applyTheme();
});
</script>

<template>
  <a href="#content" class="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 skip-link focus:px-4 focus:py-2 focus:z-50 rounded-sm text-xs font-mono shadow-md">跳到控制台</a>

  <div class="app-frame md:flex-1 md:min-h-0">
  <!-- 顶部状态导航条 -->
  <header class="app-header flex flex-wrap md:flex-nowrap items-center justify-center gap-3 px-4 md:px-8 py-3.5 bg-app shrink-0">
    <h1 class="text-lg font-extrabold tracking-tight text-ink" translate="no">MOZART</h1>

    <div class="flex items-center min-w-0" :title="activity.stateText" :aria-label="activity.stateText" role="img">
      <svg v-if="activity.running || activity.starting" :class="['system-state-icon', activity.stateTone]" aria-hidden="true" fill="currentColor" viewBox="0 0 48 48"><path d="M24 2a22 22 0 1 0 0 44 22 22 0 0 0 0-44Z"/><path fill="#fff" d="m20 15 14 9-14 9V15Z"/></svg>
      <svg v-else :class="['system-state-icon', activity.stateTone]" aria-hidden="true" fill="currentColor" viewBox="0 0 48 48"><path d="M24 2a22 22 0 1 0 0 44 22 22 0 0 0 0-44Z"/><path fill="#fff" d="M16 16h16v16H16V16Z"/></svg>
    </div>

    <div class="transport-control transport-control-2 transport-compact shrink-0" role="group" aria-label="系统运行控制">
      <button type="button" data-state="running" class="transport-button" aria-label="启动系统" title="启动业务服务组" :aria-pressed="activity.running" :class="{ 'is-active': activity.running }" :disabled="busy.lifecycle || !!unknowns.system.length" @click="ui.perform('lifecycle', client.up)">
        <svg class="w-4 h-4" aria-hidden="true" fill="currentColor" viewBox="0 0 24 24"><path d="M8 5v14l11-7L8 5Z"/></svg>
        <span class="text-tiny font-bold">启动系统</span>
      </button>
      <button type="button" data-state="stopped" class="transport-button" aria-label="停止系统" title="停止业务服务组；daemon 与页面保持在线" :aria-pressed="ready && !!status && !activity.running" :class="{ 'is-active': ready && !!status && !activity.running }" :disabled="busy.lifecycle || systemBlocked || !status" @click="ui.perform('lifecycle', client.down)">
        <svg class="w-4 h-4" aria-hidden="true" fill="currentColor" viewBox="0 0 24 24"><path d="M6 6h12v12H6V6Z"/></svg>
        <span class="text-tiny font-bold">停止系统</span>
      </button>
    </div>

    <button type="button" class="icon-button shrink-0" aria-label="刷新" title="刷新状态" :disabled="writing" @click="ui.reload()">
      <RefreshIcon />
    </button>

    <ResourceStats :online="adapterOnline" :ready="ready" :status="status" />

    <button type="button" class="icon-button shrink-0" :aria-pressed="theme === 'dark'" aria-label="切换亮暗主题" :title="theme === 'dark' ? '切换到亮色主题' : '切换到暗色主题'" @click="toggleTheme">
      <svg v-if="theme === 'dark'" class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><circle cx="12" cy="12" r="4"/><path d="M12 2v2m0 16v2M4.93 4.93l1.41 1.41m11.32 11.32 1.41 1.41M2 12h2m16 0h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41"/></svg>
      <svg v-else class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79Z"/></svg>
    </button>
  </header>

  <!-- 主布局：中会话与文件 / 右控制与设备 -->
  <main id="content" class="flex-1 min-h-0 grid grid-cols-1 bg-app overflow-visible md:overflow-hidden md:grid-cols-[1fr_340px]">

    <div class="workspace px-3 py-3 md:px-5 md:py-4 w-full flex flex-col flex-1 min-h-0 gap-3 overflow-visible md:overflow-y-auto md:h-full">
      <PendingResult :messages="unknowns.speech" :enabled="ui.canAcknowledge('speech')" :busy="busy.speech" @acknowledge="ui.acknowledge('speech')" />
      <SpeechPanel :status="ready ? status : null" :ready="ready" :busy="busy.speech || !!unknowns.speech.length" :error="errors.speech"
        @translate="enabled => ui.perform('speech', () => client.translate(enabled))"
        @speech="enabled => ui.perform('speech', () => client.speech(enabled))"
        @stop="ui.perform('speech', client.speechStop)" />
      <SubtitlesPanel :records="records" :state="subtitleState" />
      <FilesPanel :key="uploadGeneration" :jobs="jobs" :paused="paused" :ready="ready && filesFresh" :busy="busy.files" :refresh-blocked="writing"
        :ack-ready="ui.canAcknowledge('files')" :download-supported="downloadSupported" :error="filesError" :unknowns="unknowns.files" @acknowledge="ui.acknowledge('files')"
        @submit="ui.submit" @cancel="jobId => ui.perform('files', () => client.cancel(jobId))"
        @pause="value => ui.perform('files', () => client.pause(value))" @refresh="ui.reload()" @download="ui.download" />
      <ConnectionLog :status="status" :ready="ready" :status-stream-state="statusStreamState" :status-stream-error="statusStreamError" :subtitle-state="subtitleState" :subtitle-error="subtitleProblem" />
    </div>

    <aside class="right-sidebar p-3 md:p-4 border-l border-edge-soft bg-app flex flex-col gap-5 overflow-y-auto md:h-full" aria-label="控制与设备">
      <PendingResult :messages="unknowns.system" :enabled="ui.canAcknowledge('system')" :busy="busy.system" @acknowledge="ui.acknowledge('system')" />

      <!-- 路由切换 -->
      <section class="card p-3" aria-label="路由切换">
        <div class="flex items-center gap-2 mb-2 min-w-0">
          <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">路由</h2>
          <span v-if="errors.system" class="banner banner-error text-micro min-w-0 truncate" role="alert" :title="errors.system">{{ errors.system }}</span>
        </div>
        <label for="system-route" class="sr-only">路由</label>
        <select id="system-route" class="field w-full min-w-0 text-xs px-3 py-2" :value="routeValue" :disabled="busy.route || systemBlocked || !status" @change="selectRoute">
          <option value="" disabled>未知</option><option value="direct">直出</option><option value="rvc">变声</option>
        </select>
        <small class="block text-micro text-ink-3 mt-1">实际：{{ ready && status ? status.route.active : '未知' }}</small>
      </section>

      <PendingResult :messages="unknowns.devices" :enabled="ui.canAcknowledge('devices')" :busy="busy.devices" @acknowledge="ui.acknowledge('devices')" />
      <DevicesPanel :devices="devices" :status="ready ? status : null" :baselines="baselines" :ready="ready && devicesFresh" :busy="busy.devices || !!unknowns.devices.length"
        :error="[errors.devices, errors.deviceList, errors.captureBaseline, errors.playbackBaseline].filter(Boolean).join('；')"
        :muted="muted" :mute-blocked="busy.mute || !!unknowns.system.length"
        @set="(key, value) => ui.setConfig(key, value, 'devices')"
        @mute="value => ui.perform('mute', () => client.mute(value))" />
      <!-- 变声音色：模型、default 预设与参数在同一卡片 -->
      <section class="card p-3" aria-label="变声音色">
        <div class="flex items-center gap-2 mb-2 min-w-0">
          <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">变声音色</h2>
          <span v-if="errors.models" class="banner banner-error text-micro min-w-0 truncate" role="alert" :title="errors.models">{{ errors.models }}</span>
          <span v-if="errors.preset" class="banner banner-error text-micro min-w-0 truncate" role="alert" :title="errors.preset">{{ errors.preset }}</span>
        </div>
        <label for="model-select" class="sr-only">切换模型</label>
        <select id="model-select" v-model="selectedModel" class="field w-full min-w-0 text-xs px-3 py-2" autocomplete="off" :disabled="busy.model || systemBlocked || !!errors.models">
          <option value="">选择模型</option>
          <option v-for="item in models" :key="item.id" :value="item.id" :disabled="!item.exists">{{ item.id }}{{ item.exists ? '' : '（缺失）' }}</option>
        </select>
        <button class="btn btn-accent btn-sm w-full mt-2.5" :disabled="busy.model || systemBlocked || !selectedModel || !!errors.models || !models.some(item => item.id === selectedModel && item.exists)" @click="ui.perform('model', () => client.useModel(selectedModel), { context: '切换模型' })">使用模型</button>
        <PendingResult :messages="unknowns.params" :enabled="ui.canAcknowledge('params')" :busy="busy.params" @acknowledge="ui.acknowledge('params')" />
        <ParamsPanel embedded :params="ready ? status?.rvc_params ?? null : null" :model-id="ready ? status?.rvc?.model_id ?? null : null" :ready="ready" :blocked="!!unknowns.params.length" :error="errors.params"
          :preset-dirty="presetDirty" :preset-busy="busy.model || busy.system || busy.params || !!unknowns.system.length || !!unknowns.params.length"
          @set="ui.setParam"
          @save="ui.perform('model', client.savePreset, { context: '保存预设' })"
          @restore="ui.perform('model', client.usePreset, { context: '使用预设' })" />
      </section>

      <SystemPanel :status="status" :connected="ready" :busy="busy.system" :write-blocked="!!unknowns.system.length"
        @restart="ui.restart" />
    </aside>
  </main>

  <!-- 全局提示：悬浮在页面底部，不参与布局，避免出现/消失时推动内容。 -->
  <div class="toast-stack" aria-live="polite">
    <p v-if="errors.connection" class="toast toast-error" role="alert">{{ errors.connection }}</p>
    <p v-if="errors.status" class="toast toast-error" role="alert">{{ errors.status }}</p>
    <p v-if="!adapterOnline" class="toast toast-warn">HTTP 适配器未连接。当前页面不使用模拟运行状态。</p>
    <p v-if="health?.uploads.retained" class="toast toast-warn">保留上传 {{ health.uploads.retained }} 项：{{ health.uploads.reasons.join('；') }}</p>
    <p v-if="notice" class="toast" role="status">{{ notice }}</p>
  </div>
  </div>
</template>