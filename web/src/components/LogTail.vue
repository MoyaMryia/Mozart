<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { ApiError, client } from '../api';

// 日志尾随独立渲染：INFO 面板里的连接与计数刷新不会带着下拉框重渲染。
const props = defineProps<{ active: boolean }>();
const services = ['rvc', 'pre', 'daemon', 'asr', 'tts', 'bridge', 'llama', 'web'] as const;
const logService = ref<typeof services[number]>('rvc');
const logTail = ref('');
const logError = ref('');
const logBox = ref<HTMLElement | null>(null);
const REFRESH_MS = 2000;
let timer: ReturnType<typeof setInterval> | undefined;

async function load() {
  try {
    const value = await client.logTail(logService.value);
    logTail.value = value.tail;
    logError.value = '';
  } catch (error) {
    if (!(error instanceof ApiError)) throw error;
    logError.value = `${error.code}：${error.message}`;
  }
}
function stop() { if (timer !== undefined) { clearInterval(timer); timer = undefined; } }
function start() { stop(); void load(); timer = setInterval(() => void load(), REFRESH_MS); }
watch(() => props.active, active => { if (active) start(); else stop(); }, { immediate: true });
watch(logService, () => { if (props.active) void load(); });
watch(logTail, () => nextTick(() => { if (logBox.value) logBox.value.scrollTop = logBox.value.scrollHeight; }));
onBeforeUnmount(stop);
</script>

<template>
  <div class="border-t border-[var(--console-edge)] px-3 py-2 space-y-1.5">
    <div class="flex items-center gap-2 min-w-0">
      <label for="log-service" class="sr-only">服务日志</label>
      <select id="log-service" v-model="logService" class="log-select shrink-0">
        <option v-for="service in services" :key="service" :value="service">{{ service }}</option>
      </select>
      <span v-if="logError" class="text-micro state-err min-w-0 truncate" :title="logError">{{ logError }}</span>
      <span v-else class="text-micro text-ink-4">每 2 秒刷新</span>
    </div>
    <pre ref="logBox" class="log-tail">{{ logTail || '展开面板后开始显示日志尾部。' }}</pre>
  </div>
</template>