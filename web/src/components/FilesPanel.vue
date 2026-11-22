<script setup lang="ts">
import { ref } from 'vue';
import type { FileJob, ModelEntry } from '../contracts';
import PendingResult from './PendingResult.vue';

const props = defineProps<{ jobs: FileJob[]; paused: boolean; models: ModelEntry[]; ready: boolean; busy: boolean; downloadSupported: boolean; error: string; modelsError?: boolean; unknowns: string[]; ackReady: boolean }>();
const emit = defineEmits<{ submit: [file: File, modelId: string | undefined]; cancel: [jobId: string]; pause: [paused: boolean]; refresh: []; download: [jobId: string]; acknowledge: [] }>();
const file = ref<File | null>(null);
const model = ref('');
const labels: Record<FileJob['state'], string> = { queued: '排队中', processing: '处理中', cancelling: '取消中', completed: '已完成', failed: '失败', cancelled: '已取消' };
function selectFile(event: Event) { file.value = (event.target as HTMLInputElement).files?.[0] ?? null; }
function submit() { if (file.value && !props.unknowns.length && (!model.value || (!props.modelsError && props.models.some(item => item.id === model.value && item.exists)))) emit('submit', file.value, model.value || undefined); }
</script>

<template>
  <section class="card">
    <div class="card-head"><h2>文件转换</h2><button class="btn" :disabled="busy" @click="emit('refresh')">刷新</button></div>
    <p v-if="error" class="banner banner-error">{{ error }}</p>
    <PendingResult :messages="unknowns" :enabled="ackReady" :busy="busy" label="已核对队列" @acknowledge="emit('acknowledge')" />
    <form class="stack" @submit.prevent="submit">
      <label class="file-picker" :class="{ disabled: busy || !ready }"><input class="sr-only" type="file" aria-label="待转换音频" required :disabled="busy || !ready" @change="selectFile"><span class="btn">选择音频</span><span class="muted">{{ file?.name ?? '未选择文件' }}</span></label>
      <label class="field">模型<select v-model="model" :disabled="busy || !ready || modelsError"><option value="">默认</option><option v-for="item in models" :key="item.id" :value="item.id" :disabled="!item.exists">{{ item.id }}{{ item.exists ? '' : '（缺失）' }}{{ item.loaded ? '（已加载）' : '' }}</option></select></label>
      <div class="actions"><button class="btn btn-primary" type="submit" :disabled="busy || !ready || !file || !!unknowns.length || (!!model && (modelsError || !models.some(item => item.id === model && item.exists)))">加入队列</button><button class="btn" type="button" :disabled="busy || !ready || !!unknowns.length" @click="emit('pause', !paused)">{{ paused ? '继续队列' : '暂停队列' }}</button></div>
    </form>
    <p v-if="!jobs.length" class="muted">暂无任务</p>
    <ul v-else class="job-list">
      <li v-for="job in jobs" :key="job.job_id">
        <div class="row"><strong>{{ job.name }}</strong><span class="chip">{{ labels[job.state] }}</span></div>
        <p class="muted">{{ job.progress.stage }} · {{ job.progress.percent }}%<template v-if="job.progress.blocks_done !== null && job.progress.blocks_total !== null"> · {{ job.progress.blocks_done }}/{{ job.progress.blocks_total }}</template></p>
        <progress :value="job.progress.percent" max="100">{{ job.progress.percent }}%</progress>
        <p v-if="job.error" class="state-err">{{ job.error }}</p>
        <div class="actions">
          <button v-if="job.state === 'queued' || job.state === 'processing'" class="btn" :disabled="busy || !ready || !!unknowns.length" @click="emit('cancel', job.job_id)">取消</button>
          <button v-if="job.state === 'completed' && job.result_ready && downloadSupported" class="btn" :disabled="busy || !ready" @click="emit('download', job.job_id)">下载</button>
          <span v-if="job.state === 'completed' && job.result_ready && !downloadSupported" class="muted">下载未接入</span>
        </div>
      </li>
    </ul>
  </section>
</template>
