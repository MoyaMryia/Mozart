<script setup lang="ts">
import { computed } from 'vue';
import type { Status } from '../contracts';
import AlertBanner from './AlertBanner.vue';

const props = defineProps<{ status: Status | null; ready: boolean; busy: boolean; error?: string }>();
const emit = defineEmits<{ clear: []; dismissError: [] }>();

// bridge 的 *connected 是最近一次请求结果，空闲时为 false；服务就绪度看监管进程状态。
const missing = computed(() => !props.status?.bridge ? '未连接'
  : [props.status.processes.llama?.state !== 'running' ? '翻译服务' : '', props.status.processes.tts?.state !== 'running' ? '播报服务' : ''].filter(Boolean).join('、'));
const badge = computed(() => {
  if (!props.status?.bridge) return { text: '未连接', tone: 'chip-muted' };
  if (missing.value) return { text: `${missing.value}未就绪`, tone: 'chip-warn' };
  if (props.status.output_mode?.requested === 'broadcast' && props.status.output_mode?.applied !== 'broadcast') {
    return { text: '开启中', tone: 'chip-warn' };
  }
  return { text: '播报中', tone: 'chip-ok' };
});
</script>

<template>
  <section class="card p-5 md:p-3 space-y-3 md:space-y-2.5" aria-label="播报设置">
    <div class="flex items-center gap-2 min-w-0">
      <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title shrink-0">播报设置</h2>
      <span :class="['chip ml-auto shrink-0', badge.tone]">{{ badge.text }}</span>
    </div>
    <AlertBanner :message="error" dismissible @dismiss="emit('dismissError')" />
    <AlertBanner v-if="status?.bridge && !status.bridge.translate_enabled" message="翻译已关闭，当前不会触发播报。" tone="warn" />
    <template v-if="status?.bridge">
      <p class="text-xs md:text-micro text-ink-3 tabular-nums">播报队列 {{ status.bridge.speech_queue_depth }} · 翻译队列 {{ status.bridge.translate_queue_depth }} · 失败 {{ status.bridge.translate_failed }}</p>
      <div v-if="status.bridge.degraded.length" class="space-y-1.5">
        <AlertBanner v-for="reason in status.bridge.degraded" :key="reason" :message="reason" tone="warn" />
      </div>
    </template>
    <button type="button" class="btn btn-danger btn-sm w-full min-h-[46px] md:min-h-0 text-sm md:text-[11px]" :disabled="busy || !ready || !status?.bridge" @click="emit('clear')">
      <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M10 11v6"/><path d="M14 11v6"/></svg>
      <span>清除播报队列</span>
    </button>
  </section>
</template>
