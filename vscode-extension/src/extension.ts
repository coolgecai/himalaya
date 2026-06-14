import * as vscode from 'vscode';
import { HimalayaCli } from './cli';
import { HimalayaHistoryStore } from './history';
import { HimalayaSidebarChatViewProvider, HimalayaSidebarViewProvider, ChatBootstrap, ChatLaunchOptions, readWorkspaceIdentity } from './chatPanel';
import { readModelRoute, writeModelRoute } from './modelRoute';
import { HimalayaSessionTreeProvider, SessionFileNode } from './sessionTree';
import { registerHimalayaChatParticipant } from './chatParticipant';
import { DEFAULT_PERMISSION_MODE, normalizePermissionMode } from './permissionPolicy';

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const output = vscode.window.createOutputChannel('Himalaya');
  const cli = new HimalayaCli(context, output);
  const sessions = new HimalayaSessionTreeProvider();
  const history = new HimalayaHistoryStore(context);
  let sidebarChat: HimalayaSidebarChatViewProvider;

  const buildBootstrap = async (): Promise<ChatBootstrap> => {
    const config = vscode.workspace.getConfiguration('himalayaCode');
    const resolved = cli.resolveBinary();
    const recentModels = history.records()
      .map((record) => record.model?.trim())
      .filter((model): model is string => Boolean(model))
      .filter((model, index, models) => models.indexOf(model) === index);
    const localModels = await cli.listLocalModels();

    return {
      trust: vscode.workspace.isTrusted || config.get<boolean>('allowUntrustedRuns', false),
      config: {
        defaultModel: config.get<string>('defaultModel', 'sonnet'),
        defaultPermissionMode: normalizePermissionMode(config.get<string>('defaultPermissionMode', DEFAULT_PERMISSION_MODE)),
        dangerousPermissionConfirmationPolicy: config.get<string>('dangerousPermissionConfirmationPolicy', 'always'),
        defaultModelBackend: config.get<string>('defaultModelBackend', 'auto'),
        ollamaBaseUrl: config.get<string>('ollamaBaseUrl', 'http://127.0.0.1:11434/v1'),
        allowUntrustedRuns: config.get<boolean>('allowUntrustedRuns', false),
        binaryPath: resolved?.path ?? config.get<string>('binaryPath', '')
      },
      history: history.snapshot(),
      modelCatalog: {
        recentModels,
        localModels
      },
      sessions: await sessions.snapshot(),
      identity: readWorkspaceIdentity()
    };
  };

  const refreshChatPanel = async (): Promise<void> => {
    await sidebarChat.refresh();
    await sidebarActivity.refresh();
  };


  sidebarChat = new HimalayaSidebarChatViewProvider(context, cli, output, history, buildBootstrap, refreshChatPanel);
  const sidebarActivity = new HimalayaSidebarViewProvider(buildBootstrap);

  const statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  statusItem.text = 'Himalaya';
  statusItem.tooltip = 'Open Himalaya Chat';
  statusItem.command = 'himalaya.openChat';
  statusItem.show();

  const disposables: vscode.Disposable[] = [output, statusItem];

  try {
    disposables.push(registerHimalayaChatParticipant(context, cli, output));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    output.appendLine(`Failed to register Himalaya chat participant: ${message}`);
    void vscode.window.showWarningMessage('Himalaya chat participant is unavailable. The panel and commands are still enabled.');
  }

  disposables.push(
    vscode.window.registerWebviewViewProvider('himalaya.activityView', sidebarActivity, {
      webviewOptions: { retainContextWhenHidden: true }
    }),
    vscode.window.registerWebviewViewProvider('himalaya.secondaryChatView', sidebarChat, {
      webviewOptions: { retainContextWhenHidden: true }
    }),
    vscode.commands.registerCommand('himalaya.openChat', async (options?: ChatLaunchOptions) => {
      await vscode.commands.executeCommand('himalaya.secondaryChatView.focus');
      await sidebarChat.show({ ...(await readModelRoute(context)), ...(options ?? {}) });
    }),
    vscode.commands.registerCommand('himalaya.promptSelection', async () => {
      const editor = vscode.window.activeTextEditor;
      const selection = editor?.selection && !editor.selection.isEmpty
        ? editor.document.getText(editor.selection)
        : '';
      await vscode.commands.executeCommand('himalaya.secondaryChatView.focus');
      await sidebarChat.show({
        ...(await readModelRoute(context)),
        prompt: selection ? `Review this selection:\n\n${selection}` : undefined
      });
    }),
    vscode.commands.registerCommand('himalaya.configureModel', async () => {
      await vscode.commands.executeCommand('himalaya.secondaryChatView.focus');
      await sidebarChat.openModelConfig();
    }),
    vscode.commands.registerCommand('himalaya.manageSkills', async () => {
      await vscode.commands.executeCommand('himalaya.secondaryChatView.focus');
      await sidebarChat.openSkills();
    }),
    vscode.commands.registerCommand('himalaya.status', async () => {
      await cli.runToOutputChannel(['status'], 'Himalaya status');
    }),
    vscode.commands.registerCommand('himalaya.doctor', async () => {
      await cli.runToOutputChannel(['doctor'], 'Himalaya doctor');
    }),
    vscode.commands.registerCommand('himalaya.login', async () => {
      await cli.runToOutputChannel(['login'], 'Himalaya login');
    }),
    vscode.commands.registerCommand('himalaya.logout', async () => {
      await cli.runToOutputChannel(['logout'], 'Himalaya logout');
    }),
    vscode.commands.registerCommand('himalaya.refreshSessions', () => {
      sessions.refresh();
    }),
    vscode.commands.registerCommand('himalaya.openSession', async (session: SessionFileNode) => {
      await vscode.window.showTextDocument(session.uri, { preview: true });
    }),
    vscode.commands.registerCommand('himalaya.resumeSession', async (session: SessionFileNode) => {
      await vscode.commands.executeCommand('himalaya.secondaryChatView.focus');
      await sidebarChat.show({
        ...(await readModelRoute(context)),
        resumeTarget: session.sessionId()
      });
    }),
    vscode.commands.registerCommand('himalaya.configureBinaryPath', async () => {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'himalayaCode.binaryPath');
    }),
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      void refreshChatPanel();
      vscode.window.showInformationMessage('Himalaya: workspace trusted. Prompt execution is now enabled.');
    })
  );

  context.subscriptions.push(...disposables);

  context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(() => sessions.refresh()));

  context.subscriptions.push(
    history.onDidChange(() => void refreshChatPanel())
  );
}

export function deactivate(): void {
  // No-op; disposables are managed by VS Code.
}