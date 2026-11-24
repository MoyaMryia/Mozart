<script setup lang="ts">
import { computed } from 'vue';
import type { ConnectionState, SubtitleRecord } from '../contracts';
const props = defineProps<{ records: SubtitleRecord[]; state: ConnectionState }>();
const current = computed(() => props.records.at(-1));
// 推送在线即视为激活：SUB 变为墨色；否则保持浅灰。
const active = computed(() => props.state === 'online');
</script>

<template>
  <!-- 默认收起；有字幕时在两张大卡片之间弹出。 -->
  <section v-if="current" class="subtitles px-1 py-2" aria-label="实时字幕" :data-online="active">
    <div class="flex items-center gap-3">
      <h2 :class="['text-lg font-extrabold tracking-tight font-mono shrink-0', active ? 'text-ink' : 'text-ink-4']" translate="no">SUB</h2>
    </div>
    <div class="subtitle-item mt-1 space-y-1 text-center" :data-utterance-id="current.utterance_id" :data-revision="current.revision">
      <p :class="['text-xl font-bold leading-relaxed whitespace-pre-wrap break-words', active ? 'text-ink' : 'text-ink-4']">{{ current.zh || '识别中…' }}</p>
      <p :class="['subtitle-translation text-base leading-relaxed whitespace-pre-wrap break-words', active ? 'text-ink-2' : 'text-ink-4']">{{ current.en }}</p>
      <p v-if="current.translation_status === 'failed'" class="text-tiny state-err">{{ current.translation_error || '翻译失败' }}</p>
    </div>
  </section>
</template>