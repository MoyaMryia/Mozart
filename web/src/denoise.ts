import type { Status } from './contracts';

export interface DenoiseOption {
  id: string;
  label: string;
  stage: string;
  available: boolean;
  unavailable_reason?: string | null;
}

const ENHANCE_STAGE = '16k_enhance_ipc';

export const DENOISE_OFF_OPTION: DenoiseOption = {
  id: 'off',
  label: '关闭',
  stage: 'off',
  available: true,
  unavailable_reason: null,
};

function fallbackDenoiseBackends(enhanceAvailable: boolean): DenoiseOption[] {
  return [
    { id: 'wola', label: 'WOLA', stage: '48k_sync', available: true, unavailable_reason: null },
    {
      id: 'ulunas',
      label: 'UL-UNAS',
      stage: ENHANCE_STAGE,
      available: enhanceAvailable,
      unavailable_reason: enhanceAvailable ? null : '需在配置中启用 [enhance] 模型',
    },
  ];
}

export function buildDenoiseOptions(pre: Status['pre'] | null | undefined, enhanceAvailable: boolean): DenoiseOption[] {
  const remote = pre?.denoise_backends;
  const backends = remote?.length ? remote : fallbackDenoiseBackends(enhanceAvailable);
  return [DENOISE_OFF_OPTION, ...backends];
}

export function isEnhanceIpcStage(stage?: string | null): boolean {
  return stage === ENHANCE_STAGE;
}

export function denoiseModeLabel(pre: Status['pre'] | null | undefined, mode: string): string {
  const matched = buildDenoiseOptions(pre, false).find(opt => opt.id === mode);
  return matched?.label ?? mode;
}
