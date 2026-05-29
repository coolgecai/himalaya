import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { fileURLToPath } from 'url';

export type AttachmentSource = 'picker' | 'reference' | 'typed';

export interface AttachmentDescriptor {
  path: string;
  displayName: string;
  sizeBytes: number;
  modifiedMs: number;
  extension: string;
  source: AttachmentSource;
  mediaKind: 'image' | 'pdf' | 'text' | 'binary';
}

export interface AttachmentRejection {
  input: string;
  displayName: string;
  reason: 'sensitive' | 'not_found' | 'not_readable' | 'not_file' | 'duplicate';
  message: string;
}

export interface AttachmentPreparationResult {
  descriptors: AttachmentDescriptor[];
  paths: string[];
  rejections: AttachmentRejection[];
}

export function extractPromptAttachmentReferences(prompt: string): string[] {
  const references: string[] = [];
  const lines = String(prompt || '').split(/\r?\n/u);
  for (const line of lines) {
    const match = line.match(/^\s*(?:file|附件|文件)\s*[:：]\s*(.+?)\s*$/iu);
    if (!match) {
      continue;
    }
    const value = match[1].trim();
    if (value) {
      references.push(value);
    }
  }
  return references;
}

const blockedAttachmentExtensions = new Set(['.pem', '.key', '.p12', '.pfx', '.kdbx']);
const blockedAttachmentNameSnippets = ['id_rsa', 'id_ed25519', 'credentials', 'secret', 'token'];
const imageExtensions = new Set(['.png', '.jpg', '.jpeg', '.gif', '.webp', '.bmp', '.svg']);
const textExtensions = new Set([
  '.txt',
  '.md',
  '.markdown',
  '.json',
  '.jsonl',
  '.yaml',
  '.yml',
  '.toml',
  '.xml',
  '.csv',
  '.ts',
  '.tsx',
  '.js',
  '.jsx',
  '.mjs',
  '.cjs',
  '.rs',
  '.go',
  '.py',
  '.java',
  '.kt',
  '.swift',
  '.c',
  '.cc',
  '.cpp',
  '.h',
  '.hpp',
  '.cs',
  '.sh',
  '.ps1',
  '.html',
  '.css',
  '.scss',
]);

export function resolveReferenceString(value: string, workspaceFolder?: string): string {
  let trimmed = value.trim();
  if (!trimmed) {
    return '';
  }

  try { trimmed = decodeURIComponent(trimmed); } catch (_) {}

  try {
    if (trimmed.startsWith('~')) {
      trimmed = trimmed.replace(/^~(?=$|\/|\\)/u, os.homedir());
    }
  } catch (_) {}

  try {
    const url = new URL(trimmed);
    if (url.protocol === 'file:') {
      return fileURLToPath(url);
    }
  } catch {
  }

  if (path.isAbsolute(trimmed)) {
    try {
      return fs.existsSync(trimmed) ? trimmed : '';
    } catch (_) {
      return '';
    }
  }

  const candidates: string[] = [];
  if (workspaceFolder) {
    candidates.push(path.resolve(workspaceFolder, trimmed));
  }
  candidates.push(path.resolve(trimmed));

  const home = os.homedir();
  if (home) {
    candidates.push(path.resolve(home, trimmed));
    candidates.push(path.resolve(path.join(home, 'Desktop'), trimmed));
    candidates.push(path.resolve(path.join(home, '桌面'), trimmed));
    candidates.push(path.resolve(path.join(home, 'Downloads'), trimmed));
  }

  for (const candidate of candidates) {
    try {
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    } catch (_) {}
  }

  return '';
}

export function extractReferencePathCandidate(value: unknown): string | undefined {
  if (typeof value === 'string') {
    return value;
  }

  if (!value || typeof value !== 'object') {
    return undefined;
  }

  const candidate = value as {
    fsPath?: unknown;
    path?: unknown;
    uri?: unknown;
    value?: unknown;
  };

  if (typeof candidate.fsPath === 'string') {
    return candidate.fsPath;
  }

  if (typeof candidate.path === 'string') {
    return candidate.path;
  }

  const uriPath = extractNestedReferencePath(candidate.uri);
  if (uriPath) {
    return uriPath;
  }

  return extractNestedReferencePath(candidate.value);
}

export function prepareAttachmentDescriptors(
  inputs: readonly string[],
  workspaceFolder?: string,
  source: AttachmentSource = 'typed'
): AttachmentPreparationResult {
  const descriptors: AttachmentDescriptor[] = [];
  const rejections: AttachmentRejection[] = [];
  const seen = new Set<string>();

  for (const input of inputs) {
    const raw = String(input || '').trim();
    if (!raw) {
      continue;
    }

    const rawDisplayName = displayNameForPath(raw);
    if (isSensitiveAttachmentName(rawDisplayName)) {
      rejections.push(rejection(raw, rawDisplayName, 'sensitive'));
      continue;
    }

    const resolved = resolveReferenceString(raw, workspaceFolder);
    if (!resolved) {
      rejections.push(rejection(raw, rawDisplayName, 'not_found'));
      continue;
    }

    let canonical = resolved;
    try {
      canonical = fs.realpathSync(resolved);
    } catch {
      rejections.push(rejection(raw, rawDisplayName, 'not_found'));
      continue;
    }

    const displayName = path.basename(canonical) || rawDisplayName;
    if (isSensitiveAttachmentName(displayName)) {
      rejections.push(rejection(raw, displayName, 'sensitive'));
      continue;
    }

    if (seen.has(canonical)) {
      rejections.push(rejection(raw, displayName, 'duplicate'));
      continue;
    }

    let stat: fs.Stats;
    try {
      stat = fs.statSync(canonical);
    } catch {
      rejections.push(rejection(raw, displayName, 'not_found'));
      continue;
    }

    if (!stat.isFile()) {
      rejections.push(rejection(raw, displayName, 'not_file'));
      continue;
    }

    try {
      fs.accessSync(canonical, fs.constants.R_OK);
    } catch {
      rejections.push(rejection(raw, displayName, 'not_readable'));
      continue;
    }

    seen.add(canonical);
    const extension = path.extname(canonical).toLowerCase();
    descriptors.push({
      path: canonical,
      displayName,
      sizeBytes: stat.size,
      modifiedMs: stat.mtimeMs,
      extension,
      source,
      mediaKind: mediaKindForExtension(extension),
    });
  }

  return {
    descriptors,
    paths: descriptors.map((descriptor) => descriptor.path),
    rejections,
  };
}

function extractNestedReferencePath(value: unknown): string | undefined {
  if (typeof value === 'string') {
    return value;
  }

  if (!value || typeof value !== 'object') {
    return undefined;
  }

  const candidate = value as { fsPath?: unknown; path?: unknown; uri?: unknown };
  if (typeof candidate.fsPath === 'string') {
    return candidate.fsPath;
  }

  if (typeof candidate.path === 'string') {
    return candidate.path;
  }

  if (typeof candidate.uri === 'string') {
    return candidate.uri;
  }

  if (candidate.uri && typeof candidate.uri === 'object') {
    const nestedUri = candidate.uri as { fsPath?: unknown; path?: unknown };
    if (typeof nestedUri.fsPath === 'string') {
      return nestedUri.fsPath;
    }
    if (typeof nestedUri.path === 'string') {
      return nestedUri.path;
    }
  }

  return undefined;
}

function displayNameForPath(value: string): string {
  try {
    return path.basename(resolveReferenceString(value) || value) || value;
  } catch {
    return value;
  }
}

function isSensitiveAttachmentName(name: string): boolean {
  const lowerName = name.toLowerCase();
  const ext = path.extname(lowerName);
  return blockedAttachmentExtensions.has(ext)
    || blockedAttachmentNameSnippets.some((snippet) => lowerName.includes(snippet));
}

function mediaKindForExtension(extension: string): AttachmentDescriptor['mediaKind'] {
  if (extension === '.pdf') {
    return 'pdf';
  }
  if (imageExtensions.has(extension)) {
    return 'image';
  }
  if (textExtensions.has(extension)) {
    return 'text';
  }
  return 'binary';
}

function rejection(input: string, displayName: string, reason: AttachmentRejection['reason']): AttachmentRejection {
  return {
    input,
    displayName,
    reason,
    message: rejectionMessage(displayName, reason),
  };
}

function rejectionMessage(displayName: string, reason: AttachmentRejection['reason']): string {
  switch (reason) {
    case 'sensitive':
      return `Skipped sensitive attachment: ${displayName}`;
    case 'not_found':
      return `Attachment not found: ${displayName}`;
    case 'not_readable':
      return `Attachment is not readable: ${displayName}`;
    case 'not_file':
      return `Attachment is not a file: ${displayName}`;
    case 'duplicate':
      return `Skipped duplicate attachment: ${displayName}`;
  }
}
