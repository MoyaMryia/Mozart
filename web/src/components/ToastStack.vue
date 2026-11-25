<script setup lang="ts">
export interface ToastItem {
  id: string;
  tone: 'error' | 'warn' | 'info';
  message: string;
  dismissible?: boolean;
}

defineProps<{ items: ToastItem[] }>();
const emit = defineEmits<{ dismiss: [id: string] }>();
</script>

<template>
  <TransitionGroup name="toast-slide" tag="div" class="toast-stack" aria-live="polite">
    <div
      v-for="item in items"
      :key="item.id"
      :class="['toast', item.tone === 'error' ? 'toast-error' : item.tone === 'warn' ? 'toast-warn' : 'toast-info']"
      :role="item.tone === 'error' ? 'alert' : 'status'"
    >
      <svg v-if="item.tone === 'error'" class="toast-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
        <circle cx="12" cy="12" r="10"/><path d="M12 8v4"/><path d="M12 16h.01"/>
      </svg>
      <svg v-else-if="item.tone === 'warn'" class="toast-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
        <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/>
        <path d="M12 9v4"/><path d="M12 17h.01"/>
      </svg>
      <svg v-else class="toast-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
        <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><path d="m9 11 3 3L22 4"/>
      </svg>
      <span class="toast-text">{{ item.message }}</span>
      <button
        v-if="item.dismissible"
        type="button"
        class="toast-close"
        aria-label="关闭通知"
        title="关闭"
        @click="emit('dismiss', item.id)"
      >
        <svg class="w-3.5 h-3.5" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
          <path d="M18 6 6 18"/><path d="m6 6 12 12"/>
        </svg>
      </button>
    </div>
  </TransitionGroup>
</template>
