<script setup lang="ts">
import { computed, ref } from 'vue';
import type { FileJob } from '../contracts';
import AlertBanner from './AlertBanner.vue';
import PendingResult from './PendingResult.vue';
import RefreshIcon from './RefreshIcon.vue';

// 文件任务与实时共用右侧选择并已加载的模型；不带 model_id 时后端使用当前模型。
const props = defineProps<{ jobs: FileJob[]; paused: boolean; ready: boolean; busy: boolean; reloading?: boolean; downloadSupported: boolean; error: string; unknowns: string[]; refreshBlocked?: boolean; ackReady: boolean }>();
const emit = defineEmits<{ submit: [file: File, modelId: string | undefined]; cancel: [jobId: string]; pause: [paused: boolean]; refresh: []; download: [jobId: string]; acknowledge: []; dismissError: [] }>();
const dropped = ref('');
const dragging = ref(false);
const uploadDisabled = computed(() => props.busy || !props.ready || !!props.unknowns.length);
const labels: Record<FileJob['state'], string> = { queued: '排队中', processing: '处理中', cancelling: '取消中', completed: '已完成', failed: '失败', cancelled: '已取消' };
const tone: Record<FileJob['state'], string> = { queued: 'text-ink-3', processing: 'state-warn', cancelling: 'state-warn', completed: 'state-ok', failed: 'state-err', cancelled: 'text-ink-4' };
// 选择或拖入即开始转换；清空输入以便再次选择同一个文件。
function pick(event: Event) {
  dragging.value = false;
  const input = event.target as HTMLInputElement;
  const chosen = input.files?.[0];
  dropped.value = chosen?.name ?? '';
  input.value = '';
  if (chosen && !props.unknowns.length) emit('submit', chosen, undefined);
}
function onDragEnter() { if (!uploadDisabled.value) dragging.value = true; }
function onDragLeave(event: DragEvent) {
  const current = event.currentTarget as HTMLElement | null;
  if (!current || !current.contains(event.relatedTarget as Node | null)) dragging.value = false;
}
function onDrop(event: DragEvent) {
  dragging.value = false;
  if (uploadDisabled.value) event.preventDefault();
}
</script>

<template>
  <section class="card p-5 md:p-4 space-y-4" aria-label="文件转换">
    <div class="flex items-center gap-2 min-w-0">
      <h2 class="text-base md:text-lg font-extrabold text-ink shrink-0">文件转换</h2>
      <div class="ml-auto flex items-center gap-0.5 md:gap-1 shrink-0">
        <button type="button" class="icon-button" :aria-label="paused ? '继续队列' : '暂停队列'" :title="paused ? '继续队列' : '暂停队列'" :disabled="busy || !ready || !!unknowns.length" @click="emit('pause', !paused)">
          <svg v-if="paused" class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M7 4.5v15l12-7.5-12-7.5Z"/></svg>
          <svg v-else class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/></svg>
        </button>
        <button type="button" class="icon-button" aria-label="刷新" title="刷新队列" :disabled="busy || reloading || refreshBlocked" @click="emit('refresh')"><RefreshIcon :spinning="reloading" /></button>
      </div>
    </div>
    <AlertBanner :message="error" dismissible @dismiss="emit('dismissError')" />
    <PendingResult :messages="unknowns" :enabled="ackReady" :busy="busy" label="已核对队列" @acknowledge="emit('acknowledge')" />
    <label
      class="dropzone relative py-8 px-5 md:p-6 flex flex-col items-center justify-center cursor-pointer"
      :class="{ 'opacity-50 cursor-not-allowed': uploadDisabled, 'is-dragging': dragging }"
      @dragenter.prevent="onDragEnter"
      @dragover.prevent="onDragEnter"
      @dragleave="onDragLeave"
      @drop="onDrop"
    >
      <input type="file" accept="audio/*" aria-label="待转换音频" required class="absolute inset-0 w-full h-full opacity-0" :class="uploadDisabled ? 'cursor-not-allowed' : 'cursor-pointer'" :disabled="uploadDisabled" @change="pick">
      <svg class="dropzone-icon w-8 h-8 text-ink-3 mb-2" aria-hidden="true" fill="currentColor" viewBox="0 0 24 24">
        <path d="M5 16h2v2h10v-2h2v2.25A2.75 2.75 0 0 1 16.25 21h-8.5A2.75 2.75 0 0 1 5 18.25V16Zm7-13a1 1 0 0 1 1 1v8.59l2.29-2.3 1.42 1.42-4.42 4.42-4.42-4.42 1.42-1.42 2.29 2.3V4a1 1 0 0 1 1-1Z"/>
      </svg>
      <span class="block text-xs font-bold text-ink mb-1 text-center">点击或拖拽音频文件开始转换</span>
      <span class="block text-tiny text-ink-3 max-w-full truncate">{{ dropped || '未选择文件' }}</span>
    </label>
    <div>
      <div class="flex items-center justify-between gap-2 mb-2">
        <div class="flex items-center gap-2 min-w-0">
          <h3 class="text-xs font-extrabold tracking-wider text-ink uppercase section-title">处理队列</h3>
          <span v-if="paused" class="chip chip-warn">已暂停</span>
        </div>
        <span class="text-micro font-mono text-ink-3 tabular-nums">{{ jobs.length }} 项</span>
      </div>
      <ul class="bg-surface border-2 border-[color:var(--edge)] rounded-xl card-shadow overflow-hidden max-h-[280px] overflow-y-auto list-none">
        <li v-if="!jobs.length" class="px-3 py-6 text-center text-xs text-ink-3">暂无任务</li>
        <li v-for="job in jobs" :key="job.job_id" class="border-b border-edge-soft last:border-b-0">
          <div class="px-3.5 py-3.5 md:px-3 md:py-2.5">
            <div class="flex items-center gap-2.5 md:gap-3">
              <div class="min-w-0 flex-1">
                <div class="text-sm font-bold text-ink truncate">{{ job.name }}</div>
                <div class="text-micro text-ink-3 tabular-nums mt-0.5 truncate"><span>模型 {{ job.model_id }}</span> · {{ job.progress.stage }} · {{ job.progress.percent }}%<template v-if="job.progress.blocks_done !== null && job.progress.blocks_total !== null"> · {{ job.progress.blocks_done }}/{{ job.progress.blocks_total }}</template></div>
              </div>
              <span :class="['text-xs font-bold shrink-0 min-w-[3.25rem] text-right', tone[job.state]]">{{ labels[job.state] }}</span>
              <span class="flex w-10 md:w-8 shrink-0 items-center justify-center">
                <button v-if="job.state === 'queued' || job.state === 'processing'" class="icon-button" :aria-label="'取消 ' + job.name" title="取消任务" :disabled="busy || !ready || !!unknowns.length" @click="emit('cancel', job.job_id)">
                  <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>
                </button>
                <button v-else-if="job.state === 'completed' && job.result_ready && downloadSupported" class="icon-button" aria-label="下载" title="下载结果" :disabled="busy || !ready" @click="emit('download', job.job_id)">
                  <svg class="w-4 h-4" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/></svg>
                </button>
                <span v-else-if="job.state === 'completed' && job.result_ready" class="text-micro text-ink-4" title="下载未接入">—</span>
              </span>
            </div>
            <AlertBanner v-if="job.error" :message="job.error" class="mt-2" />
          </div>
          <progress v-if="job.state === 'processing' || job.state === 'cancelling'" class="queue-progress" :value="job.progress.percent" max="100">{{ job.progress.percent }}%</progress>
        </li>
      </ul>
    </div>
  </section>
</template>
