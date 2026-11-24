<script setup lang="ts">
import { ref } from 'vue';
import LogTail from './LogTail.vue';
import type { ConnectionState, Status } from '../contracts';

// 推送连接状态与推理计数随状态推送变化，独立渲染，根组件不因此重渲染。
defineProps<{ status: Status | null; ready: boolean; statusStreamState: ConnectionState; statusStreamError: string; subtitleState: ConnectionState; subtitleError: string }>();
const open = ref(false);
</script>

<template>
  <details class="console-log shrink-0 rounded-lg overflow-hidden border border-edge" @toggle="open = ($event.target as HTMLDetailsElement).open">
    <summary class="console-header cursor-pointer px-3 py-2 text-xs font-bold">INFO</summary>
    <div class="console-entries p-3 space-y-1.5 text-[12px] leading-5 font-mono">
      <p class="text-gray-300 break-words"><span class="text-emerald-400 font-bold">&gt;</span> 状态推送：{{ statusStreamState }}<template v-if="statusStreamError"> · {{ statusStreamError }}</template></p>
      <p class="text-gray-300 break-words"><span class="text-emerald-400 font-bold">&gt;</span> 字幕推送：{{ subtitleState }}<template v-if="subtitleError"> · {{ subtitleError }}</template></p>
      <p v-if="!ready" class="text-gray-300"><span class="text-emerald-400 font-bold">&gt;</span> 当前业务状态未知，操作暂停。</p>
      <p v-else-if="status?.rvc" class="text-gray-300 break-all"><span class="text-emerald-400 font-bold">&gt;</span> RVC {{ status.rvc.model_id ?? '未加载' }} · {{ status.rvc.state }} · {{ status.rvc.frames_in }}/{{ status.rvc.frames_out }} 帧 · 丢弃 {{ status.rvc.frames_dropped }}</p>
      <p v-if="ready && status?.rvc?.inference_ms_p95 != null" class="text-gray-300"><span class="text-emerald-400 font-bold">&gt;</span> 推理 P95：{{ status.rvc.inference_ms_p95.toFixed(1) }} ms</p>
    </div>
    <LogTail :active="open" />
  </details>
</template>