<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import type { ConnectionState, SubtitleRecord } from '../contracts';

const props = defineProps<{ records: SubtitleRecord[]; state: ConnectionState }>();
// 推送在线即视为激活：SUB 变为墨色；否则保持浅灰。
const active = computed(() => props.state === 'online');
const latest = computed(() => props.records.at(-1)?.utterance_id);
const scroller = ref<HTMLElement | null>(null);
// 停留底部时跟随新字幕；用户向上翻看历史时不打断。
const stick = ref(true);
// 滚动窗高度跟随当前句：默认只露出一条，向上滚动才看到历史。
const itemHeight = ref(0);
let observer: ResizeObserver | undefined;

function onScroll() {
  const box = scroller.value;
  if (box) stick.value = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
}
function toBottom() { if (scroller.value) scroller.value.scrollTop = scroller.value.scrollHeight; }
function jumpLatest() { stick.value = true; toBottom(); }
function syncHeight() {
  const last = scroller.value?.lastElementChild as HTMLElement | null;
  itemHeight.value = last?.offsetHeight ?? 0;
}
// 高度先落定，再在下一帧滚到底，否则 scrollTop 按旧高度计算会差一截。
function followLatest() {
  nextTick(() => {
    syncHeight();
    nextTick(() => { if (stick.value) toBottom(); });
  });
}
watch(() => props.records, records => {
  if (!records.length) stick.value = true;
  followLatest();
});
watch(scroller, (el, prev) => {
  if (prev) observer?.unobserve(prev);
  if (el && observer) observer.observe(el);
});
onMounted(() => {
  if (typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver(followLatest);
    if (scroller.value) observer.observe(scroller.value);
  }
  followLatest();
});
onUnmounted(() => { observer?.disconnect(); });
</script>

<template>
  <!-- 默认收起；有字幕时在两张大卡片之间弹出，向上滚动可回看之前的句子。 -->
  <section v-if="records.length" class="subtitles px-1 py-2" aria-label="实时字幕" :data-online="active">
    <div class="flex items-center justify-between gap-3">
      <h2 :class="['text-base md:text-lg font-extrabold tracking-tight font-mono shrink-0', active ? 'text-ink' : 'text-ink-4']" translate="no">SUB</h2>
      <Transition name="alert-fade">
        <button v-if="!stick && records.length > 1" type="button" class="subtitle-jump" @click="jumpLatest">
          <svg class="w-3 h-3" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M12 5v14"/><path d="m19 12-7 7-7-7"/></svg>
          回到最新
        </button>
      </Transition>
    </div>
    <div ref="scroller" class="subtitle-scroll mt-1 text-center" :style="itemHeight ? { height: `${itemHeight}px` } : undefined" @scroll.passive="onScroll">
      <div v-for="record in records" :key="record.utterance_id" class="subtitle-item py-1.5 space-y-0.5" :data-utterance-id="record.utterance_id" :data-revision="record.revision">
        <p :class="[record.utterance_id === latest ? 'text-lg md:text-xl font-bold leading-snug' : 'text-sm font-bold', active ? (record.utterance_id === latest ? 'text-ink' : 'text-ink-3') : 'text-ink-4']">{{ record.zh || '识别中…' }}</p>
        <p :class="['subtitle-translation', record.utterance_id === latest ? 'text-sm md:text-base' : 'text-xs', active ? (record.utterance_id === latest ? 'text-ink-2' : 'text-ink-4') : 'text-ink-4']">{{ record.en }}</p>
        <p v-if="record.translation_status === 'failed'" class="text-tiny state-err">{{ record.translation_error || '翻译失败' }}</p>
      </div>
    </div>
  </section>
</template>
