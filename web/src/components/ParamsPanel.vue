<script setup lang="ts">
import { reactive, watch } from 'vue';
import type { RvcParams } from '../contracts';

const props = defineProps<{ params: RvcParams | null; modelId: string | null; ready: boolean; busy: boolean }>();
const emit = defineEmits<{ set: [key: string, value: string] }>();
const fields = [
  { key: 'pitch_shift', label: '音高' },
  { key: 'index_rate', label: '索引比例' },
  { key: 'rms_mix_rate', label: '音量混合（文件）' },
  { key: 'protect', label: '保护（文件）' },
] as const;
type Key = typeof fields[number]['key'];
const draft = reactive<Record<Key, string>>({ pitch_shift: '', index_rate: '', rms_mix_rate: '', protect: '' });
const dirty = reactive<Record<Key, boolean>>({ pitch_shift: false, index_rate: false, rms_mix_rate: false, protect: false });
watch(() => props.params, (params) => {
  if (!params) return;
  for (const { key } of fields) {
    const value = String(params[key]);
    if (!dirty[key] || draft[key] === value) { draft[key] = value; dirty[key] = false; }
  }
}, { immediate: true, deep: true });
function change(key: Key, event: Event) { draft[key] = (event.target as HTMLInputElement).value; dirty[key] = true; }
</script>

<template>
  <section class="card">
    <div class="card-head"><h2>参数</h2><span class="chip mono">{{ modelId ?? '未加载模型' }}</span></div>
    <p class="muted">F0 方法：{{ params?.f0_method ?? '未知' }}（只读）</p>
    <form v-for="field in fields" :key="field.key" class="field" @submit.prevent="emit('set', 'rvc.' + field.key, draft[field.key])">
      <label :for="'param-' + field.key">{{ field.label }}</label>
      <small class="muted">当前：{{ params?.[field.key] ?? '未知' }}</small>
      <div class="input-action"><input :id="'param-' + field.key" type="number" step="any" required :value="draft[field.key]" :disabled="busy || !ready || !params" @input="change(field.key, $event)"><button class="btn" type="submit" :disabled="busy || !ready || !params || draft[field.key] === ''">应用</button></div>
      <small v-if="dirty[field.key]" class="muted">未应用</small>
    </form>
  </section>
</template>
