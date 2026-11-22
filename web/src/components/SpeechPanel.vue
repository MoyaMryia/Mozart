<script setup lang="ts">
import { computed } from 'vue';
import type { Status } from '../contracts';
import { requestCheckbox } from '../inputs';
const props = defineProps<{ status: Status | null; ready: boolean; busy: boolean }>();
const actual = computed(() => ({ translate: props.status?.bridge?.translate_enabled ?? false, speech: props.status?.bridge?.speech_enabled ?? false }));
const emit = defineEmits<{ translate: [enabled: boolean]; speech: [enabled: boolean]; stop: [] }>();
</script>

<template>
  <section class="card">
    <div class="card-head"><h2>翻译播报</h2></div>
    <p v-if="!status?.bridge" class="banner banner-warn">桥状态未知</p>
    <label class="check"><input type="checkbox" :checked="actual.translate" :disabled="busy || !ready || !status?.bridge" @change="requestCheckbox($event, actual.translate, value => emit('translate', value))"> 翻译</label>
    <label class="check"><input type="checkbox" :checked="actual.speech" :disabled="busy || !ready || !status?.bridge" @change="requestCheckbox($event, actual.speech, value => emit('speech', value))"> 播报</label>
    <template v-if="status?.bridge">
      <p class="muted">翻译服务：{{ status.bridge.llama_connected ? '已连接' : '未连接' }} · 播报服务：{{ status.bridge.tts_connected ? '已连接' : '未连接' }}</p>
      <p class="muted">翻译队列 {{ status.bridge.translate_queue_depth }} · 播报队列 {{ status.bridge.speech_queue_depth }} · 失败 {{ status.bridge.translate_failed }}</p>
      <p v-for="reason in status.bridge.degraded" :key="reason" class="banner banner-warn">{{ reason }}</p>
    </template>
    <button class="btn" :disabled="busy || !ready || !status?.bridge" @click="emit('stop')">停止播报</button>
    <p class="muted">音色上传、试听和术语尚未接入。</p>
  </section>
</template>
