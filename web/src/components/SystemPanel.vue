<script setup lang="ts">
import { services } from '../contracts';
import AlertBanner from './AlertBanner.vue';
import RefreshIcon from './RefreshIcon.vue';
import type { Service, Status } from '../contracts';

const props = defineProps<{ status: Status | null; connected: boolean; busy: boolean; writeBlocked?: boolean; error?: string }>();
const emit = defineEmits<{ restart: [service: Service]; dismissError: [] }>();
const stateLabels: Record<string, string> = {
  running: '运行中',
  starting: '启动中',
  restarting: '重启中',
  stopped: '已停止',
  unknown: '未知',
};
const stateOf = (service: Service) => props.connected && props.status ? props.status.processes[service]?.state ?? 'unknown' : 'unknown';
const isTransitional = (service: Service) => {
  const state = stateOf(service);
  return state === 'starting' || state === 'restarting';
};
const toneOf = (service: Service) => stateOf(service) === 'running' ? 'state-ok' : isTransitional(service) ? 'state-warn' : 'text-ink-4';
</script>

<template>
  <section class="card p-5 md:p-3" aria-label="服务">
    <div class="flex items-center justify-between gap-2 mb-2.5 md:mb-1.5">
      <h2 class="text-base font-extrabold tracking-wider text-ink uppercase section-title">服务</h2>
      <span v-if="!connected || !status" class="chip chip-muted">未连接</span>
    </div>

    <AlertBanner :message="error" dismissible class="mb-2.5 md:mb-2" @dismiss="emit('dismissError')" />

    <p v-if="connected && status?.pending_restart.length" class="text-micro text-ink-3 mb-1">待重启：{{ status.pending_restart.join('、') }}</p>
    <p v-if="connected && status?.pending_restart_keys.length" class="text-micro text-ink-4 font-mono mb-1 break-all">{{ status.pending_restart_keys.join('、') }}</p>

    <ul class="list-none">
      <li v-for="service in services" :key="service" class="py-2.5 md:py-1.5 border-t border-edge-soft first:border-t-0">
        <div class="flex items-center gap-2">
          <span class="font-mono text-sm font-bold text-ink">{{ service }}</span>
          <span class="relative inline-flex items-center justify-center shrink-0">
            <svg :data-service="service" :data-state="stateOf(service)" :class="['h-2.5 w-2.5 shrink-0', toneOf(service), { 'dot-pulse': isTransitional(service) }]" fill="currentColor" aria-hidden="true" viewBox="0 0 8 8"><circle cx="4" cy="4" r="4"/></svg>
          </span>
          <span class="text-micro text-ink-3">{{ stateLabels[stateOf(service)] ?? stateOf(service) }}</span>
          <span v-if="connected && status?.pending_restart.includes(service)" class="chip chip-warn">待重启</span>
          <button class="icon-button ml-auto shrink-0" :aria-label="'重启 ' + service" :title="'重启 ' + service" :disabled="busy || writeBlocked || !connected || !status?.processes[service]" @click="emit('restart', service)"><RefreshIcon /></button>
        </div>
        <AlertBanner v-if="connected && status?.processes[service]?.last_error" :message="status.processes[service]?.last_error ?? ''" class="mt-1.5" />
      </li>
    </ul>

    <p v-if="connected && status?.enhance" class="text-micro text-ink-3 mt-2 leading-relaxed">增强后端：{{ status.enhance.backend }} · 输入 {{ status.enhance.frames_in }} · 输出 {{ status.enhance.frames_out }} · 会话 {{ status.enhance.sessions }} · 算法延迟 {{ status.enhance.algorithm_delay_samples }} 采样</p>
    <AlertBanner v-if="connected && status?.enhance?.last_error" :message="status.enhance.last_error" class="mt-2" />
    <p class="mt-2 text-micro text-ink-3 tabular-nums">GPU 占用：{{ connected && status ? (status.arbiter.gpu.owner ?? '空闲') : '空闲' }}<template v-if="connected && status?.hw?.gpu.available && status.hw.gpu.load_percent !== null"> · {{ status.hw.gpu.load_percent }}%</template></p>

    <div v-if="connected && status?.degraded.length" class="space-y-1.5 mt-2">
      <AlertBanner v-for="reason in status.degraded" :key="reason" :message="reason" tone="warn" />
    </div>
  </section>
</template>
