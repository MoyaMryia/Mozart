import { services, type Service, type Status } from './contracts';

export interface SystemActivity {
  running: boolean;
  starting: boolean;
  stateText: string;
  stateTone: 'state-ok' | 'state-warn' | 'state-err' | 'state-unknown';
}

export function systemActivity(status: Status | null, connected: boolean): SystemActivity {
  const stateOf = (service: Service) => status?.processes[service]?.state;
  const running = connected && !!status && services.some(service => stateOf(service) === 'running');
  const starting = connected && !!status
    && services.some(service => stateOf(service) === 'starting' || stateOf(service) === 'restarting');
  const stateText = !connected || !status ? '状态未知'
    : status.degraded.length ? '系统降级' : starting ? '系统启动中' : running ? '系统运行中' : '系统已停止';
  const stateTone = !connected || !status ? 'state-unknown'
    : status.degraded.length || starting ? 'state-warn' : running ? 'state-ok' : 'state-err';
  return { running, starting, stateText, stateTone };
}