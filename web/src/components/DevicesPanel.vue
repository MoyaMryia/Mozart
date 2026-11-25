<script setup lang="ts">
import { computed, reactive, watch } from 'vue';
import { requestCheckbox } from '../inputs';
import type { DeviceEntry, Status } from '../contracts';
import AlertBanner from './AlertBanner.vue';

const props = defineProps<{ devices: DeviceEntry[]; status: Status | null; ready: boolean; busy: boolean; muted?: boolean; muteBlocked?: boolean; error?: string; baselines: Partial<Record<'capture' | 'playback', string>> }>();
const emit = defineEmits<{ set: [key: string, value: string]; mute: [value: boolean]; dismissError: [] }>();
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
</script>

<template>
  <section class="card p-3.5 md:p-3" aria-label="音频输入与输出设备">
    <div class="flex items-center gap-2 mb-2 min-w-0">
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
    <AlertBanner :message="error" dismissible class="mb-2.5" @dismiss="emit('dismissError')" />
    <div v-for="kind in kinds" :key="kind" class="mb-3 last:mb-0">
      <label :for="'device-' + kind" class="block text-micro font-bold tracking-wide text-ink-3 mb-1">{{ kind === 'capture' ? '输入' : '输出' }}</label>
      <div class="flex items-stretch gap-2">
        <select :id="'device-' + kind" class="field w-full min-w-0 flex-1 text-sm md:text-xs px-3 py-2.5 md:py-2" :value="draft[kind]" :disabled="busy || !ready" @change="change(kind, $event)">
          <option value="" disabled>选择设备</option>
          <option v-if="draft[kind] && !devices.some(d => d.kind === kind && d.id === draft[kind])" :value="draft[kind]">{{ draft[kind] }}</option>
          <option v-for="device in devices.filter(d => d.kind === kind)" :key="device.id" :value="device.id">{{ device.label }}</option>
        </select>
        <button type="button" class="icon-button icon-button-stretch self-stretch shrink-0" :aria-label="kind === 'capture' ? '设置输入设备' : '设置输出设备'" :title="kind === 'capture' ? '设置输入设备' : '设置输出设备'" :disabled="busy || !ready || !draft[kind]" @click="emit('set', 'pre.' + kind + '_device', draft[kind])">
          <svg class="w-5 h-5" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>
        </button>
      </div>
      <div class="flex items-center justify-between gap-2 mt-1">
        <small class="text-micro text-ink-3 min-w-0 truncate">实际：{{ actual[kind] || '未知' }} · 基线：{{ baselines[kind] ?? '未读取' }}</small>
        <small v-if="dirty[kind]" class="chip chip-warn shrink-0">未应用</small>
      </div>
    </div>
    <AlertBanner v-if="status?.pending_restart.includes('pre')" message="设备设置待重启 pre 生效。" tone="warn" class="mt-2.5" />
    <AlertBanner v-if="status?.pre?.last_error" :message="status.pre.last_error" class="mt-2.5" />
  </section>
</template>
