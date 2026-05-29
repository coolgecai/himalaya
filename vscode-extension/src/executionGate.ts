export type ExecutionGateResult = 'allowed' | 'cancelled';

export interface ExecutionGateOptions {
  permissionMode: string;
  confirmDangerousRun: () => Promise<boolean>;
  onAllowed: () => Promise<void>;
}

export async function executeWithPermissionGate(
  options: ExecutionGateOptions,
): Promise<ExecutionGateResult> {
  if (options.permissionMode === 'danger-full-access') {
    const approved = await options.confirmDangerousRun();
    if (!approved) {
      return 'cancelled';
    }
  }

  await options.onAllowed();
  return 'allowed';
}
