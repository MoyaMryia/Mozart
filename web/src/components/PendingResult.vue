<script setup lang="ts">
defineProps<{ messages: string[]; enabled: boolean; busy: boolean; label?: string }>();
const emit = defineEmits<{ acknowledge: [] }>();
</script>

<template>
  <div v-if="messages.length" class="banner banner-warn stack" role="alert">
    <p v-for="(message, index) in messages" :key="index">{{ message }}</p>
    <button class="btn" :disabled="busy || !enabled" @click="emit('acknowledge')">{{ label ?? '已核对状态' }}</button>
  </div>
</template>
