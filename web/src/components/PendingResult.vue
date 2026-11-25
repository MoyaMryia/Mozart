<script setup lang="ts">
defineProps<{ messages: string[]; enabled: boolean; busy: boolean; label?: string }>();
const emit = defineEmits<{ acknowledge: [] }>();
</script>

<template>
  <Transition name="alert-fade">
    <div v-if="messages.length" class="pending-box" role="alert">
      <div class="flex items-start gap-2.5">
        <svg class="alert-icon mt-0.5" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
          <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/>
          <path d="M12 9v4"/><path d="M12 17h.01"/>
        </svg>
        <div class="flex-1 min-w-0 space-y-1">
          <p v-for="(message, index) in messages" :key="index" class="alert-text">{{ message }}</p>
        </div>
      </div>
      <div class="mt-2.5 flex justify-end">
        <button type="button" class="btn btn-outline btn-sm min-h-[36px] md:min-h-0" :disabled="busy || !enabled" @click="emit('acknowledge')">{{ label ?? '已核对状态' }}</button>
      </div>
    </div>
  </Transition>
</template>
