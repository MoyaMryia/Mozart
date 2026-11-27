<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import { requestCheckbox } from '../inputs';
import type { DeviceEntry, Status } from '../contracts';
import AlertBanner from './AlertBanner.vue';

const props = defineProps<{ devices: DeviceEntry[]; status: Status | null; ready: boolean; busy: boolean; muted?: boolean; muteBlocked?: boolean; error?: string; baselines: Partial<Record<'capture' | 'playback', string>> }>();
const emit = defineEmits<{ set: [key: string, value: string]; mixer: [payload: { volume_percent?: number; agc_enabled?: boolean }]; mute: [value: boolean]; dismissError: [] }>();
const kinds = ['capture', 'playback'] as const;
type Kind = typeof kinds[number];
const draft = reactive<Record<Kind, string>>({ capture: '', playback: '' });
const dirty = reactive<Record<Kind, boolean>>({ capture: false, playback: false });
const actual = computed(() => ({ capture: props.status?.pre?.capture_device ?? '', playback: props.status?.pre?.playback_device ?? '' }));
watch(() => [actual.value, props.baselines, props.ready] as const, () => {
  if (!props.ready) return;
  for (const kind of kinds) {
    const next = props.baselines[kind] ?? actual.value[kind];
    if (!dirty[kind] || draft[kind] === next) { draft[kind] = next; dirty[kind] = false; }
  }
}, { immediate: true, deep: true });
function change(kind: Kind, event: Event) { draft[kind] = (event.target as HTMLSelectElement).value; dirty[kind] = true; }
const aec = computed(() => props.status?.pre?.aec ?? null);
const aecAvailable = computed(() => !!aec.value && aec.value.backend !== 'unavailable');
const aecEnabled = computed(() => !!aec.value?.enabled);
const aecStateLabel = computed(() => {
  if (!props.ready || !aec.value) return '未知';
  if (!aecAvailable.value) return '未链接';
  return aec.value.enabled ? (aec.value.active ? '已开启' : '待激活') : '已关闭';
});
const aecDetail = computed(() => {
  if (!aec.value) return '';
  const parts = [aec.value.backend];
  if (aec.value.delay_ms != null) parts.push(`延迟 ${aec.value.delay_ms} ms`);
  if (aec.value.active && aec.value.erle_db != null) parts.push(`ERLE ${aec.value.erle_db.toFixed(1)} dB`);
  if (aec.value.ref_dropped > 0) parts.push(`参考丢帧 ${aec.value.ref_dropped}`);
  return parts.join(' · ');
});

const micMixer = computed(() => props.status?.pre?.mic_mixer ?? null);
const captureClips = computed(() => props.status?.pre?.aec?.capture_clips ?? 0);
const dsp = computed(() => props.status?.pre?.dsp ?? null);
const dspSwitches = [
  { key: 'denoise', label: '降噪' },
  { key: 'gate', label: '音量门' },
  { key: 'agc', label: '软件 AGC' },
  { key: 'limiter', label: '防爆限幅' },
] as const;

const hwVolDraft = ref(100);
const hwVolDirty = ref(false);
const hwVolSubmitted = ref<number | undefined>(undefined);
watch(() => micMixer.value?.volume_percent, value => {
  if (value == null) return;
  if (!hwVolDirty.value || hwVolDraft.value === value) {
    hwVolDraft.value = value;
    hwVolDirty.value = false;
    hwVolSubmitted.value = undefined;
  }
}, { immediate: true });

const swGainDraft = ref(0);
const swGainDirty = ref(false);
const swGainSubmitted = ref<number | undefined>(undefined);
watch(() => props.status?.pre?.capture_gain_db, value => {
  if (value == null) return;
  const rounded = Math.round(value);
  if (!swGainDirty.value || swGainDraft.value === rounded) {
    swGainDraft.value = rounded;
    swGainDirty.value = false;
    swGainSubmitted.value = undefined;
  }
}, { immediate: true });

function resetHwVol() {
  const live = micMixer.value?.volume_percent;
  if (live != null) hwVolDraft.value = live;
  hwVolDirty.value = false;
  hwVolSubmitted.value = undefined;
}

function resetSwGain() {
  const live = props.status?.pre?.capture_gain_db;
  if (live != null) swGainDraft.value = Math.round(live);
  swGainDirty.value = false;
  swGainSubmitted.value = undefined;
}

watch(() => props.error, error => {
  if (!error) return;
  if (hwVolSubmitted.value !== undefined && hwVolDraft.value === hwVolSubmitted.value) resetHwVol();
  if (swGainSubmitted.value !== undefined && swGainDraft.value === swGainSubmitted.value) resetSwGain();
});

watch(() => [props.busy, props.ready] as const, ([busy, ready], [wasBusy]) => {
  if ((busy || !ready)) {
    if (hwVolDirty.value && hwVolSubmitted.value === undefined) resetHwVol();
    if (swGainDirty.value && swGainSubmitted.value === undefined) resetSwGain();
  }
  if (wasBusy && !busy) {
    if (hwVolSubmitted.value !== undefined) resetHwVol();
    if (swGainSubmitted.value !== undefined) resetSwGain();
  }
});

function formatDb(value: number) {
  return `${value > 0 ? '+' : ''}${value}`;
}

const micSummary = computed(() => {
  if (!props.ready || !props.status?.pre) return '未知';
  const hw = micMixer.value?.supported && micMixer.value.volume_percent != null
    ? `硬件 ${micMixer.value.volume_percent}%`
    : '无硬件混音';
  const sw = `软件 ${formatDb(Math.round(props.status.pre.capture_gain_db ?? 0))} dB`;
  return `${hw} · ${sw}`;
});

function onHwInput(event: Event) {
  hwVolDraft.value = Number((event.target as HTMLInputElement).value);
  hwVolDirty.value = true;
}

function commitHwVol(next = hwVolDraft.value) {
  const clamped = Math.max(0, Math.min(100, Math.round(next)));
  hwVolDraft.value = clamped;
  hwVolSubmitted.value = clamped;
  hwVolDirty.value = true;
  emit('mixer', { volume_percent: clamped });
}

function nudgeHwVol(delta: number) {
  const base = hwVolDirty.value ? hwVolDraft.value : (micMixer.value?.volume_percent ?? hwVolDraft.value);
  commitHwVol(base + delta);
}

function onSwInput(event: Event) {
  swGainDraft.value = Number((event.target as HTMLInputElement).value);
  swGainDirty.value = true;
}

function commitSwGain(next = swGainDraft.value) {
  const clamped = Math.max(-24, Math.min(24, Math.round(next)));
  swGainDraft.value = clamped;
  swGainSubmitted.value = clamped;
  swGainDirty.value = true;
  emit('set', 'pre.capture_gain_db', String(clamped));
}

function nudgeSwGain(delta: number) {
  const base = swGainDirty.value ? swGainDraft.value : Math.round(props.status?.pre?.capture_gain_db ?? 0);
  commitSwGain(base + delta);
}
</script>

<template>
  <section class="card p-5 md:p-3" aria-label="音频输入与输出设备">
    <div class="flex items-center gap-2 mb-3 md:mb-2 min-w-0">
      <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">设备</h2>
      <div class="flex items-center gap-1.5 ml-auto shrink-0">
        <label class="mute-toggle" :class="{ 'is-on': muted }" :title="muted ? '取消静音' : '静音'">
          <input type="checkbox" class="mute-toggle-input" aria-label="静音" :checked="!!muted" :disabled="muteBlocked || !ready" @change="requestCheckbox($event, !!muted, value => emit('mute', value))">
          <svg v-if="muted" class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="m3 3 18 18"/></svg>
          <svg v-else class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07m3.54-10.61a9 9 0 0 1 0 14.14"/></svg>
        </label>
        <span class="text-micro font-mono font-bold text-ink-2 bg-surface border border-edge rounded-md px-2 py-1" translate="no">{{ (status?.pre?.speaker_state ?? 'unknown').toUpperCase() }}</span>
      </div>
    </div>
    <AlertBanner :message="error" dismissible class="mb-3 md:mb-2.5" @dismiss="emit('dismissError')" />
    <div v-for="kind in kinds" :key="kind" class="mb-4 md:mb-3 last:mb-0">
      <label :for="'device-' + kind" class="block text-xs md:text-micro font-bold tracking-wide text-ink-3 mb-1.5 md:mb-1">{{ kind === 'capture' ? '输入' : '输出' }}</label>
      <div class="flex items-stretch gap-2.5 md:gap-2">
        <select :id="'device-' + kind" class="field w-full min-w-0 flex-1 min-h-[46px] md:min-h-0 text-base md:text-xs px-3.5 py-2.5 md:px-3 md:py-2" :value="draft[kind]" :disabled="busy || !ready" @change="change(kind, $event)">
          <option value="" disabled>选择设备</option>
          <option v-if="draft[kind] && !devices.some(d => d.kind === kind && d.id === draft[kind])" :value="draft[kind]">{{ draft[kind] }}</option>
          <option v-for="device in devices.filter(d => d.kind === kind)" :key="device.id" :value="device.id">{{ device.label }}</option>
        </select>
        <button type="button" class="icon-button icon-button-stretch min-w-[46px] md:min-w-0 self-stretch shrink-0" :aria-label="kind === 'capture' ? '设置输入设备' : '设置输出设备'" :title="kind === 'capture' ? '设置输入设备' : '设置输出设备'" :disabled="busy || !ready || !draft[kind]" @click="emit('set', 'pre.' + kind + '_device', draft[kind])">
          <svg class="w-5 h-5" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>
        </button>
      </div>
      <div class="flex items-center justify-between gap-2 mt-1.5 md:mt-1">
        <small class="text-micro text-ink-3 min-w-0 truncate">实际：{{ actual[kind] || '未知' }} · 基线：{{ baselines[kind] ?? '未读取' }}</small>
        <small v-if="dirty[kind]" class="chip chip-warn shrink-0">未应用</small>
      </div>
    </div>
    <div v-if="aec" class="mt-4 md:mt-3 pt-3 md:pt-2.5 border-t border-edge">
      <button
        type="button"
        class="translate-toggle"
        :class="{ 'is-active': aecEnabled }"
        :aria-pressed="aecEnabled"
        :disabled="busy || !ready || !aecAvailable"
        @click="emit('set', 'pre.aec.enabled', aecEnabled ? 'false' : 'true')"
      >
        <span class="translate-toggle-dot" aria-hidden="true"></span>
        <span>回声消除 (AEC)</span>
        <span class="translate-toggle-state">{{ aecStateLabel }}</span>
      </button>
      <small v-if="aecDetail" class="block text-micro text-ink-3 mt-1.5 md:mt-1 truncate" translate="no">{{ aecDetail }}</small>
      <AlertBanner v-if="aec?.last_error" :message="aec.last_error" tone="warn" class="mt-2" />
    </div>
    <details class="mic-tuning-details mt-4 md:mt-3 pt-3 md:pt-2.5 border-t border-edge">
      <summary class="mic-tuning-summary flex items-center gap-2 cursor-pointer select-none text-xs font-bold text-ink-2">
        <svg class="console-chevron w-3.5 h-3.5 shrink-0 text-ink-3" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="m9 18 6-6-6-6"/></svg>
        <span class="shrink-0">麦克风调校</span>
        <span v-if="captureClips > 0" class="chip chip-warn shrink-0">削波 {{ captureClips }}</span>
        <span class="ml-auto text-micro font-mono font-normal text-ink-3 truncate">{{ micSummary }}</span>
      </summary>
      <div class="pt-3 md:pt-2.5 space-y-3.5 md:space-y-3">
        <AlertBanner v-if="captureClips > 0" :message="`检测到 ${captureClips} 帧采集削波，建议调低硬件增益。`" tone="warn" />
        <div>
          <div class="flex items-center justify-between gap-2 mb-1.5">
            <label for="mic-hw-gain" class="text-xs md:text-micro font-bold text-ink-2 truncate">
              硬件增益<template v-if="micMixer?.card_name">（{{ micMixer.card_name }}）</template>
            </label>
            <span class="text-micro font-mono font-bold text-ink-2 tabular-nums shrink-0">
              <template v-if="micMixer?.supported">{{ hwVolDraft }}%<template v-if="micMixer.gain_db != null"> · {{ micMixer.gain_db >= 0 ? '+' : '' }}{{ micMixer.gain_db.toFixed(1) }} dB</template></template>
              <template v-else>不支持</template>
            </span>
          </div>
          <template v-if="micMixer?.supported">
            <div class="flex items-center gap-2">
              <button type="button" class="btn btn-sm px-2.5 min-h-[36px] md:min-h-0" aria-label="降低硬件增益" :disabled="busy || !ready || hwVolDraft <= 0" @click="nudgeHwVol(-5)">-5%</button>
              <input id="mic-hw-gain" type="range" min="0" max="100" step="1" class="mic-slider w-full min-w-0 flex-1" :value="hwVolDraft" :disabled="busy || !ready" @input="onHwInput" @change="commitHwVol()">
              <button type="button" class="btn btn-sm px-2.5 min-h-[36px] md:min-h-0" aria-label="提高硬件增益" :disabled="busy || !ready || hwVolDraft >= 100" @click="nudgeHwVol(5)">+5%</button>
            </div>
          </template>
          <p v-else class="text-micro text-ink-3">当前采集设备未暴露硬件混音器滑块<template v-if="micMixer?.error">（{{ micMixer.error }}）</template>。</p>
          <button
            v-if="micMixer?.agc_supported"
            type="button"
            class="translate-toggle mt-2.5"
            :class="{ 'is-active': !!micMixer.agc_enabled }"
            :aria-pressed="!!micMixer.agc_enabled"
            :disabled="busy || !ready"
            @click="emit('mixer', { agc_enabled: !micMixer.agc_enabled })"
          >
            <span class="translate-toggle-dot" aria-hidden="true"></span>
            <span>硬件自动增益 (AGC)</span>
            <span class="translate-toggle-state">{{ micMixer.agc_enabled ? '已开启' : '已关闭' }}</span>
          </button>
        </div>

        <div>
          <div class="flex items-center justify-between gap-2 mb-1.5">
            <label for="mic-sw-gain" class="text-xs md:text-micro font-bold text-ink-2">软件前级增益</label>
            <div class="flex items-center gap-2 shrink-0">
              <span class="text-micro font-mono font-bold text-ink-2 tabular-nums">{{ formatDb(swGainDraft) }} dB</span>
              <button type="button" class="text-micro font-bold text-ink-3 hover:text-ink disabled:opacity-40" :disabled="busy || !ready || swGainDraft === 0" @click="commitSwGain(0)">归零</button>
            </div>
          </div>
          <div class="flex items-center gap-2">
            <button type="button" class="btn btn-sm px-2.5 min-h-[36px] md:min-h-0" aria-label="降低软件增益" :disabled="busy || !ready || swGainDraft <= -24" @click="nudgeSwGain(-1)">-1</button>
            <input id="mic-sw-gain" type="range" min="-24" max="24" step="1" class="mic-slider w-full min-w-0 flex-1" :value="swGainDraft" :disabled="busy || !ready" @input="onSwInput" @change="commitSwGain()">
            <button type="button" class="btn btn-sm px-2.5 min-h-[36px] md:min-h-0" aria-label="提高软件增益" :disabled="busy || !ready || swGainDraft >= 24" @click="nudgeSwGain(1)">+1</button>
          </div>
        </div>

        <div>
          <span class="block text-xs md:text-micro font-bold text-ink-2 mb-1.5">前置处理</span>
          <div class="grid grid-cols-2 gap-2">
            <button
              v-for="item in dspSwitches"
              :key="item.key"
              type="button"
              class="translate-toggle"
              :class="{ 'is-active': !!dsp?.[item.key] }"
              :aria-pressed="!!dsp?.[item.key]"
              :disabled="busy || !ready || !dsp"
              @click="emit('set', 'pre.dsp.' + item.key, dsp?.[item.key] ? 'false' : 'true')"
            >
              <span class="translate-toggle-dot" aria-hidden="true"></span>
              <span>{{ item.label }}</span>
              <span class="translate-toggle-state">{{ dsp ? (dsp[item.key] ? '开' : '关') : '未知' }}</span>
            </button>
          </div>
        </div>
      </div>
    </details>
    <AlertBanner v-if="status?.pending_restart.includes('pre')" message="设备设置待重启 pre 生效。" tone="warn" class="mt-2.5" />
    <AlertBanner v-if="status?.pre?.last_error" :message="status.pre.last_error" class="mt-2.5" />
  </section>
</template>
