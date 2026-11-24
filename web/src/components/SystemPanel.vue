<script setup lang="ts">
import { services } from '../contracts';
import RefreshIcon from './RefreshIcon.vue';
import type { Service, Status } from '../contracts';

const props = defineProps<{ status: Status | null; connected: boolean; busy: boolean; writeBlocked?: boolean }>();
const emit = defineEmits<{ restart: [service: Service] }>();
// 运行状态用一个小号填充圆点表示：绿色运行，黄色启停中，灰色停止或未知。
const stateOf = (service: Service) => props.connected && props.status ? props.status.processes[service]?.state ?? 'unknown' : 'unknown';
const toneOf = (service: Service) => {
  const state = stateOf(service);
  return state === 'running' ? 'state-ok' : state === 'starting' || state === 'restarting' ? 'state-warn' : 'text-ink-4';
};
</script>

<template>
  <section class="card p-3" aria-label="服务">
    <div class="flex items-center justify-between gap-2 mb-1">
      <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title">服务</h2>
      <span v-if="!connected || !status" class="text-micro font-mono text-ink-4">未连接</span>
    </div>

    <p v-if="connected && status?.pending_restart.length" class="text-micro text-ink-3 mb-1">待重启：{{ status.pending_restart.join('、') }}</p>
    <p v-if="connected && status?.pending_restart_keys.length" class="text-micro text-ink-4 font-mono mb-1 break-all">{{ status.pending_restart_keys.join('、') }}</p>

    <ul class="list-none">
      <li v-for="service in services" :key="service" class="py-1.5 border-t border-edge-soft first:border-t-0">
        <div class="flex items-center gap-2">
          <span class="font-mono text-sm font-bold text-ink">{{ service }}</span>
          <svg :data-service="service" :data-state="stateOf(service)" :class="['h-2.5 w-2.5 shrink-0', toneOf(service)]" fill="currentColor" aria-hidden="true" viewBox="0 0 8 8"><circle cx="4" cy="4" r="4"/></svg>
          <span v-if="connected && status?.pending_restart.includes(service)" class="text-micro font-bold state-warn">待重启</span>
          <button class="icon-button ml-auto shrink-0" :aria-label="'重启 ' + service" :title="'重启 ' + service" :disabled="busy || writeBlocked || !connected || !status?.processes[service]" @click="emit('restart', service)"><RefreshIcon /></button>
        </div>
        <p v-if="connected && status?.processes[service]?.last_error" class="text-micro state-err mt-0.5 break-words">{{ status.processes[service]?.last_error }}</p>
      </li>
    </ul>

    <p v-if="connected && status?.enhance" class="text-micro text-ink-3 mt-2">增强后端：{{ status.enhance.backend }} · 输入 {{ status.enhance.frames_in }} · 输出 {{ status.enhance.frames_out }} · 会话 {{ status.enhance.sessions }} · 算法延迟 {{ status.enhance.algorithm_delay_samples }} 采样</p>
    <p v-if="connected && status?.enhance?.last_error" class="text-micro state-err mt-1">{{ status.enhance.last_error }}</p>
    <p class="mt-2 text-micro text-ink-3">GPU 占用：{{ connected && status ? (status.arbiter.gpu.owner ?? '空闲') : '空闲' }}<template v-if="connected && status?.hw?.gpu.available && status.hw.gpu.load_percent !== null"> · {{ status.hw.gpu.load_percent }}%</template></p>

    <p v-for="reason in connected && status ? status.degraded : []" :key="reason" class="banner banner-warn mt-2">{{ reason }}</p>
  </section>
</template>