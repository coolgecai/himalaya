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
