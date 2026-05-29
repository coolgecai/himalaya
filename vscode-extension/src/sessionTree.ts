import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';

export type SessionSource = 'workspace' | 'user';

export type SessionNode = SessionGroupNode | SessionFileNode;

export interface SessionMeta {
  model?: string;
  createdAt?: number;
  messageCount?: number;
}

export interface SessionSnapshot {
  groups: Array<{
    label: string;
    source: SessionSource;
    rootPath: string;
    sessions: Array<{
      sessionName: string;
      uri: string;
      mtimeMs: number;
      source: SessionSource;
      sessionId: string;
      model?: string;
      messageCount?: number;
    }>;
  }>;
}

// Simple in-memory cache for session metadata
const metaCache = new Map<string, { ts: number; meta: SessionMeta }>();
const CACHE_TTL_MS = 30_000; // 30 seconds

function readSessionMeta(filePath: string): SessionMeta {
  const now = Date.now();
  const cached = metaCache.get(filePath);
  if (cached && (now - cached.ts) < CACHE_TTL_MS) {
    return cached.meta;
  }

  const meta: SessionMeta = {};
  try {
    const stat = fs.statSync(filePath);
    // Only parse small-ish files (skip very large ones)
    if (stat.size > 0 && stat.size < 5 * 1024 * 1024) {
      if (filePath.endsWith('.jsonl')) {
        // Read first 4KB to find the first line
        const fd = fs.openSync(filePath, 'r');
        const buf = Buffer.alloc(4096);
        const bytesRead = fs.readSync(fd, buf, 0, buf.length, 0);
        fs.closeSync(fd);
        const content = buf.toString('utf8', 0, bytesRead);
        const firstLineEnd = content.indexOf('\n');
        const firstLine = firstLineEnd > 0 ? content.slice(0, firstLineEnd) : content;
        try {
          const obj = JSON.parse(firstLine);
          if (obj.model) { meta.model = String(obj.model); }
          if (obj.created_at_ms) { meta.createdAt = Number(obj.created_at_ms); }
          // Count total lines as rough message count
          const lines = content.split('\n').filter(l => l.trim().length > 0);
          meta.messageCount = lines.length;
        } catch { /* ignore parse errors */ }
      } else if (filePath.endsWith('.json')) {
        const content = fs.readFileSync(filePath, 'utf8').slice(0, 65536);
        try {
          const obj = JSON.parse(content);
          if (obj.model) { meta.model = String(obj.model); }
          if (obj.created_at_ms) { meta.createdAt = Number(obj.created_at_ms); }
          if (Array.isArray(obj.messages)) {
            meta.messageCount = obj.messages.length;
          }
        } catch { /* ignore parse errors */ }
      }
    }
  } catch { /* ignore read errors */ }

  metaCache.set(filePath, { ts: now, meta });
  return meta;
}

export class HimalayaSessionTreeProvider implements vscode.TreeDataProvider<SessionNode> {
  private readonly changeEmitter = new vscode.EventEmitter<SessionNode | undefined | null>();

  readonly onDidChangeTreeData = this.changeEmitter.event;

  refresh(element?: SessionNode): void {
    metaCache.clear(); // Clear cache on manual refresh
    this.changeEmitter.fire(element ?? null);
  }

  async getTreeItem(element: SessionNode): Promise<vscode.TreeItem> {
    return element;
  }

  async getChildren(element?: SessionNode): Promise<SessionNode[]> {
    if (!element) {
      const groups = await this.collectGroups();
      return groups.filter((group) => group.sessions.length > 0 || group.source === 'user');
    }

    if (element.kind === 'group') {
      return element.sessions;
    }

    return [];
  }

  async snapshot(): Promise<SessionSnapshot> {
    const groups = await this.collectGroups();

    return {
      groups: groups.map((group) => ({
        label: group.label,
        source: group.source,
        rootPath: group.rootPath,
        sessions: group.sessions.map((session) => ({
          sessionName: session.sessionName,
          uri: session.uri.fsPath,
          mtimeMs: session.mtimeMs,
          source: session.source,
          sessionId: session.sessionId(),
          model: session.meta?.model,
          messageCount: session.meta?.messageCount
        }))
      }))
    };
  }

  async getSessionMeta(uri: vscode.Uri): Promise<SessionMeta> {
    return readSessionMeta(uri.fsPath);
  }

  private async collectGroups(): Promise<SessionGroupNode[]> {
    const groups: SessionGroupNode[] = [];

    const workspaceRoots = this.workspaceSessionRoots();
    for (const root of workspaceRoots) {
      groups.push(await this.buildGroup('workspace', 'Workspace Sessions', root));
    }

    groups.push(await this.buildGroup('user', 'User Sessions', path.join(os.homedir(), '.Himalaya', 'sessions')));

    return groups;
  }

  private workspaceSessionRoots(): string[] {
    const roots: string[] = [];
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      roots.push(path.join(folder.uri.fsPath, '.Himalaya', 'sessions'));
    }
    return roots;
  }

  private async buildGroup(source: SessionSource, label: string, rootPath: string): Promise<SessionGroupNode> {
    const sessions = await this.collectSessions(rootPath, source);
    const totalSessions = sessions.length;
    return new SessionGroupNode(label, rootPath, source, sessions, totalSessions);
  }

  private async collectSessions(rootPath: string, source: SessionSource): Promise<SessionFileNode[]> {
    if (!fs.existsSync(rootPath) || !fs.statSync(rootPath).isDirectory()) {
      return [];
    }

    const files: SessionFileNode[] = [];
    const maxFiles = 200; // Limit to prevent performance issues

    const walk = async (directory: string): Promise<void> => {
      if (files.length >= maxFiles) { return; }
      let entries: fs.Dirent[];
      try {
        entries = await fs.promises.readdir(directory, { withFileTypes: true });
      } catch {
        return;
      }

      // Process directories first, then files
      const dirs: string[] = [];
      for (const entry of entries) {
        if (entry.isDirectory()) {
          dirs.push(path.join(directory, entry.name));
        } else if (entry.isFile() && (entry.name.endsWith('.jsonl') || entry.name.endsWith('.json'))) {
          const entryPath = path.join(directory, entry.name);
          const uri = vscode.Uri.file(entryPath);
          try {
            const stat = await fs.promises.stat(uri.fsPath);
            const meta = readSessionMeta(uri.fsPath);
            files.push(new SessionFileNode(entry.name, uri, stat.mtimeMs, source, meta));
          } catch {
            // Skip files that can't be read
          }
        }
      }

      for (const d of dirs) {
        if (files.length >= maxFiles) { break; }
        await walk(d);
      }
    };

    await walk(rootPath);

    files.sort((left, right) => right.mtimeMs - left.mtimeMs);
    return files.slice(0, maxFiles);
  }
}

export class SessionGroupNode extends vscode.TreeItem {
  readonly kind = 'group' as const;

  constructor(
    public readonly label: string,
    public readonly rootPath: string,
    public readonly source: SessionSource,
    public readonly sessions: SessionFileNode[],
    totalSessions: number
  ) {
    super(label, vscode.TreeItemCollapsibleState.Expanded);
    this.description = `${totalSessions} session${totalSessions !== 1 ? 's' : ''}`;
    this.tooltip = rootPath;
  }
}

export class SessionFileNode extends vscode.TreeItem {
  readonly kind = 'session' as const;

  constructor(
    public readonly sessionName: string,
    public readonly uri: vscode.Uri,
    public readonly mtimeMs: number,
    public readonly source: SessionSource,
    public readonly meta: SessionMeta = {}
  ) {
    super(sessionName, vscode.TreeItemCollapsibleState.None);

    // Build rich description and tooltip
    const parts: string[] = [];
    if (meta.model) {
      parts.push(meta.model);
    }
    const date = new Date(mtimeMs);
    const dateStr = date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
    const timeStr = date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
    parts.push(`${dateStr} ${timeStr}`);
    if (meta.messageCount != null && meta.messageCount > 0) {
      parts.push(`${meta.messageCount} msgs`);
    }

    this.description = parts.join(' · ');
    this.tooltip = [
      `Path: ${uri.fsPath}`,
      `Model: ${meta.model || 'unknown'}`,
      `Modified: ${date.toLocaleString()}`,
      meta.messageCount != null ? `Messages: ${meta.messageCount}` : null
    ].filter(Boolean).join('\n');

    this.command = {
      command: 'himalaya.openSession',
      title: 'Open Session',
      arguments: [this]
    };
    this.contextValue = 'himalayaSession';
  }

  sessionId(): string {
    return this.sessionName.replace(/\.(jsonl?|json)$/u, '');
  }
}
