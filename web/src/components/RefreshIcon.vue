<script setup lang="ts">
withDefaults(defineProps<{ spinning?: boolean; sizeClass?: string }>(), {
  sizeClass: 'w-5 h-5 md:w-4 md:h-4',
});
</script>

<template>
  <svg :class="[sizeClass, { 'icon-spin': spinning }]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24"><path d="M21 12a9 9 0 1 1-2.64-6.36L21 8"/><path d="M21 3v5h-5"/></svg>
</template>
