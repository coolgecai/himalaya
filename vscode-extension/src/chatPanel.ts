import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import { HimalayaCli } from './cli';
import { ChatHistorySnapshot, HimalayaHistoryStore } from './history';
import { readModelRoute, writeModelRoute } from './modelRoute';
import {
  buildWorkspaceDangerApprovalKey,
  normalizeDangerousPermissionConfirmationPolicy,
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
import { SessionSnapshot } from './sessionTree';

export interface ChatLaunchOptions {
  [key: string]: unknown;
  model?: string;
  modelBackend?: string;
  permissionMode?: string;
  resumeTarget?: string;
  cwd?: string;
  prompt?: string;
  files?: string[];
  cloudBaseUrl?: string;
  cloudApiKey?: string;
  cloudModel?: string;
  showReasoning?: boolean;
  selectedHistoryId?: string | null;
  selectedCliSessionId?: string | null;
}

export interface ChatBootstrap {
  [key: string]: unknown;
  trust: boolean;
  config: any;
  history: ChatHistorySnapshot;
  modelCatalog: { recentModels: string[]; localModels: string[]; [key: string]: unknown };
  sessions: SessionSnapshot;
  selectedCliSessionId?: string | null;
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
  private readonly dangerApprovalKeyPrefix = 'himalayaCode.dangerApproval.v1';
  private readonly reasoningPrefKey = 'himalayaCode.showReasoning.v1';

  private summarizePrompt(prompt: string): string {
    const compact = prompt.replace(/\s+/gu, ' ').trim();
    return compact.length > 48 ? `${compact.slice(0, 48)}…` : compact || 'Untitled session';
  }

  private handleMessage(message: unknown): void {
    if (!message || typeof message !== 'object') {
      return;
    }

    const typedMessage = message as { type?: string; command?: string; prompt?: string; model?: string; modelBackend?: string; permissionMode?: string; resumeTarget?: string; cwd?: string; historyId?: string; selectedHistoryId?: string | null; selectedCliSessionId?: string | null };

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
      
      case 'command':
        if (typedMessage.command === 'configureModel') {
          void this.openModelConfigurationWizard();
          break;
        }

        if (typedMessage.command === 'doctor' || typedMessage.command === 'status') {
          void this.executeUtilityCommand(typedMessage.command);
          break;
        }

        if (typedMessage.command === 'newSession') {
          this.selectedHistoryId = null;
          this.selectedCliSessionId = null;
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
        if (typedMessage.selectedHistoryId !== undefined) {
          this.selectedHistoryId = typedMessage.selectedHistoryId;
        }
        if (typedMessage.historyId) {
          this.selectedHistoryId = typedMessage.historyId;
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
          files: (typedMessage as any).files as string[] | undefined
        });
        break;
      case 'webview-error':
        this.output.appendLine(`[Webview JS Error] ${(typedMessage as any).message} (line ${(typedMessage as any).line})`);
        break;
    }
  }

  private async executePromptSubmission(input: ChatLaunchOptions): Promise<void> {
    const prompt = input.prompt?.trim() || '';
    if (!prompt) {
      return;
    }

    if (!this.currentBootstrap.trust) {
      this.host.webview.postMessage({ type: 'error', text: 'Prompt execution is blocked in this workspace.' });
      return;
    }

    const route = await readModelRoute(this.context);
    const model = input.model?.trim() || route.model?.trim() || this.currentBootstrap.config.defaultModel;
    const modelBackend = input.modelBackend?.trim() || route.modelBackend || this.currentBootstrap.config.defaultModelBackend || 'auto';
    const permissionMode = input.permissionMode?.trim() || this.currentBootstrap.config.defaultPermissionMode;

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

    const cwd = input.cwd?.trim() || undefined;
    const resumeTarget = input.resumeTarget?.trim() || undefined;
    const files = input.files?.filter(f => f.trim()) ?? [];
    const maxInlineAttachmentBytes = 128 * 1024;
    const blockedAttachmentExtensions = new Set(['.pem', '.key', '.p12', '.pfx', '.kdbx']);
    const blockedAttachmentNameSnippets = ['id_rsa', 'id_ed25519', 'credentials', 'secret', 'token'];

    // Read file contents in the extension process and prepend to the prompt
    let fullPrompt = prompt;
    for (const filePath of files) {
      const name = path.basename(filePath);
      const lowerName = name.toLowerCase();
      const ext = path.extname(filePath).toLowerCase();
      const textExts = new Set(['.txt', '.md', '.ts', '.js', '.py', '.json', '.yaml', '.yml', '.toml', '.rs', '.go', '.java', '.c', '.cpp', '.h', '.css', '.html', '.xml', '.csv', '.sh', '.env', '.log']);
      try {
        if (blockedAttachmentExtensions.has(ext) || blockedAttachmentNameSnippets.some(snippet => lowerName.includes(snippet))) {
          fullPrompt = `[Attachment skipped for safety: ${name}]\n\n${fullPrompt}`;
          this.host.webview.postMessage({ type: 'stderrChunk', text: `Skipped sensitive attachment: ${name}\n` });
          continue;
        }

        if (textExts.has(ext)) {
          const fileStat = fs.statSync(filePath);
          if (fileStat.size > maxInlineAttachmentBytes) {
            fullPrompt = `[File: ${name}]\n(omitted inline; file is ${fileStat.size} bytes, above ${maxInlineAttachmentBytes} bytes limit)\n\n${fullPrompt}`;
            this.host.webview.postMessage({ type: 'stderrChunk', text: `Large text attachment kept as reference only: ${name} (${fileStat.size} bytes)\n` });
            continue;
          }
          const content = fs.readFileSync(filePath, 'utf8');
          fullPrompt = `[File: ${name}]\n${content}\n\n${fullPrompt}`;
        } else {
          // For binary formats (PDF, images, etc.), pass the absolute path so the model can use its read tools
          fullPrompt = `[Attached file path: ${filePath}]\n\n${fullPrompt}`;
        }
      } catch {
        fullPrompt = `[Attachment: ${name} — could not read file]\n\n${fullPrompt}`;
      }
    }

    const record = await this.history.createDraft({
      title: this.summarizePrompt(prompt),
      model,
      modelBackend,
      permissionMode,
      resumeTarget,
      cwd
    });

    this.selectedHistoryId = record.id;
    this.selectedCliSessionId = resumeTarget ?? null;
    this.currentOptions = {
      ...this.currentOptions,
      model,
      modelBackend,
      permissionMode,
      resumeTarget,
      cwd
    };
    this.isStreamingPrompt = true;

    await this.history.appendMessage(record.id, {
      role: 'user',
      text: prompt,
      createdAt: Date.now()
    });

    await this.history.appendMessage(record.id, {
      role: 'assistant',
      text: '',
      createdAt: Date.now()
    });

    this.host.webview.postMessage({ type: 'assistantStart', historyId: record.id, model });

    const args = this.buildPromptArgs(fullPrompt, model, permissionMode, resumeTarget);
    const env = this.buildModelEnv(modelBackend, this.currentBootstrap.config.ollamaBaseUrl, route);
    let assistantText = '';
    let lineBuf = '';
    const seenUnknownEventTypes = new Set<string>();
    let malformedEventCount = 0;
    let protocolMismatchWarned = false;

    const handleStreamLine = (line: string) => {
      if (!line.trim()) { return; }
      const parsed = parseStreamEventLine(line);
      if (!parsed.ok) {
        if (parsed.reason === 'invalid-shape') {
          malformedEventCount += 1;
          if (malformedEventCount <= 3) {
            this.host.webview.postMessage({ type: 'stderrChunk', text: `Malformed stream event ignored: ${line.slice(0, 160)}\n` });
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

      switch (event.type) {
        case 'text_delta':
          if (typeof event.text === 'string' && event.text.length > 0) {
            assistantText += event.text;
            this.host.webview.postMessage({ type: 'assistantChunk', text: event.text });
            void this.history.replaceAssistantTail(record.id, assistantText);
          }
          break;
        case 'tool_use':
          this.host.webview.postMessage({ type: 'toolStep', text: `${event.name ?? 'tool'}: ${JSON.stringify(event.input)}` });
          break;
        case 'tool_result':
          this.host.webview.postMessage({ type: 'toolStep', text: `→ ${event.name ?? 'tool'}${event.is_error ? ' [error]' : ''}: ${String(event.output ?? '').slice(0, 200)}` });
          break;
        
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
      const result = await this.cli.run(args, {
        cwd,
        env,
        silent: true,
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

      if (result.exitCode !== 0) {
        const failure = `\n\nHimalaya exited with code ${result.exitCode}.`;
        assistantText += failure;
        this.host.webview.postMessage({ type: 'error', text: failure.trim() });
      }

      await this.history.replaceAssistantTail(record.id, assistantText);
      this.isStreamingPrompt = false;
      this.host.webview.postMessage({ type: 'assistantDone' });
      await this.onRefresh?.();
    } catch (error) {
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

  private buildPromptArgs(prompt: string, model: string, permissionMode: string, resumeTarget?: string): string[] {
    const args: string[] = ['--output-format', 'stream-json', '--allow-broad-cwd'];

    if (permissionMode) {
      args.push('--permission-mode', permissionMode);
    }

    if (model) {
      args.push('--model', model);
    }

    if (resumeTarget) {
      args.push('--resume', resumeTarget);
    }

    args.push('prompt', prompt);
    return args;
  }

  private async confirmPermissionForRun(permissionMode: string, prompt: string): Promise<boolean> {
    if (permissionMode !== 'danger-full-access') {
      return true;
    }

    const policy = this.getDangerConfirmationPolicy();
    const workspaceKey = this.workspaceDangerApprovalKey();
    const workspaceApproved = this.context.workspaceState.get<boolean>(workspaceKey, false) ?? false;

    if (shouldAutoAllowDangerRun(policy, workspaceApproved)) {
      this.output.appendLine(`[permission-audit] mode=danger-full-access policy=${policy} decision=auto-allow`);
      return true;
    }

    const preview = prompt.replace(/\s+/gu, ' ').trim().slice(0, 120);
    const actions = policy === 'once-per-workspace'
      ? ['Run once', 'Always for this workspace', 'Cancel'] as const
      : ['Run once', 'Cancel'] as const;

    const answer = await vscode.window.showWarningMessage(
      `Run with danger-full-access? This may execute destructive actions.\n\nPrompt: ${preview}${prompt.length > 120 ? '…' : ''}`,
      { modal: true },
      ...actions
    );

    if (answer === 'Always for this workspace') {
      await this.context.workspaceState.update(workspaceKey, true);
      this.output.appendLine(`[permission-audit] mode=danger-full-access policy=${policy} decision=allow-workspace workspace=${workspaceKey}`);
      return true;
    }

    const allowed = answer === 'Run once';
    this.output.appendLine(`[permission-audit] mode=danger-full-access policy=${policy} decision=${allowed ? 'allow-once' : 'deny'}`);

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
      permissionMode: this.currentOptions.permissionMode ?? this.currentBootstrap.config.defaultPermissionMode,
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
          void this.history.replaceAssistantTail(record.id, assistantText);
        },
        onStderr: (chunk) => {
          const clean = chunk.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\x1b[()][AB012]/g, '');
          this.host.webview.postMessage({ type: 'stderrChunk', text: clean });
        }
      });

      if (result.exitCode !== 0) {
        const failure = `\n\nHimalaya exited with code ${result.exitCode}.`;
        assistantText += failure;
        this.host.webview.postMessage({ type: 'error', text: failure.trim() });
      }

      await this.history.replaceAssistantTail(record.id, assistantText);
      await this.onRefresh?.();
    } catch (error) {
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

  async openModelConfigurationWizard(): Promise<void> {
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
      await this.configureCloudModelRoute();
      return;
    }

    await this.configureLocalModelRoute();
  }

  private async configureCloudModelRoute(): Promise<void> {
    const cloudBaseUrl = await vscode.window.showInputBox({
      title: 'Cloud model',
      prompt: 'Enter the cloud network address or OpenAI-compatible base URL',
      value: this.currentBootstrap.config.ollamaBaseUrl || 'https://api.openai.com/v1',
      ignoreFocusOut: true
    });

    if (cloudBaseUrl === undefined) {
      return;
    }

    const cloudApiKey = await vscode.window.showInputBox({
      title: 'Cloud model',
      prompt: 'Enter the API key',
      password: true,
      ignoreFocusOut: true
    });

    if (cloudApiKey === undefined) {
      return;
    }

    const cloudModel = await vscode.window.showInputBox({
      title: 'Cloud model',
      prompt: 'Enter the model name',
      value: this.currentOptions.cloudModel || this.currentOptions.model || this.currentBootstrap.config.defaultModel,
      ignoreFocusOut: true
    });

    if (cloudModel === undefined) {
      return;
    }

    const selectedModel = cloudModel.trim();
    if (!cloudBaseUrl.trim() || !cloudApiKey.trim() || !selectedModel) {
      void vscode.window.showWarningMessage('Cloud model setup requires a network address, API key, and model name.');
      return;
    }

    await writeModelRoute(this.context, {
      model: selectedModel,
      modelBackend: 'cloud',
      modelSource: 'cloud',
      cloudBaseUrl: cloudBaseUrl.trim(),
      cloudApiKey: cloudApiKey.trim(),
      cloudModel: selectedModel
    });

    this.currentOptions = {
      ...this.currentOptions,
      model: selectedModel,
      modelBackend: 'cloud',
      cloudBaseUrl: cloudBaseUrl.trim(),
      cloudApiKey: cloudApiKey.trim(),
      cloudModel: selectedModel
    };

    void this.host.webview.postMessage({ type: 'model-updated', model: selectedModel, modelBackend: 'cloud' });
    void vscode.window.showInformationMessage(`Himalaya cloud model set to ${selectedModel}.`);
  }

  private async configureLocalModelRoute(): Promise<void> {
    const localModels = this.currentBootstrap.modelCatalog.localModels.length > 0
      ? this.currentBootstrap.modelCatalog.localModels
      : await this.cli.listLocalModels();

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
      `script-src 'nonce-${nonce}'`
    ].join('; ');
    const localModels: string[] = bootstrap.modelCatalog?.localModels ?? [];
    const historyRecords = bootstrap.history?.records ?? [];
    const activeRecordId = bootstrap.history?.activeRecordId ?? null;
    const defaultModel: string = bootstrap.config?.defaultModel ?? 'sonnet';
    const currentModel: string = (options.model ?? defaultModel).trim() || defaultModel;
    const currentBackend: string = options.modelBackend ?? bootstrap.config?.defaultModelBackend ?? 'auto';
    const currentPermission: string = options.permissionMode ?? bootstrap.config?.defaultPermissionMode ?? 'workspace-write';
    const resumeTarget: string = options.resumeTarget ?? '';
    const isTrusted: boolean = Boolean(bootstrap.trust);
    const historyJson = JSON.stringify(historyRecords).replace(/</g, '\\u003c');
    const localModelsJson = JSON.stringify(localModels).replace(/</g, '\\u003c');
    const stateJson = JSON.stringify({
      model: currentModel,
      modelBackend: currentBackend,
      permissionMode: currentPermission,
      resumeTarget,
      isTrusted,
      activeRecordId,
      showReasoning: Boolean(options.showReasoning)
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
    .msg.error { background: rgba(244,71,71,0.08); border: 1px solid rgba(244,71,71,0.25); color: #f88; }
    .msg.stderr { background: rgba(255,200,0,0.06); border: 1px solid rgba(255,200,0,0.15); color: #ffd; font-size: 11px; font-family: monospace; }
    .msg.tool-step { background: rgba(78,201,176,0.05); border-left: 2px solid #4ec9b0; padding: 4px 8px; align-self: flex-start; max-width: 100%; }
    .msg.tool-step .msg-role { color: #4ec9b0; }
    .msg.tool-step .msg-body { font-size: 11.5px; font-family: monospace; color: var(--text-dim); }
    .msg.reasoning-step { background: rgba(76,132,255,0.04); border-left: 2px solid #4c84ff; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.reasoning-step .msg-role { color: #4c84ff; }
    .msg.reasoning-step .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
    
    
    .msg-role {
      font-size: 10px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: .08em;
      color: var(--text-dim);
    }
    .msg.user .msg-role { color: var(--accent-text); }
    .msg.assistant .msg-role { color: var(--success); }
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
  </style>
`;
    const _body = `</head>
<body>
  <!-- top bar -->
  <div class="topbar">
    <span class="topbar-title">Himalaya</span>
    <button class="icon-btn" id="btnHistory" title="Toggle history">&#9776;</button>
    <button class="icon-btn" id="btnNew" title="New session">&#43;</button>
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
      <span id="permLabel">workspace-write</span>
    </button>
    <span class="spacer"></span>
    <button class="icon-btn" id="btnDoctor" title="Doctor">&#10003;</button>
    <button class="icon-btn" id="btnStatus" title="Status">&#9432;</button>
  </div>

  <!-- history drawer (collapsed by default) -->
  <div class="history-drawer" id="historyDrawer">
    <div id="historyList"></div>
  </div>

  <!-- thread -->
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
    const _script = `  <script nonce="${nonce}">
  window.onerror = function(msg, src, line, col, err) {
    try {
      var api = acquireVsCodeApi();
      api.postMessage({ type: 'webview-error', message: msg, line: line, col: col });
    } catch(e) {}
  };
  (function() {
    'use strict';
    const vscode = acquireVsCodeApi();

    /* ── initial state ── */
    const INIT = ${stateJson};
    const HISTORY = ${historyJson};
    const LOCAL_MODELS = ${localModelsJson};

    const state = {
      model: INIT.model,
      modelBackend: INIT.modelBackend,
      permissionMode: INIT.permissionMode,
      resumeTarget: INIT.resumeTarget || '',
      isTrusted: INIT.isTrusted,
      activeRecordId: INIT.activeRecordId,
      showReasoning: INIT.showReasoning || false,
      historyOpen: false,
      streaming: false,
      messages: [],   /* {role, text} */
      historyRecords: HISTORY
    };

    /* ── DOM refs ── */
    const thread       = document.getElementById('thread');
    const emptyState   = document.getElementById('emptyState');
    const promptInput  = document.getElementById('promptInput');
    const sendBtn      = document.getElementById('sendBtn');
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

    function updateModelBar() {
      try {
        if (!modelLabel || !modelDot || !permLabel) { return; }
        const b = state.modelBackend;
        const dotClass = b === 'cloud' ? 'cloud' : b === 'ollama' ? 'local' : 'unknown';
        modelDot.className = 'dot ' + dotClass;
        modelLabel.textContent = state.model || 'No model';
        permLabel.textContent = state.permissionMode || 'workspace-write';
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
      emptyState.style.display = show ? 'flex' : 'none';
    }

    /* ── message rendering ── */
    let streamBubble = null;
    let streamCursor = null;

    

    function startStream() {
      try {
        if (!thread) { return; }
        showEmpty(false);
        streamBubble = document.createElement('div');
        streamBubble.className = 'msg assistant';
        streamBubble.innerHTML = '<div class="msg-role">Himalaya</div><div class="msg-body"></div>';
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
        const body = streamBubble && streamBubble.querySelector ? streamBubble.querySelector('.msg-body') : null;
        if (!body) { return; }
        const textNode = document.createTextNode(text);
        if (streamCursor && streamCursor.parentNode === body) {
          body.insertBefore(textNode, streamCursor);
        } else {
          body.appendChild(textNode);
        }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'appendStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    function endStream() {
      try {
        if (streamCursor && streamCursor.remove) { streamCursor.remove(); }
        streamCursor = null;
        streamBubble = null;
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'endStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── history drawer ── */
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
          '<span class="hi-meta">' + esc(date) + '</span>';
        item.addEventListener('click', function() {
          state.activeRecordId = rec.id;
          vscode.postMessage({ type: 'history-action', historyId: rec.id, selectedHistoryId: rec.id });
          /* load messages from record */
          state.messages = (rec.messages || []).map(function(m) { return { role: m.role, text: m.text }; });
          renderThread();
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

    function addBubble(role, text) {
      try {
        if (!thread) { return; }
        const div = document.createElement('div');
        const cls = role === 'user' ? 'msg user' : role === 'assistant' ? 'msg assistant' : role === 'error' ? 'msg error' : role === 'stderr' ? 'msg stderr' : role === 'tool-step' ? 'msg tool-step' : 'msg';
        div.className = cls;
        const label = role === 'user' ? 'You' : role === 'assistant' ? 'Himalaya' : role === 'tool-step' ? 'Tool' : (String(role || '')[0] || '').toUpperCase() + String(role || '').slice(1);
        div.innerHTML = '<div class="msg-role">' + esc(label) + '</div><div class="msg-body">' + esc(text || '') + '</div>';
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addBubble failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── reasoning visualization ── */
    function addReasoningStep(step) {
      try {
        if (!step) { return; }
        if (!state.showReasoning) { return; }
        if (!thread) { return; }
        const div = document.createElement('div');
        div.className = 'msg reasoning-step';
        const role = esc(String(step.step_type || 'reason'));
        const body = esc(JSON.stringify(step, null, 2));
        div.innerHTML = '<div class="msg-role">' + role + '</div><div class="msg-body">' + body + '</div>';
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addReasoningStep failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateReasoningToggle() {
      try {
        const btn = document.getElementById('btnReasoning');
        if (!btn) { return; }
        btn.style.opacity = state.showReasoning ? '1' : '0.6';
        btn.title = state.showReasoning ? 'Hide reasoning visualization' : 'Show reasoning visualization';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateReasoningToggle failed: ' + String(e) }); } catch (_) {}
      }
    }

    

    /* ── thread rendering ── */
    function renderThread() {
      thread.innerHTML = '';
      streamBubble = null;
      streamCursor = null;
      if (state.messages.length === 0) {
        thread.appendChild(emptyState);
        showEmpty(true);
        return;
      }
      showEmpty(false);
      state.messages.forEach(function(m) { addBubble(m.role, m.text); });
    }

    /* ── submit ── */
    function submit() {
      const text = promptInput.value.trim();
      if (!text || state.streaming) { return; }
      if (!state.isTrusted) {
        setStatus('Workspace untrusted — execution blocked.', 'error');
        return;
      }
      state.messages.push({ role: 'user', text });
      addBubble('user', text);
      promptInput.value = '';
      promptInput.style.height = 'auto';
      const filesToSend = attachedFiles.slice();
      attachedFiles = [];
      renderAttachChips();
      state.streaming = true;
      sendBtn.disabled = true;
      setStatus('Running…', 'running');
      startStream();
      vscode.postMessage({
        type: 'submit',
        prompt: text,
        model: state.model,
        modelBackend: state.modelBackend,
        permissionMode: state.permissionMode,
        resumeTarget: state.resumeTarget,
        files: filesToSend
      });
    }

    /* ── event wiring ── */
    if (sendBtn) { sendBtn.addEventListener('click', submit); }

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
          const modes = ['read-only', 'workspace-write', 'danger-full-access'];
          const idx = modes.indexOf(state.permissionMode);
          state.permissionMode = modes[(idx + 1) % modes.length];
          updateModelBar();
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
          state.activeRecordId = null;
          state.resumeTarget = '';
          renderThread();
          setStatus('New session.', '');
          vscode.postMessage({ type: 'command', command: 'newSession' });
        } catch (_) {}
      });
    }

    const btnReasoningEl = document.getElementById('btnReasoning');
    if (btnReasoningEl) {
      btnReasoningEl.addEventListener('click', function() {
        try {
          state.showReasoning = !state.showReasoning;
          updateReasoningToggle();
          try { vscode.postMessage({ type: 'toggle-reasoning', enabled: state.showReasoning }); } catch (_) {}
        } catch (_) {}
      });
    }
    // ensure initial visual state
    try { updateReasoningToggle(); } catch (_) {}

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
            if (msg.bootstrap.history) {
              state.historyRecords = msg.bootstrap.history.records || [];
              state.activeRecordId = msg.bootstrap.history.activeRecordId || null;
            }
          }
          if (msg.options) {
            if (msg.options.model) { state.model = msg.options.model; }
            if (msg.options.modelBackend) { state.modelBackend = msg.options.modelBackend; }
            if (msg.options.permissionMode) { state.permissionMode = msg.options.permissionMode; }
            if (msg.options.resumeTarget !== undefined) { state.resumeTarget = msg.options.resumeTarget || ''; }
            if (msg.options.showReasoning !== undefined) { state.showReasoning = Boolean(msg.options.showReasoning); }
          }
          trustBanner.hidden = state.isTrusted;
          updateModelBar();
          try { updateReasoningToggle(); } catch (_) {}
          break;
        
        case 'session-reset':
          state.messages = [];
          state.activeRecordId = null;
          state.resumeTarget = '';
          renderThread();
          setStatus('Ready', '');
          break;
        case 'model-updated':
          if (msg.model) { state.model = msg.model; }
          if (msg.modelBackend) { state.modelBackend = msg.modelBackend; }
          updateModelBar();
          setStatus('Model updated: ' + state.model, 'done');
          break;
        case 'assistantStart':
          if (!streamBubble) { startStream(); }
          setStatus('Running…', 'running');
          break;
        case 'assistantChunk':
          appendStream(msg.text || '');
          break;
        case 'toolStep': {
          const div = document.createElement('div');
          div.className = 'msg tool-step';
          div.innerHTML = '<div class="msg-role">Tool</div><div class="msg-body">' + esc(msg.text || '') + '</div>';
          thread.appendChild(div);
          scrollBottom();
          break;
        }
        case 'reasoningStep': {
          try { addReasoningStep(msg.step); } catch (_) {}
          break;
        }
        
        case 'assistantDone':
          endStream();
          state.streaming = false;
          sendBtn.disabled = false;
          setStatus('Done', 'done');
          break;
        case 'error':
          endStream();
          state.streaming = false;
          sendBtn.disabled = false;
          addBubble('error', msg.text || 'Unknown error');
          setStatus('Error', 'error');
          break;
      }
    });

    /* ── init ── */
    trustBanner.hidden = state.isTrusted;
    updateModelBar();
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
        retainContextWhenHidden: true
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
      enableScripts: true
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
      defaultPermissionMode: config.get<string>('defaultPermissionMode', 'workspace-write'),
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
    }
  };
}