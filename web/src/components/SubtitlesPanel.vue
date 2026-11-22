<script setup lang="ts">
import { computed } from 'vue';
import type { ConnectionState, SubtitleRecord } from '../contracts';
const props = defineProps<{ records: SubtitleRecord[]; state: ConnectionState; error: string }>();
const current = computed(() => props.records.at(-1));
const recent = computed(() => props.records.slice(0, -1).reverse());
</script>

<template>
  <section class="card subtitles">
    <div class="card-head"><h2>字幕</h2><span class="chip" :class="state === 'online' ? 'chip-ok' : 'chip-warn'">{{ state === 'online' ? '已连接' : state === 'connecting' ? '连接中' : '离线' }}</span></div>
    <p v-if="error" class="banner banner-error">{{ error }}</p>
    <div v-if="current" class="subtitle-item" :data-utterance-id="current.utterance_id" :data-revision="current.revision">
      <p>{{ current.zh || '识别中…' }}</p><p class="subtitle-translation">{{ current.en }}</p>
      <p v-if="current.translation_status === 'failed'" class="state-err">{{ current.translation_error || '翻译失败' }}</p>
    </div>
    <p v-else class="muted">暂无字幕</p>
    <details v-if="recent.length">
      <summary>最近字幕（{{ recent.length }}）</summary>
      <div v-for="record in recent" :key="record.utterance_id" class="subtitle-item" :data-utterance-id="record.utterance_id" :data-revision="record.revision">
        <p>{{ record.zh }}</p><p class="subtitle-translation">{{ record.en }}</p>
        <p v-if="record.translation_status === 'failed'" class="state-err">{{ record.translation_error || '翻译失败' }}</p>
      </div>
    </details>
  </section>
</template>
