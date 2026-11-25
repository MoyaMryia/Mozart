<script setup lang="ts">
defineProps<{ spinning?: boolean }>();
</script>

<template>
  <svg :class="['w-4 h-4', { 'icon-spin': spinning }]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M21 12a9 9 0 1 1-2.64-6.36L21 8"/><path d="M21 3v5h-5"/></svg>
</template>
