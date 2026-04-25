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
  showDecisioningDemo?: boolean;
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
    const savedDemo = this.context.workspaceState.get<boolean>(this.demoModePrefKey);
    const mergedOptions = { ...options } as ChatLaunchOptions;
    if (saved !== undefined && mergedOptions.showReasoning === undefined) {
      mergedOptions.showReasoning = Boolean(saved);
    }
    if (savedDemo !== undefined && mergedOptions.showDecisioningDemo === undefined) {
      mergedOptions.showDecisioningDemo = Boolean(savedDemo);
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
  private readonly demoModePrefKey = 'himalayaCode.showDecisioningDemo.v1';

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

      case 'toggle-decisioning-demo':
        if ((typedMessage as any).enabled !== undefined) {
          const enabled = Boolean((typedMessage as any).enabled);
          this.currentOptions = { ...this.currentOptions, showDecisioningDemo: enabled };
          void this.context.workspaceState.update(this.demoModePrefKey, enabled);
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

    const route = await readModelRoute(this.context);
    const model = route.model?.trim() || input.model?.trim() || this.currentBootstrap.config.defaultModel;
    const modelBackend = route.modelBackend || input.modelBackend?.trim() || this.currentBootstrap.config.defaultModelBackend || 'auto';

    if (!this.currentBootstrap.trust) {
      this.host.webview.postMessage({ type: 'error', text: 'Prompt execution is blocked in this workspace.' });
      return;
    }
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
    const files = input.files?.map((file) => file.trim()).filter((file): file is string => Boolean(file)) ?? [];
    const attachmentPaths: string[] = [];
    const blockedAttachmentExtensions = new Set(['.pem', '.key', '.p12', '.pfx', '.kdbx']);
    const blockedAttachmentNameSnippets = ['id_rsa', 'id_ed25519', 'credentials', 'secret', 'token'];

    for (const filePath of files) {
      const name = path.basename(filePath);
      const lowerName = name.toLowerCase();
      const ext = path.extname(filePath).toLowerCase();
      try {
        if (blockedAttachmentExtensions.has(ext) || blockedAttachmentNameSnippets.some(snippet => lowerName.includes(snippet))) {
          this.host.webview.postMessage({ type: 'stderrChunk', text: `Skipped sensitive attachment: ${name}\n` });
          continue;
        }

        if (!fs.existsSync(filePath)) {
          this.host.webview.postMessage({ type: 'stderrChunk', text: `Attachment not found: ${name}\n` });
          continue;
        }

        try {
          fs.accessSync(filePath, fs.constants.R_OK);
        } catch {
          this.host.webview.postMessage({ type: 'stderrChunk', text: `Attachment is not readable: ${name}\n` });
          continue;
        }

        attachmentPaths.push(filePath);
      } catch {
        this.host.webview.postMessage({ type: 'stderrChunk', text: `Attachment could not be prepared: ${name}\n` });
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

    const args = this.buildPromptArgs(prompt, model, permissionMode, resumeTarget, attachmentPaths);
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
        case 'decisioning_event':
          try {
            if (event.decisioning_event) {
              this.host.webview.postMessage({ type: 'decisioningEvent', event: event.decisioning_event });
            }
          } catch (e) {
            this.output.appendLine('[decisioning] failed to forward decisioning_event: ' + String(e));
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
        const stderr = result.stderr
          .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '')
          .replace(/\x1b[()][AB012]/g, '')
          .trim();
        const failure = stderr || `Himalaya exited with code ${result.exitCode}.`;
        assistantText += `\n\n${failure}`;
        this.host.webview.postMessage({ type: 'error', text: failure });
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

  private buildPromptArgs(
    prompt: string,
    model: string,
    permissionMode: string,
    resumeTarget?: string,
    attachmentPaths: string[] = []
  ): string[] {
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

    for (const filePath of attachmentPaths) {
      args.push('--file', filePath);
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
        const stderr = result.stderr
          .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '')
          .replace(/\x1b[()][AB012]/g, '')
          .trim();
        const failure = stderr || `Himalaya exited with code ${result.exitCode}.`;
        assistantText += failure;
        this.host.webview.postMessage({ type: 'error', text: failure });
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
      showReasoning: Boolean(options.showReasoning),
      showDecisioningDemo: Boolean(options.showDecisioningDemo)
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
    .msg.error { background: rgba(244,71,71,0.08); border: 1px solid rgba(244,71,71,0.25); color: #f88; }
    .msg.stderr { background: rgba(255,200,0,0.06); border: 1px solid rgba(255,200,0,0.15); color: #ffd; font-size: 11px; font-family: monospace; }
    .msg.tool-step { background: rgba(78,201,176,0.05); border-left: 2px solid #4ec9b0; padding: 4px 8px; align-self: flex-start; max-width: 100%; }
    .msg.tool-step .msg-role { color: #4ec9b0; }
    .msg.tool-step .msg-body { font-size: 11.5px; font-family: monospace; color: var(--text-dim); }
    .msg.reasoning-step { background: rgba(76,132,255,0.04); border-left: 2px solid #4c84ff; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.reasoning-step .msg-role { color: #4c84ff; }
    .msg.reasoning-step .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
    .msg.decisioning-step { background: rgba(255,167,38,0.05); border-left: 2px solid #ffa726; padding: 6px 8px; align-self: flex-start; max-width: 100%; }
    .msg.decisioning-step .msg-role { color: #ffa726; }
    .msg.decisioning-step .msg-body { font-size: 12px; color: var(--text-dim); font-family: inherit; }
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
    .decisioning-demo-surface {
      flex: 0 0 auto;
      margin: 8px 10px 0;
      padding: 10px;
      border-radius: 12px;
      border: 1px dashed rgba(76,132,255,0.40);
      background: linear-gradient(180deg, rgba(76,132,255,0.08), rgba(255,255,255,0.02));
      box-shadow: inset 0 0 0 1px rgba(255,255,255,0.02);
    }
    .decisioning-demo-surface[hidden] { display: none; }
    .decisioning-demo-header {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 10px;
      margin-bottom: 8px;
      color: var(--text-dim);
      font-size: 11px;
      line-height: 1.45;
    }
    .decisioning-demo-kicker {
      display: flex;
      flex-direction: column;
      gap: 4px;
    }
    .decisioning-demo-title {
      color: var(--text);
      font-size: 12px;
      font-weight: 700;
      letter-spacing: .03em;
    }
    .decisioning-badge.demo {
      border-color: rgba(76,132,255,0.35);
      background: rgba(76,132,255,0.12);
      color: #a8c7ff;
    }
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
    <button class="icon-btn" id="btnDemo" title="Force-visible decisioning demo mode">✨</button>
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

  <div class="decisioning-demo-surface" id="decisioningDemoSurface" hidden></div>

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
      showDecisioningDemo: INIT.showDecisioningDemo || false,
      historyOpen: false,
      streaming: false,
      lastRunFailed: false,
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
    const decisioningDemoSurface = document.getElementById('decisioningDemoSurface');

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
        sendBtn.title = blocked
          ? 'Trust the workspace or enable himalayaCode.allowUntrustedRuns to run prompts'
          : state.streaming
            ? 'A request is already running'
            : 'Send prompt';
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

    function renderDecisioningSummaryBadge(label, className) {
      return '<span class="decisioning-badge' + (className ? ' ' + className : '') + '">' + esc(label) + '</span>';
    }

    function getDecisioningDemoEvent() {
      return {
        kind: 'tool_selection',
        title: 'Forced-visible decisioning demo',
        summary: 'Synthetic snapshot showing tool scores, risk grading, and plan structure even before live decisioning events arrive.',
        task_id: 'demo-turn',
        confidence: 0.87,
        risk_score: 0.42,
        risk_level: 'medium',
        selected_tools: ['search', 'planner'],
        parallelizable: true,
        action: 'review',
        tool_scores: [
          {
            name: 'search',
            score: 0.92,
            success_rate: 0.96,
            latency_ms: 42,
            cost: 0.05,
            parallelizable: true,
            capabilities: ['search', 'read', 'context'],
            selected: true
          },
          {
            name: 'planner',
            score: 0.84,
            success_rate: 0.90,
            latency_ms: 88,
            cost: 0.12,
            parallelizable: true,
            capabilities: ['planning', 'analysis'],
            selected: true
          },
          {
            name: 'writer',
            score: 0.63,
            success_rate: 0.81,
            latency_ms: 120,
            cost: 0.10,
            parallelizable: false,
            capabilities: ['write', 'edit'],
            selected: false
          }
        ],
        plan_tree: {
          kind: 'task',
          id: 'demo-turn',
          title: 'Inspect and summarize the workspace',
          parallelizable: true,
          estimated_effort: 4,
          candidate_tools: ['search', 'planner', 'writer'],
          notes: [
            'Demo mode keeps this surface visible even if no live decisioning event is emitted.',
            'The card reuses the same rendering path as real decisioning events.'
          ],
          children: [
            {
              kind: 'step',
              id: 'demo-turn-analyze',
              title: 'Analyze the task and rank candidate tools',
              parallelizable: false,
              estimated_effort: 2,
              candidate_tools: ['search', 'planner'],
              notes: ['Shows the tool score panel and selected badges.'],
              children: []
            },
            {
              kind: 'step',
              id: 'demo-turn-verify',
              title: 'Verify the outcome and surface risk',
              parallelizable: false,
              estimated_effort: 1,
              candidate_tools: ['planner', 'writer'],
              notes: ['Shows the risk meter and the plan tree hierarchy.'],
              children: []
            }
          ]
        },
        details: [
          'Demo mode: the panel stays visible without waiting for a live decisioning turn.',
          'Use this mode to show tool scores, risk grade, and plan tree on demand.',
          'The backend decisioning pipeline still emits the same fields when enabled.'
        ]
      };
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

    function updateDecisioningDemoToggle() {
      try {
        const btn = document.getElementById('btnDemo');
        if (!btn) { return; }
        btn.classList.toggle('active', Boolean(state.showDecisioningDemo));
        btn.style.opacity = state.showDecisioningDemo ? '1' : '0.65';
        btn.title = state.showDecisioningDemo ? 'Hide forced-visible decisioning demo mode' : 'Show forced-visible decisioning demo mode';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateDecisioningDemoToggle failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateDecisioningDemoSurface() {
      try {
        if (!decisioningDemoSurface) { return; }
        if (!state.showDecisioningDemo) {
          decisioningDemoSurface.hidden = true;
          decisioningDemoSurface.innerHTML = '';
          return;
        }
        const demoEvent = getDecisioningDemoEvent();
        decisioningDemoSurface.hidden = false;
        decisioningDemoSurface.innerHTML =
          '<div class="decisioning-demo-header">' +
            '<div class="decisioning-demo-kicker">' +
              '<div class="decisioning-demo-title">Forced-visible decisioning demo</div>' +
              '<div>This surface stays visible so the new decisioning UI is obvious even when the backend does not emit a live event.</div>' +
            '</div>' +
            '<div class="decisioning-badges">' +
              renderDecisioningSummaryBadge('demo mode', 'demo') +
              renderDecisioningSummaryBadge('persistent surface') +
            '</div>' +
          '</div>' +
          '<div class="msg decisioning-step">' + renderDecisioningEventMarkup(demoEvent) + '</div>';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateDecisioningDemoSurface failed: ' + String(e) }); } catch (_) {}
      }
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
      const visibleScores = list.slice(0, 6);
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
      const childBlock = children.length ? '<div class="decisioning-tree-children">' + children.map(function(child) {
        return renderDecisioningPlanNode(child, selectedSet, depth + 1);
      }).join('') + '</div>' : '';
      return '<div class="decisioning-tree-node kind-' + esc(kind) + ' level-' + depth + '">' +
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

    function renderDecisioningNotes(details) {
      const list = Array.isArray(details) ? details.filter(Boolean) : [];
      if (!list.length) { return ''; }
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Notes</div>' +
        '<div class="decisioning-node-notes">' + list.map(function(item) {
          return '<div>' + esc(String(item || '')) + '</div>';
        }).join('') + '</div>' +
      '</section>';
    }

    function addDecisioningEvent(event) {
      try {
        if (!event) { return; }
        if (!thread) { return; }
        const div = document.createElement('div');
        div.className = 'msg decisioning-step';
        div.innerHTML = renderDecisioningEventMarkup(event);
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addDecisioningEvent failed: ' + String(e) }); } catch (_) {}
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
        showEmpty(false);
        addBubble('error', 'Prompt execution is blocked in this workspace. Trust the workspace or enable himalayaCode.allowUntrustedRuns.');
        setStatus('Workspace untrusted — execution blocked.', 'error');
        scrollBottom();
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
    const btnDemoEl = document.getElementById('btnDemo');
    if (btnDemoEl) {
      btnDemoEl.addEventListener('click', function() {
        try {
          state.showDecisioningDemo = !state.showDecisioningDemo;
          updateDecisioningDemoToggle();
          updateDecisioningDemoSurface();
          try { vscode.postMessage({ type: 'toggle-decisioning-demo', enabled: state.showDecisioningDemo }); } catch (_) {}
        } catch (_) {}
      });
    }
    // ensure initial visual state
    try { updateReasoningToggle(); } catch (_) {}
    try { updateDecisioningDemoToggle(); } catch (_) {}
    try { updateDecisioningDemoSurface(); } catch (_) {}

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
            if (msg.options.showDecisioningDemo !== undefined) { state.showDecisioningDemo = Boolean(msg.options.showDecisioningDemo); }
          }
          trustBanner.hidden = state.isTrusted;
            setStatus(
              state.isTrusted ? 'Ready' : 'Workspace untrusted — execution blocked.',
              state.isTrusted ? '' : 'error'
            );
          updateModelBar();
          updateSendButtonState();
          try { updateReasoningToggle(); } catch (_) {}
          try { updateDecisioningDemoToggle(); } catch (_) {}
          try { updateDecisioningDemoSurface(); } catch (_) {}
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
        case 'decisioningEvent': {
          try { addDecisioningEvent(msg.event); } catch (_) {}
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