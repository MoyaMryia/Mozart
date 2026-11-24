<script setup lang="ts">
import { computed } from 'vue';
import type { Status } from '../contracts';

// 占用数字随状态推送变化，独立渲染，根组件不因此重渲染。
const props = defineProps<{ online: boolean; ready: boolean; status: Status | null }>();
const stats = computed(() => props.ready ? props.status?.hw : null);
// 首次拿到监控数据后固定占用值槽宽度，之后不再随数字长短或断连伸缩。
const statsSeen = computed(() => !!props.status);
const memory = computed(() => stats.value ? `${(stats.value.memory.used_bytes / 1024 ** 3).toFixed(1)} / ${(stats.value.memory.total_bytes / 1024 ** 3).toFixed(1)} GiB` : '—');
const gpu = computed(() => {
  if (!stats.value?.gpu.available) return 'N/A';
  return stats.value.gpu.load_percent == null ? '—' : `${stats.value.gpu.load_percent.toFixed(0)}%`;
});
const stateClass = (online: boolean) => `font-mono text-tiny font-bold ${online ? 'state-ok' : 'state-err'}`;
</script>

<template>
  <div class="order-3 md:order-none w-full md:w-auto min-w-0 flex flex-wrap md:flex-nowrap items-center gap-2" role="group" aria-label="连接与资源">
    <div data-status="api" class="hardware-stat flex items-center gap-2 shrink-0 px-3 py-1">
      <svg class="hardware-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><rect width="20" height="8" x="2" y="2" rx="2" ry="2"/><rect width="20" height="8" x="2" y="14" rx="2" ry="2"/><path d="M6 6h.01M6 18h.01M10 6h8m-8 12h8"/></svg>
      <span class="hw-label">API</span> <span :class="[stateClass(online), 'inline-block min-w-[3em] text-right']">{{ online ? '在线' : '未连接' }}</span>
    </div>
    <div class="hardware-stat flex items-center gap-2 shrink-0 px-3 py-1">
      <svg class="hardware-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/></svg>
      <span class="hw-label">CPU</span> <strong class="font-mono text-xs text-ink tabular-nums" :class="statsSeen ? 'inline-block min-w-[4ch] text-right' : ''">{{ stats?.cpu_percent == null ? '—' : `${stats.cpu_percent.toFixed(0)}%` }}</strong>
    </div>
    <div class="hardware-stat flex items-center gap-2 shrink-0 px-3 py-1">
      <svg class="hardware-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>
      <span class="hw-label">RAM</span> <strong class="font-mono text-xs text-ink tabular-nums" :class="statsSeen ? 'inline-block min-w-[14ch] text-right' : ''">{{ memory }}</strong>
    </div>
    <div class="hardware-stat hidden md:flex items-center gap-2 shrink-0 px-3 py-1">
      <svg class="hardware-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><rect width="16" height="16" x="4" y="4" rx="2"/><rect width="6" height="6" x="9" y="9" rx="1"/><path d="M9 1v3m6-3v3M9 20v3m6-3v3M20 9h3m-3 6h3M1 9h3m-3 6h3"/></svg>
      <span class="hw-label">GPU</span> <strong class="font-mono text-xs text-ink tabular-nums" :class="statsSeen ? 'inline-block min-w-[4ch] text-right' : ''">{{ gpu }}</strong>
    </div>
  </div>
</template>
