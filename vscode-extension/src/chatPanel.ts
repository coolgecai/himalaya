import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import { HimalayaCli, type HimalayaReplHandle } from './cli';
import { ChatHistoryRecord, ChatHistorySnapshot, HimalayaHistoryStore, RecoveryEvidence } from './history';
import { readModelRoute, writeModelRoute } from './modelRoute';
import { SeqDeduper } from './seqDedup';
import { loadProviderSelection, listProviderProfiles, loadProviderProfile, ProviderProfile, providerCredentialsPath, saveProviderProfile, saveProviderSelection } from './providerConfig';
import {
  DANGEROUS_PERMISSION_MODE,
  DEFAULT_PERMISSION_MODE,
  PUBLIC_PERMISSION_MODES,
  buildWorkspaceDangerApprovalKey,
  normalizeDangerousPermissionConfirmationPolicy,
  normalizePermissionMode,
  shouldAutoAllowDangerRun,
} from './permissionPolicy';
import {
  classifyProtocolVersion,
  isKnownStreamEventType,
  parseStreamEventLine,
  readProtocolVersion,
  STREAM_PROTOCOL_VERSION,
} from './streamProtocol';
import { executeWithPermissionGate } from './executionGate';
import { extractPromptAttachmentReferences, extractReferencePathCandidate, prepareAttachmentDescriptors } from './attachmentPaths';
import { SessionSnapshot } from './sessionTree';

export interface ChatLaunchOptions {
  [key: string]: unknown;
  model?: string;
  modelBackend?: string;
  permissionMode?: string;
  resumeTarget?: string;
  cwd?: string;
  prompt?: string;
  files?: unknown[];
  cloudBaseUrl?: string;
  cloudApiKey?: string;
  cloudModel?: string;
  showReasoning?: boolean;
  selectedHistoryId?: string | null;
  selectedCliSessionId?: string | null;
}

export interface ChatIdentity {
  userDisplayName?: string;
  assistantDisplayName?: string;
  preferredLanguage?: string;
}

export interface ChatBootstrap {
  [key: string]: unknown;
  trust: boolean;
  config: any;
  history: ChatHistorySnapshot;
  modelCatalog: { recentModels: string[]; localModels: string[]; [key: string]: unknown };
  sessions: SessionSnapshot;
  selectedCliSessionId?: string | null;
  identity?: ChatIdentity;
}

export function readWorkspaceIdentity(): ChatIdentity {
  const candidates = (vscode.workspace.workspaceFolders ?? []).map((folder) =>
    path.join(folder.uri.fsPath, '.Himalaya', 'long_term_memory.json')
  );
  const home = process.env.HOME;
  if (home) {
    candidates.push(path.join(home, '.Himalaya', 'knowledge.json'));
  }

  for (const candidate of candidates) {
    try {
      if (!fs.existsSync(candidate)) {
        continue;
      }
      const parsed = JSON.parse(fs.readFileSync(candidate, 'utf8')) as unknown;
      if (!Array.isArray(parsed)) {
        continue;
      }
      const identity = identityFromMemoryEntries(parsed);
      if (identity.userDisplayName || identity.assistantDisplayName || identity.preferredLanguage) {
        return identity;
      }
    } catch {
      // Ignore malformed or inaccessible memory files; chat can still render default labels.
    }
  }
  return {};
}

function identityFromMemoryEntries(entries: unknown[]): ChatIdentity {
  let user: { note: string; confidence: number; ts: number } | undefined;
  let assistant: { note: string; confidence: number; ts: number } | undefined;
  let language: { note: string; confidence: number; ts: number } | undefined;

  const prefer = (
    current: { note: string; confidence: number; ts: number } | undefined,
    next: { note: string; confidence: number; ts: number }
  ): { note: string; confidence: number; ts: number } => {
    if (!current) {
      return next;
    }
    if (next.confidence > current.confidence) {
      return next;
    }
    if (next.confidence === current.confidence && next.ts >= current.ts) {
      return next;
    }
    return current;
  };

  for (const entry of entries) {
    if (!entry || typeof entry !== 'object') {
      continue;
    }
    const record = entry as { kind?: unknown; topic?: unknown; note?: unknown; confidence?: unknown; ts_ms?: unknown };
    const topic = typeof record.topic === 'string' ? record.topic : '';
    const kind = typeof record.kind === 'string' ? record.kind : '';
    const note = typeof record.note === 'string' ? record.note.trim() : '';
    if (!note) {
      continue;
    }
    const next = {
      note,
      confidence: typeof record.confidence === 'number' ? record.confidence : 0,
      ts: typeof record.ts_ms === 'number' ? record.ts_ms : 0,
    };
    if (kind === 'user_identity' || topic === 'user_identity') {
      user = prefer(user, next);
    } else if (kind === 'assistant_identity' || topic === 'assistant_identity') {
      assistant = prefer(assistant, next);
    } else if (kind === 'language_preference' || topic === 'language_preference') {
      language = prefer(language, next);
    }
  }

  return {
    userDisplayName: user?.note,
    assistantDisplayName: assistant?.note,
    preferredLanguage: language?.note,
  };
}


function detectPreferredResponseLanguage(text: string): string | undefined {
  const lower = text.toLowerCase();
  if (
    ['以后用中文', '请用中文', '用中文回答', '中文交流', '中文回复', 'output language: chinese', 'preferred response language: chinese']
      .some((marker) => text.includes(marker)) ||
    ['respond in chinese', 'answer in chinese', 'use chinese', 'speak chinese', 'reply in chinese', 'write in chinese']
      .some((marker) => lower.includes(marker))
  ) {
    return 'Chinese';
  }
  if (
    ['以后用英文', '请用英文', '用英文回答', '英文交流', '英文回复', 'output language: english', 'preferred response language: english']
      .some((marker) => text.includes(marker)) ||
    ['respond in english', 'answer in english', 'use english', 'speak english', 'reply in english', 'write in english']
      .some((marker) => lower.includes(marker))
  ) {
    return 'English';
  }
  return undefined;
}

function detectDominantResponseLanguage(text: string): string | undefined {
  let han = 0;
  let hiraganaKatakana = 0;
  let hangul = 0;
  let cyrillic = 0;
  let arabic = 0;
  let latin = 0;
  for (const ch of text) {
    const code = ch.codePointAt(0) ?? 0;
    if ((code >= 0x4e00 && code <= 0x9fff) || (code >= 0x3400 && code <= 0x4dbf) || (code >= 0xf900 && code <= 0xfaff)) {
      han += 1;
    } else if (code >= 0x3040 && code <= 0x30ff) {
      hiraganaKatakana += 1;
    } else if ((code >= 0xac00 && code <= 0xd7a3) || (code >= 0x1100 && code <= 0x11ff)) {
      hangul += 1;
    } else if (code >= 0x0400 && code <= 0x04ff) {
      cyrillic += 1;
    } else if (code >= 0x0600 && code <= 0x06ff) {
      arabic += 1;
    } else if (/^[A-Za-z]$/.test(ch)) {
      latin += 1;
    }
  }
  if (hiraganaKatakana >= 2) { return 'Japanese'; }
  if (hangul >= 2) { return 'Korean'; }
  if (han >= 2 && han * 2 >= latin) { return 'Chinese'; }
  if (cyrillic >= 2 && cyrillic * 2 >= latin) { return 'Russian'; }
  if (arabic >= 2 && arabic * 2 >= latin) { return 'Arabic'; }
  return undefined;
}

function resolvePreferredResponseLanguage(prompt: string, storedLanguage: string | undefined): string | undefined {
  return detectPreferredResponseLanguage(prompt) ?? detectDominantResponseLanguage(prompt) ?? storedLanguage;
}

function languagePromptPrefix(language: string): string {
  if (language === 'Chinese') {
    return '[Persistent interaction language: respond to the user in Chinese unless they explicitly change language. Keep commands, code, file paths, and quoted source text unchanged; prose headings and explanations must be Chinese.]';
  }
  if (language === 'English') {
    return '[Persistent interaction language: respond to the user in English unless they explicitly change language. Keep commands, code, file paths, and quoted source text unchanged.]';
  }
  return `[Persistent interaction language: respond to the user in ${language} unless they explicitly change language. Keep commands, code, file paths, and quoted source text unchanged.]`;
}

function applyLanguagePreferenceToPrompt(prompt: string, language: string | undefined): string {
  if (!language) {
    return prompt;
  }
  return `${languagePromptPrefix(language)}\n\n${prompt}`;
}

function shouldStartFreshForAttachmentDocumentTask(prompt: string, hasAttachment: boolean): boolean {
  const lower = prompt.toLowerCase();
  const continuation = /(继续|接着|上次|上一轮|刚才|前面|当前会话|已有|原来的|continue|resume|previous|above|same conversation|existing session)/iu;
  if (continuation.test(prompt)) {
    return false;
  }
  const attachmentSignal = hasAttachment || /(\bfile\s*[:：]|附件\s*[:：]|文件\s*[:：]|依据附件|根据附件|基于附件|attached|attachment)/iu.test(prompt);
  if (!attachmentSignal) {
    return false;
  }
  const documentFormat = /(pptx?|powerpoint|幻灯片|演示文稿|答辩|word|docx|pdf|excel|xlsx|电子文档|文档|表格|spreadsheet|deck|slides?)/iu;
  const createVerb = /(生成|制作|创建|输出|导出|整理|撰写|编写|做一份|转换|generate|create|make|build|export|produce|draft)/iu;
  return documentFormat.test(lower) && createVerb.test(lower);
}


interface ChatHost {
  webview: vscode.Webview;
  onDidDispose: vscode.WebviewPanel['onDidDispose'];
  reveal?: (viewColumn?: vscode.ViewColumn, preserveFocus?: boolean) => void;
  viewColumn?: vscode.ViewColumn;
}

export class HimalayaChatPanel {
  static currentPanel: HimalayaChatPanel | undefined;

  static hasCurrentPanel(): boolean {
    return Boolean(HimalayaChatPanel.currentPanel);
  }

  static updateCurrentBootstrap(bootstrap: ChatBootstrap): void {
    HimalayaChatPanel.currentPanel?.postBootstrap(bootstrap);
  }

  static async openCurrentModelConfigurationWizard(): Promise<void> {
    await HimalayaChatPanel.currentPanel?.openModelConfigurationWizard();
  }

  static attachToWebviewPanel(
    context: vscode.ExtensionContext,
    cli: HimalayaCli,
    output: vscode.OutputChannel,
    history: HimalayaHistoryStore,
    host: ChatHost,
    bootstrap: ChatBootstrap,
    onRefresh?: () => Promise<void>
  ): HimalayaChatPanel {
    return new HimalayaChatPanel(context, cli, output, history, host, bootstrap, onRefresh);
  }

  private readonly disposables: vscode.Disposable[] = [];
  private isStreamingPrompt = false;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly cli: HimalayaCli,
    private readonly output: vscode.OutputChannel,
    private readonly history: HimalayaHistoryStore,
    private readonly host: ChatHost,
    private currentBootstrap: ChatBootstrap,
    private readonly onRefresh?: () => Promise<void>
  ) {
    HimalayaChatPanel.currentPanel = this;
    this.host.webview.onDidReceiveMessage((message) => this.handleMessage(message), undefined, this.disposables);
    this.host.onDidDispose(() => this.dispose(), undefined, this.disposables);
  }

  initialize(options: ChatLaunchOptions = {}): void {
    // Restore persisted showReasoning preference when not explicitly provided
    const saved = this.context.workspaceState.get<boolean>(this.reasoningPrefKey);
    const mergedOptions = { ...options } as ChatLaunchOptions;
    if (saved !== undefined && mergedOptions.showReasoning === undefined) {
      mergedOptions.showReasoning = Boolean(saved);
    }
    this.currentOptions = mergedOptions;
    this.selectedHistoryId = options.selectedHistoryId ?? this.currentBootstrap.history.activeRecordId;
    this.selectedCliSessionId = options.selectedCliSessionId ?? options.resumeTarget ?? null;
    this.host.webview.html = this.getHtml(this.host.webview, this.currentBootstrap, this.currentOptions);
  }

  reveal(options: ChatLaunchOptions = {}): void {
    this.initialize(options);
    this.host.reveal?.(this.host.viewColumn ?? vscode.ViewColumn.Beside, false);
  }

  postBootstrap(bootstrap: ChatBootstrap): void {
    this.currentBootstrap = bootstrap;
    // Send a lightweight update instead of re-rendering the full HTML (which would wipe the thread)
    void this.host.webview.postMessage({
      type: 'bootstrap',
      bootstrap,
      options: this.currentOptions
    });
  }

  private currentOptions: ChatLaunchOptions = {};
  private selectedHistoryId: string | null = null;
  private selectedCliSessionId: string | null = null;
  private abortController: AbortController | null = null;
  private replHandle: HimalayaReplHandle | null = null;
  private replKey: string | null = null;
  private replBusy = false;
  private replCanReuse = true;
  // Set when the user explicitly starts a new session, so the next REPL spawn
  // passes --new and the CLI does NOT auto-resume the latest workspace session.
  private forceNewSession = false;
  private replEventHandler: ((event: unknown) => void) | null = null;
  private replStderrHandler: ((chunk: string) => void) | null = null;
  private readonly dangerApprovalKeyPrefix = 'himalayaCode.dangerApproval.v1';
  private readonly reasoningPrefKey = 'himalayaCode.showReasoning.v1';
  private readonly languagePreferenceKey = 'himalayaCode.preferredResponseLanguage.v1';

  // Resolve a (possibly relative) path from a tool card and open it in the editor.
  private async openFileFromCard(rawPath: string, line: number | null): Promise<void> {
    const path = (rawPath || '').trim();
    if (!path) { return; }
    try {
      let uri: vscode.Uri;
      if (path.startsWith('/') || /^[a-zA-Z]:[\\/]/.test(path)) {
        uri = vscode.Uri.file(path);
      } else {
        const folder = vscode.workspace.workspaceFolders?.[0];
        uri = folder ? vscode.Uri.joinPath(folder.uri, path) : vscode.Uri.file(path);
      }
      const doc = await vscode.workspace.openTextDocument(uri);
      const options: vscode.TextDocumentShowOptions = { preview: true };
      if (line != null && line > 0) {
        const pos = new vscode.Position(Math.max(0, line - 1), 0);
        options.selection = new vscode.Range(pos, pos);
      }
      await vscode.window.showTextDocument(doc, options);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      void vscode.window.showWarningMessage(`Could not open ${path}: ${message}`);
    }
  }

  private summarizePrompt(prompt: string): string {
    const compact = prompt.replace(/\s+/gu, ' ').trim();
    return compact.length > 48 ? `${compact.slice(0, 48)}…` : compact || 'Untitled session';
  }

  private handleMessage(message: unknown): void {
    if (!message || typeof message !== 'object') {
      return;
    }

    const typedMessage = message as { type?: string; action?: string; command?: string; prompt?: string; model?: string; modelBackend?: string; permissionMode?: string; resumeTarget?: string; cwd?: string; historyId?: string; selectedHistoryId?: string | null; selectedCliSessionId?: string | null; files?: unknown };

    switch (typedMessage.type) {
      case 'pick-file':
        void (async () => {
          const uris = await vscode.window.showOpenDialog({
            canSelectMany: true,
            openLabel: 'Attach',
            title: 'Select files to attach'
          });
          if (uris && uris.length > 0) {
            const paths = uris.map(u => u.fsPath);
            void this.host.webview.postMessage({ type: 'files-picked', paths });
          }
        })();
        break;
      case 'ready':
        void this.host.webview.postMessage({
          type: 'bootstrap',
          bootstrap: this.currentBootstrap,
          options: this.currentOptions,
          selectedHistoryId: this.selectedHistoryId,
          selectedCliSessionId: this.selectedCliSessionId
        });
        break;
      case 'openFile':
        void this.openFileFromCard(
          typeof (typedMessage as any).path === 'string' ? (typedMessage as any).path : '',
          typeof (typedMessage as any).line === 'number' ? (typedMessage as any).line : null
        );
        break;
      case 'cancel':
        if (this.replHandle && this.replBusy) {
          this.replHandle.kill();
          this.replHandle = null;
          this.replKey = null;
          this.replCanReuse = false;
        }
        if (this.abortController) {
          this.abortController.abort();
        }
        this.isStreamingPrompt = false;
        this.output.appendLine('[stream] cancelled by user');
        break;
      case 'configureModel':
        // Support webviews that send a direct 'configureModel' message (sidebar-style)
        void this.openModelConfigurationWizard();
        break;
      case 'refresh':
        void this.onRefresh?.();
        break;
      case 'toggle-reasoning':
        // Webview toggled the reasoning visualization; persist and remember preference for this panel instance
        if ((typedMessage as any).enabled !== undefined) {
          const enabled = Boolean((typedMessage as any).enabled);
          this.currentOptions = { ...this.currentOptions, showReasoning: enabled };
          void this.context.workspaceState.update(this.reasoningPrefKey, enabled);
        }
        break;
      case 'permission-change':
        if ((typedMessage as any).permissionMode !== undefined) {
          const pm = String((typedMessage as any).permissionMode || '').trim();
          this.currentOptions = { ...this.currentOptions, permissionMode: normalizePermissionMode(pm) };
        }
        break;

      case 'command':
        if (typedMessage.command === 'configureModel') {
          void this.openModelConfigurationWizard();
          break;
        }

        if (typedMessage.command === 'manageSkills') {
          void this.manageSkills();
          break;
        }

        if (typedMessage.command === 'doctor' || typedMessage.command === 'status') {
          void this.executeUtilityCommand(typedMessage.command);
          break;
        }

        if (typedMessage.command === 'newSession') {
          this.closeReplWorker();
          this.selectedHistoryId = null;
          this.selectedCliSessionId = null;
          this.forceNewSession = true;
          this.sessionAllowedTools.clear();
          this.currentOptions = {
            ...this.currentOptions,
            prompt: undefined,
            resumeTarget: undefined,
            selectedHistoryId: null,
            selectedCliSessionId: null
          };
          void this.host.webview.postMessage({
            type: 'session-reset',
            options: this.currentOptions,
            selectedHistoryId: this.selectedHistoryId,
            selectedCliSessionId: this.selectedCliSessionId
          });
          break;
        }
        break;
      
      case 'history-action':
        if (typedMessage.action === 'delete' && typedMessage.historyId) {
          const historyId = typedMessage.historyId;
          void (async () => {
            const record = this.history.records().find((item) => item.id === historyId);
            if (!record) {
              void this.host.webview.postMessage({
                type: 'historyDeleted',
                historyId,
                activeRecordId: this.history.activeRecordId(),
                selectedHistoryId: this.selectedHistoryId,
                selectedCliSessionId: this.selectedCliSessionId
              });
              return;
            }

            const choice = await vscode.window.showWarningMessage(
              `Delete history "${this.summarizePrompt(record.title)}"? This cannot be undone.`,
              { modal: true },
              'Delete'
            );
            if (choice !== 'Delete') {
              void this.host.webview.postMessage({ type: 'historyDeleteCancelled', historyId });
              return;
            }

            const wasSelected = this.selectedHistoryId === historyId || this.history.activeRecordId() === historyId;
            await this.history.remove(historyId);
            if (wasSelected) {
              this.abortController?.abort();
              this.closeReplWorker();
              this.isStreamingPrompt = false;
              this.selectedHistoryId = null;
              this.selectedCliSessionId = null;
              this.currentOptions = {
                ...this.currentOptions,
                resumeTarget: undefined,
                selectedHistoryId: null,
                selectedCliSessionId: null
              };
            }
            void this.host.webview.postMessage({
              type: 'historyDeleted',
              historyId,
              activeRecordId: this.history.activeRecordId(),
              selectedHistoryId: this.selectedHistoryId,
              selectedCliSessionId: this.selectedCliSessionId
            });
          })().catch((error) => {
            const text = error instanceof Error ? error.message : String(error);
            this.output.appendLine(`[history] delete failed: ${text}`);
            void this.host.webview.postMessage({ type: 'error', text: `Failed to delete history: ${text}` });
          });
          break;
        }
        if (typedMessage.selectedHistoryId !== undefined) {
          this.selectedHistoryId = typedMessage.selectedHistoryId;
        }
        if (typedMessage.historyId) {
          this.selectedHistoryId = typedMessage.historyId;
          const record = this.history.records().find((item) => item.id === typedMessage.historyId);
          if (record) {
            void this.history.setActiveRecord(record.id);
            this.selectedCliSessionId = record.resumeTarget ?? this.selectedCliSessionId;
            void this.host.webview.postMessage({
              type: 'historyRecord',
              record: this.webviewRecord(record),
            });
          }
        }
        break;
      case 'session-action':
        if (typedMessage.selectedCliSessionId !== undefined) {
          this.selectedCliSessionId = typedMessage.selectedCliSessionId;
        }
        break;
      case 'submit':
        void this.executePromptSubmission({
          prompt: typedMessage.prompt || '',
          model: typedMessage.model,
          modelBackend: typedMessage.modelBackend,
          permissionMode: typedMessage.permissionMode,
          resumeTarget: typedMessage.resumeTarget,
          cwd: typedMessage.cwd,
          files: Array.isArray(typedMessage.files) ? typedMessage.files : undefined
        });
        break;
      case 'webview-error':
        this.output.appendLine(`[Webview JS Error] ${(typedMessage as any).message} (line ${(typedMessage as any).line})`);
        break;
    }
  }

  private webviewRecord(record: ChatHistoryRecord): ChatHistoryRecord {
    return {
      ...record,
      messages: record.messages.map((message) => ({
        ...message,
        attachments: message.attachments ? message.attachments.slice() : undefined,
      })),
      recoveryEvidence: record.recoveryEvidence ? record.recoveryEvidence.slice() : [],
    };
  }

  private createAssistantTailPersistence(recordId: string, readText: () => string, delayMs = 250): { schedule: () => void; flush: () => Promise<void> } {
    let timer: NodeJS.Timeout | undefined;
    let chain = Promise.resolve();
    const enqueue = () => {
      const text = readText();
      chain = chain
        .catch(() => undefined)
        .then(() => this.history.replaceAssistantTail(recordId, text));
    };

    return {
      schedule: () => {
        if (timer) {
          return;
        }
        timer = setTimeout(() => {
          timer = undefined;
          enqueue();
        }, delayMs);
      },
      flush: async () => {
        if (timer) {
          clearTimeout(timer);
          timer = undefined;
        }
        enqueue();
        await chain;
      },
    };
  }

  private closeReplWorker(): void {
    if (!this.replHandle) {
      return;
    }
    try {
      this.replHandle.close();
    } catch {
      this.replHandle.kill();
    }
    this.replHandle = null;
    this.replKey = null;
    this.replBusy = false;
    this.replEventHandler = null;
    this.replStderrHandler = null;
  }

  private createReplKey(model: string, permissionMode: string, cwd: string | undefined, env: NodeJS.ProcessEnv | undefined): string {
    return JSON.stringify({
      model,
      permissionMode,
      cwd: cwd ?? '',
      openaiBaseUrl: env?.OPENAI_BASE_URL ?? '',
      hasOpenAiKey: Boolean(env?.OPENAI_API_KEY),
    });
  }

  private async ensureReplWorker(input: {
    model: string;
    permissionMode: string;
    cwd?: string;
    env?: NodeJS.ProcessEnv;
    resumeTarget?: string;
    onEvent: (event: unknown) => void;
    onStderr: (chunk: string) => void;
  }): Promise<HimalayaReplHandle> {
    const key = this.createReplKey(input.model, input.permissionMode, input.cwd, input.env);
    this.replEventHandler = input.onEvent;
    this.replStderrHandler = input.onStderr;
    if (this.replHandle && this.replKey !== key) {
      this.closeReplWorker();
      this.replEventHandler = input.onEvent;
      this.replStderrHandler = input.onStderr;
    }
    if (this.replHandle && input.resumeTarget && this.selectedCliSessionId && input.resumeTarget !== this.selectedCliSessionId) {
      this.closeReplWorker();
      this.replEventHandler = input.onEvent;
      this.replStderrHandler = input.onStderr;
    }
    if (this.replHandle && this.forceNewSession && !input.resumeTarget) {
      this.closeReplWorker();
      this.replEventHandler = input.onEvent;
      this.replStderrHandler = input.onStderr;
    }
    if (!this.replHandle) {
      this.host.webview.postMessage({ type: 'runStatus', text: 'Starting REPL worker…', kind: 'running' });
      let handle: HimalayaReplHandle | null = null;
      const effectiveResumeTarget = input.resumeTarget;
      // Consume the one-shot "new session" intent: when set (and no explicit
      // resume target), tell the CLI to start fresh instead of auto-resuming.
      const startFresh = this.forceNewSession && !effectiveResumeTarget;
      this.forceNewSession = false;
      const created = await this.cli.startRepl({
        model: input.model,
        permissionMode: input.permissionMode,
        cwd: input.cwd,
        resumeTarget: effectiveResumeTarget,
        forceNewSession: startFresh,
        allowBroadCwd: this.shouldAllowBroadCwd(input.cwd),
        env: input.env,
        onEvent: (event) => this.replEventHandler?.(event),
        onStderr: (chunk) => this.replStderrHandler?.(chunk),
        onExit: (code, signal) => {
          if (handle && this.replHandle === handle) {
            this.replHandle = null;
            this.replKey = null;
            this.replBusy = false;
            this.output.appendLine(`[repl] worker exited code=${String(code)} signal=${String(signal)}`);
          }
        },
      });
      handle = created;
      await created.ready;
      this.replHandle = created;
      this.replKey = key;
      this.host.webview.postMessage({ type: 'runStatus', text: 'REPL worker ready.', kind: 'running' });
    } else {
      this.host.webview.postMessage({ type: 'runStatus', text: 'Reusing REPL worker…', kind: 'running' });
    }
    return this.replHandle;
  }

  private offerDangerFullAccessRetry(prompt: string, toolName: string, reason: string, source: string): void {
    if (!this.isStreamingPrompt) {
      return;
    }

    const capturedPrompt = prompt.trim();
    if (!capturedPrompt) {
      return;
    }

    const capturedOptions = {
      ...this.currentOptions,
      prompt: capturedPrompt,
      files: this.currentOptions.files,
    };
    const detail = reason ? ` Reason: ${reason.slice(0, 200)}` : '';
    vscode.window.showErrorMessage(
      `Himalaya needs permission to run \`${toolName}\`. Retry with full access?${detail}`,
      { modal: true },
      'Allow & Retry'
    ).then((action) => {
      if (action === 'Allow & Retry') {
        this.abortController?.abort();
        if (this.replHandle && this.replBusy) {
          this.replHandle.kill();
          this.replHandle = null;
          this.replKey = null;
          this.replCanReuse = false;
        }
        this.isStreamingPrompt = false;
        this.output.appendLine(`[permission] user approved via ${source} — retrying with ${DANGEROUS_PERMISSION_MODE}`);
        void this.executePromptSubmission({
          ...capturedOptions,
          permissionMode: DANGEROUS_PERMISSION_MODE,
        });
      }
    });
  }

  private async executePromptSubmission(input: ChatLaunchOptions): Promise<void> {
    const prompt = input.prompt?.trim() || '';
    if (!prompt) {
      return;
    }
    const detectedLanguage = detectPreferredResponseLanguage(prompt);
    if (detectedLanguage) {
      await this.context.workspaceState.update(this.languagePreferenceKey, detectedLanguage);
      this.currentBootstrap = {
        ...this.currentBootstrap,
        identity: {
          ...(this.currentBootstrap.identity ?? {}),
          preferredLanguage: detectedLanguage,
        },
      };
    }
    const preferredLanguage = detectedLanguage
      ? detectedLanguage
      : resolvePreferredResponseLanguage(
        prompt,
        this.currentBootstrap.identity?.preferredLanguage
        ?? this.context.workspaceState.get<string>(this.languagePreferenceKey)
      );
    const cliPrompt = applyLanguagePreferenceToPrompt(prompt, preferredLanguage);

    const route = await readModelRoute(this.context);
    const model = route.model?.trim() || input.model?.trim() || this.currentBootstrap.config.defaultModel;
    const modelBackend = route.modelBackend || input.modelBackend?.trim() || this.currentBootstrap.config.defaultModelBackend || 'auto';

    if (!this.currentBootstrap.trust) {
      this.host.webview.postMessage({ type: 'error', text: 'Prompt execution is blocked in this workspace.' });
      return;
    }
    const permissionMode = normalizePermissionMode(input.permissionMode?.trim() || this.currentBootstrap.config.defaultPermissionMode);

    let gateBlocked = false;
    await executeWithPermissionGate({
      permissionMode,
      confirmDangerousRun: () => this.confirmPermissionForRun(permissionMode, prompt),
      onAllowed: async () => {
        // No-op: gate decision is used to protect the remaining execution path.
      }
    }).then((result) => {
      gateBlocked = result === 'cancelled';
    });

    if (gateBlocked) {
      this.host.webview.postMessage({ type: 'error', text: 'Execution cancelled before run.' });
      return;
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const cwd = input.cwd?.trim() || workspaceFolder;
    const explicitFiles = (input.files ?? [])
      .map((file) => extractReferencePathCandidate(file))
      .filter((file): file is string => Boolean(file && file.trim()))
      .map((file) => file.trim());
    const files = [...explicitFiles, ...extractPromptAttachmentReferences(prompt)];
    const preparedAttachments = prepareAttachmentDescriptors(files, cwd, 'picker');
    const attachmentPaths = preparedAttachments.paths;
    const startFreshForTask = shouldStartFreshForAttachmentDocumentTask(prompt, attachmentPaths.length > 0);
    if (startFreshForTask) {
      this.output.appendLine('[session] starting a fresh session for an attachment-backed document generation task');
      this.closeReplWorker();
      this.selectedHistoryId = null;
      this.selectedCliSessionId = null;
      this.forceNewSession = true;
    }
    const selectedRecord = this.selectedHistoryId
      ? this.history.records().find((item) => item.id === this.selectedHistoryId)
      : undefined;
    const existingRecord = startFreshForTask ? undefined : selectedRecord;
    const resumeTarget = startFreshForTask
      ? undefined
      : input.resumeTarget?.trim() || existingRecord?.resumeTarget?.trim() || this.selectedCliSessionId || undefined;
    if (attachmentPaths.length > 0) {
      const names = preparedAttachments.descriptors.map((attachment) => attachment.displayName).join(', ');
      this.output.appendLine(`[attachments] included ${attachmentPaths.length} file(s): ${attachmentPaths.join(', ')}`);
      this.host.webview.postMessage({ type: 'stderrChunk', text: `Attachments included in request: ${names}\n` });
    }
    for (const rejection of preparedAttachments.rejections) {
      this.host.webview.postMessage({ type: 'stderrChunk', text: `${rejection.message}\n` });
    }

    const record = existingRecord ?? await this.history.createDraft({
      title: this.summarizePrompt(prompt),
      model,
      modelBackend,
      permissionMode,
      resumeTarget,
      cwd
    });
    if (existingRecord) {
      await this.history.setActiveRecord(existingRecord.id);
    }

    this.selectedHistoryId = record.id;
    this.selectedCliSessionId = resumeTarget ?? this.selectedCliSessionId;
    this.currentOptions = {
      ...this.currentOptions,
      model,
      modelBackend,
      permissionMode,
      resumeTarget,
      cwd,
      files: attachmentPaths
    };
    this.isStreamingPrompt = true;
    this.abortController = new AbortController();

    await this.history.appendMessage(record.id, {
      role: 'user',
      text: prompt,
      createdAt: Date.now(),
      attachments: preparedAttachments.descriptors
    });

    await this.history.appendMessage(record.id, {
      role: 'assistant',
      text: '',
      createdAt: Date.now()
    });

    this.host.webview.postMessage({ type: 'assistantStart', historyId: record.id, model });

    const args = this.buildPromptArgs(cliPrompt, model, permissionMode, resumeTarget, cwd, attachmentPaths);
    const env = this.buildModelEnv(modelBackend, this.currentBootstrap.config.ollamaBaseUrl, route);
    let assistantText = '';
    const assistantTailPersistence = this.createAssistantTailPersistence(record.id, () => assistantText);
    let lineBuf = '';
    const seenUnknownEventTypes = new Set<string>();
    let malformedEventCount = 0;
    let protocolMismatchWarned = false;
    let protocolMissingWarned = false;
    const offeredPermissionRetries = new Set<string>();
    const offerPermissionRetryOnce = (toolName: string, reason: string, source: string) => {
      const key = `${toolName}:${reason}`;
      if (offeredPermissionRetries.has(key)) {
        return;
      }
      offeredPermissionRetries.add(key);
      this.offerDangerFullAccessRetry(prompt, toolName, reason, source);
    };

    const handleStreamLine = (line: string) => {
      if (!line.trim()) { return; }
      // seq de-dup helper (stored on closure so it survives across lines)
      const deduper: SeqDeduper = (handleStreamLine as any)._deduper ?? ((handleStreamLine as any)._deduper = new SeqDeduper());
      const shouldForwardEvent = deduper.shouldForward.bind(deduper);
      const parsed = parseStreamEventLine(line);
      if (!parsed.ok) {
        if (parsed.reason === 'invalid-shape') {
          malformedEventCount += 1;
          if (malformedEventCount <= 3) {
            // Log to the output channel at full length (the 160-char slice in
            // the webview made complete events look truncated). The webview
            // gets a short, accurate note instead of a misleading fragment.
            this.output.appendLine(`[stream] unknown event type ignored: ${line}`);
            this.host.webview.postMessage({ type: 'stderrChunk', text: `Unknown stream event type ignored (see output channel)\n` });
          }
        }
        return;
      }

      const event = parsed.event;
      const versionStatus = classifyProtocolVersion(readProtocolVersion(event));
      if (!protocolMismatchWarned && versionStatus === 'mismatch') {
        protocolMismatchWarned = true;
        const actual = readProtocolVersion(event);
        this.output.appendLine(`[protocol] stream protocol mismatch expected=${STREAM_PROTOCOL_VERSION} actual=${String(actual)}`);
        this.host.webview.postMessage({ type: 'stderrChunk', text: `Protocol mismatch detected. Expected v${STREAM_PROTOCOL_VERSION}, got v${String(actual)}\n` });
      }
      if (!protocolMissingWarned && versionStatus === 'missing') {
        protocolMissingWarned = true;
        this.output.appendLine(`[protocol] stream event is missing protocol_version (expected v${STREAM_PROTOCOL_VERSION})`);
        this.host.webview.postMessage({ type: 'stderrChunk', text: `Warning: stream event is missing protocol_version (expected v${STREAM_PROTOCOL_VERSION})\n` });
      }

      switch (event.type) {
        case 'session_meta': {
          const sessionId = typeof event.session_id === 'string' ? event.session_id.trim() : '';
          if (sessionId) {
            this.selectedCliSessionId = sessionId;
            this.currentOptions = {
              ...this.currentOptions,
              resumeTarget: sessionId,
              model: typeof event.model === 'string' && event.model.trim() ? event.model : this.currentOptions.model,
            };
            const latestRecord = this.history.records().find((item) => item.id === record.id);
            if (latestRecord && latestRecord.resumeTarget !== sessionId) {
              void this.history.upsert({
                ...latestRecord,
                resumeTarget: sessionId,
                model: typeof event.model === 'string' && event.model.trim() ? event.model : latestRecord.model,
              });
            }
            this.host.webview.postMessage({
              type: 'sessionMeta',
              sessionId,
              sessionPath: typeof event.session_path === 'string' ? event.session_path : undefined,
              model: typeof event.model === 'string' ? event.model : undefined,
            });
          }
          break;
        }
        case 'text_delta':
          if (typeof event.text === 'string' && event.text.length > 0) {
            assistantText += event.text;
            this.host.webview.postMessage({ type: 'assistantChunk', text: event.text });
            assistantTailPersistence.schedule();
          }
          break;
        case 'tool_use': {
          this.host.webview.postMessage({
            type: 'toolStep',
            step: 'use',
            toolUseId: typeof event.id === 'string' ? event.id : undefined,
            name: event.name,
            input: JSON.stringify(event.input),
            inputData: event.input ?? null
          });
          break;
        }
        case 'tool_result': {
          const toolName = event.name ?? 'tool';
          const output = String(event.output ?? '');
          const isError = Boolean(event.is_error);
          this.host.webview.postMessage({
            type: 'toolStep',
            step: 'result',
            toolUseId: typeof event.id === 'string' ? event.id : undefined,
            name: toolName,
            output,
            isError
          });

          const permissionDenied =
            isError &&
            /\bpermission\b.*\bdenied\b|\bdenied\b.*\bpermission\b|requires.*(?:permission|elevation|sudo|root|approval|allow)|not\s+allowed/i.test(output);

          if (permissionDenied) {
            offerPermissionRetryOnce(toolName, output, 'tool_result');
          }
          break;
        }
        
        case 'reasoning_step':
          try {
            // forward reasoning steps only if the webview opted-in via options
            if (event.reasoning_step) {
              // default to false when not set
              const show = Boolean(this.currentOptions.showReasoning);
              if (show) {
                this.host.webview.postMessage({ type: 'reasoningStep', step: event.reasoning_step });
              }
            }
          } catch (e) {
            this.output.appendLine('[reasoning] failed to forward reasoning_step: ' + String(e));
          }
          break;
        case 'decisioning_event':
          try {
            // Decisioning events ARE the reasoning visualization (they carry the
            // chain-of-thought / analysis). Only forward when the user opted in,
            // otherwise reasoning leaks into the thread despite the toggle being off.
            if (event.decisioning_event && Boolean(this.currentOptions.showReasoning)) {
              this.host.webview.postMessage({ type: 'decisioningEvent', event: event.decisioning_event });
            }
          } catch (e) {
            this.output.appendLine('[decisioning] failed to forward decisioning_event: ' + String(e));
          }
          break;
        case 'plan_execution_event': {
          const ev = event.plan_execution_event ?? event;
          if (shouldForwardEvent('plan_execution_event', ev)) {
            this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.plan_execution_event ?? event });
            void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `plan_execution_event ${JSON.stringify({ seq: (ev as any).seq, task_id: ev.task_id, node_id: ev.node_id, status: ev.status })}`, createdAt: Date.now() });
          }
          break;
        }
        case 'task_ledger_event': {
          const ev = event.task_ledger_event ?? event;
          if (shouldForwardEvent('task_ledger_event', ev)) {
            this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.task_ledger_event ?? event });
            void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `task_ledger_event ${JSON.stringify({ seq: (ev as any).seq, task_id: ev.task_id, event: ev.event, status: ev.status })}`, createdAt: Date.now() });
          }
          break;
        }
        case 'model_route_event':
          this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.model_route_event ?? event });
          break;
        case 'team_execution_event':
          this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.team_execution_event ?? event });
          break;
        case 'recovery_event':
          this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.recovery_event ?? event });
          break;
        case 'recovery_action_event': {
          const ev = event.recovery_action_event ?? event;
          try {
            // Inspect risk; if high risk, require explicit user approval before forwarding
            const hasRisk = (function findRisk(e: any) {
              if (!e || typeof e !== 'object') { return false; }
              if (typeof e.risk === 'string' && (e.risk === 'needs_workspace_write' || e.risk === 'needs_danger_full_access' || e.risk === 'needs_human')) { return true; }
              if (Array.isArray(e.results)) {
                return e.results.some((r: any) => r && r.action && (r.action.risk === 'needs_workspace_write' || r.action.risk === 'needs_danger_full_access' || r.action.risk === 'needs_human'));
              }
              if (Array.isArray(e.actions)) {
                return e.actions.some((a: any) => a && (a.risk === 'needs_workspace_write' || a.risk === 'needs_danger_full_access' || a.risk === 'needs_human'));
              }
              return false;
            })(ev);

            if (hasRisk) {
              // ask user asynchronously, do not forward automatically
              void (async () => {
                const choice = await vscode.window.showWarningMessage('Recovery action requires approval (may write to workspace). Approve?', 'Approve', 'Deny');
                if (choice === 'Approve') {
                  this.output.appendLine('[recovery-audit] user approved recovery action');
                  this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.recovery_action_event ?? event });
                  void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `recovery_action_event.approved ${JSON.stringify({ seq: (ev as any).seq, details: ev })}`, createdAt: Date.now() });
                } else {
                  this.output.appendLine('[recovery-audit] user denied recovery action');
                  void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `recovery_action_event.denied ${JSON.stringify({ seq: (ev as any).seq, details: ev })}`, createdAt: Date.now() });
                }
              })();
            } else {
              if (shouldForwardEvent('recovery_action_event', ev)) {
                this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.recovery_action_event ?? event });
                void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `recovery_action_event ${JSON.stringify({ seq: (ev as any).seq, details: ev })}`, createdAt: Date.now() });
              }
            }
          } catch (e) {
            this.output.appendLine('[recovery] failed processing recovery_action_event: ' + String(e));
          }
          break;
        }
        case 'task_execution_event': {
          const ev = event.task_execution_event ?? event;
          if (shouldForwardEvent('task_execution_event', ev)) {
            this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event: event.task_execution_event ?? event });
            void this.history.appendRecoveryEvidence(record.id, { sourceEvent: `task_execution_event ${JSON.stringify({ seq: (ev as any).seq, task_id: ev.task_id, completed: ev.completed })}`, createdAt: Date.now() });
          }
          break;
        }
        case 'task_list':
        case 'task_show':
        case 'task_packet_create':
        case 'task_packet_run':
        case 'task_packet_status':
        case 'task_scheduler_tick':
        case 'task_scheduler_queue':
        case 'task_scheduler_daemon_run':
        case 'task_scheduler_daemon_status':
        case 'task_scheduler_daemon_logs':
        case 'route_feedback_summary':
        case 'benchmark_suite':
        case 'benchmark_task':
        case 'benchmark_run':
        case 'task_execution':
        case 'task_recovery':
        case 'task_verification':
        case 'task_node_retry':
        case 'task_node_verification':
        case 'task_compacted':
        case 'task_cancelled':
        case 'worker_list':
        case 'worker_create':
        case 'worker_spawn':
        case 'worker_probe':
        case 'worker_observe':
        case 'worker_ready':
        case 'worker_resolve_trust':
        case 'worker_prompt':
        case 'worker_complete':
        case 'worker_restart':
        case 'worker_terminate':
        case 'worker_supervisor_tick':
          this.host.webview.postMessage({ type: 'runtimeEvent', kind: event.type, event });
          break;
        case 'context_event': {
          const kind = typeof event.kind === 'string' ? event.kind : '';
          if (kind === 'context_compact') {
            const removed = typeof event.removed_entries === 'number' ? event.removed_entries : undefined;
            const notice = typeof event.notice === 'string' ? event.notice : undefined;
            this.host.webview.postMessage({ type: 'contextCompacted', removed, notice });
          }
          break;
        }
        case 'recovery_suggestion': {
          const failureClass = typeof event.failure_class === 'string' ? event.failure_class : (typeof (event as any).failureClass === 'string' ? (event as any).failureClass : undefined);
          const recoveryEvidence: RecoveryEvidence = {
            tool: typeof event.tool === 'string' ? event.tool : undefined,
            reason: typeof event.reason === 'string' ? event.reason : undefined,
            action: typeof event.action === 'string' ? event.action : undefined,
            suggestion: typeof event.suggestion === 'string' ? event.suggestion : undefined,
            sourceEvent: typeof event.source_event === 'string' ? event.source_event : undefined,
            failureClass: failureClass,
            createdAt: Date.now(),
          };
          void this.history.appendRecoveryEvidence(record.id, recoveryEvidence);
          const fc = recoveryEvidence.failureClass ?? (event as any).failure_class ?? (event as any).failureClass;
          this.host.webview.postMessage({
            type: 'recoverySuggestion',
            tool: recoveryEvidence.tool,
            reason: recoveryEvidence.reason,
            action: recoveryEvidence.action,
            suggestion: recoveryEvidence.suggestion,
            sourceEvent: recoveryEvidence.sourceEvent,
            failureClass: recoveryEvidence.failureClass,
            failure_class: fc,
          });
          break;
        }
        case 'permission_request': {
          const requestedTool = typeof event.tool === 'string' ? event.tool : 'unknown';
          const requestReason = typeof event.reason === 'string' ? event.reason : '';
          const requiredMode = typeof event.required_mode === 'string' ? event.required_mode : undefined;
          const toolInput = typeof event.input === 'string' ? event.input : JSON.stringify(event.input ?? '');
          this.host.webview.postMessage({
            type: 'permissionRequest',
            tool: requestedTool,
            reason: requestReason,
            currentMode: typeof event.current_mode === 'string' ? event.current_mode : undefined,
            requiredMode,
            input: toolInput,
          });
          // The CLI is now BLOCKED waiting for our decision on stdin. Ask the user
          // via a native modal and write the decision back to the REPL worker.
          void this.resolvePermissionRequest(requestedTool, requestReason, requiredMode, toolInput);
          break;
        }
        case 'permission_denial': {
          const deniedTool = typeof event.tool === 'string' ? event.tool : 'unknown';
          const denialReason = typeof event.reason === 'string' ? event.reason : '';
          this.host.webview.postMessage({
            type: 'permissionDenial',
            tool: deniedTool,
            reason: denialReason,
          });
          offerPermissionRetryOnce(deniedTool, denialReason, event.type);
          break;
        }
        case 'error': {
          const message = typeof event.error === 'string' ? event.error : 'Himalaya reported an unknown stream error.';
          assistantText += `\n\n${message}`;
          this.host.webview.postMessage({ type: 'error', text: message });
          break;
        }
        case 'done':
        case 'message_start':
        case 'message_stop':
        case 'command_match':
        case 'tool_match':
          break;
        default:
          if (!isKnownStreamEventType(event.type) && !seenUnknownEventTypes.has(event.type)) {
            seenUnknownEventTypes.add(event.type);
            this.host.webview.postMessage({ type: 'stderrChunk', text: `Unknown stream event ignored: ${event.type}\n` });
          }
          break;
        }
    };

    try {
      const runOnce = async () => {
        const result = await this.cli.run(args, {
          cwd,
          env,
          silent: true,
          signal: this.abortController?.signal,
          onStdout: (chunk) => {
            const parts = chunk.split('\n');
            lineBuf += parts[0];
            for (let i = 1; i < parts.length; i++) {
              handleStreamLine(lineBuf);
              lineBuf = parts[i];
            }
          },
          onStderr: (chunk) => {
            const clean = chunk.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\x1b[()][AB012]/g, '');
            this.host.webview.postMessage({ type: 'stderrChunk', text: clean });
          }
        });
        if (lineBuf.trim()) { handleStreamLine(lineBuf); lineBuf = ''; }
        return result;
      };

      let result: { exitCode: number; stderr: string } | null = null;
      if (this.replCanReuse) {
        try {
          let completed = false;
          const handle = await this.ensureReplWorker({
            model,
            permissionMode,
            cwd,
            env,
            resumeTarget: this.selectedCliSessionId ?? resumeTarget,
            onEvent: (event) => {
              handleStreamLine(JSON.stringify(event));
              if (event && typeof event === 'object' && (event as { type?: unknown }).type === 'done') {
                completed = true;
              }
            },
            onStderr: (chunk) => {
              const clean = chunk.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\x1b[()][AB012]/g, '');
              this.host.webview.postMessage({ type: 'stderrChunk', text: clean });
            }
          });
          this.replBusy = true;
          this.host.webview.postMessage({ type: 'runStatus', text: 'Sending prompt…', kind: 'running' });
          handle.send(JSON.stringify({ type: 'prompt', text: cliPrompt, files: attachmentPaths }));
          await new Promise<void>((resolve, reject) => {
            const started = Date.now();
            const timer = setInterval(() => {
              if (completed) {
                clearInterval(timer);
                resolve();
                return;
              }
              if (!this.replHandle || !this.replBusy || this.abortController?.signal.aborted) {
                clearInterval(timer);
                reject(new Error(this.abortController?.signal.aborted ? 'Operation cancelled.' : 'Himalaya REPL worker stopped.'));
                return;
              }
              if (Date.now() - started > 30 * 60 * 1000) {
                clearInterval(timer);
                reject(new Error('Himalaya REPL request timed out.'));
              }
            }, 100);
          });
        } catch (error) {
          this.replBusy = false;
          this.closeReplWorker();
          if (this.abortController?.signal.aborted) {
            throw error;
          }
          const text = error instanceof Error ? error.message : String(error);
          this.host.webview.postMessage({ type: 'runStatus', text: 'Falling back to one-shot execution…', kind: 'running' });
          this.host.webview.postMessage({ type: 'stderrChunk', text: `REPL worker unavailable; falling back to one-shot execution. ${text}\n` });
          result = await runOnce();
        } finally {
          this.replBusy = false;
          this.replEventHandler = null;
          this.replStderrHandler = null;
        }
      } else {
        this.host.webview.postMessage({ type: 'runStatus', text: 'Running one-shot command…', kind: 'running' });
        result = await runOnce();
        this.replCanReuse = true;
      }

      if (result && result.exitCode !== 0) {
        const stderr = result.stderr
          .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '')
          .replace(/\x1b[()][AB012]/g, '')
          .trim();
        const failure = stderr || `Himalaya exited with code ${result.exitCode}.`;
        assistantText += `\n\n${failure}`;
        this.host.webview.postMessage({ type: 'error', text: failure });
      }

      await assistantTailPersistence.flush();
      this.isStreamingPrompt = false;
      this.host.webview.postMessage({ type: 'assistantDone' });
      await this.onRefresh?.();
    } catch (error) {
      await assistantTailPersistence.flush();
      const text = error instanceof Error ? error.message : String(error);
      this.host.webview.postMessage({ type: 'error', text });
      await this.history.appendMessage(record.id, {
        role: 'error',
        text,
        createdAt: Date.now()
      });
      this.isStreamingPrompt = false;
      await this.onRefresh?.();
    }
  }

  private buildPromptArgs(
    prompt: string,
    model: string,
    permissionMode: string,
    resumeTarget?: string,
    cwd?: string,
    attachmentPaths: string[] = []
  ): string[] {
    const args: string[] = ['--output-format', 'stream-json'];

    if (this.shouldAllowBroadCwd(cwd)) {
      args.push('--allow-broad-cwd');
    }

    if (permissionMode) {
      args.push('--permission-mode', permissionMode);
    }

    if (model) {
      args.push('--model', model);
    }

    if (resumeTarget) {
      args.push('--resume', resumeTarget);
    }

    for (const filePath of attachmentPaths) {
      args.push('--file', filePath);
    }

    args.push('prompt', prompt);
    return args;
  }

  private shouldAllowBroadCwd(cwd?: string): boolean {
    const workspaceRoots = (vscode.workspace.workspaceFolders ?? [])
      .map((folder) => path.resolve(folder.uri.fsPath));
    if (workspaceRoots.length <= 1) {
      return false;
    }

    const resolvedCwd = cwd ? path.resolve(cwd) : workspaceRoots[0];
    return workspaceRoots.every((root) => root === resolvedCwd || root.startsWith(resolvedCwd + path.sep));
  }
  // Tools the user approved "for this session" via the permission modal, so we
  // don't re-prompt for the same tool repeatedly within one chat session.
  private sessionAllowedTools = new Set<string>();

  private async resolvePermissionRequest(
    tool: string,
    reason: string,
    requiredMode: string | undefined,
    input: string
  ): Promise<void> {
    let decision: 'allow' | 'allow_always' | 'deny';
    if (this.sessionAllowedTools.has(tool)) {
      decision = 'allow';
    } else {
      const detailParts = [
        reason ? reason : `The tool "${tool}" needs elevated permission to run.`,
        requiredMode ? `Required mode: ${requiredMode}.` : '',
        input ? `Input: ${input.length > 200 ? input.slice(0, 200) + '…' : input}` : ''
      ].filter(Boolean);
      const choice = await vscode.window.showWarningMessage(
        `Himalaya wants to run "${tool}".`,
        { modal: true, detail: detailParts.join('\n') },
        'Allow once',
        'Allow for this session',
        'Deny'
      );
      if (choice === 'Allow once') {
        decision = 'allow';
      } else if (choice === 'Allow for this session') {
        decision = 'allow_always';
        this.sessionAllowedTools.add(tool);
      } else {
        decision = 'deny';
      }
    }
    // Write the decision back to the blocked CLI turn over the REPL stdin.
    try {
      this.replHandle?.send(JSON.stringify({ type: 'permission_response', decision }));
    } catch (error) {
      this.output.appendLine(`[permission] failed to send decision: ${String(error)}`);
    }
    this.host.webview.postMessage({
      type: 'stderrChunk',
      text: `Permission for ${tool}: ${decision === 'deny' ? 'denied' : 'allowed'}\n`
    });
  }

  private async confirmPermissionForRun(permissionMode: string, prompt: string): Promise<boolean> {
    if (permissionMode !== DANGEROUS_PERMISSION_MODE) {
      return true;
    }

    const policy = this.getDangerConfirmationPolicy();
    const workspaceKey = this.workspaceDangerApprovalKey();
    const workspaceApproved = this.context.workspaceState.get<boolean>(workspaceKey, false) ?? false;

    if (shouldAutoAllowDangerRun(policy, workspaceApproved)) {
      this.output.appendLine(`[permission-audit] mode=${DANGEROUS_PERMISSION_MODE} policy=${policy} decision=auto-allow`);
      return true;
    }

    const preview = prompt.replace(/\s+/gu, ' ').trim().slice(0, 120);
    const actions = policy === 'once-per-workspace'
      ? ['Run once', 'Always for this workspace', 'Cancel'] as const
      : ['Run once', 'Cancel'] as const;

    const answer = await vscode.window.showWarningMessage(
      `Run with ${DANGEROUS_PERMISSION_MODE}? This may execute destructive actions.\n\nPrompt: ${preview}${prompt.length > 120 ? '…' : ''}`,
      { modal: true },
      ...actions
    );

    if (answer === 'Always for this workspace') {
      await this.context.workspaceState.update(workspaceKey, true);
      this.output.appendLine(`[permission-audit] mode=${DANGEROUS_PERMISSION_MODE} policy=${policy} decision=allow-workspace workspace=${workspaceKey}`);
      return true;
    }

    const allowed = answer === 'Run once';
    this.output.appendLine(`[permission-audit] mode=${DANGEROUS_PERMISSION_MODE} policy=${policy} decision=${allowed ? 'allow-once' : 'deny'}`);

    if (!allowed && policy === 'once-per-workspace') {
      await this.context.workspaceState.update(workspaceKey, false);
    }

    return allowed;
  }

  private getDangerConfirmationPolicy(): 'always' | 'once-per-workspace' | 'never' {
    const config = vscode.workspace.getConfiguration('himalayaCode');
    const raw = config.get<string>('dangerousPermissionConfirmationPolicy', 'always');
    return normalizeDangerousPermissionConfirmationPolicy(raw);
  }

  private workspaceDangerApprovalKey(): string {
    const folderIds = (vscode.workspace.workspaceFolders ?? []).map(folder => folder.uri.fsPath);
    return buildWorkspaceDangerApprovalKey(folderIds, this.dangerApprovalKeyPrefix);
  }

  private async executeUtilityCommand(command: 'doctor' | 'status'): Promise<void> {
    if (this.isStreamingPrompt) {
      this.host.webview.postMessage({ type: 'error', text: 'Another request is still running.' });
      return;
    }

    if (!this.currentBootstrap.trust) {
      this.host.webview.postMessage({ type: 'error', text: 'Prompt execution is blocked in this workspace.' });
      return;
    }

    const route = await readModelRoute(this.context);
    const model = route.model?.trim() || this.currentOptions.model || this.currentBootstrap.config.defaultModel;
    const modelBackend = route.modelBackend || this.currentOptions.modelBackend || this.currentBootstrap.config.defaultModelBackend || 'auto';

    const record = await this.history.createDraft({
      title: `/${command}`,
      model,
      modelBackend,
      permissionMode: normalizePermissionMode(this.currentOptions.permissionMode ?? this.currentBootstrap.config.defaultPermissionMode),
      resumeTarget: this.currentOptions.resumeTarget,
      cwd: this.currentOptions.cwd
    });

    this.selectedHistoryId = record.id;

    await this.history.appendMessage(record.id, {
      role: 'user',
      text: `/${command}`,
      createdAt: Date.now()
    });

    await this.history.appendMessage(record.id, {
      role: 'assistant',
      text: '',
      createdAt: Date.now()
    });

    let assistantText = '';
    const assistantTailPersistence = this.createAssistantTailPersistence(record.id, () => assistantText);
    this.isStreamingPrompt = true;
    this.host.webview.postMessage({ type: 'assistantStart', historyId: record.id, model });

    try {
      const result = await this.cli.run([command], {
        cwd: this.currentOptions.cwd,
        env: this.buildModelEnv(modelBackend, this.currentBootstrap.config.ollamaBaseUrl, route),
        silent: true,
        onStdout: (chunk) => {
          assistantText += chunk;
          this.host.webview.postMessage({ type: 'assistantChunk', text: chunk });
          assistantTailPersistence.schedule();
        },
        onStderr: (chunk) => {
          const clean = chunk.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\x1b[()][AB012]/g, '');
          this.host.webview.postMessage({ type: 'stderrChunk', text: clean });
        }
      });

      if (result.exitCode !== 0) {
        const stderr = result.stderr
          .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '')
          .replace(/\x1b[()][AB012]/g, '')
          .trim();
        const failure = stderr || `Himalaya exited with code ${result.exitCode}.`;
        assistantText += failure;
        this.host.webview.postMessage({ type: 'error', text: failure });
      }

      await assistantTailPersistence.flush();
      await this.onRefresh?.();
    } catch (error) {
      await assistantTailPersistence.flush();
      const text = error instanceof Error ? error.message : String(error);
      this.host.webview.postMessage({ type: 'error', text });
      await this.history.appendMessage(record.id, {
        role: 'error',
        text,
        createdAt: Date.now()
      });
      await this.onRefresh?.();
    } finally {
      this.isStreamingPrompt = false;
      this.host.webview.postMessage({ type: 'assistantDone' });
    }
  }

  private buildModelEnv(modelBackend: string, ollamaBaseUrl: string, route?: Awaited<ReturnType<typeof readModelRoute>>): NodeJS.ProcessEnv | undefined {
    const backend = modelBackend.toLowerCase();
    if (backend !== 'cloud' && backend !== 'ollama' && route?.modelBackend !== 'cloud' && route?.modelBackend !== 'local' && route?.modelBackend !== 'ollama') {
      return undefined;
    }

    if (backend === 'cloud' || route?.modelBackend === 'cloud') {
      const baseUrl = route?.cloudBaseUrl?.trim();
      const apiKey = route?.cloudApiKey?.trim();
      if (!baseUrl || !apiKey) {
        return undefined;
      }

      return {
        OPENAI_BASE_URL: baseUrl,
        OPENAI_API_KEY: apiKey
      };
    }

    return {
      OPENAI_BASE_URL: ollamaBaseUrl || 'http://127.0.0.1:11434/v1',
      OPENAI_API_KEY: 'ollama'
    };
  }

  /// Skills manager: list discovered skills, invoke one (inserts `$skill` into
  /// the composer), or install a new skill from a local path. Shares the CLI's
  /// skill backend (.Himalaya/skills) so CLI and extension see the same skills.
  async manageSkills(): Promise<void> {
    const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const cwd = this.currentOptions.cwd?.trim() || workspaceFolder;
    const skills = await this.cli.listSkills(cwd);

    type SkillItem = vscode.QuickPickItem & { action: 'invoke' | 'install' | 'refresh'; skillName?: string };
    const items: SkillItem[] = [];
    const active = skills.filter((s) => !s.shadowed);
    for (const skill of active) {
      items.push({
        label: `$(star) ${skill.name}`,
        description: skill.source || undefined,
        detail: skill.description || undefined,
        action: 'invoke',
        skillName: skill.name
      });
    }
    if (active.length === 0) {
      items.push({
        label: '$(info) No skills found',
        detail: 'Install a skill (a directory with SKILL.md, or a .md file) to get started.',
        action: 'install'
      });
    }
    items.push({ label: '', kind: vscode.QuickPickItemKind.Separator, action: 'refresh' });
    items.push({ label: '$(cloud-download) Install skill…', detail: 'Pick a SKILL.md directory or markdown file to install', action: 'install' });
    items.push({ label: '$(refresh) Refresh skills', action: 'refresh' });

    const picked = await vscode.window.showQuickPick(items, {
      title: `Skills (${active.length} available)`,
      placeHolder: 'Invoke a skill, or install a new one',
      ignoreFocusOut: true,
      matchOnDescription: true,
      matchOnDetail: true
    });
    if (!picked) { return; }

    if (picked.action === 'refresh') {
      await this.manageSkills();
      return;
    }

    if (picked.action === 'install') {
      await this.installSkillInteractive(cwd);
      return;
    }

	    if (picked.action === 'invoke' && picked.skillName) {
	      // Insert the `$skill` invocation token into the composer so the user can
	      // add arguments before sending. The CLI resolves `$skill args` on submit.
	      void this.host.webview.postMessage({ type: 'insertComposerText', text: '$' + picked.skillName + ' ' });
	    }
  }

  private async installSkillInteractive(cwd?: string): Promise<void> {
    const uris = await vscode.window.showOpenDialog({
      canSelectMany: false,
      canSelectFiles: true,
      canSelectFolders: true,
      openLabel: 'Install skill',
      title: 'Select a skill directory (with SKILL.md) or a markdown file'
    });
    if (!uris || uris.length === 0) { return; }
    const sourcePath = uris[0].fsPath;
    try {
      const result = await this.cli.installSkill(sourcePath, cwd);
      if (result.ok) {
        void vscode.window.showInformationMessage(`Skill installed: ${result.message}`);
        await this.manageSkills();
      } else {
        void vscode.window.showWarningMessage(`Skill install failed: ${result.message}`);
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      void vscode.window.showErrorMessage(`Skill install error: ${message}`);
    }
  }

  async openModelConfigurationWizard(): Promise<void> {
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const profiles = workspaceRoot ? listProviderProfiles(workspaceRoot) : {};

    // When saved provider profiles exist, present them directly for one-click switching.
    // This skips the "Cloud / Local" picker when profiles are available.
    if (Object.keys(profiles).length > 0) {
      await this.selectFromProfiles(workspaceRoot!, profiles);
      return;
    }

    // No saved profiles: show the original Cloud / Local picker.
    const routeMode = await vscode.window.showQuickPick(
      [
        {
          label: 'Cloud model',
          description: 'Provide a network address, API key, and model name',
          value: 'cloud' as const
        },
        {
          label: 'Local model',
          description: 'Choose from Ollama-managed local models',
          value: 'local' as const
        }
      ],
      {
        placeHolder: 'Choose the model route',
        ignoreFocusOut: true
      }
    );

    if (!routeMode) {
      return;
    }

    if (routeMode.value === 'cloud') {
      await this.configureNewCloudRoute(workspaceRoot);
      return;
    }

    await this.configureLocalModelRoute();
  }

  private async configureCloudModelRoute(): Promise<void> {
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const savedSelection = workspaceRoot ? loadProviderSelection(workspaceRoot) : null;
    const profiles = workspaceRoot ? listProviderProfiles(workspaceRoot) : {};
    const profileNames = Object.keys(profiles).sort();

    let selectedProfileName: string | undefined;
    const profileItems: vscode.QuickPickItem[] = [];

    if (profileNames.length > 0) {
      const hasApiKey = !!savedSelection?.apiKey;
      profileItems.push(...profileNames.map((profileName) => {
        const profile = profiles[profileName];
        const descriptionParts = [
          profile.model ? `"model": "${profile.model}"` : undefined,
          profile.base_url ? `"base_url": "${profile.base_url}"` : undefined,
          `"api_key": "${hasApiKey ? 'saved' : 'missing'}"`
        ].filter(Boolean);
        return {
          label: profileName,
          description: `{ ${descriptionParts.join(', ')} }`,
          detail: 'Configured profile'
        };
      }));
    } else {
      profileItems.push({
        label: 'No configured provider profiles found',
        description: 'Add provider profiles to provider.json to reuse saved cloud route settings.',
        detail: 'Use Custom cloud route… to create a new profile'
      });
    }

    profileItems.push({ label: 'Custom cloud route…', description: 'Enter a new base URL, API key, and model', detail: 'Custom' });

    const profilePick = await vscode.window.showQuickPick(profileItems, {
      title: 'Cloud model route settings',
      placeHolder: 'Select a configured profile or define a custom cloud route',
      ignoreFocusOut: true,
      matchOnDescription: true,
      matchOnDetail: true
    });
    if (!profilePick) {
      return;
    }
    if (profilePick.label !== 'Custom cloud route…' && profilePick.label !== 'No configured provider profiles found') {
      selectedProfileName = profilePick.label;
    }

    let cloudBaseUrl = savedSelection?.baseUrl || this.currentBootstrap.config.ollamaBaseUrl || 'https://api.openai.com/v1';
    let effectiveApiKey = savedSelection?.apiKey || '';
    let selectedModel = this.currentOptions.cloudModel || this.currentOptions.model || this.currentBootstrap.config.defaultModel;

    if (selectedProfileName) {
      const profile = profiles[selectedProfileName];
      cloudBaseUrl = profile.base_url ?? cloudBaseUrl;
      selectedModel = profile.model || selectedModel;
      effectiveApiKey = savedSelection?.apiKey || effectiveApiKey;

      if (!effectiveApiKey) {
        const apiKey = await vscode.window.showInputBox({
          title: 'Cloud model route settings',
          prompt: `Enter api_key for profile ${selectedProfileName}`,
          password: true,
          ignoreFocusOut: true
        });
        if (apiKey === undefined) {
          return;
        }
        effectiveApiKey = apiKey.trim();
      }

      if (!profile.model || !profile.base_url) {
        const confirmed = await this.confirmCloudRouteSettings(
          cloudBaseUrl,
          effectiveApiKey,
          selectedModel,
          selectedProfileName
        );
        if (!confirmed) {
          return;
        }
        cloudBaseUrl = confirmed.cloudBaseUrl;
        effectiveApiKey = confirmed.cloudApiKey;
        selectedModel = confirmed.selectedModel;
      }
    } else {
      const confirmed = await this.confirmCloudRouteSettings(
        cloudBaseUrl,
        effectiveApiKey,
        selectedModel,
        selectedProfileName
      );
      if (!confirmed) {
        return;
      }
      cloudBaseUrl = confirmed.cloudBaseUrl;
      effectiveApiKey = confirmed.cloudApiKey;
      selectedModel = confirmed.selectedModel;
    }

    if (!cloudBaseUrl.trim() || !effectiveApiKey || !selectedModel) {
      void vscode.window.showWarningMessage('Cloud model setup requires a network address, API key, and model name.');
      return;
    }

    await writeModelRoute(this.context, {
      model: selectedModel,
      modelBackend: 'cloud',
      modelSource: 'cloud',
      cloudBaseUrl: cloudBaseUrl.trim(),
      cloudApiKey: effectiveApiKey,
      cloudModel: selectedModel
    });

    if (workspaceRoot) {
      try {
        saveProviderSelection(workspaceRoot, {
          model: selectedModel,
          baseUrl: cloudBaseUrl.trim(),
          apiKey: effectiveApiKey
        });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.output.appendLine(`[model] failed to mirror cloud config to provider.json: ${message}`);
      }
    }

    this.currentOptions = {
      ...this.currentOptions,
      model: selectedModel,
      modelBackend: 'cloud',
      cloudBaseUrl: cloudBaseUrl.trim(),
      cloudApiKey: effectiveApiKey,
      cloudModel: selectedModel
    };

    void this.host.webview.postMessage({ type: 'model-updated', model: selectedModel, modelBackend: 'cloud' });
    void vscode.window.showInformationMessage(`Himalaya cloud model set to ${selectedModel} (shared with CLI).`);
  }

  /// Show a profile switcher QuickPick with all saved provider profiles,
  /// plus options to create a new cloud route or switch to local models.
  private async selectFromProfiles(
    workspaceRoot: string,
    profiles: Record<string, ProviderProfile>
  ): Promise<void> {
    const items: vscode.QuickPickItem[] = [];

    for (const [name, profile] of Object.entries(profiles)) {
      const descParts = [
        profile.model ? `model: ${profile.model}` : undefined,
        profile.base_url ? `url: ${profile.base_url}` : undefined
      ].filter(Boolean);
      items.push({
        label: name,
        description: descParts.join(' · '),
        detail: 'Select to switch immediately'
      });
    }

    items.push(
      { label: '', kind: vscode.QuickPickItemKind.Separator },
      { label: 'Configure new cloud route…', description: 'Add a new provider and save as a profile', detail: 'New' },
      { label: 'Switch to local model…', description: 'Use Ollama-managed models', detail: 'Local' }
    );

    const pick = await vscode.window.showQuickPick(items, {
      title: 'Switch model',
      placeHolder: 'Select a configured profile or add a new one',
      ignoreFocusOut: true,
      matchOnDescription: true,
      matchOnDetail: true
    });

    if (!pick) { return; }

    if (pick.label === 'Configure new cloud route…') {
      await this.configureNewCloudRoute(workspaceRoot);
    } else if (pick.label === 'Switch to local model…') {
      await this.configureLocalModelRoute();
    } else {
      // One-click profile switch: no confirmation for complete profiles.
      await this.applyProfile(workspaceRoot, pick.label, profiles[pick.label]);
    }
  }

  /// One-click profile switch. Reads the profile from provider.json, looks up
  /// the api_key from credentials, and applies the route immediately without
  /// prompting for base_url or model name.
  private async applyProfile(
    workspaceRoot: string,
    profileName: string,
    profile: ProviderProfile
  ): Promise<void> {
    const baseUrl = profile.base_url?.trim();
    const model = profile.model?.trim();

    if (!model || !baseUrl) {
      void vscode.window.showWarningMessage(`Profile "${profileName}" is incomplete; please edit provider.json to include model and base_url.`);
      return;
    }

    // Read the api_key from credentials (the same file the CLI reads).
    let apiKey = '';
    try {
      const credsPath = providerCredentialsPath();
      if (fs.existsSync(credsPath)) {
        const raw = JSON.parse(fs.readFileSync(credsPath, 'utf8'));
        apiKey = typeof raw.api_key === 'string' ? raw.api_key : '';
      }
    } catch { /* best effort */}
    apiKey = apiKey.trim();

    // If no api_key is saved, prompt once.
    if (!apiKey && baseUrl) {
      const entered = await vscode.window.showInputBox({
        title: 'API Key required',
        prompt: `Enter the API key for "${profileName}" (${baseUrl})`,
        password: true,
        ignoreFocusOut: true
      });
      if (entered === undefined) { return; } // user cancelled
      apiKey = entered.trim();
      if (!apiKey) {
        void vscode.window.showWarningMessage('No API key configured. Please set one first.');
        return;
      }
    }

    // If we have base_url + api_key, save them to credentials for future use.
    if (apiKey) {
      try {
        const credsPath = providerCredentialsPath();
        fs.mkdirSync(path.dirname(credsPath), { recursive: true });
        fs.writeFileSync(credsPath, JSON.stringify({ api_key: apiKey }, null, 2), 'utf8');
        if (process.platform !== 'win32') {
          try { fs.chmodSync(credsPath, 0o600); } catch { /* best effort */}
        }
      } catch { /* best effort */}
    }

    // Write to the extension's own model route storage (Layer A).
    await writeModelRoute(this.context, {
      model,
      modelBackend: 'cloud',
      modelSource: 'cloud',
      cloudBaseUrl: baseUrl || undefined,
      cloudApiKey: apiKey || undefined,
      cloudModel: model
    });

    this.currentOptions = {
      ...this.currentOptions,
      model,
      modelBackend: 'cloud',
      cloudBaseUrl: baseUrl || undefined,
      cloudApiKey: apiKey || undefined,
      cloudModel: model
    };

    void this.host.webview.postMessage({ type: 'model-updated', model, modelBackend: 'cloud' });
    void vscode.window.showInformationMessage(`Switched to ${profileName}: ${model}`);
  }

  /// Configure a brand-new cloud provider in 4 steps (base_url → api_key →
  /// model → profile name) and persist it both as a named profile and as the
  /// active model route.
  private async configureNewCloudRoute(workspaceRoot?: string): Promise<void> {
    // Step 1: base_url
    const baseUrl = await vscode.window.showInputBox({
      title: 'New cloud provider',
      prompt: 'Enter the provider base URL (OpenAI-compatible endpoint)',
      value: 'https://api.openai.com/v1',
      ignoreFocusOut: true
    });
    if (!baseUrl) { return; }

    // Step 2: api_key
    const apiKey = await vscode.window.showInputBox({
      title: 'New cloud provider',
      prompt: 'Enter the API key',
      password: true,
      ignoreFocusOut: true
    });
    if (!apiKey) { return; }

    // Step 3: auto-fetch model list and let the user pick (or type manually).
    const selectedModel = await this.pickCloudModelWithBaseUrl(
      baseUrl.trim(), apiKey.trim(), undefined
    );
    if (!selectedModel) { return; }

    // Step 4: ask for a profile name so the user can switch back later.
    let defaultName = 'default';
    try {
      defaultName = new URL(baseUrl.trim()).hostname.split('.')[0] || 'custom';
    } catch { defaultName = 'custom'; }
    const profileName = await vscode.window.showInputBox({
      title: 'Save as profile',
      prompt: 'Give this configuration a name for quick switching later (e.g. "openai", "deepseek")',
      value: defaultName,
      ignoreFocusOut: true
    });
    if (!profileName) { return; }

    // --- Persist ---
    // Layer A: extension's own route storage
    await writeModelRoute(this.context, {
      model: selectedModel,
      modelBackend: 'cloud',
      modelSource: 'cloud',
      cloudBaseUrl: baseUrl.trim(),
      cloudApiKey: apiKey.trim(),
      cloudModel: selectedModel
    });

    // Layer B: provider.json + credentials (shared with CLI)
    if (workspaceRoot) {
      try {
        saveProviderProfile(workspaceRoot, profileName.trim(), {
          model: selectedModel,
          base_url: baseUrl.trim()
        });
        // Also save the api_key to credentials (shared global file).
        const credsPath = providerCredentialsPath();
        fs.mkdirSync(path.dirname(credsPath), { recursive: true });
        fs.writeFileSync(credsPath, JSON.stringify({ api_key: apiKey.trim() }, null, 2), 'utf8');
        if (process.platform !== 'win32') {
          try { fs.chmodSync(credsPath, 0o600); } catch { /* best effort */}
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.output.appendLine(`[model] failed to persist cloud config: ${message}`);
      }
    }

    this.currentOptions = {
      ...this.currentOptions,
      model: selectedModel,
      modelBackend: 'cloud',
      cloudBaseUrl: baseUrl.trim(),
      cloudApiKey: apiKey.trim(),
      cloudModel: selectedModel
    };

    void this.host.webview.postMessage({ type: 'model-updated', model: selectedModel, modelBackend: 'cloud' });
    void vscode.window.showInformationMessage(
      `Cloud model set to ${selectedModel} (profile: ${profileName.trim()}). Shared with CLI.`
    );
  }

  private async fetchCloudModelOptions(baseUrl: string, apiKey: string): Promise<{ name: string }[]> {
    const models: { name: string }[] = [];
    if (!baseUrl || !apiKey) {
      return models;
    }
    try {
      return await this.cli.listCloudModels(baseUrl, apiKey);
    } catch (_) {
      return models;
    }
  }

  private async confirmCloudRouteSettings(
    baseUrl: string,
    apiKey: string,
    currentModel: string,
    profileName?: string
  ): Promise<{ cloudBaseUrl: string; cloudApiKey: string; selectedModel: string } | undefined> {
    const cloudBaseUrl = await vscode.window.showInputBox({
      title: 'Cloud model route settings',
      prompt: profileName
        ? `Confirm or edit base_url for profile ${profileName}`
        : 'Enter the provider base_url (OpenAI-compatible endpoint)',
      value: baseUrl,
      ignoreFocusOut: true
    });
    if (cloudBaseUrl === undefined) {
      return undefined;
    }

    const cloudApiKey = await vscode.window.showInputBox({
      title: 'Cloud model route settings',
      prompt: apiKey
        ? `Confirm or edit api_key for profile ${profileName ?? 'custom route'} (leave blank to keep existing)`
        : 'Enter the provider api_key',
      password: true,
      ignoreFocusOut: true
    });
    if (cloudApiKey === undefined) {
      return undefined;
    }
    const effectiveApiKey = cloudApiKey.trim() || apiKey;

    const selectedModel = await this.pickCloudModelWithBaseUrl(cloudBaseUrl.trim(), effectiveApiKey, currentModel);
    if (!selectedModel) {
      return undefined;
    }

    return {
      cloudBaseUrl: cloudBaseUrl.trim(),
      cloudApiKey: effectiveApiKey,
      selectedModel
    };
  }

  private async pickCloudModelWithBaseUrl(baseUrl: string, apiKey: string, currentModel: string | undefined): Promise<string | undefined> {
    const cloudModels = await this.fetchCloudModelOptions(baseUrl, apiKey);
    const builtinAliases = [
      'Himalaya-opus-4-6', 'Himalaya-sonnet-4-6', 'Himalaya-haiku-4-5-20251213',
      'gpt-4o', 'gpt-4o-mini', 'Himalaya-fable-5', 'claude-sonnet-4-6'
    ];
    const seen = new Set<string>();
    const modelOptions: vscode.QuickPickItem[] = [];
    for (const m of cloudModels) {
      const label = m.name.trim();
      if (!label || seen.has(label)) { continue; }
      seen.add(label);
      modelOptions.push({ label, description: 'Provider', detail: 'Cloud model from endpoint' });
    }
    for (const alias of builtinAliases) {
      if (!seen.has(alias)) {
        seen.add(alias);
        modelOptions.push({ label: alias, description: 'Built-in', detail: 'Known alias' });
      }
    }
    if (currentModel && !seen.has(currentModel)) {
      modelOptions.unshift({ label: currentModel, description: 'Current', detail: 'Preserved selection' });
    }
    modelOptions.push({ label: 'Custom model name…', description: 'Enter a model name not in the list', detail: 'Other' });

    const modelPick = await vscode.window.showQuickPick(modelOptions, {
      title: 'Cloud model route settings',
      placeHolder: `Select or confirm a model name (default: ${currentModel ?? 'none'})`,
      ignoreFocusOut: true,
      matchOnDescription: true,
      matchOnDetail: true
    });
    if (!modelPick) {
      return undefined;
    }
    if (modelPick.label === 'Custom model name…') {
      const custom = await vscode.window.showInputBox({
        title: 'Cloud model route settings',
        prompt: 'Enter the provider model name',
        value: currentModel,
        ignoreFocusOut: true
      });
      if (custom === undefined) {
        return undefined;
      }
      return custom.trim();
    }
    return modelPick.label.trim();
  }

  private async configureLocalModelRoute(): Promise<void> {
    const localModels = this.currentBootstrap.modelCatalog.localModels.length > 0
      ? this.currentBootstrap.modelCatalog.localModels
      : await this.cli.listLocalModels({ force: true });

    if (localModels.length === 0) {
      void vscode.window.showWarningMessage('No Ollama-managed local models were found. Start Ollama and try again.');
      return;
    }

    const selectedModel = await vscode.window.showQuickPick(
      localModels.map((model) => ({ label: model, description: 'Ollama-managed local model' })),
      {
        placeHolder: 'Select a local Ollama model',
        ignoreFocusOut: true
      }
    );

    if (!selectedModel) {
      return;
    }

    await writeModelRoute(this.context, {
      model: selectedModel.label,
      modelBackend: 'ollama',
      modelSource: 'local'
    });

    this.currentOptions = {
      ...this.currentOptions,
      model: selectedModel.label,
      modelBackend: 'ollama',
      cloudBaseUrl: undefined,
      cloudApiKey: undefined,
      cloudModel: undefined
    };

    void this.host.webview.postMessage({ type: 'model-updated', model: selectedModel.label, modelBackend: 'ollama' });
    void vscode.window.showInformationMessage(`Himalaya local model set to ${selectedModel.label}.`);
  }

  private getHtml(webview: vscode.Webview, bootstrap: ChatBootstrap, options: ChatLaunchOptions): string {
    return this.buildClaudeLikeHtml(webview, bootstrap, options);
  }


  private buildClaudeLikeHtml(webview: vscode.Webview, bootstrap: ChatBootstrap, options: ChatLaunchOptions): string {
    const nonce = `n${Date.now()}`;
    const csp = [
      "default-src 'none'",
      `img-src ${webview.cspSource} https: data:`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${nonce}' ${webview.cspSource}`
    ].join('; ');
    const markedUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.context.extensionUri, 'media', 'marked.umd.js')
    );
    const localModels: string[] = bootstrap.modelCatalog?.localModels ?? [];
    const historyRecords = bootstrap.history?.records ?? [];
    const activeRecordId = bootstrap.history?.activeRecordId ?? null;
    const defaultModel: string = bootstrap.config?.defaultModel ?? 'sonnet';
    const currentModel: string = (options.model ?? defaultModel).trim() || defaultModel;
    const currentBackend: string = options.modelBackend ?? bootstrap.config?.defaultModelBackend ?? 'auto';
    const currentPermission: string = normalizePermissionMode(options.permissionMode ?? bootstrap.config?.defaultPermissionMode);
    const resumeTarget: string = options.resumeTarget ?? '';
    const isTrusted: boolean = Boolean(bootstrap.trust);
    const historyJson = JSON.stringify(historyRecords).replace(/</g, '\\u003c');
    const localModelsJson = JSON.stringify(localModels).replace(/</g, '\\u003c');
    const permissionModesJson = JSON.stringify(PUBLIC_PERMISSION_MODES).replace(/</g, '\\u003c');
    const stateJson = JSON.stringify({
      model: currentModel,
      modelBackend: currentBackend,
      permissionMode: currentPermission,
      resumeTarget,
      isTrusted,
      activeRecordId,
      showReasoning: Boolean(options.showReasoning),
      identity: bootstrap.identity ?? {}
    }).replace(/</g, '\\u003c');

    const _head = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="${csp}">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <style>
    :root {
      color-scheme: dark;
      --bg: #1e1e1e;
      --surface: #252526;
      --surface2: #2d2d30;
      --border: rgba(255,255,255,0.08);
      --text: #cccccc;
      --text-dim: #858585;
      --accent: #007acc;
      --accent-soft: rgba(0,122,204,0.15);
      --accent-text: #4fc1ff;
      --user-bg: rgba(0,122,204,0.10);
      --assistant-bg: rgba(255,255,255,0.03);
      --danger: #f44747;
      --success: #4ec9b0;
      --radius: 6px;
    }
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
      font-size: 13px;
      line-height: 1.5;
      background: var(--bg);
      color: var(--text);
      height: 100vh;
      overflow: hidden;
      display: flex;
      flex-direction: column;
    }
    /* ── top bar ── */
    .topbar {
      flex: 0 0 auto;
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 6px 10px;
      background: var(--surface);
      border-bottom: 1px solid var(--border);
    }
    .topbar-title {
      font-size: 11px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: .06em;
      color: var(--text-dim);
      flex: 1;
    }
    .icon-btn {
      background: none;
      border: none;
      color: var(--text-dim);
      cursor: pointer;
      padding: 3px 6px;
      border-radius: var(--radius);
      font-size: 12px;
      line-height: 1;
    }
    .icon-btn.active {
      color: var(--accent-text);
      background: rgba(255,255,255,0.04);
    }
    .icon-btn:hover { background: var(--surface2); color: var(--text); }
    /* ── model pill ── */
    .model-bar {
      flex: 0 0 auto;
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 5px 10px;
      background: var(--surface);
      border-bottom: 1px solid var(--border);
    }
    .model-pill {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      padding: 3px 8px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: var(--surface2);
      font-size: 11px;
      color: var(--text-dim);
      cursor: pointer;
      transition: border-color .15s, color .15s;
    }
    .model-pill:hover { border-color: var(--accent); color: var(--accent-text); }
    .model-pill .dot {
      width: 6px; height: 6px;
      border-radius: 50%;
      background: var(--success);
      flex-shrink: 0;
    }
    .model-pill .dot.cloud { background: var(--accent-text); }
    .model-pill .dot.local { background: var(--success); }
    .model-pill .dot.unknown { background: var(--text-dim); }
    .model-pill-label { max-width: 140px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .perm-pill {
      display: inline-flex;
      align-items: center;
      padding: 3px 8px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: var(--surface2);
      font-size: 11px;
      color: var(--text-dim);
      cursor: pointer;
    }
    .perm-pill:hover { border-color: rgba(255,255,255,0.2); color: var(--text); }
    .spacer { flex: 1; }
    /* ── thread ── */
    .thread {
      flex: 1 1 0;
      overflow-y: auto;
      padding: 12px 10px;
      display: flex;
      flex-direction: column;
      gap: 2px;
    }
    .thread::-webkit-scrollbar { width: 4px; }
    .thread::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.1); border-radius: 2px; }
    .msg {
      display: flex;
      flex-direction: column;
      gap: 4px;
      padding: 8px 10px;
      border-radius: var(--radius);
      max-width: 100%;
      word-break: break-word;
      white-space: pre-wrap;
    }
    .msg.user { background: var(--user-bg); align-self: flex-end; max-width: 88%; }
    .msg.assistant { background: var(--assistant-bg); align-self: flex-start; max-width: 100%; }
    /* ── rendered markdown in assistant bodies ── */
    .msg.assistant .msg-body { white-space: normal; line-height: 1.5; }
    .msg.assistant .msg-body > :first-child { margin-top: 0; }
    .msg.assistant .msg-body > :last-child { margin-bottom: 0; }
    .msg.assistant .msg-body p { margin: 0 0 8px; }
    .msg.assistant .msg-body h1,
    .msg.assistant .msg-body h2,
    .msg.assistant .msg-body h3,
    .msg.assistant .msg-body h4 { margin: 14px 0 6px; line-height: 1.3; font-weight: 600; }
    .msg.assistant .msg-body h1 { font-size: 1.4em; }
    .msg.assistant .msg-body h2 { font-size: 1.25em; }
    .msg.assistant .msg-body h3 { font-size: 1.1em; }
    .msg.assistant .msg-body h4 { font-size: 1em; }
    .msg.assistant .msg-body ul,
    .msg.assistant .msg-body ol { margin: 0 0 8px; padding-left: 22px; }
    .msg.assistant .msg-body li { margin: 2px 0; }
    .msg.assistant .msg-body li > p { margin: 0; }
    .msg.assistant .msg-body a { color: var(--link, #4c84ff); text-decoration: none; }
    .msg.assistant .msg-body a:hover { text-decoration: underline; }
    .msg.assistant .msg-body code {
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 0.92em;
      background: rgba(127,127,127,0.16);
      padding: 1px 5px;
      border-radius: 4px;
    }
    .msg.assistant .msg-body pre {
      margin: 0 0 10px;
      padding: 10px 12px;
      background: rgba(127,127,127,0.12);
      border: 1px solid rgba(127,127,127,0.18);
      border-radius: 8px;
      overflow-x: auto;
      white-space: pre;
    }
    .msg.assistant .msg-body pre code {
      background: none;
      padding: 0;
      font-size: 0.88em;
      line-height: 1.45;
    }
    .msg.assistant .msg-body blockquote {
      margin: 0 0 8px;
      padding: 2px 0 2px 12px;
      border-left: 3px solid rgba(127,127,127,0.4);
      color: var(--text-dim);
    }
    .msg.assistant .msg-body table {
      border-collapse: collapse;
      margin: 0 0 10px;
      display: block;
      overflow-x: auto;
      max-width: 100%;
    }
    .msg.assistant .msg-body th,
    .msg.assistant .msg-body td {
      border: 1px solid rgba(127,127,127,0.28);
      padding: 4px 8px;
      text-align: left;
    }
    .msg.assistant .msg-body th { background: rgba(127,127,127,0.12); font-weight: 600; }
    .msg.assistant .msg-body hr { border: none; border-top: 1px solid rgba(127,127,127,0.25); margin: 12px 0; }
    .msg.assistant .msg-body img { max-width: 100%; height: auto; border-radius: 6px; }
    .msg.assistant .msg-body .cursor { display: inline-block; }
    .msg.error { background: rgba(244,71,71,0.08); border: 1px solid rgba(244,71,71,0.25); color: #f88; }
    .msg.stderr { background: rgba(255,200,0,0.06); border: 1px solid rgba(255,200,0,0.15); color: #ffd; font-size: 11px; font-family: monospace; }
    .msg.tool-step { background: rgba(78,201,176,0.05); border-left: 2px solid #4ec9b0; padding: 4px 8px; align-self: flex-start; max-width: 100%; }
    .msg.tool-step .msg-role { color: #4ec9b0; }
    .msg.tool-step .msg-body { font-size: 11.5px; font-family: monospace; color: var(--text-dim); }
    /* ── rich tool cards ── */
    .msg.tool-card {
      align-self: flex-start;
      max-width: 100%;
      width: 100%;
      background: rgba(78,201,176,0.045);
      border: 1px solid rgba(78,201,176,0.18);
      border-left: 2px solid #4ec9b0;
      border-radius: 8px;
      padding: 7px 9px;
      gap: 6px;
    }
    .msg.tool-card.has-error { border-left-color: #f44; border-color: rgba(244,71,71,0.25); }
    .tool-head { display: flex; align-items: center; gap: 7px; }
    .tool-icon { color: #4ec9b0; font-size: 12px; width: 14px; text-align: center; }
    .tool-card.has-error .tool-icon { color: #ff8a8a; }
    .tool-name { font-size: 11.5px; font-weight: 600; color: var(--text); letter-spacing: 0.01em; }
    .tool-subtle { font-size: 11px; color: var(--text-dim); margin: 2px 0; }
    .tool-pathline { margin: 2px 0; }
    .tool-path {
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11.5px;
      color: var(--link, #4c84ff);
      background: rgba(76,132,255,0.08);
      border: 1px solid rgba(76,132,255,0.18);
      border-radius: 5px;
      padding: 1px 6px;
      cursor: pointer;
      word-break: break-all;
      text-align: left;
    }
    .tool-path:hover { background: rgba(76,132,255,0.18); text-decoration: underline; }
    .tool-kv { display: flex; gap: 6px; align-items: baseline; font-size: 11.5px; margin: 2px 0; flex-wrap: wrap; }
    .tool-k { color: var(--text-dim); min-width: 52px; }
    .tool-kv code { font-family: var(--vscode-editor-font-family, monospace); background: rgba(127,127,127,0.14); padding: 1px 5px; border-radius: 4px; }
    pre.tool-code {
      margin: 4px 0 0;
      padding: 8px 10px;
      background: rgba(127,127,127,0.12);
      border: 1px solid rgba(127,127,127,0.16);
      border-radius: 6px;
      overflow-x: auto;
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11.5px;
      line-height: 1.5;
      white-space: pre;
    }
    .tok-cmd { color: #4ec9b0; }
    .tok-flag { color: #c586c0; }
    .tok-str { color: #ce9178; }
    .tok-num { color: #b5cea8; }
    .tok-cmt { color: #6a9955; font-style: italic; }
    .tool-diff {
      margin: 4px 0 0;
      border: 1px solid rgba(127,127,127,0.16);
      border-radius: 6px;
      overflow-x: auto;
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11.5px;
      line-height: 1.45;
    }
    .diff-row { padding: 0 8px; white-space: pre; }
    .diff-del { background: rgba(244,71,71,0.10); color: #f4a0a0; }
    .diff-add { background: rgba(78,201,176,0.12); color: #8fe6cf; }
    .tool-result-slot:empty { display: none; }
    details.tool-result {
      margin: 5px 0 0;
      border-top: 1px dashed rgba(127,127,127,0.2);
      padding-top: 4px;
    }
    details.tool-result > summary {
      cursor: pointer;
      font-size: 11px;
      color: var(--text-dim);
      list-style: none;
      user-select: none;
    }
    details.tool-result > summary::-webkit-details-marker { display: none; }
    details.tool-result[open] > summary { color: var(--text); margin-bottom: 4px; }
    details.tool-result-error > summary { color: #ff8a8a; }
    pre.tool-result-body {
      margin: 0;
      padding: 8px 10px;
      background: rgba(127,127,127,0.10);
      border-radius: 6px;
      overflow-x: auto;
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11px;
      line-height: 1.45;
      white-space: pre-wrap;
      word-break: break-word;
      max-height: 320px;
      overflow-y: auto;
    }
    .tool-result-error pre.tool-result-body { color: #f4b0b0; }
    .tool-result-empty { font-size: 11px; color: var(--text-dim); font-style: italic; }
    .tool-more { font-size: 10.5px; color: var(--text-dim); margin-top: 3px; }
    .msg.reasoning-step { background: rgba(76,132,255,0.04); border-left: 2px solid #4c84ff; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.reasoning-step .msg-role { color: #4c84ff; }
    .msg.reasoning-step .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
    /* ── inline thinking blocks (Claude-style foldable reasoning) ── */
    .msg.thinking-block { align-self: stretch; max-width: 100%; padding: 0; background: none; }
    .thinking-details {
      border-left: 2px solid rgba(140,140,150,0.4);
      background: rgba(127,127,127,0.05);
      border-radius: 0 8px 8px 0;
      padding: 2px 0;
    }
    .thinking-summary {
      cursor: pointer; list-style: none; user-select: none;
      display: flex; align-items: center; gap: 6px;
      padding: 5px 10px; font-size: 11.5px; color: var(--text-dim);
    }
    .thinking-summary::-webkit-details-marker { display: none; }
    .thinking-icon { opacity: 0.8; }
    .thinking-label { font-weight: 600; letter-spacing: 0.02em; }
    .thinking-kind { color: rgba(140,140,150,0.7); font-size: 10.5px; text-transform: lowercase; }
    .thinking-summary:hover { color: var(--text); }
    .thinking-details[open] > .thinking-summary { border-bottom: 1px solid rgba(127,127,127,0.12); margin-bottom: 4px; }
    .thinking-body {
      padding: 2px 12px 8px 14px; font-size: 12px; line-height: 1.5;
      color: var(--text-dim); font-style: normal;
    }
    .thinking-body p { margin: 0 0 6px; }
    .thinking-body code { background: rgba(127,127,127,0.16); padding: 1px 4px; border-radius: 3px; }
    .msg.decisioning-step { background: rgba(255,167,38,0.05); border-left: 2px solid #ffa726; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.decisioning-step .msg-role { color: #ffa726; }
    .msg.decisioning-step .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
    /* ── runtime event cards ── */
    .msg.runtime-event {
      align-self: flex-start;
      width: 100%;
      max-width: 100%;
      background: rgba(127,127,127,0.05);
      border: 1px solid rgba(127,127,127,0.16);
      border-left: 2px solid var(--rt-tone, #8a8a8a);
      border-radius: 8px;
      padding: 7px 9px;
      gap: 5px;
    }
    .msg.runtime-event.tone-ok { --rt-tone: #4ec9b0; }
    .msg.runtime-event.tone-err { --rt-tone: #f4564f; }
    .msg.runtime-event.tone-warn { --rt-tone: #e2b341; }
    .msg.runtime-event.tone-run { --rt-tone: #4c84ff; }
    .msg.runtime-event.tone-idle { --rt-tone: #8a8a8a; }
    .rt-head { display: flex; align-items: center; gap: 7px; flex-wrap: wrap; }
    .rt-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--rt-tone, #8a8a8a); flex-shrink: 0; }
    .msg.runtime-event.tone-run .rt-dot { box-shadow: 0 0 0 3px rgba(76,132,255,0.18); }
    .rt-kind { font-size: 10.5px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--rt-tone, #8a8a8a); font-weight: 600; }
    .rt-summary { font-size: 11.5px; color: var(--text-dim); flex: 1; min-width: 0; }
    .rt-detail { font-size: 11.5px; color: var(--text); display: flex; flex-direction: column; gap: 3px; }
    .rt-pills { display: flex; flex-wrap: wrap; gap: 4px; }
    .rt-pill { font-size: 10.5px; padding: 1px 7px; border-radius: 10px; border: 1px solid transparent; }
    .rt-pill-ok { background: rgba(78,201,176,0.14); color: #8fe6cf; border-color: rgba(78,201,176,0.3); }
    .rt-pill-err { background: rgba(244,71,71,0.14); color: #ffa0a0; border-color: rgba(244,71,71,0.3); }
    .rt-pill-warn { background: rgba(226,179,65,0.14); color: #ecd08a; border-color: rgba(226,179,65,0.3); }
    .rt-pill-run { background: rgba(76,132,255,0.14); color: #a9c4ff; border-color: rgba(76,132,255,0.3); }
    .rt-pill-idle { background: rgba(127,127,127,0.14); color: var(--text-dim); border-color: rgba(127,127,127,0.25); }
    .rt-row { display: flex; gap: 6px; align-items: baseline; }
    .rt-k { color: var(--text-dim); min-width: 64px; font-size: 11px; }
    .rt-v { color: var(--text); word-break: break-word; flex: 1; min-width: 0; }
    .rt-muted { color: var(--text-dim); font-size: 10.5px; }
    .rt-route { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; font-size: 12px; }
    .rt-phase { color: var(--text-dim); text-transform: uppercase; font-size: 10px; letter-spacing: 0.04em; }
    .rt-arrow { color: var(--text-dim); }
    .rt-model { color: #4ec9b0; font-weight: 600; font-family: var(--vscode-editor-font-family, monospace); }
    .rt-conf { display: flex; align-items: center; gap: 7px; }
    .rt-conf-track { flex: 1; height: 5px; background: rgba(127,127,127,0.2); border-radius: 3px; overflow: hidden; }
    .rt-conf-fill { height: 100%; background: linear-gradient(90deg, #4c84ff, #4ec9b0); border-radius: 3px; }
    .rt-conf-label { font-size: 10.5px; color: var(--text-dim); min-width: 30px; text-align: right; }
    .rt-steps { margin: 2px 0 0; padding-left: 18px; font-size: 11px; }
    .rt-steps li { margin: 1px 0; }
    .rt-timeline { list-style: none; margin: 2px 0 0; padding: 0; font-size: 11px; }
    .rt-timeline li { position: relative; padding: 1px 0 1px 14px; }
    .rt-timeline li::before { content: ''; position: absolute; left: 2px; top: 7px; width: 6px; height: 6px; border-radius: 50%; }
    .rt-timeline li.rt-tl-ok::before { background: #4ec9b0; }
    .rt-timeline li.rt-tl-err::before { background: #f4564f; }
    .rt-timeline li.rt-tl-run::before { background: #4c84ff; }
    details.rt-raw { margin-top: 2px; }
    details.rt-raw > summary { cursor: pointer; font-size: 10.5px; color: var(--text-dim); list-style: none; }
    details.rt-raw > summary::-webkit-details-marker { display: none; }
    details.rt-raw pre { margin: 4px 0 0; padding: 7px 9px; background: rgba(127,127,127,0.10); border-radius: 6px; overflow-x: auto; font-size: 10.5px; line-height: 1.4; max-height: 220px; overflow-y: auto; }
    .msg.recovery-suggestion { background: rgba(181,126,220,0.06); border-left: 2px solid #b57edc; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.recovery-suggestion .msg-role { color: #d6a8ff; }
    .msg.recovery-suggestion .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
    .msg.notice { background: rgba(120,170,255,0.06); border-left: 2px solid #6b9fff; padding: 5px 8px; align-self: center; max-width: 92%; }
    .msg.notice .msg-role { color: #9cc0ff; font-size: 10px; text-transform: uppercase; letter-spacing: 0.04em; }
    .msg.notice .msg-body { font-size: 11.5px; color: var(--text-dim); font-family: inherit; }
    .decisioning-card {
      display: flex;
      flex-direction: column;
      gap: 10px;
      margin-top: 4px;
    }
    .decisioning-header {
      display: flex;
      flex-wrap: wrap;
      align-items: flex-start;
      justify-content: space-between;
      gap: 10px;
    }
    .decisioning-summary {
      min-width: 180px;
      color: var(--text);
      line-height: 1.45;
    }
    .decisioning-badges {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
      justify-content: flex-end;
    }
    .decisioning-badge,
    .decisioning-chip {
      display: inline-flex;
      align-items: center;
      gap: 4px;
      padding: 3px 8px;
      border-radius: 999px;
      border: 1px solid rgba(255,255,255,0.12);
      background: rgba(255,255,255,0.04);
      color: var(--text-dim);
      font-size: 10px;
      line-height: 1.2;
      white-space: nowrap;
    }
    .decisioning-badge.risk-low { border-color: rgba(102,187,106,0.35); background: rgba(102,187,106,0.12); color: #9be28d; }
    .decisioning-badge.risk-medium { border-color: rgba(255,167,38,0.35); background: rgba(255,167,38,0.12); color: #ffcb7a; }
    .decisioning-badge.risk-high { border-color: rgba(244,71,71,0.35); background: rgba(244,71,71,0.12); color: #ff9a9a; }
    .decisioning-badge.action-allow { border-color: rgba(102,187,106,0.35); }
    .decisioning-badge.action-review { border-color: rgba(255,167,38,0.35); }
    .decisioning-badge.action-deny { border-color: rgba(244,71,71,0.35); }
    .decisioning-overview {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(118px, 1fr));
      gap: 8px;
    }
    .decisioning-metric-card {
      display: flex;
      flex-direction: column;
      gap: 3px;
      padding: 8px 10px;
      border-radius: 10px;
      border: 1px solid rgba(255,255,255,0.08);
      background: rgba(0,0,0,0.1);
    }
    .decisioning-metric-label {
      color: var(--text-dim);
      font-size: 10px;
      letter-spacing: .08em;
      text-transform: uppercase;
    }
    .decisioning-metric-value {
      color: var(--text);
      font-size: 12px;
      font-weight: 700;
    }
    .decisioning-metric-card.risk-low .decisioning-metric-value { color: #9be28d; }
    .decisioning-metric-card.risk-medium .decisioning-metric-value { color: #ffcb7a; }
    .decisioning-metric-card.risk-high .decisioning-metric-value { color: #ff9a9a; }
    .decisioning-section {
      display: flex;
      flex-direction: column;
      gap: 8px;
      padding: 10px 12px;
      border-radius: 12px;
      border: 1px solid rgba(255,255,255,0.08);
      background: rgba(255,255,255,0.03);
    }
    .decisioning-section-title {
      font-size: 10px;
      font-weight: 700;
      letter-spacing: .12em;
      text-transform: uppercase;
      color: var(--accent-text);
    }
    .decisioning-section-note {
      font-size: 11px;
      color: var(--text-dim);
    }
    .decisioning-risk-panel {
      display: flex;
      flex-direction: column;
      gap: 8px;
    }
    .decisioning-risk-summary {
      display: flex;
      flex-wrap: wrap;
      align-items: flex-start;
      justify-content: space-between;
      gap: 10px;
    }
    .decisioning-risk-label {
      color: var(--text);
      font-size: 12px;
      font-weight: 700;
    }
    .decisioning-risk-meter {
      position: relative;
      height: 10px;
      border-radius: 999px;
      overflow: hidden;
      background: rgba(255,255,255,0.06);
    }
    .decisioning-risk-meter span {
      display: block;
      height: 100%;
      border-radius: inherit;
      background: linear-gradient(90deg, rgba(102,187,106,0.92), rgba(255,167,38,0.92), rgba(244,71,71,0.92));
      box-shadow: 0 0 12px rgba(244,71,71,0.14);
    }
    .decisioning-risk-details {
      display: flex;
      flex-direction: column;
      gap: 4px;
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.45;
    }
    .decisioning-score-list {
      display: flex;
      flex-direction: column;
      gap: 8px;
    }
    .decisioning-score-item {
      display: flex;
      flex-direction: column;
      gap: 6px;
      padding: 8px 10px;
      border-radius: 10px;
      border: 1px solid rgba(255,255,255,0.08);
      background: rgba(0,0,0,0.12);
    }
    .decisioning-score-item.selected {
      border-color: rgba(255,167,38,0.32);
      background: rgba(255,167,38,0.08);
    }
    .decisioning-score-header,
    .decisioning-tree-head {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 10px;
    }
    .decisioning-score-name,
    .decisioning-tree-text {
      display: flex;
      align-items: center;
      flex-wrap: wrap;
      gap: 6px;
      color: var(--text);
    }
    .decisioning-score-meta,
    .decisioning-tree-meta,
    .decisioning-node-notes {
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.4;
    }
    .decisioning-score-value {
      color: var(--accent-text);
      font-size: 12px;
      font-weight: 700;
      flex-shrink: 0;
    }
    .decisioning-score-bar {
      position: relative;
      height: 7px;
      border-radius: 999px;
      overflow: hidden;
      background: rgba(255,255,255,0.06);
    }
    .decisioning-score-bar span {
      display: block;
      height: 100%;
      border-radius: inherit;
      background: linear-gradient(90deg, rgba(255,167,38,0.92), rgba(78,201,176,0.92));
      box-shadow: 0 0 12px rgba(255,167,38,0.18);
    }
    .decisioning-chip-row {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }
    .decisioning-chip.selected {
      border-color: rgba(255,167,38,0.32);
      background: rgba(255,167,38,0.14);
      color: #ffd7a1;
    }
    .decisioning-chip.selected-tag {
      margin-left: 4px;
      border-color: rgba(255,167,38,0.32);
      background: rgba(255,167,38,0.14);
      color: #ffd7a1;
    }
    .decisioning-tree {
      display: flex;
      flex-direction: column;
      gap: 8px;
    }
    .decisioning-tree-node {
      display: flex;
      flex-direction: column;
      gap: 6px;
      padding: 8px 10px 8px 12px;
      border-radius: 10px;
      border: 1px solid rgba(255,255,255,0.08);
      background: rgba(0,0,0,0.1);
      border-left: 3px solid rgba(255,167,38,0.38);
    }
    .decisioning-tree-node.kind-task { border-left-color: rgba(255,167,38,0.68); }
    .decisioning-tree-node.kind-step { border-left-color: rgba(78,201,176,0.68); }
    .decisioning-tree-node.level-0 { margin-left: 0; }
    .decisioning-tree-node.level-1 { margin-left: 12px; }
    .decisioning-tree-node.level-2 { margin-left: 24px; }
    .decisioning-tree-node.level-3 { margin-left: 36px; }
    .decisioning-tree-node.level-deep { margin-left: 48px; }
    .decisioning-node-overflow {
      color: var(--text-dim);
      font-size: 11px;
      padding: 4px 0 0 10px;
      border-left: 1px dashed rgba(255,255,255,0.12);
    }
    .decisioning-detail-list {
      display: flex;
      flex-direction: column;
      gap: 6px;
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.45;
    }
    .decisioning-detail-item {
      padding: 6px 8px;
      border-radius: 8px;
      background: rgba(0,0,0,0.1);
      border: 1px solid rgba(255,255,255,0.06);
    }
    .decisioning-detail-label {
      color: var(--text);
      font-weight: 700;
    }
    .decisioning-detail-json {
      margin-top: 4px;
      font-family: monospace;
      white-space: pre-wrap;
    }
    .decisioning-tree-title {
      display: flex;
      flex-direction: column;
      gap: 2px;
    }
    .decisioning-tree-kind {
      font-size: 10px;
      font-weight: 700;
      letter-spacing: .1em;
      text-transform: uppercase;
      color: var(--text-dim);
    }
    .decisioning-tree-children {
      display: flex;
      flex-direction: column;
      gap: 8px;
      margin-top: 2px;
    }
    
    
    .msg-role {
      font-size: 10px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: .08em;
      color: var(--text-dim);
    }
    .msg.user .msg-role { color: var(--accent-text); text-transform: none; }
    .msg.assistant .msg-role { color: var(--success); text-transform: none; }
    .msg-body { font-size: 13px; }
    .empty-state {
      flex: 1;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: 10px;
      color: var(--text-dim);
      text-align: center;
      padding: 24px;
    }
    .empty-state .logo { font-size: 28px; font-weight: 900; letter-spacing: -.02em; color: var(--text); }
    .empty-state .tagline { font-size: 12px; max-width: 220px; }
    .quick-chips {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
      justify-content: center;
      margin-top: 8px;
    }
    .chip {
      padding: 4px 10px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: var(--surface2);
      color: var(--text-dim);
      font-size: 11px;
      cursor: pointer;
      transition: border-color .15s, color .15s;
    }
    .chip:hover { border-color: var(--accent); color: var(--accent-text); }
    /* ── history drawer ── */
    .history-drawer {
      flex: 0 0 auto;
      border-top: 1px solid var(--border);
      background: var(--surface);
      max-height: 0;
      overflow: hidden;
      transition: max-height .2s ease;
    }
    .history-drawer.open { max-height: 220px; overflow-y: auto; }
    .history-drawer::-webkit-scrollbar { width: 4px; }
    .history-drawer::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.1); border-radius: 2px; }
    .history-item {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 7px 10px;
      cursor: pointer;
      border-bottom: 1px solid var(--border);
      font-size: 12px;
    }
    .history-item:hover { background: var(--surface2); }
    .history-item.active { background: var(--accent-soft); color: var(--accent-text); }
    .history-item .hi-title { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .history-item .hi-meta { font-size: 10px; color: var(--text-dim); flex-shrink: 0; }
    .history-delete {
      width: 22px;
      height: 22px;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      border: 1px solid transparent;
      border-radius: 6px;
      background: transparent;
      color: var(--text-dim);
      cursor: pointer;
      font-size: 12px;
    }
    .history-delete:hover {
      border-color: rgba(244,71,71,0.35);
      background: rgba(244,71,71,0.10);
      color: #ff9a9a;
    }
    .history-delete:disabled {
      opacity: 0.5;
      cursor: wait;
    }
    /* ── composer ── */
    .composer {
      flex: 0 0 auto;
      border-top: 1px solid var(--border);
      background: var(--surface);
      padding: 8px 10px;
      display: flex;
      flex-direction: column;
      gap: 6px;
    }
    .composer-row {
      display: flex;
      align-items: flex-end;
      gap: 6px;
    }
    .composer-input {
      flex: 1;
      background: var(--surface2);
      border: 1px solid var(--border);
      border-radius: var(--radius);
      color: var(--text);
      font: inherit;
      font-size: 13px;
      padding: 8px 10px;
      resize: none;
      min-height: 36px;
      max-height: 160px;
      overflow-y: auto;
      outline: none;
      transition: border-color .15s;
    }
    .composer-input:focus { border-color: var(--accent); }
    .composer-input::placeholder { color: var(--text-dim); }
    .attach-btn {
      flex-shrink: 0;
      background: none;
      border: 1px solid var(--border);
      border-radius: var(--radius);
      color: var(--text-dim);
      cursor: pointer;
      padding: 0 8px;
      font-size: 15px;
      height: 36px;
      display: flex;
      align-items: center;
      transition: border-color .15s, color .15s;
    }
    .attach-btn:hover { border-color: var(--accent); color: var(--accent-text); }
    .attach-chips {
      display: flex;
      flex-wrap: wrap;
      gap: 4px;
      padding: 0 2px 4px;
    }
    .attach-chip {
      display: inline-flex;
      align-items: center;
      gap: 4px;
      padding: 2px 8px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: var(--surface2);
      font-size: 11px;
      color: var(--text-dim);
      max-width: 220px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .attach-chip .remove-chip {
      cursor: pointer;
      opacity: .6;
      flex-shrink: 0;
    }
    .attach-chip .remove-chip:hover { opacity: 1; }
    .send-btn {
      flex-shrink: 0;
      background: var(--accent);
      border: none;
      border-radius: var(--radius);
      color: #fff;
      cursor: pointer;
      padding: 8px 12px;
      font-size: 13px;
      font-weight: 600;
      transition: opacity .15s;
      height: 36px;
      display: flex;
      align-items: center;
      gap: 4px;
    }
    .send-btn:hover { opacity: .85; }
    .send-btn:disabled { opacity: .4; cursor: not-allowed; }
    .stop-btn {
      flex-shrink: 0;
      background: rgba(248,81,73,0.18);
      border: 1px solid rgba(248,81,73,0.5);
      border-radius: var(--radius);
      color: #ff7b72;
      cursor: pointer;
      padding: 6px 10px;
      font-size: 14px;
      height: 36px;
      display: flex;
      align-items: center;
      justify-content: center;
      transition: opacity .15s;
    }
    .stop-btn:hover { background: rgba(248,81,73,0.3); }
    .composer-hint {
      font-size: 10px;
      color: var(--text-dim);
      display: flex;
      align-items: center;
      gap: 8px;
    }
    .composer-hint kbd {
      background: var(--surface2);
      border: 1px solid var(--border);
      border-radius: 3px;
      padding: 1px 4px;
      font-size: 10px;
      font-family: inherit;
    }
    /* ── status bar ── */
    .statusbar {
      flex: 0 0 auto;
      padding: 3px 10px;
      font-size: 10px;
      color: var(--text-dim);
      background: var(--surface);
      border-top: 1px solid var(--border);
      display: flex;
      align-items: center;
      gap: 8px;
      min-height: 20px;
    }
    .statusbar .status-dot {
      width: 5px; height: 5px;
      border-radius: 50%;
      background: var(--text-dim);
      flex-shrink: 0;
    }
    .statusbar .status-dot.running { background: var(--accent-text); animation: pulse 1s infinite; }
    .statusbar .status-dot.done { background: var(--success); }
    .statusbar .status-dot.error { background: var(--danger); }
    @keyframes pulse { 0%,100% { opacity:1; } 50% { opacity:.4; } }
    /* ── streaming cursor ── */
    .cursor { display: inline-block; width: 2px; height: 13px; background: var(--accent-text); animation: blink .8s step-end infinite; vertical-align: text-bottom; margin-left: 1px; }
    @keyframes blink { 0%,100% { opacity:1; } 50% { opacity:0; } }
    /* ── trust warning ── */
    .trust-banner {
      flex: 0 0 auto;
      background: rgba(244,71,71,0.10);
      border-bottom: 1px solid rgba(244,71,71,0.25);
      padding: 6px 10px;
      font-size: 11px;
      color: #f88;
      display: flex;
      align-items: center;
      gap: 6px;
    }
    .trust-banner[hidden] { display: none; }
    .task-board-surface.collapsed {
      padding: 7px 9px;
    }
    .task-board-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      margin-bottom: 8px;
    }
    .task-board-surface.collapsed .task-board-header {
      margin-bottom: 0;
    }
    .task-board-heading {
      flex: 1;
      min-width: 0;
      cursor: pointer;
    }
    .task-board-toggle {
      width: 24px;
      height: 24px;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      border: 1px solid rgba(78,201,176,0.22);
      border-radius: 6px;
      background: rgba(78,201,176,0.07);
      color: var(--text);
      cursor: pointer;
      font-size: 11px;
    }
    .task-board-toggle:hover {
      border-color: rgba(78,201,176,0.42);
      background: rgba(78,201,176,0.13);
    }
    .task-board-title {
      color: var(--text);
      font-size: 12px;
      font-weight: 700;
      letter-spacing: .03em;
    }
    .task-board-subtitle {
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.45;
      margin-top: 2px;
    }
    .task-board-summary {
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.35;
      margin-top: 2px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .task-board-body[hidden] {
      display: none;
    }
    .task-board-grid {
      display: grid;
      grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr);
      gap: 8px;
    }
    .task-board-panel {
      border: 1px solid rgba(255,255,255,0.08);
      border-radius: 10px;
      background: rgba(0,0,0,0.10);
      padding: 8px;
      min-width: 0;
    }
    .task-board-panel-title {
      color: var(--text);
      font-size: 11px;
      font-weight: 700;
      margin-bottom: 6px;
    }
    .task-board-list,
    .task-board-recovery-list,
    .task-board-worker-list,
    .task-board-metric-list {
      display: flex;
      flex-direction: column;
      gap: 6px;
    }
    .task-board-worker-detail-grid {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 6px;
      margin-top: 6px;
    }
    .task-board-task,
    .task-board-node,
    .task-board-recovery-item,
    .task-board-worker,
    .task-board-worker-detail,
    .task-board-supervisor,
    .task-board-metric-card {
      border: 1px solid rgba(255,255,255,0.07);
      border-radius: 8px;
      background: rgba(255,255,255,0.03);
      padding: 7px 8px;
    }
    .task-board-worker {
      cursor: pointer;
    }
    .task-board-worker.selected {
      border-color: rgba(78,201,176,0.45);
      background: rgba(78,201,176,0.08);
    }
    .task-board-detail-label {
      color: var(--text-dim);
      font-size: 10px;
      text-transform: uppercase;
      letter-spacing: .04em;
      margin-bottom: 2px;
    }
    .task-board-detail-value {
      color: var(--text);
      font-size: 11px;
      line-height: 1.35;
      overflow-wrap: anywhere;
    }
    .task-board-task-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin-bottom: 3px;
    }
    .task-board-task-title {
      color: var(--text);
      font-size: 12px;
      font-weight: 600;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .task-board-meta,
    .task-board-empty {
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.4;
    }
    /* ── task board progress visualization ── */
    .tb-seg-ok { background: #4ec9b0; }
    .tb-seg-err { background: #f4564f; }
    .tb-seg-warn { background: #e2b341; }
    .tb-seg-run { background: #4c84ff; }
    .tb-seg-idle { background: rgba(127,127,127,0.45); }
    .tb-progress { margin: 4px 0 8px; }
    .tb-progress-head { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 4px; }
    .tb-progress-label { font-size: 11px; color: var(--text); }
    .tb-progress-pct { font-size: 11px; color: var(--text-dim); font-weight: 600; }
    .tb-bar { display: flex; height: 6px; border-radius: 4px; overflow: hidden; background: rgba(127,127,127,0.18); }
    .tb-bar-seg { height: 100%; }
    .tb-chips { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 5px; }
    .tb-chip { font-size: 10px; padding: 1px 6px; border-radius: 9px; border: 1px solid transparent; }
    .tb-chip-ok { background: rgba(78,201,176,0.14); color: #8fe6cf; border-color: rgba(78,201,176,0.3); }
    .tb-chip-err { background: rgba(244,71,71,0.14); color: #ffa0a0; border-color: rgba(244,71,71,0.3); }
    .tb-chip-warn { background: rgba(226,179,65,0.14); color: #ecd08a; border-color: rgba(226,179,65,0.3); }
    .tb-chip-run { background: rgba(76,132,255,0.14); color: #a9c4ff; border-color: rgba(76,132,255,0.3); }
    .tb-chip-idle { background: rgba(127,127,127,0.14); color: var(--text-dim); border-color: rgba(127,127,127,0.25); }
    .tb-nodes { margin-top: 8px; }
    .tb-nodes-head { display: flex; justify-content: space-between; align-items: baseline; }
    .tb-nodes-count { font-size: 10.5px; color: var(--text-dim); font-weight: 600; }
    .tb-node-grid { display: flex; flex-wrap: wrap; gap: 3px; margin-top: 5px; }
    .tb-node-dot { width: 9px; height: 9px; border-radius: 2px; flex-shrink: 0; }
    .task-board-status {
      display: inline-flex;
      align-items: center;
      padding: 2px 7px;
      border-radius: 999px;
      border: 1px solid rgba(255,255,255,0.12);
      color: var(--text-dim);
      font-size: 10px;
      white-space: nowrap;
    }
    .task-board-status.status-running { border-color: rgba(255,167,38,0.45); color: #ffcb7a; }
    .task-board-status.status-completed { border-color: rgba(102,187,106,0.45); color: #9be28d; }
    .task-board-status.status-blocked,
    .task-board-status.status-failed,
    .task-board-status.status-cancelled { border-color: rgba(244,71,71,0.45); color: #ff9a9a; }
    .task-board-status.status-created,
    .task-board-status.status-pending { border-color: rgba(76,132,255,0.35); color: #a8c7ff; }
    .content-shell {
      flex: 1 1 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) minmax(280px, var(--trace-width, 340px));
      gap: 10px;
      overflow: hidden;
      padding: 0 10px;
    }
    .pane-resizer {
      width: 8px;
      cursor: col-resize;
      background: rgba(255,255,255,0.04);
      transition: background 0.2s ease;
    }
    .pane-resizer:hover {
      background: rgba(255,255,255,0.11);
    }
    .trace-shell {
      display: flex;
      flex-direction: column;
      min-height: 0;
      max-height: 100%;
      border: 1px solid var(--border);
      border-radius: var(--radius);
      background: var(--surface);
      overflow: hidden;
    }
    .trace-shell-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      padding: 10px 12px;
      border-bottom: 1px solid var(--border);
      background: rgba(255,255,255,0.03);
    }
    .trace-shell-title {
      font-size: 11px;
      font-weight: 700;
      color: var(--text);
      letter-spacing: .04em;
      text-transform: uppercase;
    }
    .trace-shell-body {
      flex: 1 1 0;
      display: flex;
      flex-direction: column;
      min-height: 0;
      overflow: hidden;
    }
    .trace-feed {
      flex: 0 0 auto;
      min-height: 120px;
      height: var(--trace-feed-height, 240px);
      overflow-y: auto;
      padding: 10px;
      border-bottom: 1px solid var(--border);
    }
    .trace-feed::-webkit-scrollbar { width: 4px; }
    .trace-feed::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.1); border-radius: 2px; }
    .trace-feed .msg { margin-bottom: 10px; }
    .trace-body-resizer {
      height: 6px;
      cursor: row-resize;
      background: rgba(255,255,255,0.04);
      transition: background 0.2s ease;
    }
    .trace-body-resizer:hover {
      background: rgba(255,255,255,0.11);
    }
    .task-board-surface {
      flex: 1 1 0;
      min-height: 0;
      margin: 0;
      padding: 8px;
      border-radius: 0 0 8px 8px;
      border-top: none;
      border-left: none;
      border-right: none;
      border-bottom: none;
      background: linear-gradient(180deg, rgba(78,201,176,0.08), rgba(255,255,255,0.02));
      box-shadow: inset 0 0 0 1px rgba(255,255,255,0.02);
      overflow-y: auto;
    }
    .task-board-surface[hidden] { display: none; }
  </style>
`;
    const _body = `</head>
<body>
  <!-- top bar -->
  <div class="topbar">
    <span class="topbar-title">Himalaya</span>
    <button class="icon-btn" id="btnHistory" title="Toggle history">&#9776;</button>
    <button class="icon-btn" id="btnNew" title="New session">&#43;</button>
    <button class="icon-btn" id="btnSkills" title="Skills">&#9733;</button>
    <button class="icon-btn" id="btnReasoning" title="Toggle reasoning visualization">🔎</button>
    <button class="icon-btn" id="btnRefresh" title="Refresh">&#8635;</button>
  </div>

  <!-- trust warning -->
  <div class="trust-banner" id="trustBanner" hidden>
    &#9888; Workspace is untrusted — prompt execution is blocked.
  </div>

  <!-- model / permission bar -->
  <div class="model-bar">
    <button class="model-pill" id="btnModel" title="Configure model route">
      <span class="dot" id="modelDot"></span>
      <span class="model-pill-label" id="modelLabel">Loading…</span>
      <span>&#9660;</span>
    </button>
    <button class="perm-pill" id="btnPerm" title="Change permission mode">
      <span id="permLabel">read-only</span>
    </button>
    <span class="spacer"></span>
    <button class="icon-btn" id="btnDoctor" title="Doctor">&#10003;</button>
    <button class="icon-btn" id="btnStatus" title="Status">&#9432;</button>
  </div>

  <div class="content-shell">
    <div class="thread" id="thread">
      <div class="empty-state" id="emptyState">
        <div class="logo">H</div>
        <div class="tagline">Ask Himalaya to inspect, refactor, or recover a session.</div>
        <div class="quick-chips" id="quickChips">
          <button class="chip" data-prompt="Summarize this repository">Summarize repo</button>
          <button class="chip" data-prompt="Show the current status">Status</button>
          <button class="chip" data-prompt="Run the doctor health check">Doctor</button>
          <button class="chip" data-prompt="Open my latest session">Resume latest</button>
        </div>
      </div>
    </div>
    <div class="pane-resizer" id="traceResizer" title="Drag to resize trace panel"></div>
    <div class="trace-shell" id="traceShell" hidden>
      <div class="trace-shell-header">
        <div class="trace-shell-title">Trace</div>
        <button class="icon-btn" id="btnTraceClose" title="Close trace panel">✕</button>
      </div>
      <div class="trace-shell-body">
        <div class="trace-feed" id="traceFeed"></div>
        <div class="trace-body-resizer" id="traceBodyResizer" title="Drag to resize trace feed"></div>
        <div class="task-board-surface" id="taskBoardSurface" hidden></div>
      </div>
    </div>
  </div>

  <!-- history drawer (collapsed by default) -->
  <div class="history-drawer" id="historyDrawer">
    <div id="historyList"></div>
  </div>

  <!-- composer -->
  <div class="composer">
    <div id="attachChips" class="attach-chips" style="display:none"></div>
    <div class="composer-row">
      <button class="attach-btn" id="attachBtn" title="Attach files">📎</button>
      <textarea
        id="promptInput"
        class="composer-input"
        placeholder="Ask Himalaya…"
        rows="1"
        autocomplete="off"
        spellcheck="false"
      ></textarea>
      <button class="send-btn" id="sendBtn">Send</button>
      <button class="stop-btn" id="stopBtn" style="display:none" title="Stop generation">⏹</button>
    </div>
    <div class="composer-hint">
      <kbd>Enter</kbd> send &nbsp;·&nbsp; <kbd>Shift+Enter</kbd> newline &nbsp;·&nbsp; 📎 attach files
    </div>
  </div>

  <!-- status bar -->
  <div class="statusbar">
    <span class="status-dot" id="statusDot"></span>
    <span id="statusText">Ready</span>
  </div>

`;
    const _script = `  <script nonce="${nonce}" src="${markedUri}"></script>
  <script nonce="${nonce}">
  window.onerror = function(msg, src, line, col, err) {
    try {
      if (window.__himalayaPostError) {
        window.__himalayaPostError(msg, line, col);
      }
    } catch(e) {}
  };
  (function() {
    'use strict';
    const vscode = acquireVsCodeApi();
    window.__himalayaPostError = function(msg, line, col) {
      try { vscode.postMessage({ type: 'webview-error', message: String(msg), line: line, col: col }); } catch (_) {}
    };

    /* ── initial state ── */
    const INIT = ${stateJson};
    const HISTORY = ${historyJson};
    const LOCAL_MODELS = ${localModelsJson};
    const DEFAULT_PERMISSION = ${JSON.stringify(DEFAULT_PERMISSION_MODE)};
    const PERMISSION_MODES = ${permissionModesJson};
    const ACTIVE_RECORD = ((HISTORY || []).find(function(rec) { return rec.id === INIT.activeRecordId; }) || {});

    const state = {
      model: INIT.model,
      modelBackend: INIT.modelBackend,
      permissionMode: INIT.permissionMode,
      resumeTarget: INIT.resumeTarget || ACTIVE_RECORD.resumeTarget || '',
      isTrusted: INIT.isTrusted,
      activeRecordId: INIT.activeRecordId,
      showReasoning: INIT.showReasoning || false,
      identity: INIT.identity || {},
      historyOpen: false,
      streaming: false,
      lastRunFailed: false,
      messages: (ACTIVE_RECORD.messages || []).map(function(m) {
        return {
          role: m.role,
          text: m.text,
          attachments: Array.isArray(m.attachments) ? m.attachments.slice() : []
        };
      }),
      recoveryEvidence: (ACTIVE_RECORD.recoveryEvidence || []).slice(),
      taskBoard: {
        collapsed: true,
        tasks: {},
        taskOrder: [],
        currentNode: null,
        planNodes: {},
        planNodeOrder: [],
        recoveryEvents: [],
        workers: {},
        workerOrder: [],
        selectedWorkerId: null,
        workerSupervisor: null,
        daemon: null,
        routeSummary: null,
        benchmark: null
      },
      traceOpen: Boolean(INIT.showReasoning),
      traceManualClosed: !Boolean(INIT.showReasoning),
      traceAutoOpenEnabled: true,
      historyRecords: HISTORY
    };

    /* ── DOM refs ── */
    const thread       = document.getElementById('thread');
    const emptyState   = document.getElementById('emptyState');
    const promptInput  = document.getElementById('promptInput');
    const sendBtn      = document.getElementById('sendBtn');
    const stopBtn      = document.getElementById('stopBtn');
    const attachBtn    = document.getElementById('attachBtn');
    const attachChips  = document.getElementById('attachChips');
    const modelLabel   = document.getElementById('modelLabel');
    const modelDot     = document.getElementById('modelDot');
    const permLabel    = document.getElementById('permLabel');
    const statusDot    = document.getElementById('statusDot');
    const statusText   = document.getElementById('statusText');
    const historyDrawer= document.getElementById('historyDrawer');
    const historyList  = document.getElementById('historyList');
    const trustBanner  = document.getElementById('trustBanner');
    const traceShell = document.getElementById('traceShell');
    const traceFeed = document.getElementById('traceFeed');
    const traceResizer = document.getElementById('traceResizer');
    const traceBodyResizer = document.getElementById('traceBodyResizer');
    const taskBoardSurface = document.getElementById('taskBoardSurface');

    /* ── attachment state ── */
    let attachedFiles = [];

    function renderAttachChips() {
      if (attachedFiles.length === 0) {
        attachChips.style.display = 'none';
        attachChips.innerHTML = '';
        return;
      }
      attachChips.style.display = 'flex';
      attachChips.innerHTML = attachedFiles.map((f, i) => {
        const name = f.split(/[\\/]/).pop() || f;
        return '<span class="attach-chip" title="' + esc(f) + '">'
          + '📄 ' + esc(name)
          + '<span class="remove-chip" data-idx="' + i + '">✕</span>'
          + '</span>';
      }).join('');
      attachChips.querySelectorAll('.remove-chip').forEach(el => {
        el.addEventListener('click', () => {
          attachedFiles.splice(Number(el.dataset.idx), 1);
          renderAttachChips();
        });
      });
    }

    if (attachBtn) {
      attachBtn.addEventListener('click', () => {
        try { vscode.postMessage({ type: 'pick-file' }); } catch (_) {}
      });
    }

    /* ── helpers ── */
    function esc(s) {
      return String(s ?? '')
        .replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')
        .replace(/"/g,'&quot;').replace(/'/g,'&#39;');
    }

    function setStatus(text, kind) {
      statusText.textContent = text;
      statusDot.className = 'status-dot' + (kind ? ' ' + kind : '');
    }

    function updateSendButtonState() {
      try {
        if (!sendBtn) { return; }
        const blocked = !state.isTrusted;
        sendBtn.disabled = state.streaming || blocked;
        sendBtn.style.display = state.streaming ? 'none' : '';
        sendBtn.title = blocked
          ? 'Trust the workspace or enable himalayaCode.allowUntrustedRuns to run prompts'
          : state.streaming
            ? 'A request is already running'
            : 'Send prompt';
        if (stopBtn) {
          stopBtn.style.display = state.streaming ? '' : 'none';
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateSendButtonState failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateModelBar() {
      try {
        if (!modelLabel || !modelDot || !permLabel) { return; }
        const b = state.modelBackend;
        const dotClass = b === 'cloud' ? 'cloud' : b === 'ollama' ? 'local' : 'unknown';
        modelDot.className = 'dot ' + dotClass;
        modelLabel.textContent = state.model || 'No model';
        permLabel.textContent = state.permissionMode || DEFAULT_PERMISSION;
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateModelBar failed: ' + String(e) }); } catch (_) {}
      }
    }

    function autoResize() {
      promptInput.style.height = 'auto';
      promptInput.style.height = Math.min(promptInput.scrollHeight, 160) + 'px';
    }

    function scrollBottom() {
      thread.scrollTop = thread.scrollHeight;
    }

    function showEmpty(show) {
      if (emptyState) { emptyState.style.display = show ? 'flex' : 'none'; }
    }

    function cleanMemoryValue(value) {
      return String(value || '').replace(/^[\\s,.;:!?，。；：！？"'“”‘’]+|[\\s,.;:!?，。；：！？"'“”‘’]+$/g, '').trim();
    }

    function takeMemoryValue(value) {
      const raw = String(value || '');
      const lower = raw.toLowerCase();
      let end = raw.length;
      [' and ', ' but ', ' from now on', ' going forward', '以后', '以后用', '以后请'].forEach(function(delimiter) {
        const index = lower.indexOf(delimiter);
        if (index >= 0) { end = Math.min(end, index); }
      });
      const delimiterMatch = raw.match(/[,.;!?，。；！？\\n\\r]/);
      if (delimiterMatch && delimiterMatch.index !== undefined) {
        end = Math.min(end, delimiterMatch.index);
      }
      return cleanMemoryValue(raw.slice(0, end));
    }

    function extractAfterAny(text, lower, markers) {
      for (let i = 0; i < markers.length; i += 1) {
        const marker = markers[i];
        const index = lower.indexOf(marker);
        if (index >= 0) {
          const value = takeMemoryValue(text.slice(index + marker.length));
          if (value && Array.from(value).length <= 64) { return value; }
        }
      }
      return '';
    }

    function updateIdentityFromPrompt(text) {
      const lower = String(text || '').toLowerCase();
      const next = Object.assign({}, state.identity || {});
      const userName = extractAfterAny(text, lower, ['我叫', '我的名字是', '叫我', 'my name is ', 'call me ']);
      const assistantName = extractAfterAny(text, lower, ['以后你叫', '你叫', '你的名字是', 'your name is ', 'i will call you ', "i'll call you "]);
      if (userName) { next.userDisplayName = userName; }
      if (assistantName) { next.assistantDisplayName = assistantName; }
      state.identity = next;
    }

    function labelForRole(role) {
      if (role === 'user') { return (state.identity && state.identity.userDisplayName) || 'You'; }
      if (role === 'assistant') { return (state.identity && state.identity.assistantDisplayName) || 'Himalaya'; }
      if (role === 'tool-step') { return 'Tool'; }
      if (role === 'notice') { return 'Context'; }
      return (String(role || '')[0] || '').toUpperCase() + String(role || '').slice(1);
    }


    let streamBubble = null;
    let streamCursor = null;
    let streamBuffer = '';
    let streamRendered = '';
    let streamRenderQueued = false;

    if (typeof marked !== 'undefined' && marked.setOptions) {
      marked.setOptions({ gfm: true, breaks: false });
    }

    function renderMarkdown(md) {
      try {
        if (typeof marked !== 'undefined' && marked.parse) {
          return marked.parse(String(md == null ? '' : md), { mangle: false, headerIds: false });
        }
      } catch (e) { /* fall through to escaped text */ }
      return '<p>' + esc(md) + '</p>';
    }

    // Find a boundary in the buffer that is safe to render — avoid splitting an
    // open code fence so partial \`\`\` blocks don't flash as broken markup.
    function findSafeRenderBoundary(text) {
      const fences = (text.match(/\`\`\`/g) || []).length;
      if (fences % 2 === 0) { return text.length; }
      const lastFence = text.lastIndexOf('\`\`\`');
      return lastFence > 0 ? lastFence : text.length;
    }

    function smartScroll() {
      try {
        if (!thread) { return; }
        const nearBottom = thread.scrollHeight - thread.scrollTop - thread.clientHeight < 60;
        if (nearBottom) { thread.scrollTop = thread.scrollHeight; }
      } catch (_) {}
    }

    function flushStreamRender() {
      streamRenderQueued = false;
      if (!streamBubble) { return; }
      const body = streamBubble.querySelector ? streamBubble.querySelector('.msg-body') : null;
      if (!body) { return; }
      const boundary = findSafeRenderBoundary(streamBuffer);
      const safeText = streamBuffer.slice(0, boundary);
      if (safeText === streamRendered) { return; }
      streamRendered = safeText;
      body.innerHTML = renderMarkdown(safeText);
      if (streamCursor) { body.appendChild(streamCursor); }
      smartScroll();
    }

    function startStream() {
      try {
        if (!thread) { return; }
        showEmpty(false);
        clearPendingToolCards();
        streamBuffer = '';
        streamRendered = '';
        streamBubble = document.createElement('div');
        streamBubble.className = 'msg assistant';
        streamBubble.innerHTML = '<div class="msg-role">' + esc(labelForRole('assistant')) + '</div><div class="msg-body"></div>';
        thread.appendChild(streamBubble);
        streamCursor = document.createElement('span');
        streamCursor.className = 'cursor';
        const body = streamBubble.querySelector('.msg-body');
        if (body) { body.appendChild(streamCursor); }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'startStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    function appendStream(text) {
      try {
        if (!streamBubble) { startStream(); }
        if (!streamBubble) { return; }
        streamBuffer += (text == null ? '' : text);
        if (!streamRenderQueued) {
          streamRenderQueued = true;
          (window.requestAnimationFrame || window.setTimeout)(flushStreamRender, 16);
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'appendStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    function endStream() {
      try {
        if (streamBubble) {
          const body = streamBubble.querySelector('.msg-body');
          if (body) {
            body.innerHTML = renderMarkdown(streamBuffer);
          }
        }
        clearPendingToolCards();
        if (streamCursor && streamCursor.remove) { streamCursor.remove(); }
        streamCursor = null;
        streamBuffer = '';
        streamRendered = '';
        streamRenderQueued = false;
        streamBubble = null;
        smartScroll();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'endStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── history drawer ── */
    function applyHistoryRecord(rec) {
      if (!rec) { return; }
      state.activeRecordId = rec.id;
      state.resumeTarget = rec.resumeTarget || '';
      state.messages = (rec.messages || []).map(function(m) {
        return {
          role: m.role,
          text: m.text,
          attachments: Array.isArray(m.attachments) ? m.attachments.slice() : []
        };
      });
      state.recoveryEvidence = (rec.recoveryEvidence || []).slice();
      state.taskBoard = taskBoardInitialState();
      const existingIndex = state.historyRecords.findIndex(function(item) { return item.id === rec.id; });
      if (existingIndex >= 0) {
        state.historyRecords[existingIndex] = rec;
      } else {
        state.historyRecords.unshift(rec);
      }
      renderThread();
    }

    function renderHistory() {
      historyList.innerHTML = '';
      if (state.historyRecords.length === 0) {
        historyList.innerHTML = '<div style="padding:10px;color:var(--text-dim);font-size:12px;">No history yet.</div>';
        return;
      }
      state.historyRecords.forEach(function(rec) {
        const item = document.createElement('div');
        item.className = 'history-item' + (rec.id === state.activeRecordId ? ' active' : '');
        const date = new Date(rec.updatedAt || rec.createdAt || 0).toLocaleDateString();
        item.innerHTML =
          '<span class="hi-title">' + esc(rec.title || 'Untitled') + '</span>' +
          '<span class="hi-meta">' + esc(date) + '</span>' +
          '<button type="button" class="history-delete" title="Delete history" aria-label="Delete history">&#128465;</button>';
        const deleteButton = item.querySelector('.history-delete');
        if (deleteButton) {
          deleteButton.addEventListener('click', function(event) {
            event.stopPropagation();
            deleteButton.disabled = true;
            setStatus('Confirm deletion in VS Code...', '');
            vscode.postMessage({ type: 'history-action', action: 'delete', historyId: rec.id });
          });
        }
        item.addEventListener('click', function() {
          state.activeRecordId = rec.id;
          state.resumeTarget = rec.resumeTarget || '';
          vscode.postMessage({ type: 'history-action', historyId: rec.id, selectedHistoryId: rec.id });
          if ((rec.messages || []).length > 0 || (rec.recoveryEvidence || []).length > 0) {
            applyHistoryRecord(rec);
          } else {
            state.messages = [];
            state.recoveryEvidence = [];
            state.taskBoard = taskBoardInitialState();
            renderThread();
          }
          renderHistory();
          toggleHistory(false);
        });
        historyList.appendChild(item);
      });
    }

    function toggleHistory(force) {
      state.historyOpen = force !== undefined ? force : !state.historyOpen;
      historyDrawer.classList.toggle('open', state.historyOpen);
    }

    function addBubble(role, text, attachments) {
      try {
        if (!thread) { return; }
        const div = document.createElement('div');
        const cls = role === 'user' ? 'msg user' : role === 'assistant' ? 'msg assistant' : role === 'error' ? 'msg error' : role === 'stderr' ? 'msg stderr' : role === 'tool-step' ? 'msg tool-step' : role === 'notice' ? 'msg notice' : 'msg';
        div.className = cls;
        const label = labelForRole(role);
        let body = role === 'assistant' ? renderMarkdown(text || '') : esc(text || '');
        if (Array.isArray(attachments) && attachments.length > 0) {
          const chips = attachments.map(function(attachment) {
            const name = attachment && attachment.displayName ? attachment.displayName : attachment && attachment.path ? String(attachment.path).split(/[\\/]/).pop() : 'attachment';
            const kind = attachment && attachment.mediaKind ? attachment.mediaKind : 'file';
            return '<span class="attach-chip" title="' + esc((attachment && attachment.path) || name) + '">' + esc(kind + ': ' + name) + '</span>';
          }).join('');
          body += '<div class="attach-history">' + chips + '</div>';
        }
        div.innerHTML = '<div class="msg-role">' + esc(label) + '</div><div class="msg-body">' + body + '</div>';
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addBubble failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── rich tool cards ── */
    // Per-tool icon + human label. Falls back to a generic gear.
    function toolMeta(name) {
      const n = String(name || '').toLowerCase();
      const map = {
        bash: { icon: '➜', label: 'Shell', lang: 'bash' },
        read_file: { icon: '\u{1F4C4}', label: 'Read', lang: '' },
        write_file: { icon: '✎', label: 'Write', lang: '' },
        generate_file: { icon: '\u{1F4DD}', label: 'Generate', lang: '' },
        edit_file: { icon: '✎', label: 'Edit', lang: '' },
        glob_search: { icon: '\u{1F50D}', label: 'Glob', lang: '' },
        grep_search: { icon: '\u{1F50D}', label: 'Grep', lang: '' },
        webfetch: { icon: '\u{1F310}', label: 'Fetch', lang: '' },
        websearch: { icon: '\u{1F50E}', label: 'Search', lang: '' },
        todowrite: { icon: '☑', label: 'Todo', lang: '' }
      };
      return map[n] || { icon: '⚙', label: String(name || 'Tool'), lang: '' };
    }

    function parseToolInput(msg) {
      if (msg && msg.inputData && typeof msg.inputData === 'object') { return msg.inputData; }
      if (msg && typeof msg.input === 'string' && msg.input) {
        try { return JSON.parse(msg.input); } catch (_) { return null; }
      }
      return null;
    }

    // Split "path:line" or "path#L12" suffix off a file reference.
    function splitPathLine(raw) {
      const s = String(raw || '');
      let m = s.match(/^(.*?):(\\d+)(?::\\d+)?$/);
      if (m) { return { path: m[1], line: parseInt(m[2], 10) }; }
      m = s.match(/^(.*?)#L(\\d+)/);
      if (m) { return { path: m[1], line: parseInt(m[2], 10) }; }
      return { path: s, line: null };
    }

    // Clickable file path chip — posts openFile back to the extension host.
    function filePathChip(rawPath) {
      const parsed = splitPathLine(rawPath);
      const display = esc(String(rawPath || ''));
      const dataPath = esc(parsed.path);
      const dataLine = parsed.line != null ? String(parsed.line) : '';
      return '<button type="button" class="tool-path" data-open-path="' + dataPath +
        '" data-open-line="' + dataLine + '" title="' + esc('Open ' + parsed.path) + '">' +
        display + '</button>';
    }

    // Minimal, dependency-free token highlighter for shell/code snippets.
    function highlightCode(code, lang) {
      let html = esc(String(code == null ? '' : code));
      if (lang === 'bash') {
        html = html
          .replace(/(^|\\n)(\\s*)(#[^\\n]*)/g, '$1$2<span class="tok-cmt">$3</span>')
          .replace(/(&quot;[^&]*?&quot;|&#39;[^&]*?&#39;)/g, '<span class="tok-str">$1</span>')
          .replace(/(^|[|&;]\\s*)([a-zA-Z_][\\w.-]*)/g, '$1<span class="tok-cmd">$2</span>')
          .replace(/(\\s)(--?[a-zA-Z][\\w-]*)/g, '$1<span class="tok-flag">$2</span>');
      } else {
        html = html
          .replace(/(&quot;[^&]*?&quot;|&#39;[^&]*?&#39;)/g, '<span class="tok-str">$1</span>')
          .replace(/\\b(\\d+(?:\\.\\d+)?)\\b/g, '<span class="tok-num">$1</span>');
      }
      return html;
    }

    function codeBlock(code, lang) {
      return '<pre class="tool-code' + (lang ? ' lang-' + lang : '') + '"><code>' +
        highlightCode(code, lang) + '</code></pre>';
    }

    // TOOL_CARD_BUILDERS_1
    // Render a unified-diff style preview for edit_file (old → new).
    function renderDiff(oldStr, newStr) {
      const oldLines = String(oldStr == null ? '' : oldStr).split('\\n');
      const newLines = String(newStr == null ? '' : newStr).split('\\n');
      const rows = [];
      oldLines.forEach(function(l) {
        if (l.length || oldLines.length > 1) { rows.push('<div class="diff-row diff-del">- ' + esc(l) + '</div>'); }
      });
      newLines.forEach(function(l) {
        if (l.length || newLines.length > 1) { rows.push('<div class="diff-row diff-add">+ ' + esc(l) + '</div>'); }
      });
      return '<div class="tool-diff">' + rows.join('') + '</div>';
    }

    // Build the body markup for a tool_use, dispatched by tool name.
    function toolUseBody(name, input) {
      const n = String(name || '').toLowerCase();
      const meta = toolMeta(name);
      if (!input || typeof input !== 'object') { return ''; }
      if (n === 'bash') {
        const cmd = input.command || '';
        const desc = input.description ? '<div class="tool-subtle">' + esc(String(input.description)) + '</div>' : '';
        return desc + codeBlock(cmd, 'bash');
      }
      if (n === 'read_file' || n === 'write_file' || n === 'generate_file' || n === 'edit_file') {
        let out = '<div class="tool-pathline">' + filePathChip(input.path || '') + '</div>';
        if (n === 'edit_file') { out += renderDiff(input.old_string, input.new_string); }
        else if (n === 'write_file' || n === 'generate_file') {
          const content = String(input.content || '');
          const preview = content.length > 600 ? content.slice(0, 600) + '\\n…' : content;
          if (preview) { out += codeBlock(preview, meta.lang); }
        }
        return out;
      }
      if (n === 'glob_search' || n === 'grep_search') {
        let out = '<div class="tool-kv"><span class="tool-k">pattern</span><code>' + esc(String(input.pattern || '')) + '</code></div>';
        if (input.path) { out += '<div class="tool-kv"><span class="tool-k">path</span>' + filePathChip(input.path) + '</div>'; }
        if (input.glob) { out += '<div class="tool-kv"><span class="tool-k">glob</span><code>' + esc(String(input.glob)) + '</code></div>'; }
        return out;
      }
      if (n === 'webfetch') {
        return '<div class="tool-kv"><span class="tool-k">url</span><code>' + esc(String(input.url || '')) + '</code></div>' +
          (input.prompt ? '<div class="tool-subtle">' + esc(String(input.prompt)) + '</div>' : '');
      }
      if (n === 'websearch') {
        return '<div class="tool-kv"><span class="tool-k">query</span><code>' + esc(String(input.query || '')) + '</code></div>';
      }
      // Generic fallback: pretty-print JSON.
      try { return codeBlock(JSON.stringify(input, null, 2), ''); } catch (_) { return ''; }
    }

    // TOOL_CARD_BUILDERS_2
    // Track pending tool cards by tool-use id when available, falling back to
    // per-name queues for legacy or partial events.
    const pendingToolCardsById = {};
    const pendingToolCardsByName = {};

    function pendingToolCardName(msg) {
      return msg && msg.name ? String(msg.name) : 'tool';
    }

    function storePendingToolCard(msg, card) {
      const name = pendingToolCardName(msg);
      const toolUseId = msg && msg.toolUseId ? String(msg.toolUseId) : '';
      const entry = { toolUseId: toolUseId, card: card };
      if (toolUseId) {
        pendingToolCardsById[toolUseId] = entry;
      }
      if (!pendingToolCardsByName[name]) {
        pendingToolCardsByName[name] = [];
      }
      pendingToolCardsByName[name].push(entry);
    }

    function removePendingToolCardFromNameQueue(name, card) {
      const queue = pendingToolCardsByName[name];
      if (!queue || queue.length === 0) {
        return;
      }
      const remaining = queue.filter(function(entry) { return entry.card !== card; });
      if (remaining.length === 0) {
        delete pendingToolCardsByName[name];
      } else {
        pendingToolCardsByName[name] = remaining;
      }
    }

    function takePendingToolCard(msg) {
      const name = pendingToolCardName(msg);
      const toolUseId = msg && msg.toolUseId ? String(msg.toolUseId) : '';
      if (toolUseId && pendingToolCardsById[toolUseId]) {
        const entry = pendingToolCardsById[toolUseId];
        delete pendingToolCardsById[toolUseId];
        removePendingToolCardFromNameQueue(name, entry.card);
        return entry.card;
      }
      const queue = pendingToolCardsByName[name];
      if (!queue || queue.length === 0) {
        return undefined;
      }
      const entry = queue.shift();
      if (queue.length === 0) {
        delete pendingToolCardsByName[name];
      }
      if (entry && entry.toolUseId) {
        delete pendingToolCardsById[entry.toolUseId];
      }
      return entry ? entry.card : undefined;
    }

    function clearPendingToolCards() {
      Object.keys(pendingToolCardsById).forEach(function(key) { delete pendingToolCardsById[key]; });
      Object.keys(pendingToolCardsByName).forEach(function(key) { delete pendingToolCardsByName[key]; });
    }

    function buildResultBlock(output, isError) {
      const text = String(output == null ? '' : output);
      const trimmed = text.trim();
      if (!trimmed) {
        return '<div class="tool-result-empty">' + (isError ? 'failed' : 'no output') + '</div>';
      }
      const lines = text.split('\\n');
      const collapsed = lines.length > 12 || text.length > 800;
      const shown = collapsed ? lines.slice(0, 12).join('\\n') + '\\n…' : text;
      const cls = 'tool-result' + (isError ? ' tool-result-error' : '');
      const moreNote = collapsed ? '<div class="tool-more">' + (lines.length - 12 > 0 ? '+' + (lines.length - 12) + ' more lines' : 'truncated') + '</div>' : '';
      return '<details class="' + cls + '"' + (isError || !collapsed ? ' open' : '') + '>' +
        '<summary>' + (isError ? '✗ error' : '✓ output') + '</summary>' +
        '<pre class="tool-result-body"><code>' + esc(shown) + '</code></pre>' + moreNote + '</details>';
    }

    function renderToolCard(msg) {
      try {
        if (!thread) { return; }
        const name = msg.name ? String(msg.name) : 'tool';
        if (msg.step === 'result') {
          // Attach to the pending use-card if present, else create a standalone card.
          const card = takePendingToolCard(msg);
          const block = buildResultBlock(msg.output, msg.isError);
          if (card && card.querySelector) {
            const slot = card.querySelector('.tool-result-slot');
            if (slot) { slot.innerHTML = block; }
            if (msg.isError) { card.classList.add('has-error'); }
            smartScroll();
            return;
          }
          const div = document.createElement('div');
          div.className = 'msg tool-card' + (msg.isError ? ' has-error' : '');
          const meta = toolMeta(name);
          div.innerHTML = '<div class="tool-head"><span class="tool-icon">' + meta.icon + '</span>' +
            '<span class="tool-name">' + esc(meta.label) + '</span></div>' +
            '<div class="tool-result-slot">' + block + '</div>';
          thread.appendChild(div);
          smartScroll();
          return;
        }
        // step === 'use'
        showEmpty(false);
        const input = parseToolInput(msg);
        const meta = toolMeta(name);
        const div = document.createElement('div');
        div.className = 'msg tool-card';
        div.innerHTML = '<div class="tool-head"><span class="tool-icon">' + meta.icon + '</span>' +
          '<span class="tool-name">' + esc(meta.label) + '</span></div>' +
          '<div class="tool-use-body">' + toolUseBody(name, input) + '</div>' +
          '<div class="tool-result-slot"></div>';
        thread.appendChild(div);
        storePendingToolCard(msg, div);
        smartScroll();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'renderToolCard failed: ' + String(e) }); } catch (_) {}
      }
    }

    function addRecoverySuggestion(msg) {
      try {
        if (!thread) { return; }
        const div = document.createElement('div');
        const tool = msg && msg.tool ? String(msg.tool) : 'tool';
        const suggestion = msg && msg.suggestion ? String(msg.suggestion) : 'Review the failure and retry when the cause is resolved.';
        const action = msg && msg.action ? String(msg.action).replace(/_/g, ' ') : '';
        const failureClass = msg && msg.failureClass ? String(msg.failureClass).replace(/_/g, ' ') : '';
        const reason = msg && msg.reason ? String(msg.reason) : '';
        let body = 'Suggested recovery for ' + tool + ': ' + suggestion;
        if (failureClass) { body += '\\nFailure class: ' + failureClass; }
        if (action) { body += '\\nAction: ' + action; }
        if (reason) { body += '\\nReason: ' + reason.slice(0, 240); }
        div.className = 'msg recovery-suggestion';
        div.innerHTML = '<div class="msg-role">Recovery</div><div class="msg-body">' + esc(body) + '</div>';
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addRecoverySuggestion failed: ' + String(e) }); } catch (_) {}
      }
    }

    function replayRecoveryEvidence() {
      try {
        (state.recoveryEvidence || []).forEach(function(evidence) {
          addRecoverySuggestion(evidence);
        });
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'replayRecoveryEvidence failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── reasoning visualization ── */
    // Render a reasoning step as an inline, collapsible "Thinking" block in the
    // conversation timeline (Claude-style). Interleaves with answer text by
    // arrival order: any in-progress answer stream is finalized first so the
    // next answer chunk starts a fresh bubble AFTER this thinking block.
    function thinkingTextFromStep(step) {
      if (!step || typeof step !== 'object') { return String(step || ''); }
      if (typeof step.content === 'string' && step.content.trim()) { return step.content; }
      if (typeof step.text === 'string' && step.text.trim()) { return step.text; }
      if (step.step_type === 'redacted_thinking') { return '(redacted by provider)'; }
      try { return JSON.stringify(step, null, 2); } catch (_) { return String(step); }
    }

    function addReasoningStep(step) {
      try {
        if (!step) { return; }
        if (!state.showReasoning) { return; }
        // Close any active answer stream so the thinking block lands before the
        // next answer segment (preserves think → answer → think ordering).
        if (streamBubble) { endStream(); }
        const text = thinkingTextFromStep(step);
        const kind = String(step.step_type || 'analysis').replace(/_/g, ' ');
        const div = document.createElement('div');
        div.className = 'msg thinking-block';
        const details = document.createElement('details');
        details.className = 'thinking-details';
        details.innerHTML = '<summary class="thinking-summary">' +
          '<span class="thinking-icon">💭</span><span class="thinking-label">Thinking</span>' +
          '<span class="thinking-kind">' + esc(kind) + '</span></summary>' +
          '<div class="thinking-body">' + renderMarkdown(text) + '</div>';
        div.appendChild(details);
        if (traceFeed) {
          appendTraceEntry(div);
        } else if (thread) {
          thread.appendChild(div);
          smartScroll();
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addReasoningStep failed: ' + String(e) }); } catch (_) {}
      }
    }

    function normalizeDecisioningRiskLevel(event) {
      const explicit = String(event && event.risk_level ? event.risk_level : '').toLowerCase();
      if (explicit === 'low' || explicit === 'medium' || explicit === 'high') {
        return explicit;
      }
      const action = String(event && event.action ? event.action : '').toLowerCase();
      if (action === 'deny') { return 'high'; }
      if (action === 'review') { return 'medium'; }
      if (action === 'allow') { return 'low'; }
      const score = typeof event.risk_score === 'number' ? event.risk_score : undefined;
      if (typeof score === 'number') {
        if (score >= 0.7) { return 'high'; }
        if (score >= 0.35) { return 'medium'; }
        return 'low';
      }
      return 'unknown';
    }

    function sanitizeDecisioningClassToken(value) {
      return String(value || '')
        .toLowerCase()
        .replace(/[^a-z0-9_-]+/g, '-')
        .replace(/^-+|-+$/g, '') || 'unknown';
    }

    function sanitizeDecisioningClassList(className) {
      return String(className || '')
        .split(/\s+/)
        .map(sanitizeDecisioningClassToken)
        .filter(Boolean)
        .join(' ');
    }

    function renderDecisioningSummaryBadge(label, className) {
      const safeClass = sanitizeDecisioningClassList(className);
      return '<span class="decisioning-badge' + (safeClass ? ' ' + safeClass : '') + '">' + esc(label) + '</span>';
    }

    function renderDecisioningMetric(label, value, className) {
      const safeClass = sanitizeDecisioningClassList(className);
      return '<div class="decisioning-metric-card' + (safeClass ? ' ' + safeClass : '') + '">' +
        '<div class="decisioning-metric-label">' + esc(label) + '</div>' +
        '<div class="decisioning-metric-value">' + esc(value) + '</div>' +
      '</div>';
    }

    function renderDecisioningOverview(event, riskLevel) {
      const metrics = [];
      if (event.task_id) { metrics.push(renderDecisioningMetric('Task', String(event.task_id))); }
      if (typeof event.confidence === 'number') { metrics.push(renderDecisioningMetric('Confidence', Math.round(event.confidence * 100) + '%')); }
      if (typeof event.risk_score === 'number' || event.risk_level || event.action) {
        const riskText = typeof event.risk_score === 'number' ? Math.round(Math.max(0, Math.min(1, event.risk_score)) * 100) + '% · ' + riskLevel : riskLevel;
        metrics.push(renderDecisioningMetric('Risk', riskText, 'risk-' + riskLevel));
      }
      if (event.action) { metrics.push(renderDecisioningMetric('Action', String(event.action))); }
      if (typeof event.parallelizable === 'boolean') { metrics.push(renderDecisioningMetric('Parallelism', event.parallelizable ? 'Parallel' : 'Serial')); }
      if (Array.isArray(event.selected_tools)) { metrics.push(renderDecisioningMetric('Selected tools', String(event.selected_tools.length))); }
      if (Array.isArray(event.tool_scores)) { metrics.push(renderDecisioningMetric('Scored tools', String(event.tool_scores.length))); }
      if (!metrics.length) { return ''; }
      return '<section class="decisioning-section"><div class="decisioning-section-title">Workbench Overview</div><div class="decisioning-overview">' + metrics.join('') + '</div></section>';
    }

    function renderDecisioningEventMarkup(event) {
      const kind = esc(renderDecisioningKindLabel(event.kind));
      const title = esc(String(event.title || 'Decisioning'));
      const summary = String(event.summary || '');
      const riskLevel = normalizeDecisioningRiskLevel(event);
      const badges = [];
      if (event.task_id) { badges.push(renderDecisioningSummaryBadge('task ' + String(event.task_id))); }
      if (typeof event.confidence === 'number') { badges.push(renderDecisioningSummaryBadge('confidence ' + Math.round(event.confidence * 100) + '%')); }
      if (typeof event.risk_score === 'number') { badges.push(renderDecisioningSummaryBadge('risk ' + Math.round(event.risk_score * 100) + '%', 'risk-' + riskLevel)); }
      if (typeof event.parallelizable === 'boolean') { badges.push(renderDecisioningSummaryBadge(event.parallelizable ? 'parallel' : 'serial')); }
      if (event.action) { badges.push(renderDecisioningSummaryBadge(String(event.action), 'action-' + String(event.action).toLowerCase())); }
      if (Array.isArray(event.selected_tools) && event.selected_tools.length > 0) { badges.push(renderDecisioningSummaryBadge(event.selected_tools.length + ' tool(s)')); }
      if (typeof event.risk_score === 'number' || event.risk_level || event.action) {
        badges.push(renderDecisioningSummaryBadge(riskLevel + ' risk', 'risk-' + riskLevel));
      }
      const sections = [];
      const overviewHtml = renderDecisioningOverview(event, riskLevel);
      if (overviewHtml) { sections.push(overviewHtml); }
      const riskHtml = renderDecisioningRiskPanel(event, riskLevel);
      if (riskHtml) { sections.push(riskHtml); }
      const toolScoresHtml = renderDecisioningToolScores(event.tool_scores, event.selected_tools);
      if (toolScoresHtml) { sections.push(toolScoresHtml); }
      const planTreeHtml = renderDecisioningPlanTree(event.plan_tree, event.selected_tools);
      if (planTreeHtml) { sections.push(planTreeHtml); }
      const notesHtml = renderDecisioningNotes(event.details);
      if (notesHtml) { sections.push(notesHtml); }
      const body = summary ? '<div class="decisioning-summary">' + esc(summary) + '</div>' : '<div class="decisioning-summary">' + esc(JSON.stringify(event, null, 2)) + '</div>';
      return '<div class="msg-role">Decisioning · ' + kind + ' · ' + title + '</div><div class="msg-body"><div class="decisioning-card"><div class="decisioning-header">' + body + '<div class="decisioning-badges">' + badges.join('') + '</div></div>' + sections.join('') + '</div></div>';
    }

    function renderDecisioningKindLabel(kind) {
      const raw = String(kind || '').trim();
      const lookup = {
        tool_selection: 'Tool selection',
        task_decomposition: 'Task decomposition',
        parallelism_decision: 'Parallelism decision',
        safety_assessment: 'Safety assessment',
        plan_adjustment: 'Plan adjustment'
      };
      if (lookup[raw]) {
        return lookup[raw];
      }
      if (!raw) {
        return 'Decision event';
      }
      return raw
        .split('_')
        .map(function(part) {
          return part ? part.charAt(0).toUpperCase() + part.slice(1) : part;
        })
        .join(' ');
    }

    function renderDecisioningRiskPanel(event, riskLevel) {
      const riskScore = typeof event.risk_score === 'number' ? Math.max(0, Math.min(1, event.risk_score)) : undefined;
      const action = String(event && event.action ? event.action : '').toLowerCase();
      const hasAction = action === 'allow' || action === 'review' || action === 'deny';
      const hasRisk = typeof riskScore === 'number' || riskLevel !== 'unknown' || hasAction;
      const reasonList = Array.isArray(event && event.details) ? event.details.filter(Boolean).slice(0, 4) : [];
      if (!hasRisk && !reasonList.length) { return ''; }
      const fill = typeof riskScore === 'number'
        ? Math.round(riskScore * 100)
        : riskLevel === 'high'
          ? 88
          : riskLevel === 'medium'
            ? 55
            : riskLevel === 'low'
              ? 18
              : 0;
      const summaryBits = [];
      if (typeof riskScore === 'number') { summaryBits.push('score ' + riskScore.toFixed(2)); }
      if (hasAction) { summaryBits.push('action ' + action); }
      if (reasonList.length) { summaryBits.push(reasonList.length + ' reason(s)'); }
      const details = reasonList.length ? '<div class="decisioning-risk-details">' + reasonList.map(function(reason) {
        return '<div>• ' + esc(String(reason || '')) + '</div>';
      }).join('') + '</div>' : '';
      return '<section class="decisioning-section decisioning-risk-panel">' +
        '<div class="decisioning-section-title">Risk Grade</div>' +
        '<div class="decisioning-risk-summary">' +
          '<div class="decisioning-risk-label">' + esc(riskLevel.toUpperCase() + ' risk') + '</div>' +
          '<div class="decisioning-score-value">' + (typeof riskScore === 'number' ? esc(Math.round(riskScore * 100) + '%') : esc(riskLevel)) + '</div>' +
        '</div>' +
        '<div class="decisioning-risk-meter"><span style="width:' + fill + '%"></span></div>' +
        (summaryBits.length ? '<div class="decisioning-section-note">' + esc(summaryBits.join(' · ')) + '</div>' : '') +
        details +
      '</section>';
    }

    function renderDecisioningToolScores(toolScores, selectedTools) {
      const list = Array.isArray(toolScores) ? toolScores.filter(Boolean) : [];
      if (!list.length) { return ''; }
      const selectedSet = new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      const visibleScores = list.slice().sort(function(left, right) {
        const leftScore = typeof left.score === 'number' ? left.score : -Infinity;
        const rightScore = typeof right.score === 'number' ? right.score : -Infinity;
        if (rightScore !== leftScore) { return rightScore - leftScore; }
        return String(left.name || '').localeCompare(String(right.name || ''));
      }).slice(0, 6);
      const baseline = visibleScores[0] && typeof visibleScores[0].score === 'number' ? visibleScores[0].score : 0;
      const minScore = visibleScores.reduce(function(acc, item) {
        return Math.min(acc, typeof item.score === 'number' ? item.score : 0);
      }, baseline);
      const maxScore = visibleScores.reduce(function(acc, item) {
        return Math.max(acc, typeof item.score === 'number' ? item.score : 0);
      }, baseline);
      const leaderScore = typeof maxScore === 'number' ? maxScore : 0;
      const span = maxScore - minScore;
      const items = visibleScores.map(function(item) {
        const scoreValue = typeof item.score === 'number' ? item.score : 0;
        const selected = Boolean(item.selected) || selectedSet.has(String(item.name || ''));
        const fill = span === 0 ? 100 : Math.max(0, Math.min(100, Math.round(((scoreValue - minScore) / span) * 100)));
        const meta = [];
        meta.push('#' + (visibleScores.indexOf(item) + 1));
        if (scoreValue === leaderScore) { meta.push('leader'); }
        else if (typeof leaderScore === 'number') { meta.push((leaderScore - scoreValue).toFixed(2) + ' behind leader'); }
        if (typeof item.success_rate === 'number') { meta.push(Math.round(item.success_rate * 100) + '% success'); }
        if (typeof item.latency_ms === 'number') { meta.push(item.latency_ms + ' ms'); }
        if (typeof item.cost === 'number') { meta.push('$' + item.cost.toFixed(2)); }
        meta.push(item.parallelizable ? 'parallel' : 'serial');
        const capabilities = Array.isArray(item.capabilities) ? item.capabilities.slice(0, 3) : [];
        const capabilityCount = Array.isArray(item.capabilities) ? item.capabilities.length : 0;
        return '<div class="decisioning-score-item' + (selected ? ' selected' : '') + '">' +
          '<div class="decisioning-score-header">' +
            '<div>' +
              '<div class="decisioning-score-name">' +
                esc(String(item.name || 'tool')) +
                (selected ? '<span class="decisioning-chip selected-tag">selected</span>' : '') +
              '</div>' +
              '<div class="decisioning-score-meta">' + esc(meta.join(' · ')) + '</div>' +
            '</div>' +
            '<div class="decisioning-score-value">' + esc(scoreValue.toFixed(2)) + '</div>' +
          '</div>' +
          '<div class="decisioning-score-bar"><span style="width:' + fill + '%"></span></div>' +
          (capabilities.length ? '<div class="decisioning-chip-row">' +
            capabilities.map(function(capability) { return '<span class="decisioning-chip">' + esc(String(capability || '')) + '</span>'; }).join('') +
            (capabilityCount > capabilities.length ? '<span class="decisioning-chip">+' + (capabilityCount - capabilities.length) + '</span>' : '') +
          '</div>' : '') +
        '</div>';
      }).join('');
      const footer = list.length > visibleScores.length ? '<div class="decisioning-section-note">Showing top ' + visibleScores.length + ' of ' + list.length + ' scored tools.</div>' : '';
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Tool Scores</div>' +
        '<div class="decisioning-score-list">' + items + '</div>' +
        footer +
      '</section>';
    }

    function renderDecisioningPlanNode(node, selectedTools, depth) {
      if (!node) { return ''; }
      const selectedSet = selectedTools instanceof Set ? selectedTools : new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      const kind = String(node.kind || 'step').toLowerCase();
      const title = esc(String(node.title || 'Untitled plan node'));
      const id = String(node.id || '');
      const tools = Array.isArray(node.candidate_tools) ? node.candidate_tools : [];
      const notes = Array.isArray(node.notes) ? node.notes : [];
      const children = Array.isArray(node.children) ? node.children : [];
      const meta = [];
      if (typeof node.estimated_effort === 'number') { meta.push('effort ' + node.estimated_effort); }
      meta.push(node.parallelizable ? 'parallel' : 'serial');
      if (id) { meta.push(id); }
      const toolChips = tools.length ? '<div class="decisioning-chip-row">' + tools.map(function(toolName) {
        const label = String(toolName || '');
        return '<span class="decisioning-chip' + (selectedSet.has(label) ? ' selected' : '') + '">' + esc(label) + '</span>';
      }).join('') + '</div>' : '';
      const noteBlock = notes.length ? '<div class="decisioning-node-notes">' + notes.map(function(note) {
        return '<div>' + esc(String(note || '')) + '</div>';
      }).join('') + '</div>' : '';
      const visibleChildren = children.slice(0, 8);
      const hiddenChildren = children.length - visibleChildren.length;
      const childBlock = visibleChildren.length ? '<div class="decisioning-tree-children">' + visibleChildren.map(function(child) {
        return renderDecisioningPlanNode(child, selectedSet, depth + 1);
      }).join('') + (hiddenChildren > 0 ? '<div class="decisioning-node-overflow">+' + hiddenChildren + ' more nested step(s)</div>' : '') + '</div>' : '';
      const safeKindClass = sanitizeDecisioningClassToken(kind);
      const safeLevelClass = depth >= 4 ? 'level-deep' : 'level-' + depth;
      return '<div class="decisioning-tree-node kind-' + safeKindClass + ' ' + safeLevelClass + '">' +
        '<div class="decisioning-tree-head">' +
          '<div class="decisioning-tree-title">' +
            '<span class="decisioning-tree-kind">' + esc(kind) + '</span>' +
            '<span class="decisioning-tree-text">' + title + '</span>' +
          '</div>' +
          (meta.length ? '<div class="decisioning-tree-meta">' + esc(meta.join(' · ')) + '</div>' : '') +
        '</div>' +
        toolChips +
        noteBlock +
        childBlock +
      '</div>';
    }

    function renderDecisioningPlanTree(node, selectedTools) {
      if (!node) { return ''; }
      const selectedSet = new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Plan Tree</div>' +
        '<div class="decisioning-tree">' + renderDecisioningPlanNode(node, selectedSet, 0) + '</div>' +
      '</section>';
    }

    function renderDecisioningDetailItem(item, index) {
      if (item && typeof item === 'object' && !Array.isArray(item)) {
        const label = item.label || item.title || item.kind || item.name || ('detail ' + (index + 1));
        const text = item.text || item.summary || item.reason || item.value;
        const json = text === undefined ? JSON.stringify(item, null, 2) : String(text);
        return '<div class="decisioning-detail-item"><div class="decisioning-detail-label">' + esc(String(label)) + '</div><div class="decisioning-detail-json">' + esc(json) + '</div></div>';
      }
      return '<div class="decisioning-detail-item">' + esc(String(item || '')) + '</div>';
    }

    function renderDecisioningNotes(details) {
      const list = Array.isArray(details) ? details.filter(Boolean) : [];
      if (!list.length) { return ''; }
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Details</div>' +
        '<div class="decisioning-detail-list">' + list.slice(0, 8).map(renderDecisioningDetailItem).join('') + '</div>' +
        (list.length > 8 ? '<div class="decisioning-section-note">Showing first 8 of ' + list.length + ' detail item(s).</div>' : '') +
      '</section>';
    }

    function recoveryEventSummary(value) {
      if (typeof value === 'string') {
        if (value === 'recovery_succeeded') { return 'Recovery succeeded'; }
        if (value === 'recovery_failed') { return 'Recovery failed'; }
        if (value === 'escalated') { return 'Recovery escalated'; }
        return value.replace(/_/g, ' ');
      }
      if (!value || typeof value !== 'object') { return 'Recovery event'; }
      if (value.recovery_attempted && typeof value.recovery_attempted === 'object') {
        const attempted = value.recovery_attempted;
        const result = attempted.result && typeof attempted.result === 'object' ? attempted.result : {};
        let outcome = 'attempted';
        if (result.recovered) { outcome = 'recovered'; }
        else if (result.partial_recovery) { outcome = 'partial recovery'; }
        else if (result.escalation_required) { outcome = 'escalation required'; }
        return ['Recovery', attempted.scenario, outcome].filter(Boolean).join(' · ');
      }
      if (Object.prototype.hasOwnProperty.call(value, 'recovery_succeeded')) { return 'Recovery succeeded'; }
      if (Object.prototype.hasOwnProperty.call(value, 'recovery_failed')) { return 'Recovery failed'; }
      if (Object.prototype.hasOwnProperty.call(value, 'escalated')) { return 'Recovery escalated'; }
      return 'Recovery event';
    }

    function runtimeEventSummary(kind, event) {
      const value = event && typeof event === 'object' ? event : {};
      if (kind === 'plan_execution_event') {
        const label = String(value.kind || '').replace(/_/g, ' ');
        const attempt = value.attempt ? 'attempt ' + value.attempt : undefined;
        const blocked = value.blocking_reason ? 'blocked: ' + value.blocking_reason : undefined;
        const gate = value.verification_gate ? 'gate: ' + value.verification_gate : undefined;
        return ['Plan', label, value.node_id, value.status, attempt, blocked, gate].filter(Boolean).join(' · ') || 'Plan updated';
      }
      if (kind === 'task_ledger_event') {
        const label = String(value.event || '').replace(/_/g, ' ');
        return ['Task', value.task_id, label, value.status].filter(Boolean).join(' · ') || 'Task ledger updated';
      }
      if (kind === 'model_route_event') {
        const confidence = typeof value.confidence === 'number' ? 'confidence ' + Math.round(value.confidence * 100) + '%' : undefined;
        const fallback = value.fallback_model ? 'fallback ' + value.fallback_model : undefined;
        return ['Route', value.phase, value.model, confidence, fallback, value.reason].filter(Boolean).join(' · ') || 'Model route selected';
      }
      if (kind === 'team_execution_event') {
        const label = String(value.kind || '').replace(/_/g, ' ');
        return ['Team', value.role, label, value.message].filter(Boolean).join(' · ') || 'Team event';
      }
      if (kind === 'recovery_event') {
        return recoveryEventSummary(value);
      }
      if (kind === 'recovery_action_event' || kind === 'task_recovery') {
        const execution = value.execution && typeof value.execution === 'object' ? value.execution : value;
        const results = Array.isArray(execution.results) ? execution.results.length + ' action(s)' : undefined;
        const label = kind === 'recovery_action_event' ? 'Recovery action' : 'Task recovery';
        return [label, execution.task_id, results].filter(Boolean).join(' · ') || label + ' updated';
      }
      if (kind === 'task_execution_event' || kind === 'task_execution') {
        const outcome = value.outcome && typeof value.outcome === 'object' ? value.outcome : value;
        const stepCount = Array.isArray(outcome.steps) ? outcome.steps.length + ' step(s)' : undefined;
        const status = outcome.completed ? 'completed' : (outcome.blocked ? 'blocked' : undefined);
        const label = kind === 'task_execution_event' ? 'Task execution event' : 'Task execution';
        return [label, outcome.task_id, status, stepCount, outcome.message].filter(Boolean).join(' · ') || label + ' updated';
      }
      if (kind === 'task_verification') {
        const result = value.result && typeof value.result === 'object' ? value.result : value;
        const status = result.passed === true ? 'passed' : (result.passed === false ? 'failed' : undefined);
        return ['Task verification', result.task_id, status, result.summary].filter(Boolean).join(' · ') || 'Task verification updated';
      }
      if (kind === 'task_list') {
        const count = Array.isArray(value.tasks) ? value.tasks.length + ' task(s)' : undefined;
        return ['Task list', count].filter(Boolean).join(' · ') || 'Task list updated';
      }
      if (kind === 'task_show') {
        const task = value.task && typeof value.task === 'object' ? value.task : {};
        return ['Task detail', task.task_id, task.status].filter(Boolean).join(' · ') || 'Task detail updated';
      }
      if (kind === 'task_node_retry') {
        return ['Task node retry', value.node_id].filter(Boolean).join(' · ') || 'Task node retry scheduled';
      }
      if (kind === 'task_node_verification') {
        return ['Task node verification', value.node_id, value.command].filter(Boolean).join(' · ') || 'Task node verification updated';
      }
      if (kind === 'task_compacted') {
        return ['Task compacted', value.keep_last ? 'keep ' + value.keep_last : undefined].filter(Boolean).join(' · ') || 'Task compacted';
      }
      if (kind === 'task_cancelled') {
        const task = value.task && typeof value.task === 'object' ? value.task : {};
        return ['Task cancelled', task.task_id].filter(Boolean).join(' · ') || 'Task cancelled';
      }
      if (kind === 'worker_supervisor_tick') {
        const tick = value.tick && typeof value.tick === 'object' ? value.tick : value;
        const capacity = tick.capacity && typeof tick.capacity === 'object' ? tick.capacity : {};
        const trustQueue = Array.isArray(tick.trust_queue) ? tick.trust_queue : [];
        return ['Workers', tick.status, tick.active_workers !== undefined ? tick.active_workers + ' active' : undefined, tick.restarted_workers ? tick.restarted_workers + ' restarted' : undefined, capacity.available_slots !== undefined ? capacity.available_slots + ' slots free' : undefined, trustQueue.length ? trustQueue.length + ' trust blocked' : undefined].filter(Boolean).join(' · ') || 'Worker supervisor updated';
      }
      return event && typeof event === 'object' ? JSON.stringify(event) : String(event || '');
    }
    // RUNTIME_EVENT_CARD_HELPERS
    // Map a status/kind string to a visual tone: ok | warn | err | run | idle.
    function toneFromStatus(s) {
      const v = String(s || '').toLowerCase();
      if (/(fail|error|blocked|escalat|reject|denied|cancel)/.test(v)) { return 'err'; }
      if (/(complete|success|passed|recovered|done|resolved|assigned)/.test(v)) { return 'ok'; }
      if (/(running|in_progress|started|retry|pending|resume|scheduled)/.test(v)) { return 'run'; }
      if (/(warn|partial|skip|degraded)/.test(v)) { return 'warn'; }
      return 'idle';
    }

    function runtimeEventTone(kind, event) {
      const v = event && typeof event === 'object' ? event : {};
      if (kind === 'plan_execution_event') { return toneFromStatus(v.status || v.kind); }
      if (kind === 'task_ledger_event') { return toneFromStatus(v.status || v.event); }
      if (kind === 'model_route_event') { return 'run'; }
      if (kind === 'team_execution_event') { return toneFromStatus(v.kind); }
      if (kind === 'recovery_event') {
        const a = v.recovery_attempted;
        if (a && a.result) {
          if (a.result.recovered) { return 'ok'; }
          if (a.result.escalation_required) { return 'err'; }
          if (a.result.partial_recovery) { return 'warn'; }
        }
        if (Object.prototype.hasOwnProperty.call(v, 'recovery_failed') || Object.prototype.hasOwnProperty.call(v, 'escalated')) { return 'err'; }
        if (Object.prototype.hasOwnProperty.call(v, 'recovery_succeeded')) { return 'ok'; }
        return 'run';
      }
      if (kind === 'recovery_action_event' || kind === 'task_recovery') {
        const ex = v.execution && typeof v.execution === 'object' ? v.execution : v;
        const results = Array.isArray(ex.results) ? ex.results : [];
        if (results.some(function(r) { return r && r.blocked; })) { return 'err'; }
        if (results.length && results.every(function(r) { return r && r.executed; })) { return 'ok'; }
        return 'run';
      }
      if (kind === 'task_execution_event' || kind === 'task_execution') {
        const o = v.outcome && typeof v.outcome === 'object' ? v.outcome : v;
        if (o.blocked) { return 'err'; }
        if (o.completed) { return 'ok'; }
        return 'run';
      }
      if (kind === 'task_verification') {
        const r = v.result && typeof v.result === 'object' ? v.result : v;
        return r.passed === true ? 'ok' : (r.passed === false ? 'err' : 'idle');
      }
      return 'idle';
    }

    // Small helpers shared by the detail renderers.
    function rtRow(k, v) {
      if (v == null || v === '') { return ''; }
      return '<div class="rt-row"><span class="rt-k">' + esc(k) + '</span><span class="rt-v">' + esc(String(v)) + '</span></div>';
    }
    function rtPill(text, tone) {
      return '<span class="rt-pill rt-pill-' + (tone || 'idle') + '">' + esc(String(text)) + '</span>';
    }
    function rtConfidenceBar(conf) {
      const pct = Math.max(0, Math.min(100, Math.round(Number(conf) * 100)));
      return '<div class="rt-conf"><div class="rt-conf-track"><div class="rt-conf-fill" style="width:' + pct + '%"></div></div>' +
        '<span class="rt-conf-label">' + pct + '%</span></div>';
    }

    // RUNTIME_EVENT_CARD_DETAIL
    function renderRuntimeEventDetail(kind, event) {
      try {
        const v = event && typeof event === 'object' ? event : {};
        if (kind === 'plan_execution_event') {
          const rows = rtRow('node', v.node_id) + rtRow('task', v.task_id) +
            (v.attempt ? rtRow('attempt', v.attempt) : '') +
            (v.message ? rtRow('message', v.message) : '') +
            (v.blocking_reason ? rtRow('blocked', v.blocking_reason) : '') +
            (v.verification_gate ? rtRow('gate', v.verification_gate) : '');
          const head = (v.kind ? rtPill(String(v.kind).replace(/_/g, ' '), runtimeEventTone(kind, event)) : '') +
            (v.status ? rtPill(v.status, toneFromStatus(v.status)) : '');
          return (head ? '<div class="rt-pills">' + head + '</div>' : '') + rows;
        }
        if (kind === 'task_ledger_event') {
          const head = (v.event ? rtPill(String(v.event).replace(/_/g, ' '), runtimeEventTone(kind, event)) : '') +
            (v.status ? rtPill(v.status, toneFromStatus(v.status)) : '');
          return (head ? '<div class="rt-pills">' + head + '</div>' : '') +
            rtRow('task', v.task_id) + (v.detail ? rtRow('detail', v.detail) : '');
        }
        if (kind === 'task_execution_event' || kind === 'task_execution') {
          const o = v.outcome && typeof v.outcome === 'object' ? v.outcome : v;
          const steps = Array.isArray(o.steps) ? o.steps : [];
          const status = o.completed ? 'completed' : (o.blocked ? 'blocked' : 'running');
          let out = '<div class="rt-pills">' + rtPill(status, toneFromStatus(status)) +
            (steps.length ? rtPill(steps.length + ' step' + (steps.length === 1 ? '' : 's'), 'idle') : '') + '</div>' +
            rtRow('task', o.task_id) + (o.message ? rtRow('message', o.message) : '');
          if (steps.length) {
            out += '<ol class="rt-steps">' + steps.slice(0, 6).map(function(s) {
              const label = String((s && (s.kind || s.message)) || 'step').replace(/_/g, ' ');
              return '<li>' + esc(label) + (s && s.node_id ? ' <span class="rt-muted">' + esc(s.node_id) + '</span>' : '') + '</li>';
            }).join('') + '</ol>';
            if (steps.length > 6) { out += '<div class="rt-muted">+' + (steps.length - 6) + ' more</div>'; }
          }
          return out;
        }
        // RUNTIME_EVENT_CARD_DETAIL_2
        if (kind === 'model_route_event') {
          let out = '<div class="rt-route">' +
            (v.phase ? '<span class="rt-phase">' + esc(String(v.phase)) + '</span>' : '') +
            '<span class="rt-arrow">→</span>' +
            '<span class="rt-model">' + esc(String(v.model || 'model')) + '</span>' +
            (v.provider ? '<span class="rt-muted">(' + esc(String(v.provider)) + ')</span>' : '') + '</div>';
          if (typeof v.confidence === 'number') { out += rtConfidenceBar(v.confidence); }
          if (v.fallback_model) { out += rtRow('fallback', v.fallback_model); }
          if (v.reason) { out += rtRow('reason', v.reason); }
          return out;
        }
        if (kind === 'recovery_event') {
          const a = v.recovery_attempted && typeof v.recovery_attempted === 'object' ? v.recovery_attempted : null;
          if (a) {
            const result = a.result && typeof a.result === 'object' ? a.result : {};
            let outcome = 'attempted', tone = 'run';
            if (result.recovered) { outcome = 'recovered'; tone = 'ok'; }
            else if (result.partial_recovery) { outcome = 'partial recovery'; tone = 'warn'; }
            else if (result.escalation_required) { outcome = 'escalation required'; tone = 'err'; }
            const steps = result.recovered && typeof result.recovered === 'object' ? result.recovered.steps_taken : undefined;
            return '<div class="rt-pills">' + rtPill(outcome, tone) + '</div>' +
              rtRow('scenario', a.scenario) + (steps != null ? rtRow('steps taken', steps) : '');
          }
          return '';
        }
        if (kind === 'recovery_action_event' || kind === 'task_recovery') {
          const ex = v.execution && typeof v.execution === 'object' ? v.execution : v;
          const results = Array.isArray(ex.results) ? ex.results : [];
          let out = rtRow('task', ex.task_id);
          if (results.length) {
            out += '<ul class="rt-timeline">' + results.slice(0, 6).map(function(r) {
              const ok = r && r.executed && !r.blocked;
              const cls = r && r.blocked ? 'err' : (ok ? 'ok' : 'run');
              const label = String((r && (r.action || r.reason)) || (ok ? 'executed' : 'pending'));
              return '<li class="rt-tl-' + cls + '">' + esc(label) + '</li>';
            }).join('') + '</ul>';
            if (results.length > 6) { out += '<div class="rt-muted">+' + (results.length - 6) + ' more action(s)</div>'; }
          }
          return out;
        }
        if (kind === 'team_execution_event') {
          const head = (v.kind ? rtPill(String(v.kind).replace(/_/g, ' '), toneFromStatus(v.kind)) : '') +
            (v.role ? rtPill(v.role, 'idle') : '');
          return (head ? '<div class="rt-pills">' + head + '</div>' : '') +
            rtRow('team', v.team_id) + rtRow('task', v.task_id) + (v.message ? rtRow('message', v.message) : '');
        }
        if (kind === 'task_verification') {
          const r = v.result && typeof v.result === 'object' ? v.result : v;
          const status = r.passed === true ? 'passed' : (r.passed === false ? 'failed' : 'unknown');
          return '<div class="rt-pills">' + rtPill(status, toneFromStatus(status)) + '</div>' +
            rtRow('task', r.task_id) + (r.summary ? rtRow('summary', r.summary) : '');
        }
        if (kind === 'worker_supervisor_tick') {
          const tick = v.tick && typeof v.tick === 'object' ? v.tick : v;
          const cap = tick.capacity && typeof tick.capacity === 'object' ? tick.capacity : {};
          return '<div class="rt-pills">' +
            (tick.status ? rtPill(tick.status, toneFromStatus(tick.status)) : '') +
            (tick.active_workers !== undefined ? rtPill(tick.active_workers + ' active', 'run') : '') +
            (cap.available_slots !== undefined ? rtPill(cap.available_slots + ' free', 'idle') : '') + '</div>';
        }
        return '';
      } catch (e) {
        return '';
      }
    }

    function taskBoardInitialState() {
      return {
        collapsed: true,
        tasks: {},
        taskOrder: [],
        currentNode: null,
        planNodes: {},
        planNodeOrder: [],
        recoveryEvents: [],
        workers: {},
        workerOrder: [],
        selectedWorkerId: null,
        workerSupervisor: null,
        daemon: null,
        routeSummary: null,
        benchmark: null
      };
    }

    function taskBoardTaskId(task) {
      if (!task || typeof task !== 'object') { return ''; }
      return String(task.task_id || task.id || '').trim();
    }

    function rememberTaskBoardTask(task) {
      const taskId = taskBoardTaskId(task);
      if (!taskId) { return; }
      const previous = state.taskBoard.tasks[taskId] || {};
      const patch = {};
      Object.keys(task).forEach(function(key) {
        if (task[key] !== undefined && task[key] !== null && task[key] !== '') {
          patch[key] = task[key];
        }
      });
      state.taskBoard.tasks[taskId] = Object.assign({}, previous, patch, { task_id: taskId });
      if (state.taskBoard.taskOrder.indexOf(taskId) < 0) {
        state.taskBoard.taskOrder.unshift(taskId);
      }
    }

    // Accumulate plan node states (by node_id) so the board can show DAG progress,
    // not just the single most-recent node.
    function rememberPlanNode(nodeId, status, kind) {
      const id = String(nodeId || '').trim();
      if (!id) { return; }
      const prev = state.taskBoard.planNodes[id] || {};
      state.taskBoard.planNodes[id] = {
        nodeId: id,
        status: status || prev.status || 'pending',
        kind: kind || prev.kind
      };
      if (state.taskBoard.planNodeOrder.indexOf(id) < 0) {
        state.taskBoard.planNodeOrder.push(id);
      }
    }

    // Shared status→tone mapping (mirrors the runtime-event card tones).
    function taskBoardTone(value) {
      const v = String(value || '').toLowerCase();
      if (/(fail|error|blocked|escalat|reject|denied|cancel)/.test(v)) { return 'err'; }
      if (/(complete|success|passed|recovered|done|resolved|finished)/.test(v)) { return 'ok'; }
      if (/(running|in_progress|started|retry|pending|resume|verifying|scheduled|queued)/.test(v)) { return 'run'; }
      if (/(warn|partial|skip|degraded)/.test(v)) { return 'warn'; }
      return 'idle';
    }

    function rememberTaskBoardWorker(worker) {
      if (!worker || typeof worker !== 'object') { return; }
      const workerId = String(worker.worker_id || worker.id || '').trim();
      if (!workerId) { return; }
      const previous = state.taskBoard.workers[workerId] || {};
      const patch = {};
      Object.keys(worker).forEach(function(key) {
        if (worker[key] !== undefined && worker[key] !== null && worker[key] !== '') {
          patch[key] = worker[key];
        }
      });
      state.taskBoard.workers[workerId] = Object.assign({}, previous, patch, { worker_id: workerId });
      if (state.taskBoard.workerOrder.indexOf(workerId) < 0) {
        state.taskBoard.workerOrder.unshift(workerId);
      }
      if (!state.taskBoard.selectedWorkerId) {
        state.taskBoard.selectedWorkerId = workerId;
      }
    }

    function rememberTaskBoardWorkerSupervisor(tick) {
      if (!tick || typeof tick !== 'object') { return; }
      state.taskBoard.workerSupervisor = tick;
      if (Array.isArray(tick.workers)) {
        tick.workers.forEach(function(worker) { rememberTaskBoardWorker(worker); });
      }
    }

    function rememberTaskBoardDaemon(kind, event) {
      if (!event || typeof event !== 'object') { return; }
      const daemon = Object.assign({}, state.taskBoard.daemon || {}, { kind: kind });
      if (Array.isArray(event.runs)) { daemon.runs = event.runs; }
      if ('state' in event) { daemon.state = event.state; }
      if (event.state_path) { daemon.state_path = event.state_path; }
      if (event.events_path) { daemon.events_path = event.events_path; }
      if (Array.isArray(event.events)) { daemon.events = event.events; }
      state.taskBoard.daemon = daemon;
      const latestRun = Array.isArray(event.runs) && event.runs.length ? event.runs[event.runs.length - 1] : null;
      const tick = latestRun && latestRun.tick && typeof latestRun.tick === 'object' ? latestRun.tick : null;
      if (tick && tick.task && typeof tick.task === 'object') { rememberTaskBoardTask(tick.task); }
    }

    function rememberTaskBoardRouteSummary(event) {
      if (!event || typeof event !== 'object') { return; }
      state.taskBoard.routeSummary = event;
    }

    function rememberTaskBoardBenchmark(kind, event) {
      if (!event || typeof event !== 'object') { return; }
      const benchmark = Object.assign({}, state.taskBoard.benchmark || {});
      if (kind === 'benchmark_suite') { benchmark.suite = event; }
      if (kind === 'benchmark_task') { benchmark.task = event.task || event; }
      if (kind === 'benchmark_run') { benchmark.run = event.run || event; }
      state.taskBoard.benchmark = benchmark;
    }

    function rememberTaskBoardRecovery(kind, event) {
      const summary = kind === 'recoverySuggestion'
        ? ['Suggestion', event.tool, event.action, event.reason].filter(Boolean).join(' · ')
        : runtimeEventSummary(kind, event);
      state.taskBoard.recoveryEvents.unshift({
        kind: kind,
        summary: summary || 'Recovery updated',
        event: event,
        createdAt: Date.now()
      });
      state.taskBoard.recoveryEvents = state.taskBoard.recoveryEvents.slice(0, 6);
    }

    function updateTaskBoardFromRuntimeEvent(kind, event) {
      try {
        const value = event && typeof event === 'object' ? event : {};
        if (kind === 'task_list' && Array.isArray(value.tasks)) {
          value.tasks.forEach(function(task) { rememberTaskBoardTask(task); });
        } else if (kind === 'task_show' || kind === 'task_packet_create' || kind === 'task_packet_run' || kind === 'task_packet_status') {
          if (value.task && typeof value.task === 'object') { rememberTaskBoardTask(value.task); }
        } else if (kind === 'task_scheduler_queue' && Array.isArray(value.queue)) {
          value.queue.forEach(function(item) {
            if (item && item.task_id) {
              rememberTaskBoardTask({
                task_id: item.task_id,
                status: item.task_status || item.status,
                last_event: 'scheduler ' + String(item.status || 'queued')
              });
            }
          });
        } else if (kind === 'task_scheduler_tick') {
          const tick = value.tick && typeof value.tick === 'object' ? value.tick : value;
          if (tick.task && typeof tick.task === 'object') { rememberTaskBoardTask(tick.task); }
          if (Array.isArray(tick.queue)) {
            tick.queue.forEach(function(item) {
              if (item && item.task_id) {
                rememberTaskBoardTask({
                  task_id: item.task_id,
                  status: item.task_status || item.status,
                  last_event: 'scheduler ' + String(item.status || 'tick')
                });
              }
            });
          }
        } else if (kind === 'task_ledger_event') {
          if (value.task_id) {
            rememberTaskBoardTask({
              task_id: value.task_id,
              status: value.status,
              last_event: value.event,
              updated_at: value.timestamp
            });
          }
        } else if (kind === 'plan_execution_event') {
          state.taskBoard.currentNode = {
            taskId: value.task_id,
            nodeId: value.node_id,
            status: value.status,
            kind: value.kind,
            attempt: value.attempt,
            blockingReason: value.blocking_reason,
            verificationGate: value.verification_gate
          };
          rememberPlanNode(value.node_id, value.status, value.kind);
          if (value.task_id) { rememberTaskBoardTask({ task_id: value.task_id, last_event: value.kind || 'plan_execution_event' }); }
        } else if (kind === 'task_node_retry' || kind === 'task_node_verification') {
          state.taskBoard.currentNode = {
            taskId: value.task_id,
            nodeId: value.node_id,
            status: kind === 'task_node_retry' ? 'retrying' : 'verifying',
            kind: kind,
            attempt: value.attempt,
            blockingReason: value.command
          };
          rememberPlanNode(value.node_id, kind === 'task_node_retry' ? 'retrying' : 'verifying', kind);
        } else if (kind === 'task_execution' || kind === 'task_execution_event') {
          const outcome = value.outcome && typeof value.outcome === 'object' ? value.outcome : value;
          if (outcome.task_id) {
            const status = outcome.completed ? 'completed' : (outcome.blocked ? 'blocked' : outcome.status);
            rememberTaskBoardTask({ task_id: outcome.task_id, status: status, last_event: outcome.message });
          }
        } else if (kind === 'task_verification') {
          const result = value.result && typeof value.result === 'object' ? value.result : value;
          if (result.task_id) {
            rememberTaskBoardTask({
              task_id: result.task_id,
              verification: result.passed === true ? 'passed' : (result.passed === false ? 'failed' : 'unknown')
            });
          }
        } else if (kind === 'task_cancelled') {
          const task = value.task && typeof value.task === 'object' ? value.task : value;
          if (task.task_id) { rememberTaskBoardTask(Object.assign({}, task, { status: 'cancelled' })); }
        } else if (kind === 'worker_list' && Array.isArray(value.workers)) {
          value.workers.forEach(function(worker) { rememberTaskBoardWorker(worker); });
        } else if (kind === 'worker_create' || kind === 'worker_spawn' || kind === 'worker_probe' || kind === 'worker_observe' || kind === 'worker_resolve_trust' || kind === 'worker_prompt' || kind === 'worker_complete' || kind === 'worker_restart' || kind === 'worker_terminate') {
          if (value.worker && typeof value.worker === 'object') { rememberTaskBoardWorker(value.worker); }
        } else if (kind === 'worker_ready') {
          if (value.ready && typeof value.ready === 'object') { rememberTaskBoardWorker(value.ready); }
        } else if (kind === 'worker_supervisor_tick') {
          const tick = value.tick && typeof value.tick === 'object' ? value.tick : value;
          rememberTaskBoardWorkerSupervisor(tick);
        } else if (kind === 'task_scheduler_daemon_run' || kind === 'task_scheduler_daemon_status' || kind === 'task_scheduler_daemon_logs') {
          rememberTaskBoardDaemon(kind, value);
        } else if (kind === 'route_feedback_summary') {
          rememberTaskBoardRouteSummary(value);
        } else if (kind === 'benchmark_suite' || kind === 'benchmark_task' || kind === 'benchmark_run') {
          rememberTaskBoardBenchmark(kind, value);
        }
        if (kind === 'recovery_event' || kind === 'recovery_action_event' || kind === 'task_recovery') {
          rememberTaskBoardRecovery(kind, value);
        }
        updateTaskBoardSurface();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateTaskBoardFromRuntimeEvent failed: ' + String(e) }); } catch (_) {}
      }
    }

    // Aggregate task statuses into a progress bar + status-distribution chips.
    function renderTaskBoardProgress(tasks) {
      if (!tasks || !tasks.length) { return ''; }
      const counts = { ok: 0, err: 0, run: 0, warn: 0, idle: 0 };
      tasks.forEach(function(t) { counts[taskBoardTone(t.status)] += 1; });
      const total = tasks.length;
      const done = counts.ok;
      const pct = Math.round((done / total) * 100);
      const seg = function(tone) {
        const n = counts[tone];
        if (!n) { return ''; }
        return '<div class="tb-bar-seg tb-seg-' + tone + '" style="width:' + Math.round((n / total) * 100) + '%"></div>';
      };
      const chips = ['ok', 'run', 'warn', 'err', 'idle'].map(function(tone) {
        if (!counts[tone]) { return ''; }
        const label = { ok: 'done', run: 'active', warn: 'partial', err: 'blocked', idle: 'pending' }[tone];
        return '<span class="tb-chip tb-chip-' + tone + '">' + counts[tone] + ' ' + label + '</span>';
      }).join('');
      return '<div class="tb-progress">' +
        '<div class="tb-progress-head"><span class="tb-progress-label">' + done + '/' + total + ' done</span>' +
        '<span class="tb-progress-pct">' + pct + '%</span></div>' +
        '<div class="tb-bar">' + seg('ok') + seg('run') + seg('warn') + seg('err') + seg('idle') + '</div>' +
        '<div class="tb-chips">' + chips + '</div>' +
      '</div>';
    }

    // Render accumulated plan nodes as a compact status dot grid + completion count.
    function renderPlanNodeProgress() {
      const order = state.taskBoard.planNodeOrder || [];
      if (!order.length) { return ''; }
      const nodes = order.map(function(id) { return state.taskBoard.planNodes[id]; }).filter(Boolean);
      const done = nodes.filter(function(n) { return taskBoardTone(n.status) === 'ok'; }).length;
      const dots = nodes.slice(-40).map(function(n) {
        const tone = taskBoardTone(n.status);
        return '<span class="tb-node-dot tb-seg-' + tone + '" title="' + esc(String(n.nodeId) + ' · ' + String(n.status || '')) + '"></span>';
      }).join('');
      return '<div class="tb-nodes"><div class="tb-nodes-head"><span class="task-board-panel-title">Plan nodes</span>' +
        '<span class="tb-nodes-count">' + done + '/' + nodes.length + '</span></div>' +
        '<div class="tb-node-grid">' + dots + '</div></div>';
    }

    function renderTaskBoardTask(task) {
      const taskId = String(task.task_id || 'task');
      const title = String(task.prompt || task.description || taskId);
      const status = String(task.status || 'unknown');
      const meta = [taskId, task.last_event, task.verification ? 'verification ' + task.verification : undefined].filter(Boolean).join(' · ');
      const safeStatus = sanitizeDecisioningClassToken(status);
      return '<div class="task-board-task" data-task-id="' + esc(taskId) + '">' +
        '<div class="task-board-task-head"><div class="task-board-task-title" title="' + esc(title) + '">' + esc(title) + '</div>' +
        '<span class="task-board-status status-' + esc(safeStatus) + '">' + esc(status) + '</span></div>' +
        '<div class="task-board-meta">' + esc(meta || taskId) + '</div>' +
      '</div>';
    }

    function renderTaskBoardNode(node) {
      if (!node) {
        return '<div class="task-board-empty">No active node yet.</div>';
      }
      const status = String(node.status || 'unknown');
      const meta = [node.taskId, node.nodeId, node.kind, node.attempt ? 'attempt ' + node.attempt : undefined].filter(Boolean).join(' · ');
      const details = [node.blockingReason ? 'Blocked: ' + node.blockingReason : undefined, node.verificationGate ? 'Gate: ' + node.verificationGate : undefined].filter(Boolean).join(' · ');
      return '<div class="task-board-node">' +
        '<div class="task-board-task-head"><div class="task-board-task-title">Current node</div>' +
        '<span class="task-board-status status-' + esc(sanitizeDecisioningClassToken(status)) + '">' + esc(status) + '</span></div>' +
        '<div class="task-board-meta">' + esc(meta || 'Plan node updated') + '</div>' +
        (details ? '<div class="task-board-meta">' + esc(details) + '</div>' : '') +
      '</div>';
    }

    function taskBoardDetailCell(label, value) {
      if (value === undefined || value === null || value === '') { return ''; }
      return '<div class="task-board-worker-detail"><div class="task-board-detail-label">' + esc(label) + '</div><div class="task-board-detail-value">' + esc(value) + '</div></div>';
    }

    function renderTaskBoardWorker(worker) {
      const workerId = String(worker.worker_id || 'worker');
      const status = String(worker.status || (worker.ready ? 'ready_for_prompt' : 'unknown'));
      const lease = worker.lease_expires_at ? 'lease ' + worker.lease_expires_at : undefined;
      const restarts = worker.restart_count !== undefined ? 'restarts ' + worker.restart_count + '/' + (worker.max_restarts !== undefined ? worker.max_restarts : '?') : undefined;
      const replay = worker.replay_prompt || worker.replay_prompt_ready ? 'replay armed' : undefined;
      const process = worker.process && typeof worker.process === 'object' ? worker.process : null;
      const processMeta = process && process.pid ? 'pid ' + process.pid : undefined;
      const meta = [workerId, lease, restarts, replay, processMeta].filter(Boolean).join(' · ');
      const selected = state.taskBoard.selectedWorkerId === workerId ? ' selected' : '';
      return '<div class="task-board-worker' + selected + '" data-worker-id="' + esc(workerId) + '">' +
        '<div class="task-board-task-head"><div class="task-board-task-title" title="' + esc(workerId) + '">' + esc(workerId) + '</div>' +
        '<span class="task-board-status status-' + esc(sanitizeDecisioningClassToken(status)) + '">' + esc(status) + '</span></div>' +
        '<div class="task-board-meta">' + esc(meta || workerId) + '</div>' +
      '</div>';
    }

    function renderTaskBoardWorkerDetail(worker) {
      if (!worker) {
        return '<div class="task-board-empty">Select a worker to inspect process and isolation details.</div>';
      }
      const workerId = String(worker.worker_id || 'worker');
      const status = String(worker.status || (worker.ready ? 'ready_for_prompt' : 'unknown'));
      const process = worker.process && typeof worker.process === 'object' ? worker.process : {};
      const isolation = worker.isolation && typeof worker.isolation === 'object' ? worker.isolation : {};
      const lastError = worker.last_error && typeof worker.last_error === 'object' ? worker.last_error : {};
      const command = Array.isArray(process.command) ? process.command.join(' ') : undefined;
      const recentEvents = Array.isArray(worker.events) ? worker.events.slice(-3).map(function(event) {
        return [event.kind, event.status, event.detail].filter(Boolean).join(' · ');
      }).join('\\n') : undefined;
      const cells = [
        taskBoardDetailCell('Worker', workerId),
        taskBoardDetailCell('Status', status),
        taskBoardDetailCell('PID', process.pid),
        taskBoardDetailCell('Command', command),
        taskBoardDetailCell('Cwd', worker.cwd),
        taskBoardDetailCell('Isolation', isolation.kind),
        taskBoardDetailCell('Worktree', isolation.worktree_path),
        taskBoardDetailCell('Source cwd', isolation.source_cwd),
        taskBoardDetailCell('Started', process.started_at),
        taskBoardDetailCell('Exited', process.exited_at),
        taskBoardDetailCell('Exit status', process.exit_status),
        taskBoardDetailCell('Lease expires', worker.lease_expires_at),
        taskBoardDetailCell('Restarts', worker.restart_count !== undefined ? worker.restart_count + '/' + (worker.max_restarts !== undefined ? worker.max_restarts : '?') : undefined),
        taskBoardDetailCell('Replay prompt', worker.replay_prompt || worker.replay_prompt_ready ? 'armed' : undefined),
        taskBoardDetailCell('Last error', lastError.message),
        taskBoardDetailCell('Recent events', recentEvents)
      ].filter(Boolean).join('');
      return '<div class="task-board-worker-detail"><div class="task-board-task-head"><div class="task-board-task-title" title="' + esc(workerId) + '">Worker detail</div>' +
        '<span class="task-board-status status-' + esc(sanitizeDecisioningClassToken(status)) + '">' + esc(status) + '</span></div>' +
        '<div class="task-board-worker-detail-grid">' + cells + '</div></div>';
    }

    function renderTaskBoardWorkerSupervisor() {
      const tick = state.taskBoard.workerSupervisor;
      const workers = state.taskBoard.workerOrder
        .map(function(workerId) { return state.taskBoard.workers[workerId]; })
        .filter(Boolean)
        .slice(0, 4);
      if (!tick && !workers.length) {
        return '<div class="task-board-empty">No worker events yet.</div>';
      }
      const status = tick ? String(tick.status || 'unknown') : 'unknown';
      const capacity = tick && tick.capacity && typeof tick.capacity === 'object' ? tick.capacity : {};
      const trustQueue = tick && Array.isArray(tick.trust_queue) ? tick.trust_queue : [];
      const eventIndex = tick && Array.isArray(tick.event_index) ? tick.event_index.slice(0, 3) : [];
      const meta = tick ? [
        tick.active_workers !== undefined ? tick.active_workers + ' active' : undefined,
        tick.blocked_workers !== undefined ? tick.blocked_workers + ' blocked' : undefined,
        tick.restarted_workers !== undefined ? tick.restarted_workers + ' restarted' : undefined,
        capacity.available_slots !== undefined ? capacity.available_slots + '/' + capacity.max_workers + ' slots free' : undefined,
        trustQueue.length ? 'trust: ' + trustQueue.join(', ') : undefined
      ].filter(Boolean).join(' · ') : '';
      const events = eventIndex.map(function(entry) {
        const event = entry && entry.event && typeof entry.event === 'object' ? entry.event : {};
        return '<div class="task-board-meta">' + esc([entry.worker_id, event.kind, event.detail].filter(Boolean).join(' · ')) + '</div>';
      }).join('');
      const selectedWorker = state.taskBoard.workers[state.taskBoard.selectedWorkerId] || workers[0];
      return '<div class="task-board-supervisor">' +
        '<div class="task-board-task-head"><div class="task-board-task-title">Worker supervisor</div>' +
        '<span class="task-board-status status-' + esc(sanitizeDecisioningClassToken(status)) + '">' + esc(status) + '</span></div>' +
        (meta ? '<div class="task-board-meta">' + esc(meta) + '</div>' : '') +
        (tick && tick.message ? '<div class="task-board-meta">' + esc(String(tick.message)) + '</div>' : '') +
        (events ? '<div style="margin-top:6px">' + events + '</div>' : '') +
      '</div>' +
      (workers.length ? '<div class="task-board-worker-list" style="margin-top:6px">' + workers.map(renderTaskBoardWorker).join('') + '</div>' : '') +
      '<div class="task-board-panel-title" style="margin-top:8px;">Worker Detail</div>' + renderTaskBoardWorkerDetail(selectedWorker);
    }


    function renderTaskBoardRecovery() {
      const events = state.taskBoard.recoveryEvents || [];
      if (!events.length) {
        return '<div class="task-board-empty">No recovery events.</div>';
      }
      return '<div class="task-board-recovery-list">' + events.map(function(item) {
        return '<div class="task-board-recovery-item"><div class="task-board-task-title">' + esc(String(item.kind || 'recovery')) + '</div><div class="task-board-meta">' + esc(item.summary || 'Recovery updated') + '</div></div>';
      }).join('') + '</div>';
    }

    function renderTaskBoardMetricCard(title, lines, status) {
      const safeStatus = sanitizeDecisioningClassToken(String(status || 'unknown'));
      return '<div class="task-board-metric-card"><div class="task-board-task-head"><div class="task-board-task-title">' + esc(title) + '</div>' +
        (status ? '<span class="task-board-status status-' + esc(safeStatus) + '">' + esc(String(status)) + '</span>' : '') + '</div>' +
        lines.filter(Boolean).map(function(line) { return '<div class="task-board-meta">' + esc(line) + '</div>'; }).join('') + '</div>';
    }

    function renderTaskBoardDaemon() {
      const daemon = state.taskBoard.daemon;
      if (!daemon) { return '<div class="task-board-empty">No daemon events.</div>'; }
      const stateValue = daemon.state && typeof daemon.state === 'object' ? daemon.state : daemon.state;
      const lastTick = stateValue && typeof stateValue === 'object' && stateValue.last_tick ? stateValue.last_tick : null;
      const status = stateValue && typeof stateValue === 'object' ? stateValue.status : (lastTick && lastTick.status);
      const runs = Array.isArray(daemon.runs) ? daemon.runs.length : undefined;
      const logs = Array.isArray(daemon.events) ? daemon.events.length : undefined;
      return renderTaskBoardMetricCard('Scheduler daemon', [
        daemon.kind ? 'event ' + daemon.kind : undefined,
        runs !== undefined ? runs + ' run(s)' : undefined,
        lastTick && lastTick.selected_task_id ? 'selected ' + lastTick.selected_task_id : undefined,
        logs !== undefined ? logs + ' log event(s)' : undefined,
        daemon.events_path ? 'logs ' + daemon.events_path : undefined
      ], status || 'unknown');
    }

    function renderTaskBoardRouteSummary() {
      const routeSummary = state.taskBoard.routeSummary;
      if (!routeSummary) { return '<div class="task-board-empty">No route feedback summary.</div>'; }
      const summaries = Array.isArray(routeSummary.summaries) ? routeSummary.summaries.slice(0, 3) : [];
      const lines = ['feedback ' + String(routeSummary.feedback_count || 0)];
      summaries.forEach(function(summary) {
        const success = Math.round(Number(summary.success_rate || 0) * 100) + '%';
        const latency = summary.avg_latency_ms !== undefined && summary.avg_latency_ms !== null ? Math.round(Number(summary.avg_latency_ms)) + 'ms' : 'n/a';
        lines.push([summary.phase, summary.model, success, latency, 'fail ' + String(summary.failures || 0)].filter(Boolean).join(' · '));
      });
      return renderTaskBoardMetricCard('Route feedback', lines, summaries.some(function(summary) { return Number(summary.failures || 0) > 0; }) ? 'failed' : 'completed');
    }

    function renderTaskBoardBenchmark() {
      const benchmark = state.taskBoard.benchmark;
      if (!benchmark) { return '<div class="task-board-empty">No benchmark run.</div>'; }
      const run = benchmark.run && typeof benchmark.run === 'object' ? benchmark.run : {};
      const summary = run.summary && typeof run.summary === 'object' ? run.summary : {};
      return renderTaskBoardMetricCard('Benchmark', [
        benchmark.suite && benchmark.suite.suite_id ? 'suite ' + benchmark.suite.suite_id : undefined,
        summary.total_tasks !== undefined ? summary.total_tasks + ' task(s)' : undefined,
        summary.passed_tasks !== undefined ? summary.passed_tasks + ' passed' : undefined,
        summary.average_total_score !== undefined ? 'score ' + Number(summary.average_total_score).toFixed(2) : undefined,
        summary.average_adaptive_routing_quality_score !== undefined ? 'routing ' + Number(summary.average_adaptive_routing_quality_score).toFixed(2) : undefined
      ], summary.passed_tasks !== undefined && summary.total_tasks !== undefined && summary.passed_tasks < summary.total_tasks ? 'failed' : 'completed');
    }

    function renderTaskBoardOperations() {
      return '<div class="task-board-metric-list">' + renderTaskBoardDaemon() + renderTaskBoardRouteSummary() + renderTaskBoardBenchmark() + '</div>';
    }


    function updateTaskBoardSurface() {
      try {
        if (!taskBoardSurface) { return; }
        const tasks = state.taskBoard.taskOrder
          .map(function(taskId) { return state.taskBoard.tasks[taskId]; })
          .filter(Boolean)
          .slice(0, 5);
        const workers = state.taskBoard.workerOrder
          .map(function(workerId) { return state.taskBoard.workers[workerId]; })
          .filter(Boolean);
        const hasWorkerState = workers.length > 0 || state.taskBoard.workerSupervisor;
        const hasOperationsState = state.taskBoard.daemon || state.taskBoard.routeSummary || state.taskBoard.benchmark;
        const hasBoardState = tasks.length > 0 || state.taskBoard.currentNode || state.taskBoard.recoveryEvents.length > 0 || hasWorkerState || hasOperationsState;
        if (!hasBoardState) {
          taskBoardSurface.hidden = true;
          taskBoardSurface.innerHTML = '';
          return;
        }
        const operationCount = [state.taskBoard.daemon, state.taskBoard.routeSummary, state.taskBoard.benchmark].filter(Boolean).length;
        const collapsed = state.taskBoard.collapsed !== false;
        const currentNodeStatus = state.taskBoard.currentNode && state.taskBoard.currentNode.status
          ? 'node ' + String(state.taskBoard.currentNode.status)
          : undefined;
        const summaryParts = [
          tasks.length + ' task' + (tasks.length === 1 ? '' : 's'),
          hasWorkerState ? workers.length + ' worker' + (workers.length === 1 ? '' : 's') : undefined,
          currentNodeStatus,
          state.taskBoard.recoveryEvents.length ? state.taskBoard.recoveryEvents.length + ' recovery' : undefined,
          operationCount ? operationCount + ' ops' : undefined
        ].filter(Boolean);
        taskBoardSurface.hidden = false;
        taskBoardSurface.classList.toggle('collapsed', collapsed);
        taskBoardSurface.innerHTML = '<div class="task-board-header">' +
          '<button type="button" class="task-board-toggle" data-task-board-toggle="1" aria-label="' + (collapsed ? 'Expand task board' : 'Collapse task board') + '" aria-expanded="' + (collapsed ? 'false' : 'true') + '">' + (collapsed ? '▶' : '▼') + '</button>' +
          '<div class="task-board-heading" data-task-board-toggle="1"><div class="task-board-title">Task Board</div><div class="task-board-summary">' + esc(summaryParts.join(' · ') || 'Runtime task status') + '</div>' + (collapsed ? '' : '<div class="task-board-subtitle">Live task status, worker health, active node, and recovery timeline from stream events.</div>') + '</div>' +
          '<div class="decisioning-badges">' + renderDecisioningSummaryBadge(tasks.length + ' task(s)', 'demo') + (hasWorkerState ? renderDecisioningSummaryBadge(workers.length + ' worker(s)', 'demo') : '') + '</div>' +
        '</div>' +
        '<div class="task-board-body"' + (collapsed ? ' hidden' : '') + '><div class="task-board-grid">' +
          '<div class="task-board-panel"><div class="task-board-panel-title">Tasks</div>' + renderTaskBoardProgress(tasks) + '<div class="task-board-list">' +
            (tasks.length ? tasks.map(renderTaskBoardTask).join('') : '<div class="task-board-empty">No tasks yet.</div>') +
          '</div></div>' +
          '<div class="task-board-panel"><div class="task-board-panel-title">Workers</div>' + renderTaskBoardWorkerSupervisor() + '</div>' +
          '<div class="task-board-panel"><div class="task-board-panel-title">Current Node</div>' + renderTaskBoardNode(state.taskBoard.currentNode) + renderPlanNodeProgress() + '<div class="task-board-panel-title" style="margin-top:8px;">Recovery</div>' + renderTaskBoardRecovery() + '</div>' +
          '<div class="task-board-panel"><div class="task-board-panel-title">Operations</div>' + renderTaskBoardOperations() + '</div>' +
        '</div></div>';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateTaskBoardSurface failed: ' + String(e) }); } catch (_) {}
      }
    }

    function addRuntimeEvent(kind, event) {
      try {
        // webview-level seq de-dup: keep last seq per (kind + id) to ignore stale events
        try {
          state._lastSeqByKey = state._lastSeqByKey || Object.create(null);
          const seq = event && typeof event.seq === 'number' ? event.seq : null;
          let key = 'global:' + String(kind);
          if (event && typeof event === 'object') {
            const id = event.task_id || event.taskId || event.node_id || event.nodeId || (event.task && event.task.task_id) || '';
            key = String(kind) + ':' + String(id || 'global');
          }
          if (seq !== null) {
            const prev = typeof state._lastSeqByKey[key] === 'number' ? state._lastSeqByKey[key] : -Infinity;
            if (seq <= prev) { return; }
            state._lastSeqByKey[key] = seq;
          }
        } catch (_) {}
        updateTaskBoardFromRuntimeEvent(kind, event);
      } catch (_) {}
      try {
        const div = document.createElement('div');
        const normalizedKind = String(kind || 'runtime_event').replace(/_/g, ' ');
        const summary = runtimeEventSummary(kind, event);
        const detail = renderRuntimeEventDetail(kind, event);
        const raw = event && typeof event === 'object' ? JSON.stringify(event, null, 2) : String(event || '');
        const tone = runtimeEventTone(kind, event);
        div.className = 'msg runtime-event tone-' + tone;
        div.innerHTML = '<div class="rt-head"><span class="rt-dot"></span>' +
          '<span class="rt-kind">' + esc(normalizedKind) + '</span>' +
          '<span class="rt-summary">' + esc(summary) + '</span></div>' +
          (detail ? '<div class="rt-detail">' + detail + '</div>' : '') +
          '<details class="rt-raw"><summary>Raw event</summary><pre>' + esc(raw) + '</pre></details>';
        if (traceFeed) {
          appendTraceEntry(div);
        } else if (thread) {
          thread.appendChild(div);
          smartScroll();
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addRuntimeEvent failed: ' + String(e) }); } catch (_) {}
      }
    }

    function addDecisioningEvent(event) {
      try {
        if (!event) { return; }
        if (!state.showReasoning) { return; }
        const div = document.createElement('div');
        div.className = 'msg decisioning-step';
        div.innerHTML = renderDecisioningEventMarkup(event);
        if (traceFeed) {
          appendTraceEntry(div);
        } else if (thread) {
          thread.appendChild(div);
          scrollBottom();
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addDecisioningEvent failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateReasoningToggle() {
      try {
        const btn = document.getElementById('btnReasoning');
        if (!btn) { return; }
        btn.style.opacity = state.traceOpen ? '1' : '0.6';
        btn.title = state.traceOpen ? 'Hide trace panel' : 'Show trace panel';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateReasoningToggle failed: ' + String(e) }); } catch (_) {}
      }
    }

    function maybeAutoOpenTrace() {
      try {
        if (!state.traceAutoOpenEnabled || state.traceOpen || state.traceManualClosed) { return; }
        state.traceOpen = true;
        state.showReasoning = true;
        updateReasoningToggle();
        updateTracePanel();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'maybeAutoOpenTrace failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateTracePanel() {
      try {
        if (!traceShell) { return; }
        traceShell.hidden = !state.traceOpen;
        if (traceResizer) { traceResizer.hidden = !state.traceOpen; }
        if (traceShell.hidden) {
          return;
        }
        if (traceBodyResizer) { traceBodyResizer.hidden = taskBoardSurface ? taskBoardSurface.hidden : true; }
        if (taskBoardSurface) {
          updateTaskBoardSurface();
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateTracePanel failed: ' + String(e) }); } catch (_) {}
      }
    }

    function appendTraceEntry(div) {
      try {
        if (!traceFeed) { return; }
        maybeAutoOpenTrace();
        traceFeed.appendChild(div);
        traceFeed.scrollTop = traceFeed.scrollHeight;
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'appendTraceEntry failed: ' + String(e) }); } catch (_) {}
      }
    }

    function clearTraceFeed() {
      try {
        if (!traceFeed) { return; }
        traceFeed.innerHTML = '';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'clearTraceFeed failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── thread rendering ── */
    function renderThread() {
      thread.innerHTML = '';
      streamBubble = null;
      streamCursor = null;
      clearPendingToolCards();
      clearTraceFeed();
      if (state.messages.length === 0 && state.recoveryEvidence.length === 0) {
        thread.appendChild(emptyState);
        showEmpty(true);
        updateTaskBoardSurface();
        return;
      }
      showEmpty(false);
      state.messages.forEach(function(m) { addBubble(m.role, m.text, m.attachments); });
      replayRecoveryEvidence();
      updateTaskBoardSurface();
    }

    /* ── submit ── */
    function submit() {
      const text = promptInput.value.trim();
      if (!text || state.streaming) { return; }
      if (!state.isTrusted) {
        showEmpty(false);
        addBubble('error', 'Prompt execution is blocked in this workspace. Trust the workspace or enable himalayaCode.allowUntrustedRuns.');
        setStatus('Workspace untrusted — execution blocked.', 'error');
        scrollBottom();
        return;
      }
      updateIdentityFromPrompt(text);
      state.messages.push({ role: 'user', text, attachments: attachedFiles.map(function(file) {
        const name = String(file).split(/[\\/]/).pop() || String(file);
        return { path: String(file), displayName: name, mediaKind: 'file' };
      }) });
      addBubble('user', text, state.messages[state.messages.length - 1].attachments);
      promptInput.value = '';
      promptInput.style.height = 'auto';
      const filesToSend = attachedFiles.slice();
      attachedFiles = [];
      renderAttachChips();
      state.streaming = true;
      state.lastRunFailed = false;
      updateSendButtonState();
      setStatus('Running…', 'running');
      startStream();
      vscode.postMessage({
        type: 'submit',
        prompt: text,
        permissionMode: state.permissionMode,
        resumeTarget: state.resumeTarget,
        files: filesToSend
      });
    }

    /* ── event wiring ── */
    if (sendBtn) { sendBtn.addEventListener('click', submit); }
    // Delegate clicks on clickable file paths inside tool cards.
    if (thread) {
      thread.addEventListener('click', function(event) {
        const target = event.target && event.target.closest ? event.target.closest('[data-open-path]') : null;
        if (!target) { return; }
        event.preventDefault();
        const path = target.getAttribute('data-open-path') || '';
        const lineAttr = target.getAttribute('data-open-line') || '';
        const line = lineAttr ? parseInt(lineAttr, 10) : null;
        if (path) { vscode.postMessage({ type: 'openFile', path: path, line: line }); }
      });
    }
    if (stopBtn) {
      stopBtn.addEventListener('click', () => {
        try {
          state.streaming = false;
          updateSendButtonState();
          setStatus('Cancelling…', 'warning');
          vscode.postMessage({ type: 'cancel' });
          promptInput.focus();
        } catch (e) {
          try { vscode.postMessage({ type: 'webview-error', message: 'stop failed: ' + String(e) }); } catch (_) {}
        }
      });
    }

    if (promptInput) {
      promptInput.addEventListener('keydown', function(e) {
        try {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
        } catch (_) {}
        setTimeout(autoResize, 0);
      });
      promptInput.addEventListener('input', autoResize);
    }

    const btnModelEl = document.getElementById('btnModel');
    if (btnModelEl) {
      btnModelEl.addEventListener('click', function() {
        try { vscode.postMessage({ type: 'command', command: 'configureModel' }); } catch (_) {}
      });
    }

    const btnPermEl = document.getElementById('btnPerm');
    if (btnPermEl) {
      btnPermEl.addEventListener('click', function() {
        try {
          const modes = PERMISSION_MODES;
          const idx = modes.indexOf(state.permissionMode);
          state.permissionMode = modes[(idx + 1) % modes.length];
          updateModelBar();
          try { vscode.postMessage({ type: 'permission-change', permissionMode: state.permissionMode }); } catch (_) {}
        } catch (_) {}
      });
    }

    const btnHistoryEl = document.getElementById('btnHistory');
    if (btnHistoryEl) {
      btnHistoryEl.addEventListener('click', function() {
        try { renderHistory(); toggleHistory(); } catch (_) {}
      });
    }

    const btnNewEl = document.getElementById('btnNew');
    if (btnNewEl) {
      btnNewEl.addEventListener('click', function() {
        try {
          state.messages = [];
          state.recoveryEvidence = [];
          state.activeRecordId = null;
          state.resumeTarget = '';
          state.taskBoard = taskBoardInitialState();
          state.traceOpen = false;
          clearTraceFeed();
          renderThread();
          updateTracePanel();
          setStatus('New session.', '');
          vscode.postMessage({ type: 'command', command: 'newSession' });
        } catch (_) {}
      });
    }

    const btnSkillsEl = document.getElementById('btnSkills');
    if (btnSkillsEl) {
      btnSkillsEl.addEventListener('click', function() {
        try { vscode.postMessage({ type: 'command', command: 'manageSkills' }); } catch (_) {}
      });
    }

    const btnReasoningEl = document.getElementById('btnReasoning');
    if (btnReasoningEl) {
      btnReasoningEl.addEventListener('click', function() {
        try {
          state.traceOpen = !state.traceOpen;
          state.traceManualClosed = !state.traceOpen;
          state.showReasoning = state.traceOpen;
          updateReasoningToggle();
          updateTracePanel();
          try { vscode.postMessage({ type: 'toggle-reasoning', enabled: state.showReasoning }); } catch (_) {}
        } catch (_) {}
      });
    }
    const btnTraceCloseEl = document.getElementById('btnTraceClose');
    if (btnTraceCloseEl) {
      btnTraceCloseEl.addEventListener('click', function() {
        try {
          state.traceOpen = false;
          state.showReasoning = false;
          state.traceManualClosed = true;
          updateReasoningToggle();
          updateTracePanel();
          try { vscode.postMessage({ type: 'toggle-reasoning', enabled: state.showReasoning }); } catch (_) {}
        } catch (_) {}
      });
    }
    // ensure initial visual state
    try { updateReasoningToggle(); } catch (_) {}
    try { updateTaskBoardSurface(); } catch (_) {}
    try { updateTracePanel(); } catch (_) {}

    const btnRefreshEl = document.getElementById('btnRefresh');
    if (btnRefreshEl) {
      btnRefreshEl.addEventListener('click', function() {
        try { vscode.postMessage({ type: 'refresh' }); } catch (_) {}
      });
    }

    const btnDoctorEl = document.getElementById('btnDoctor');
    if (btnDoctorEl) {
      btnDoctorEl.addEventListener('click', function() { try { vscode.postMessage({ type: 'command', command: 'doctor' }); } catch (_) {} });
    }

    const btnStatusEl = document.getElementById('btnStatus');
    if (btnStatusEl) {
      btnStatusEl.addEventListener('click', function() { try { vscode.postMessage({ type: 'command', command: 'status' }); } catch (_) {} });
    }

    const quickChipsEl = document.getElementById('quickChips');
    if (quickChipsEl) {
      quickChipsEl.addEventListener('click', function(e) {
        try {
          if (!(e.target instanceof Element)) { return; }
          const chip = e.target.closest('[data-prompt]');
          if (!chip) { return; }
          promptInput.value = chip.getAttribute('data-prompt');
          promptInput.focus();
          autoResize();
        } catch (_) {}
      });
    }

    if (taskBoardSurface) {
      taskBoardSurface.addEventListener('click', function(e) {
        try {
          if (!(e.target instanceof Element)) { return; }
          const toggle = e.target.closest('[data-task-board-toggle]');
          if (toggle) {
            state.taskBoard.collapsed = !state.taskBoard.collapsed;
            updateTaskBoardSurface();
            return;
          }
          const worker = e.target.closest('[data-worker-id]');
          if (!worker) { return; }
          const workerId = worker.getAttribute('data-worker-id');
          if (!workerId || !state.taskBoard.workers[workerId]) { return; }
          state.taskBoard.selectedWorkerId = workerId;
          updateTaskBoardSurface();
        } catch (_) {}
      });
    }
    if (traceResizer) {
      let isTraceResizing = false;
      let traceResizeStartX = 0;
      let traceStartWidth = 0;
      const minTraceWidth = 280;
      const maxTraceWidth = 640;
      traceResizer.addEventListener('pointerdown', function(event) {
        try {
          event.preventDefault();
          if (!traceShell) { return; }
          isTraceResizing = true;
          traceResizeStartX = event.clientX;
          traceStartWidth = traceShell.getBoundingClientRect().width;
          traceResizer.setPointerCapture(event.pointerId);
          document.addEventListener('pointermove', onTraceResize);
          document.addEventListener('pointerup', stopTraceResize);
        } catch (_) {}
      });
      function onTraceResize(event) {
        if (!isTraceResizing || !traceShell) { return; }
        const delta = traceResizeStartX - event.clientX;
        let width = traceStartWidth + delta;
        width = Math.min(Math.max(width, minTraceWidth), maxTraceWidth);
        traceShell.style.setProperty('--trace-width', width + 'px');
      }
      function stopTraceResize() {
        if (!isTraceResizing) { return; }
        isTraceResizing = false;
        document.removeEventListener('pointermove', onTraceResize);
        document.removeEventListener('pointerup', stopTraceResize);
      }
    }
    if (traceBodyResizer) {
      let isTraceBodyResizing = false;
      let traceBodyStartY = 0;
      let traceBodyStartHeight = 0;
      const minTraceFeedHeight = 120;
      const maxTraceFeedHeight = 520;
      traceBodyResizer.addEventListener('pointerdown', function(event) {
        try {
          event.preventDefault();
          if (!traceShell) { return; }
          isTraceBodyResizing = true;
          traceBodyStartY = event.clientY;
          traceBodyStartHeight = traceFeed ? traceFeed.getBoundingClientRect().height : 0;
          traceBodyResizer.setPointerCapture(event.pointerId);
          document.addEventListener('pointermove', onTraceBodyResize);
          document.addEventListener('pointerup', stopTraceBodyResize);
        } catch (_) {}
      });
      function onTraceBodyResize(event) {
        if (!isTraceBodyResizing || !traceShell) { return; }
        const delta = event.clientY - traceBodyStartY;
        let height = traceBodyStartHeight + delta;
        height = Math.min(Math.max(height, minTraceFeedHeight), maxTraceFeedHeight);
        traceShell.style.setProperty('--trace-feed-height', height + 'px');
      }
      function stopTraceBodyResize() {
        if (!isTraceBodyResizing) { return; }
        isTraceBodyResizing = false;
        document.removeEventListener('pointermove', onTraceBodyResize);
        document.removeEventListener('pointerup', stopTraceBodyResize);
      }
    }

    /* ── messages from extension host ── */
    window.addEventListener('message', function(event) {
      const msg = event.data;
      if (!msg || !msg.type) { return; }
      switch (msg.type) {
        case 'files-picked':
          if (Array.isArray(msg.paths)) {
            attachedFiles = attachedFiles.concat(msg.paths);
            renderAttachChips();
          }
          break;
        case 'bootstrap':
          if (msg.bootstrap) {
            state.isTrusted = Boolean(msg.bootstrap.trust);
            if (msg.bootstrap.identity) {
              state.identity = msg.bootstrap.identity;
            }
            if (msg.bootstrap.history) {
              state.historyRecords = msg.bootstrap.history.records || [];
              state.activeRecordId = msg.bootstrap.history.activeRecordId || null;
              const activeRecord = state.historyRecords.find(function(rec) { return rec.id === state.activeRecordId; });
              state.resumeTarget = activeRecord && activeRecord.resumeTarget ? activeRecord.resumeTarget : state.resumeTarget;
              if (activeRecord && ((activeRecord.messages || []).length > 0 || (activeRecord.recoveryEvidence || []).length > 0)) {
                state.messages = (activeRecord.messages || []).map(function(m) {
                  return {
                    role: m.role,
                    text: m.text,
                    attachments: Array.isArray(m.attachments) ? m.attachments.slice() : []
                  };
                });
                state.recoveryEvidence = (activeRecord.recoveryEvidence || []).slice();
              }
            }
          }
          if (msg.options) {
            if (msg.options.model) { state.model = msg.options.model; }
            if (msg.options.modelBackend) { state.modelBackend = msg.options.modelBackend; }
            if (msg.options.permissionMode) { state.permissionMode = msg.options.permissionMode; }
            if (msg.options.resumeTarget !== undefined) { state.resumeTarget = msg.options.resumeTarget || ''; }
            if (msg.options.showReasoning !== undefined) {
              state.showReasoning = Boolean(msg.options.showReasoning);
              state.traceOpen = state.showReasoning;
            }
          }
          trustBanner.hidden = state.isTrusted;
          try { updateTracePanel(); } catch (_) {}
            setStatus(
              state.isTrusted ? 'Ready' : 'Workspace untrusted — execution blocked.',
              state.isTrusted ? '' : 'error'
            );
          updateModelBar();
          updateSendButtonState();
          try { updateReasoningToggle(); } catch (_) {}
          try { updateTaskBoardSurface(); } catch (_) {}
          break;
        
        case 'historyRecord':
          if (msg.record) {
            applyHistoryRecord(msg.record);
            renderHistory();
          }
          break;
        case 'historyDeleted':
          if (msg.historyId) {
            state.historyRecords = state.historyRecords.filter(function(rec) { return rec.id !== msg.historyId; });
            if (state.activeRecordId === msg.historyId || msg.selectedHistoryId === null) {
              state.activeRecordId = msg.activeRecordId || null;
              if (!state.activeRecordId) {
                state.messages = [];
                state.recoveryEvidence = [];
                state.resumeTarget = '';
                state.taskBoard = taskBoardInitialState();
                renderThread();
              }
            }
            renderHistory();
            updateSendButtonState();
            setStatus('History deleted.', 'done');
          }
          break;
        case 'historyDeleteCancelled':
          renderHistory();
          setStatus('Delete cancelled.', '');
          break;
        case 'session-reset':
          state.messages = [];
          state.recoveryEvidence = [];
          state.activeRecordId = null;
          state.resumeTarget = '';
          state.taskBoard = taskBoardInitialState();
          state.traceOpen = false;
          clearTraceFeed();
          renderThread();
          updateTracePanel();
          setStatus('Ready', '');
          break;
        case 'model-updated':
          if (msg.model) { state.model = msg.model; }
          if (msg.modelBackend) { state.modelBackend = msg.modelBackend; }
          updateModelBar();
          setStatus('Model updated: ' + state.model, 'done');
          break;
        case 'insertComposerText': {
          try {
            const promptEl = document.getElementById('promptInput');
            if (promptEl && typeof msg.text === 'string') {
              const existing = promptEl.value || '';
              promptEl.value = existing && !existing.endsWith(' ') && !existing.endsWith('\\n')
                ? existing + ' ' + msg.text
                : existing + msg.text;
              promptEl.style.height = 'auto';
              promptEl.style.height = Math.min(promptEl.scrollHeight, 160) + 'px';
              promptEl.focus();
              if (typeof updateSendButtonState === 'function') { updateSendButtonState(); }
            }
          } catch (_) {}
          break;
        }
        case 'sessionMeta': {
          const sessionId = msg.sessionId ? String(msg.sessionId) : '';
          if (sessionId) {
            state.resumeTarget = sessionId;
            if (msg.model) { state.model = String(msg.model); }
            setStatus('Session ' + sessionId.slice(0, 12), 'done');
          }
          break;
        }
        case 'runStatus':
          setStatus(String(msg.text || 'Running…'), msg.kind || 'running');
          break;
        case 'assistantStart':
          state.lastRunFailed = false;
          if (!streamBubble) { startStream(); }
          updateSendButtonState();
          setStatus('Running…', 'running');
          break;
        case 'stderrChunk': {
          const stderrText = String(msg.text || '').trim();
          if (stderrText) {
            addBubble('stderr', stderrText);
          }
          break;
        }
        case 'assistantChunk':
          appendStream(msg.text || '');
          break;
        case 'toolStep': {
          renderToolCard(msg);
          break;
        }
        case 'permissionRequest': {
          const tool = msg.tool ? String(msg.tool) : 'unknown';
          const reason = msg.reason ? String(msg.reason) : '';
          const currentMode = msg.currentMode ? String(msg.currentMode) : '';
          const requiredMode = msg.requiredMode ? String(msg.requiredMode) : '';
          const input = msg.input ? String(msg.input) : '';
          let body = 'Permission requested for ' + tool;
          if (currentMode || requiredMode) {
            body += ' (' + (currentMode || 'unknown') + ' → ' + (requiredMode || 'unknown') + ')';
          }
          if (reason) {
            body += ': ' + reason;
          }
          if (input) {
            body += '\\nInput: ' + input.slice(0, 240);
          }
          addBubble('tool-step', body);
          break;
        }
        case 'permissionDenial': {
          const tool = msg.tool ? String(msg.tool) : 'unknown';
          const reason = msg.reason ? String(msg.reason) : '';
          addBubble('tool-step', 'Permission denied for ' + tool + (reason ? ': ' + reason : ''));
          break;
        }
        case 'reasoningStep': {
          try { addReasoningStep(msg.step); } catch (_) {}
          break;
        }
        case 'decisioningEvent': {
          try { addDecisioningEvent(msg.event); } catch (_) {}
          break;
        }
        case 'runtimeEvent': {
          try { addRuntimeEvent(msg.kind, msg.event); } catch (_) {}
          break;
        }
        case 'contextCompacted': {
          try {
            const removed = typeof msg.removed === 'number' ? msg.removed : undefined;
            const body = msg.notice
              ? String(msg.notice)
              : 'Context auto-compacted to stay within the model window'
                + (typeof removed === 'number' ? ' (removed ' + removed + ' message' + (removed === 1 ? '' : 's') + ')' : '') + '.';
            addBubble('notice', body);
          } catch (_) {}
          break;
        }
        case 'recoverySuggestion': {
          try {
            state.recoveryEvidence.push({
              tool: msg.tool,
              reason: msg.reason,
              action: msg.action,
              suggestion: msg.suggestion,
              sourceEvent: msg.sourceEvent,
              failureClass: msg.failureClass,
              createdAt: Date.now()
            });
            rememberTaskBoardRecovery('recoverySuggestion', msg);
            updateTaskBoardSurface();
            addRecoverySuggestion(msg);
          } catch (_) {}
          break;
        }

        case 'assistantDone':
          endStream();
          state.streaming = false;
          updateSendButtonState();
          if (!state.lastRunFailed) {
            setStatus('Done', 'done');
          }
          break;
        case 'error':
          endStream();
          state.streaming = false;
          state.lastRunFailed = true;
          updateSendButtonState();
          addBubble('error', msg.text || 'Unknown error');
          setStatus('Error', 'error');
          break;
      }
    });

    /* ── init ── */
    trustBanner.hidden = state.isTrusted;
    setStatus(state.isTrusted ? 'Ready' : 'Workspace untrusted — execution blocked.', state.isTrusted ? '' : 'error');
    updateModelBar();
    updateSendButtonState();
    renderThread();
    try { updateReasoningToggle(); } catch (_) {}
    try { vscode.postMessage({ type: 'ready' }); } catch (_) {}

    // Self-check: if model label remained at the placeholder, try re-requesting bootstrap after a short delay.
    setTimeout(function() {
      try {
        if (modelLabel && (modelLabel.textContent === 'Loading…' || !modelLabel.textContent)) {
          try { vscode.postMessage({ type: 'ready' }); } catch (_) {}
        }
      } catch (_) {}
    }, 3000);
  })();
  </script>
</body>
</html>
`;
    return _head + _body + _script;
  }

  private escapeHtml(value: string): string {
    return value
      .replace(/&/gu, '&amp;')
      .replace(/</gu, '&lt;')
      .replace(/>/gu, '&gt;')
      .replace(/"/gu, '&quot;')
      .replace(/'/gu, '&#39;');
  }

  dispose(): void {
    if (HimalayaChatPanel.currentPanel === this) {
      HimalayaChatPanel.currentPanel = undefined;
    }
    this.closeReplWorker();
    while (this.disposables.length > 0) {
      const disposable = this.disposables.pop();
      disposable?.dispose();
    }
  }
}

export class HimalayaPanelManager {
  private surface: HimalayaChatPanel | undefined;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly cli: HimalayaCli,
    private readonly output: vscode.OutputChannel,
    private readonly history: HimalayaHistoryStore,
    private readonly buildBootstrap: () => Promise<ChatBootstrap>,
    private readonly onRefresh?: () => Promise<void>
  ) {}

  async reveal(options: ChatLaunchOptions = {}): Promise<void> {
    if (this.surface) {
      this.surface.reveal(options);
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      'himalayaChatPanel',
      'Himalaya Chat',
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: false },
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, 'media')]
      }
    );

    this.surface = HimalayaChatPanel.attachToWebviewPanel(
      this.context,
      this.cli,
      this.output,
      this.history,
      panel,
      createFallbackBootstrap(),
      this.onRefresh
    );
    panel.onDidDispose(() => {
      this.surface = undefined;
    });
    this.surface.initialize(options);
    panel.reveal(panel.viewColumn ?? vscode.ViewColumn.Beside, false);

    void (async () => {
      try {
        const bootstrap = await this.buildBootstrap();
        if (this.surface) {
          this.surface.postBootstrap(bootstrap);
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.output.appendLine(`Failed to build Himalaya panel bootstrap: ${message}`);
      }
    })();
  }

  async openModelConfig(): Promise<void> {
    const modelRoute = await readModelRoute(this.context);
    const options: ChatLaunchOptions = {
      model: modelRoute.model || 'sonnet',
      modelBackend: modelRoute.modelBackend || 'auto',
      cloudBaseUrl: modelRoute.cloudBaseUrl,
      cloudApiKey: modelRoute.cloudApiKey,
      cloudModel: modelRoute.cloudModel,
    };
    await this.reveal(options);
    await this.surface?.openModelConfigurationWizard();
  }

  async refresh(): Promise<void> {
    if (!this.surface) {
      return;
    }

    try {
      this.surface.postBootstrap(await this.buildBootstrap());
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`Failed to refresh Himalaya panel bootstrap: ${message}`);
    }
  }
}

export class HimalayaSidebarViewProvider implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | undefined;

  constructor(
    private readonly buildBootstrap: () => Promise<ChatBootstrap>
  ) {}

  async resolveWebviewView(webviewView: vscode.WebviewView): Promise<void> {
    this.view = webviewView;
    webviewView.webview.options = {
      enableScripts: true
    };
    webviewView.webview.onDidReceiveMessage(async (message) => {
      if (!message || typeof message !== 'object') {
        return;
      }

      const typedMessage = message as { type?: string };
      switch (typedMessage.type) {
        case 'openChat':
          await vscode.commands.executeCommand('himalaya.openChat');
          break;
        case 'configureModel':
          await vscode.commands.executeCommand('himalaya.configureModel');
          break;
        case 'status':
          await vscode.commands.executeCommand('himalaya.status');
          break;
        case 'doctor':
          await vscode.commands.executeCommand('himalaya.doctor');
          break;
        case 'refresh':
          await this.refresh();
          break;
      }
    }, undefined, []);

    await this.refresh();
  }

  async refresh(): Promise<void> {
    if (!this.view) {
      return;
    }

    let bootstrap: ChatBootstrap;
    try {
      bootstrap = await this.buildBootstrap();
    } catch {
      bootstrap = createFallbackBootstrap();
    }

    this.view.webview.html = this.buildHtml(this.view.webview, bootstrap);
  }

  private buildHtml(webview: vscode.Webview, bootstrap: ChatBootstrap): string {
    const nonce = String(Date.now());
    const totalHistory = bootstrap.history.records.length;
    const totalSessions = bootstrap.sessions.groups.reduce((count, group) => count + group.sessions.length, 0);
    const activeModel = bootstrap.history.records.find((record) => record.id === bootstrap.history.activeRecordId)?.model ?? bootstrap.config.defaultModel;

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${webview.cspSource} https:; style-src ${webview.cspSource} 'unsafe-inline'; script-src 'nonce-${nonce}'">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <style>
    :root { color-scheme: dark; --bg: #0c0f16; --panel: rgba(16, 20, 30, 0.94); --line: rgba(99,114,140,0.28); --text: #eef3fb; --muted: #8e9ab0; --accent: #f6c177; --accent2: #91d7e3; }
    * { box-sizing: border-box; }
    body { margin: 0; background: linear-gradient(180deg, #141a28 0%, #0c0f16 100%); color: var(--text); font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, sans-serif; }
    .shell { min-height: 100vh; padding: 16px; display: grid; gap: 14px; }
    .hero { border: 1px solid var(--line); border-radius: 18px; background: rgba(18,23,34,0.9); padding: 16px; display: grid; gap: 8px; }
    .brand { font-size: 18px; font-weight: 900; letter-spacing: .02em; }
    .subtle { color: var(--muted); font-size: 12px; line-height: 1.5; }
    .stats { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 8px; }
    .stat { border: 1px solid var(--line); border-radius: 14px; background: rgba(10,13,19,0.76); padding: 12px; }
    .stat .label { color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: .12em; }
    .stat .value { margin-top: 6px; font-size: 18px; font-weight: 900; }
    .actions { display: grid; gap: 8px; }
    button { border: 1px solid var(--line); border-radius: 12px; background: rgba(10,13,19,0.84); color: var(--text); padding: 10px 12px; cursor: pointer; font: inherit; }
    button.primary { background: linear-gradient(180deg, rgba(246,193,119,.18), rgba(246,193,119,.08)); }
    .footer { color: var(--muted); font-size: 12px; line-height: 1.5; }
  </style>
</head>
<body>
  <div class="shell">
    <section class="hero">
      <div class="brand">Himalaya Agent</div>
      <div class="subtle">Right-side entry point for the Himalaya workspace. Open the chat panel, inspect the current model route, or run diagnostics.</div>
      <div class="stats">
        <div class="stat"><div class="label">History</div><div class="value">${totalHistory}</div></div>
        <div class="stat"><div class="label">Sessions</div><div class="value">${totalSessions}</div></div>
        <div class="stat"><div class="label">Model</div><div class="value">${this.escapeHtml(activeModel)}</div></div>
      </div>
    </section>
    <section class="actions">
      <button class="primary" data-action="openChat">Open chat</button>
      <button data-action="configureModel">Configure model</button>
      <button data-action="status">Status</button>
      <button data-action="doctor">Doctor</button>
      <button data-action="refresh">Refresh view</button>
    </section>
    <div class="footer">If this view is empty after reload, check that the extension was reinstalled from the latest VSIX and that the view container is pinned in the activity bar or secondary sidebar.</div>
  </div>
  <script nonce="${nonce}">
    (function() {
      const vscode = acquireVsCodeApi();
      document.body.addEventListener('click', (event) => {
        const target = event.target;
        if (!(target instanceof HTMLElement)) {
          return;
        }
        const action = target.getAttribute('data-action');
        if (!action) {
          return;
        }
        vscode.postMessage({ type: action });
      });
    })();
  </script>
</body>
</html>`;
  }

  private escapeHtml(value: string): string {
    return value
      .replace(/&/gu, '&amp;')
      .replace(/</gu, '&lt;')
      .replace(/>/gu, '&gt;')
      .replace(/"/gu, '&quot;')
      .replace(/'/gu, '&#39;');
  }
}

export class HimalayaSidebarChatViewProvider implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | undefined;
  private surface: HimalayaChatPanel | undefined;
  private pendingOptions: ChatLaunchOptions = {};

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly cli: HimalayaCli,
    private readonly output: vscode.OutputChannel,
    private readonly history: HimalayaHistoryStore,
    private readonly buildBootstrap: () => Promise<ChatBootstrap>,
    private readonly onRefresh?: () => Promise<void>
  ) {}

  async resolveWebviewView(webviewView: vscode.WebviewView): Promise<void> {
    this.view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, 'media')]
    };

    let bootstrap: ChatBootstrap;
    try {
      bootstrap = await this.buildBootstrap();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`Failed to build Himalaya sidebar chat bootstrap: ${message}`);
      bootstrap = createFallbackBootstrap();
    }

    const host: ChatHost = {
      webview: webviewView.webview,
      onDidDispose: webviewView.onDidDispose,
      viewColumn: undefined,
      reveal: () => undefined
    };

    this.surface = HimalayaChatPanel.attachToWebviewPanel(this.context, this.cli, this.output, this.history, host, bootstrap, this.onRefresh);

    // Initialize with current model route merged with pending options
    const modelRoute = await readModelRoute(this.context);
    const options: ChatLaunchOptions = {
      ...this.pendingOptions,
      model: modelRoute.model || 'sonnet',
      modelBackend: modelRoute.modelBackend || 'auto',
      cloudBaseUrl: modelRoute.cloudBaseUrl,
      cloudApiKey: modelRoute.cloudApiKey,
      cloudModel: modelRoute.cloudModel,
    };
    this.surface.initialize(options);
    this.pendingOptions = {};
  }

  async show(options: ChatLaunchOptions = {}): Promise<void> {
    this.pendingOptions = { ...this.pendingOptions, ...options };
    this.surface?.initialize(this.pendingOptions);
  }

  async openModelConfig(): Promise<void> {
    await this.surface?.openModelConfigurationWizard();
  }

  async openSkills(): Promise<void> { await this.surface?.manageSkills(); }

  async refresh(): Promise<void> {
    if (!this.surface) {
      return;
    }

    try {
      this.surface.postBootstrap(await this.buildBootstrap());
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`Failed to refresh Himalaya sidebar chat bootstrap: ${message}`);
    }
  }
}

interface SidebarActionNode {
  label: string;
  description: string;
  command: vscode.Command;
}

export class HimalayaSidebarTreeProvider implements vscode.TreeDataProvider<SidebarActionNode> {
  private readonly changeEmitter = new vscode.EventEmitter<SidebarActionNode | undefined | null>();

  readonly onDidChangeTreeData = this.changeEmitter.event;

  refresh(): void {
    this.changeEmitter.fire(undefined);
  }

  getTreeItem(element: SidebarActionNode): vscode.TreeItem {
    const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
    item.description = element.description;
    item.command = element.command;
    return item;
  }

  getChildren(element?: SidebarActionNode): SidebarActionNode[] {
    if (element) {
      return [];
    }

    return [
      {
        label: 'Open chat',
        description: 'Launch the standalone Himalaya panel',
        command: { command: 'himalaya.openChat', title: 'Open Himalaya Chat' }
      },
      {
        label: 'Configure model',
        description: 'Pick a model route and API settings',
        command: { command: 'himalaya.configureModel', title: 'Configure Himalaya Model' }
      },
      {
        label: 'Status',
        description: 'Check CLI and environment status',
        command: { command: 'himalaya.status', title: 'Himalaya Status' }
      },
      {
        label: 'Doctor',
        description: 'Run the health check',
        command: { command: 'himalaya.doctor', title: 'Himalaya Doctor' }
      },
      {
        label: 'Refresh sessions',
        description: 'Reload the session tree',
        command: { command: 'himalaya.refreshSessions', title: 'Refresh Himalaya Sessions' }
      }
    ];
  }
}

function createFallbackBootstrap(): ChatBootstrap {
  const config = vscode.workspace.getConfiguration('himalayaCode');

  return {
    trust: vscode.workspace.isTrusted || config.get<boolean>('allowUntrustedRuns', false),
    config: {
      defaultModel: config.get<string>('defaultModel', 'sonnet'),
      defaultPermissionMode: normalizePermissionMode(config.get<string>('defaultPermissionMode', DEFAULT_PERMISSION_MODE)),
      defaultModelBackend: config.get<string>('defaultModelBackend', 'auto'),
      ollamaBaseUrl: config.get<string>('ollamaBaseUrl', 'http://127.0.0.1:11434/v1'),
      allowUntrustedRuns: config.get<boolean>('allowUntrustedRuns', false),
      binaryPath: vscode.workspace.getConfiguration('himalayaCode').get<string>('binaryPath', '')
    },
    history: {
      records: [],
      activeRecordId: null
    },
    modelCatalog: {
      recentModels: [],
      localModels: []
    },
    sessions: {
      groups: []
    },
    identity: readWorkspaceIdentity()
  };
}
