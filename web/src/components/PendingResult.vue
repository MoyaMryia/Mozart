<script setup lang="ts">
defineProps<{ messages: string[]; enabled: boolean; busy: boolean; label?: string }>();
const emit = defineEmits<{ acknowledge: [] }>();
</script>

<template>
  <div v-if="messages.length" class="space-y-1.5" role="alert">
    <p v-for="(message, index) in messages" :key="index" class="banner banner-warn text-xs">{{ message }}</p>
    <button class="btn btn-outline btn-sm" :disabled="busy || !enabled" @click="emit('acknowledge')">{{ label ?? '已核对状态' }}</button>
  </div>
</template>