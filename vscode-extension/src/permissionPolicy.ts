export type PublicPermissionMode = 'read-only' | 'workspace-write' | 'danger-full-access';

export const DEFAULT_PERMISSION_MODE: PublicPermissionMode = 'workspace-write';
export const DANGEROUS_PERMISSION_MODE: PublicPermissionMode = 'danger-full-access';
export const PUBLIC_PERMISSION_MODES: readonly PublicPermissionMode[] = [
  'read-only',
  DEFAULT_PERMISSION_MODE,
  DANGEROUS_PERMISSION_MODE,
];

export function normalizePermissionMode(value: string | undefined): PublicPermissionMode {
  return PUBLIC_PERMISSION_MODES.includes(value as PublicPermissionMode)
    ? value as PublicPermissionMode
    : DEFAULT_PERMISSION_MODE;
}

export type DangerousPermissionConfirmationPolicy = 'always' | 'once-per-workspace' | 'never';

export function normalizeDangerousPermissionConfirmationPolicy(
  value: string | undefined,
): DangerousPermissionConfirmationPolicy {
  if (value === 'always' || value === 'once-per-workspace' || value === 'never') {
    return value;
  }
  return 'always';
}

export function buildWorkspaceDangerApprovalKey(
  workspaceFolderPaths: string[],
  keyPrefix = 'himalayaCode.dangerApproval.v1',
): string {
  const folderIds = [...workspaceFolderPaths].sort().join('|');
  return `${keyPrefix}:${folderIds || 'no-workspace'}`;
}

export function shouldAutoAllowDangerRun(
  policy: DangerousPermissionConfirmationPolicy,
  workspaceApproved: boolean,
): boolean {
  // 'never' means never prompt (i.e. auto-allow). 'once-per-workspace'
  // auto-allows only when the workspace has been approved. 'always'
  // means always prompt (never auto-allow).
  if (policy === 'never') {
    return true;
  }
  if (policy === 'once-per-workspace' && workspaceApproved) {
    return true;
  }
  return false;
}
