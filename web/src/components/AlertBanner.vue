<script setup lang="ts">
withDefaults(defineProps<{
  message?: string;
  tone?: 'error' | 'warn';
  dismissible?: boolean;
}>(), {
  tone: 'error',
  dismissible: false,
});
const emit = defineEmits<{ dismiss: [] }>();
</script>

<template>
  <Transition name="alert-fade">
    <div
      v-if="message"
      :class="['alert-box', tone === 'warn' ? 'alert-box-warn' : 'alert-box-error']"
      :role="tone === 'error' ? 'alert' : 'status'"
    >
      <svg v-if="tone === 'warn'" class="alert-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
        <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/>
        <path d="M12 9v4"/><path d="M12 17h.01"/>
      </svg>
      <svg v-else class="alert-icon" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
        <circle cx="12" cy="12" r="10"/><path d="M12 8v4"/><path d="M12 16h.01"/>
      </svg>
      <p class="alert-text">{{ message }}</p>
      <button
        v-if="dismissible"
        type="button"
        class="alert-close"
        aria-label="关闭提示"
        title="关闭提示"
        @click="emit('dismiss')"
      >
        <svg class="w-3.5 h-3.5" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
          <path d="M18 6 6 18"/><path d="m6 6 12 12"/>
        </svg>
      </button>
    </div>
  </Transition>
</template>
