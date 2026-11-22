// 原生控件提交意图后恢复已确认值，等待服务端快照，拒绝后不留下假状态。
export function requestCheckbox(event: Event, actual: boolean, submit: (enabled: boolean) => void) {
  const input = event.target as HTMLInputElement;
  const requested = input.checked;
  input.checked = actual;
  submit(requested);
}

export function requestSelect<T extends string>(event: Event, actual: string, submit: (value: T) => void) {
  const input = event.target as HTMLSelectElement;
  const requested = input.value as T;
  input.value = actual;
  submit(requested);
}
