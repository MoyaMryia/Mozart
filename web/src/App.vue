<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { client } from './api';
import { useConsole } from './useConsole';
import SystemPanel from './components/SystemPanel.vue';
import FilesPanel from './components/FilesPanel.vue';
import DevicesPanel from './components/DevicesPanel.vue';
import ParamsPanel from './components/ParamsPanel.vue';
import SpeechPanel from './components/SpeechPanel.vue';
import SubtitlesPanel from './components/SubtitlesPanel.vue';
import PendingResult from './components/PendingResult.vue';

const ui = useConsole();
const { status, health, ready, adapterOnline, busy, notice, errors, models, preset, devices, jobs, paused,
  records, subtitleState, subtitleError, statusStreamState, statusStreamError, downloadSupported,
  uploadGeneration, baselines, filesFresh, devicesFresh, unknowns } = ui;
const selectedModel = ref('');
const theme = ref<'light' | 'dark'>('light');
const stats = computed(() => ready.value ? status.value?.hw : null);
const memory = computed(() => stats.value ? `${(stats.value.memory.used_bytes / 1024 ** 3).toFixed(1)} / ${(stats.value.memory.total_bytes / 1024 ** 3).toFixed(1)} GiB` : '—');
const filesError = computed(() => [errors.value.files, errors.value.fileList].filter(Boolean).join('\n'));

function applyTheme() { document.documentElement.dataset.theme = theme.value; }
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
  <a href="#content" class="skip-link">跳到控制台</a>
  <header class="app-header">
    <h1 class="brand">MOZART</h1>
    <div class="header-stats">
      <span class="chip" :class="adapterOnline ? 'chip-ok' : 'chip-warn'">API {{ adapterOnline ? '在线' : '未连接' }}</span>
      <span class="muted mono">CPU {{ stats?.cpu_percent == null ? '—' : `${stats.cpu_percent.toFixed(0)}%` }}</span>
      <span class="muted mono">RAM {{ memory }}</span>
      <button type="button" class="btn" :aria-pressed="theme === 'dark'" aria-label="切换亮暗主题" @click="toggleTheme">{{ theme === 'light' ? '暗色' : '亮色' }}</button>
    </div>
  </header>
  <main id="content" class="app-main">
    <aside class="left-sidebar" aria-label="系统控制">
      <p v-if="errors.system" class="banner banner-error" role="alert">{{ errors.system }}</p>
      <PendingResult :messages="unknowns.system" :enabled="ui.canAcknowledge('system')" :busy="busy" @acknowledge="ui.acknowledge('system')" />
      <SystemPanel :status="status" :connected="ready" :busy="busy" :write-blocked="!!unknowns.system.length"
        @refresh="ui.reload()" @up="ui.perform('system', client.up)" @down="ui.perform('system', client.down)"
        @route="value => ui.perform('system', () => client.route(value))"
        @mute="value => ui.perform('system', () => client.mute(value))"
        @restart="service => ui.perform('system', () => client.restart(service))" />
      <button class="btn" :disabled="busy || !ready || !!unknowns.system.length" @click="ui.perform('system', client.saveConfig, '运行时覆盖已写入基线。')">保存运行时设置</button>
      <label class="field">切换模型<select v-model="selectedModel" :disabled="busy || !ready || !!errors.models || !!unknowns.system.length"><option value="">选择模型</option><option v-for="item in models" :key="item.id" :value="item.id" :disabled="!item.exists">{{ item.id }}{{ item.exists ? '' : '（缺失）' }}</option></select></label>
      <button class="btn" :disabled="busy || !ready || !selectedModel || !!errors.models || !models.some(item => item.id === selectedModel && item.exists) || unknowns.system.length > 0" @click="ui.perform('system', () => client.useModel(selectedModel), undefined, '切换模型')">使用模型</button>
      <p v-if="errors.preset" class="banner banner-error" role="alert">{{ errors.preset }}</p>
      <p v-if="preset && !errors.preset">模型 {{ preset.model_id }} 的 default 预设：{{ preset.dirty ? '与当前值不同' : '与当前值一致' }}</p>
      <button class="btn" :disabled="busy || !ready || !!errors.preset || !preset || unknowns.system.length > 0" @click="ui.perform('system', client.savePreset, undefined, '保存预设')">保存 default 预设</button>
      <button class="btn" :disabled="busy || !ready || !!errors.preset || !preset || unknowns.system.length > 0" @click="ui.perform('system', client.usePreset, undefined, '使用预设')">使用 default 预设</button>
    </aside>

    <div class="workspace">
      <p v-if="errors.connection" class="banner banner-error" role="alert">{{ errors.connection }}</p>
      <p v-if="errors.status" class="banner banner-error" role="alert">{{ errors.status }}</p>
      <p v-if="!adapterOnline" class="banner banner-warn">HTTP 适配器未连接。当前页面不使用模拟运行状态。</p>
      <p v-if="notice" class="banner" role="status">{{ notice }}</p>
      <p v-if="health?.uploads.retained" class="banner banner-warn">保留上传 {{ health.uploads.retained }} 项：{{ health.uploads.reasons.join('；') }}</p>
      <p v-if="errors.speech" class="banner banner-error" role="alert">{{ errors.speech }}</p>
      <PendingResult :messages="unknowns.speech" :enabled="ui.canAcknowledge('speech')" :busy="busy" @acknowledge="ui.acknowledge('speech')" />
      <SpeechPanel :status="ready ? status : null" :ready="ready" :busy="busy || !!unknowns.speech.length"
        @translate="enabled => ui.perform('speech', () => client.translate(enabled))"
        @speech="enabled => ui.perform('speech', () => client.speech(enabled))"
        @stop="ui.perform('speech', client.speechStop)" />
      <p class="muted">术语表编辑和清空功能暂未提供。</p>
      <SubtitlesPanel :records="records" :state="subtitleState" :error="subtitleError" />
      <p v-if="errors.models" class="banner banner-error" role="alert">{{ errors.models }}</p>
      <FilesPanel :key="uploadGeneration" :jobs="jobs" :paused="paused" :models="models" :ready="ready && filesFresh" :busy="busy"
        :ack-ready="ui.canAcknowledge('files')" :models-error="!!errors.models" :download-supported="downloadSupported" :error="filesError" :unknowns="unknowns.files" @acknowledge="ui.acknowledge('files')"
        @submit="ui.submit" @cancel="jobId => ui.perform('files', () => client.cancel(jobId))"
        @pause="value => ui.perform('files', () => client.pause(value))" @refresh="ui.reload()" @download="ui.download" />
      <details class="card muted">
        <summary>连接详情</summary>
        <p>状态推送：{{ statusStreamState }}<template v-if="statusStreamError"> · {{ statusStreamError }}</template></p>
        <p>字幕推送：{{ subtitleState }}</p>
        <p v-if="!ready">当前业务状态未知，操作暂停。</p>
        <p v-else-if="status?.rvc" class="mono">RVC {{ status.rvc.model_id ?? '未加载' }} · {{ status.rvc.state }} · {{ status.rvc.frames_in }}/{{ status.rvc.frames_out }} 帧 · 丢弃 {{ status.rvc.frames_dropped }}</p>
        <p v-if="ready && status?.rvc?.inference_ms_p95 != null">推理 P95：{{ status.rvc.inference_ms_p95.toFixed(1) }} ms</p>
      </details>
    </div>

    <aside class="right-sidebar" aria-label="设备与参数">
      <p v-if="errors.deviceList || errors.devices || errors.captureBaseline || errors.playbackBaseline" class="banner banner-error" role="alert">{{ [errors.devices, errors.deviceList, errors.captureBaseline, errors.playbackBaseline].filter(Boolean).join('\n') }}</p>
      <PendingResult :messages="unknowns.devices" :enabled="ui.canAcknowledge('devices')" :busy="busy" @acknowledge="ui.acknowledge('devices')" />
      <DevicesPanel :devices="devices" :status="ready ? status : null" :baselines="baselines" :ready="ready && devicesFresh" :busy="busy || !!unknowns.devices.length"
        @set="(key, value) => ui.setConfig(key, value, 'devices')" @restart="ui.perform('devices', () => client.restart('pre'))" />
      <p v-if="errors.params" class="banner banner-error" role="alert">{{ errors.params }}</p>
      <PendingResult :messages="unknowns.params" :enabled="ui.canAcknowledge('params')" :busy="busy" @acknowledge="ui.acknowledge('params')" />
      <ParamsPanel :params="ready ? status?.rvc_params ?? null : null" :model-id="ready ? status?.rvc?.model_id ?? null : null" :ready="ready" :busy="busy || !!unknowns.params.length" @set="(key, value) => ui.setConfig(key, value, 'params')" />
    </aside>
  </main>
</template>
