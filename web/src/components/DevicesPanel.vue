<script setup lang="ts">
import { computed, reactive, watch } from 'vue';
import type { DeviceEntry, Status } from '../contracts';

const props = defineProps<{ devices: DeviceEntry[]; status: Status | null; ready: boolean; busy: boolean; baselines: Partial<Record<'capture' | 'playback', string>> }>();
const emit = defineEmits<{ set: [key: string, value: string]; restart: [] }>();
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
  <section class="card">
    <div class="card-head"><h2>设备</h2></div>
    <div v-for="kind in kinds" :key="kind" class="field">
      <label :for="'device-' + kind">{{ kind === 'capture' ? '输入' : '输出' }}</label>
      <select :id="'device-' + kind" :value="draft[kind]" :disabled="busy || !ready" @change="change(kind, $event)">
        <option value="" disabled>选择设备</option>
        <option v-if="draft[kind] && !devices.some(d => d.kind === kind && d.id === draft[kind])" :value="draft[kind]">{{ draft[kind] }}</option>
        <option v-for="device in devices.filter(d => d.kind === kind)" :key="device.id" :value="device.id">{{ device.label }}</option>
      </select>
      <small class="muted">实际：{{ actual[kind] || '未知' }} · 基线：{{ baselines[kind] ?? '未读取' }}</small>
      <small v-if="dirty[kind]" class="muted">未应用</small>
      <button class="btn" :disabled="busy || !ready || !draft[kind]" @click="emit('set', 'pre.' + kind + '_device', draft[kind])">设置</button>
    </div>
    <p v-if="status?.pending_restart.includes('pre')" class="banner banner-warn">设备设置待重启 pre 生效。</p>
    <p v-if="status?.pre?.last_error" class="state-err">{{ status.pre.last_error }}</p>
    <button class="btn" :disabled="busy || !ready || !status" @click="emit('restart')">重启 pre</button>
  </section>
</template>
