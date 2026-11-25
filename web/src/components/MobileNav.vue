<script lang="ts">
export const MOBILE_SECTIONS = ['live', 'voice', 'files', 'system'] as const;
export type MobileSection = typeof MOBILE_SECTIONS[number];
</script>

<script setup lang="ts">
defineProps<{
  active: MobileSection;
  alerts: Record<MobileSection, boolean>;
  hasSubtitles?: boolean;
}>();
const emit = defineEmits<{ select: [section: MobileSection] }>();

const labels: Record<MobileSection, string> = {
  live: '会话',
  voice: '音色',
  files: '文件',
  system: '系统',
};
const tabs = MOBILE_SECTIONS.map(id => ({ id, label: labels[id] }));
</script>

<template>
  <nav class="mobile-nav md:hidden" aria-label="快捷导航">
    <div class="mobile-nav-inner">
      <button
        v-for="tab in tabs"
        :key="tab.id"
        type="button"
        :class="['mobile-nav-item', { 'is-active': active === tab.id }]"
        :aria-current="active === tab.id ? 'location' : undefined"
        @click="emit('select', tab.id)"
      >
        <span class="relative inline-flex items-center justify-center">
          <svg v-if="tab.id === 'live'" class="w-[18px] h-[18px]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
            <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/>
            <path d="M19 10v2a7 7 0 0 1-14 0v-2"/><line x1="12" x2="12" y1="19" y2="22"/>
          </svg>
          <svg v-else-if="tab.id === 'voice'" class="w-[18px] h-[18px]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
            <line x1="4" x2="4" y1="21" y2="14"/><line x1="4" x2="4" y1="10" y2="3"/>
            <line x1="12" x2="12" y1="21" y2="12"/><line x1="12" x2="12" y1="8" y2="3"/>
            <line x1="20" x2="20" y1="21" y2="16"/><line x1="20" x2="20" y1="12" y2="3"/>
            <line x1="2" x2="6" y1="14" y2="14"/><line x1="10" x2="14" y1="8" y2="8"/><line x1="18" x2="22" y1="16" y2="16"/>
          </svg>
          <svg v-else-if="tab.id === 'files'" class="w-[18px] h-[18px]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
            <path d="M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7.5L14.5 2z"/>
            <polyline points="14 2 14 8 20 8"/><path d="M12 18v-6"/><path d="m9 15 3-3 3 3"/>
          </svg>
          <svg v-else class="w-[18px] h-[18px]" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" viewBox="0 0 24 24">
            <rect width="20" height="8" x="2" y="2" rx="2" ry="2"/><rect width="20" height="8" x="2" y="14" rx="2" ry="2"/>
            <line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/>
          </svg>
          <span v-if="alerts[tab.id]" class="mobile-nav-dot mobile-nav-dot-err" aria-hidden="true"></span>
          <span v-else-if="tab.id === 'live' && hasSubtitles" class="mobile-nav-dot mobile-nav-dot-ok" aria-hidden="true"></span>
        </span>
        <span class="mobile-nav-label">{{ tab.label }}</span>
      </button>
    </div>
  </nav>
</template>
