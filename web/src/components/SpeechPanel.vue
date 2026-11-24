<script setup lang="ts">
import { computed } from 'vue';
import type { Status } from '../contracts';
import { requestCheckbox } from '../inputs';
const props = defineProps<{ status: Status | null; ready: boolean; busy: boolean; error?: string }>();
const actual = computed(() => ({ translate: props.status?.bridge?.translate_enabled ?? false, speech: props.status?.bridge?.speech_enabled ?? false }));
// 只在服务不可用时提示；bridge 的 *connected 是“最近一次请求结果”，空闲时为 false，不能当连接状态。
const missing = computed(() => !props.status?.bridge ? '未连接'
  : [props.status.processes.llama?.state !== 'running' ? '翻译服务' : '', props.status.processes.tts?.state !== 'running' ? '播报服务' : ''].filter(Boolean).join('、'));
const emit = defineEmits<{ translate: [enabled: boolean]; speech: [enabled: boolean]; stop: [] }>();
</script>

<template>
  <section class="card p-4 space-y-3" aria-label="翻译播报">
    <div class="flex items-center gap-3 min-w-0">
      <h2 class="text-lg font-extrabold text-ink shrink-0">翻译播报</h2>
      <span v-if="error" class="banner banner-error text-micro min-w-0 truncate" role="alert" :title="error">{{ error }}</span>
      <span v-if="missing" class="text-xs font-mono text-ink-4 ml-auto shrink-0">{{ missing === '未连接' ? '未连接' : `${missing}未连接` }}</span>
    </div>
    <div class="flex flex-wrap gap-4">
      <label class="text-xs text-ink-2 flex gap-2 items-center"><input type="checkbox" class="accent-[var(--ink)]" :checked="actual.translate" :disabled="busy || !ready || !status?.bridge" @change="requestCheckbox($event, actual.translate, value => emit('translate', value))"> 翻译</label>
      <label class="text-xs text-ink-2 flex gap-2 items-center"><input type="checkbox" class="accent-[var(--ink)]" :checked="actual.speech" :disabled="busy || !ready || !status?.bridge" @change="requestCheckbox($event, actual.speech, value => emit('speech', value))"> 播报</label>
    </div>
    <template v-if="status?.bridge">
      <p class="text-xs text-ink-3">翻译队列 {{ status.bridge.translate_queue_depth }} · 播报队列 {{ status.bridge.speech_queue_depth }} · 失败 {{ status.bridge.translate_failed }}</p>
      <p v-for="reason in status.bridge.degraded" :key="reason" class="banner banner-warn">{{ reason }}</p>
    </template>
    <div class="flex items-end justify-between gap-3">
      <p class="text-micro text-ink-4 min-w-0">音色上传、试听与术语表编辑、清空尚未接入。</p>
      <button type="button" class="btn-trash shrink-0" :disabled="busy || !ready || !status?.bridge" @click="emit('stop')">
        <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M10 11v6"/><path d="M14 11v6"/></svg>
        停止播报
      </button>
    </div>
  </section>
</template>