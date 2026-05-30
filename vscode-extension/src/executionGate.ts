import { DANGEROUS_PERMISSION_MODE } from './permissionPolicy';

export type ExecutionGateResult = 'allowed' | 'cancelled';

export interface ExecutionGateOptions {
  permissionMode: string;
  confirmDangerousRun: () => Promise<boolean>;
  onAllowed: () => Promise<void>;
}

export async function executeWithPermissionGate(
  options: ExecutionGateOptions,
): Promise<ExecutionGateResult> {
  if (options.permissionMode === DANGEROUS_PERMISSION_MODE) {
    const approved = await options.confirmDangerousRun();
    if (!approved) {
      return 'cancelled';
    }
  }

  await options.onAllowed();
  return 'allowed';
}
