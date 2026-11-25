<script setup lang="ts">
import { computed, onUnmounted, reactive, watch } from 'vue';
import type { RvcParams } from '../contracts';
import AlertBanner from './AlertBanner.vue';

const props = defineProps<{ params: RvcParams | null; modelId: string | null; ready: boolean; blocked: boolean; error?: string; embedded?: boolean; presetDirty: boolean; presetBusy: boolean }>();
const emit = defineEmits<{ set: [key: string, value: string]; save: []; restore: []; dismissError: [] }>();
const fields = [
  { key: 'pitch_shift', label: '音高增益', step: 1 },
  { key: 'index_rate', label: '贴合音色比例', step: 0.05 },
  { key: 'rms_mix_rate', label: '音量跟随', step: 0.05 },
  { key: 'protect', label: '清辅音保护', step: 0.05 },
] as const;
type Key = typeof fields[number]['key'];
// 调节后停顿片刻即应用；连点上下箭头只写一次最新值。
const APPLY_MS = 200;
const draft = reactive<Record<Key, string>>({ pitch_shift: '', index_rate: '', rms_mix_rate: '', protect: '' });
const dirty = reactive<Record<Key, boolean>>({ pitch_shift: false, index_rate: false, rms_mix_rate: false, protect: false });
// 已发出但未确认的值：报错时只回滚这些字段，不抹掉其他字段正在输入的内容。
const submitted = reactive<Record<Key, string | undefined>>({ pitch_shift: undefined, index_rate: undefined, rms_mix_rate: undefined, protect: undefined });
const timers = new Map<Key, ReturnType<typeof setTimeout>>();
const live = (key: Key) => props.params ? String(props.params[key]) : null;
const valid = (value: string) => value.trim() !== '' && Number.isFinite(Number(value));
const reset = (key: Key) => { const value = live(key); if (value === null) return; draft[key] = value; dirty[key] = false; submitted[key] = undefined; };
const pending = computed(() => fields.some(({ key }) => dirty[key]));
watch(() => props.params, (params) => {
  if (!params) return;
  for (const { key } of fields) {
    const value = String(params[key]);
    // 数值相等即视为已确认，吸收服务端的规范化格式。
    if (!dirty[key] || (valid(draft[key]) && Number(draft[key]) === Number(value))) {
      draft[key] = value;
      dirty[key] = false;
      submitted[key] = undefined;
    }
  }
}, { immediate: true, deep: true });
watch(() => props.error, error => {
  if (!error) return;
  for (const { key } of fields) if (dirty[key] && submitted[key] !== undefined && draft[key] === submitted[key]) reset(key);
});
// 结果未知期间输入不可编辑；解除后重新提交仍然挂起的编辑。
watch(() => props.blocked, blocked => {
  if (blocked) return;
  for (const { key } of fields) if (dirty[key] && valid(draft[key])) schedule(key);
});
function apply(key: Key) {
  timers.delete(key);
  const value = draft[key]; const target = live(key);
  if (target === null || !valid(value)) return;
  if (Number(value) === Number(target) && submitted[key] === undefined) {
    draft[key] = target;
    dirty[key] = false;
    return;
  }
  submitted[key] = value;
  emit('set', 'rvc.' + key, value);
}
function schedule(key: Key) { clearTimeout(timers.get(key)); timers.set(key, setTimeout(() => apply(key), APPLY_MS)); }
function change(key: Key, event: Event) { draft[key] = (event.target as HTMLInputElement).value; dirty[key] = true; schedule(key); }
function nudge(key: Key, delta: number) {
  if (!props.params) return;
  const base = valid(draft[key]) ? Number(draft[key]) : props.params[key];
  const next = Number((base + delta).toFixed(2));
  draft[key] = String(next);
  dirty[key] = true;
  schedule(key);
}
function blur(key: Key) {
  const hadTimer = timers.has(key);
  clearTimeout(timers.get(key)); timers.delete(key);
  if (!valid(draft[key])) { reset(key); return; }
  if (hadTimer && dirty[key]) apply(key);
}
onUnmounted(() => { for (const timer of timers.values()) clearTimeout(timer); });
</script>

<template>
  <section :class="embedded ? 'border-t border-edge-soft pt-4 md:pt-3 mt-4 md:mt-3' : 'card p-5 md:p-3'" aria-label="变声参数">
    <div class="flex items-center gap-2 mb-2.5 md:mb-2 min-w-0">
      <h2 class="text-sm font-extrabold tracking-wider text-ink uppercase section-title shrink-0">参数</h2>
      <!-- 与 default 预设不同时才出现：保存为默认、恢复默认。 -->
      <div class="preset-actions" :class="{ 'is-visible': presetDirty }" :aria-hidden="!presetDirty">
        <button type="button" class="icon-button icon-button-accent" aria-label="保存为 default 预设" title="把当前参数保存为 default 预设" :disabled="!presetDirty || presetBusy || pending || !ready" @click="emit('save')">
          <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"/><path d="M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"/><path d="M7 3v4a1 1 0 0 0 1 1h7"/></svg>
        </button>
        <button type="button" class="icon-button" aria-label="恢复 default 预设" title="重新载入 default 预设" :disabled="!presetDirty || presetBusy || pending || !ready" @click="emit('restore')">
          <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></svg>
        </button>
      </div>
      <span v-if="!embedded" class="text-micro font-mono font-bold text-ink-2 bg-surface border border-edge rounded-md px-2 py-1 max-w-[60%] truncate ml-auto shrink-0" translate="no">{{ modelId ?? '未加载模型' }}</span>
    </div>
    <AlertBanner :message="error" dismissible class="mb-3 md:mb-2.5" @dismiss="emit('dismissError')" />
    <p class="text-xs md:text-micro text-ink-3 mb-2.5 md:mb-2">F0 方法：{{ params?.f0_method ?? '未知' }}（只读）</p>
    <form v-for="field in fields" :key="field.key" class="grid grid-cols-[1fr_auto] items-center gap-3 py-3 md:py-2 border-b border-edge last:border-b-0" @submit.prevent>
      <label :for="'param-' + field.key" class="text-sm md:text-xs font-bold text-ink-2 truncate">{{ field.label }}</label>
      <div class="param-stepper">
        <button type="button" class="stepper-btn md:hidden" :aria-label="'减小' + field.label" :disabled="blocked || !ready || !params" @click="nudge(field.key, -field.step)">−</button>
        <input :id="'param-' + field.key" type="number" inputmode="decimal" step="any" required class="field w-20 md:w-24 h-11 md:h-8 text-center md:text-right font-mono text-base md:text-sm font-extrabold px-2 tabular-nums" :value="draft[field.key]" :disabled="blocked || !ready || !params" @input="change(field.key, $event)" @blur="blur(field.key)">
        <button type="button" class="stepper-btn md:hidden" :aria-label="'增大' + field.label" :disabled="blocked || !ready || !params" @click="nudge(field.key, field.step)">+</button>
      </div>
    </form>
  </section>
</template>
