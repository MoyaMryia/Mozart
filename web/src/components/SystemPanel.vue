<script setup lang="ts">
import { computed } from 'vue';
import { services } from '../contracts';
import { requestCheckbox, requestSelect } from '../inputs';
import type { Route, Service, Status } from '../contracts';

const props = defineProps<{ status: Status | null; connected: boolean; busy: boolean; writeBlocked?: boolean }>();
const muted = computed(() => props.connected && !!props.status?.mute);
const routeValue = computed(() => props.connected && props.status ? props.status.route.desired : '');
function selectRoute(event: Event) { requestSelect<Route>(event, routeValue.value, value => emit('route', value)); }
const emit = defineEmits<{ up: []; down: []; route: [value: Route]; mute: [value: boolean]; refresh: []; restart: [service: Service] }>();
</script>

<template>
  <section class="card">
    <div class="card-head"><h2>系统</h2><button class="btn" :disabled="busy" @click="emit('refresh')">刷新</button></div>
    <p><span class="chip" :class="connected && status ? 'chip-ok' : 'chip-warn'">{{ connected && status ? '已连接' : '状态未知' }}</span></p>
    <div class="actions">
      <button class="btn btn-primary" :disabled="busy || writeBlocked" @click="emit('up')">启动系统</button>
      <button class="btn btn-danger" :disabled="busy || writeBlocked || !connected || !status" @click="emit('down')">停止系统</button>
    </div>
    <div class="field">
      <label for="system-route">路由</label>
      <select id="system-route" :value="routeValue" :disabled="busy || writeBlocked || !connected || !status" @change="selectRoute">
        <option value="" disabled>未知</option><option value="direct">直出</option><option value="rvc">变声</option>
      </select>
      <small class="muted">实际：{{ connected && status ? status.route.active : '未知' }}</small>
    </div>
    <label class="check"><input type="checkbox" :checked="muted" :disabled="busy || writeBlocked || !connected || !status" @change="requestCheckbox($event, muted, value => emit('mute', value))"> 静音</label>
    <p class="muted">GPU 占用：{{ connected && status ? (status.arbiter.gpu.owner ?? '空闲') : '未知' }}<template v-if="connected && status?.hw?.gpu.available && status.hw.gpu.load_percent !== null"> · {{ status.hw.gpu.load_percent }}%</template></p>
    <p v-if="connected && status?.pending_restart.length" class="chip chip-warn">待重启：{{ status.pending_restart.join('、') }}</p>
    <details>
      <summary>服务</summary>
      <p v-if="connected && status?.pending_restart_keys.length" class="muted mono">{{ status.pending_restart_keys.join('、') }}</p>
      <ul class="service-list">
      <li v-for="service in services" :key="service">
        <div class="row"><span class="mono">{{ service }}</span><span class="muted">{{ connected && status ? (status.processes[service]?.state ?? '未知') : '未知' }}</span></div>
        <p v-if="connected && status?.processes[service]?.last_error" class="state-err">{{ status.processes[service]?.last_error }}</p>
        <div class="row"><span v-if="connected && status?.pending_restart.includes(service)" class="chip chip-warn">待重启</span><button class="btn" :aria-label="'重启 ' + service" :disabled="busy || writeBlocked || !connected || !status?.processes[service]" @click="emit('restart', service)">↻</button></div>
      </li>
      </ul>
      <p v-if="connected && status?.enhance" class="muted">增强后端：{{ status.enhance.backend }} · 输入 {{ status.enhance.frames_in }} · 输出 {{ status.enhance.frames_out }} · 会话 {{ status.enhance.sessions }} · 算法延迟 {{ status.enhance.algorithm_delay_samples }} 采样</p>
      <p v-if="connected && status?.enhance?.last_error" class="state-err">{{ status.enhance.last_error }}</p>
    </details>
    <p v-for="reason in connected && status ? status.degraded : []" :key="reason" class="banner banner-warn">{{ reason }}</p>
  </section>
</template>
