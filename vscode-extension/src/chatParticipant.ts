import * as vscode from 'vscode';
import { extractReferencePathCandidate, prepareAttachmentDescriptors } from './attachmentPaths';
import { HimalayaCli } from './cli';
import { readModelRoute, ModelRouteState } from './modelRoute';

export function registerHimalayaChatParticipant(
  context: vscode.ExtensionContext,
  cli: HimalayaCli,
  output: vscode.OutputChannel
): vscode.Disposable {
  const participant = vscode.chat.createChatParticipant('himalaya-code-vscode.himalaya', async (request, chatContext, stream, token) => {
    void token;

    const prompt = request.prompt.trim();
    const config = vscode.workspace.getConfiguration('himalayaCode');

    if (request.command === 'doctor') {
      await runAndStream(cli, ['doctor'], stream);
      return;
    }

    if (request.command === 'status') {
      await runAndStream(cli, ['status'], stream);
      return;
    }

    if (request.command === 'login') {
      await runAndStream(cli, ['login'], stream);
      return;
    }

    if (request.command === 'logout') {
      await runAndStream(cli, ['logout'], stream);
      return;
    }

    if (!prompt) {
      stream.markdown(renderWelcomeMarkdown());
      return;
    }

    const trusted = vscode.workspace.isTrusted || config.get<boolean>('allowUntrustedRuns', false);
    if (!trusted) {
      stream.markdown('Prompt execution is blocked in this workspace. Trust the workspace or enable `himalayaCode.allowUntrustedRuns`.');
      return;
    }

    const route = await readModelRoute(context);
    const attachmentPaths = extractAttachmentPaths(request.references);
    const promptWithHistory = buildPromptWithChatHistory(prompt, chatContext);
    const args = buildPromptArgs(promptWithHistory, config, route, attachmentPaths);
    const modelBackend = route.modelBackend || config.get<string>('defaultModelBackend', 'auto');
    const env = buildModelEnv(modelBackend, config.get<string>('ollamaBaseUrl', 'http://127.0.0.1:11434/v1'), route);

    stream.progress('Running Himalaya...');
    await runAndStream(cli, args, stream, env);
  });

  participant.iconPath = vscode.Uri.joinPath(context.extensionUri, 'media', 'himalaya.svg');
  participant.followupProvider = {
    provideFollowups() {
      return [
        { prompt: 'summarize this repository', label: 'Summarize this repository' },
        { prompt: 'review the selected code', label: 'Review the selected code' },
        { prompt: 'show the current status', label: 'Show status', command: 'status' },
        { prompt: 'run the doctor health check', label: 'Run doctor', command: 'doctor' },
        { prompt: 'open the login flow', label: 'Login', command: 'login' }
      ];
    }
  };
  output.appendLine('Registered Himalaya chat participant (@himalaya).');
  return participant;
}

async function runAndStream(
  cli: HimalayaCli,
  args: string[],
  stream: vscode.ChatResponseStream,
  env?: NodeJS.ProcessEnv
): Promise<void> {
  const result = await cli.run(args, {
    env,
    onStdout: (chunk) => {
      stream.markdown(chunk);
    },
    onStderr: (chunk) => {
      stream.markdown(`\n${chunk}`);
    }
  });

  if (result.exitCode !== 0) {
    stream.markdown(`\n\nHimalaya exited with code ${result.exitCode}.`);
  }
}

function buildPromptWithChatHistory(prompt: string, chatContext: vscode.ChatContext): string {
  const history = chatContext.history.slice(-8);
  const lines: string[] = [];

  for (const turn of history) {
    if (turn instanceof vscode.ChatRequestTurn) {
      const text = trimHistoryText(turn.prompt);
      if (text) {
        lines.push(`User: ${text}`);
      }
      continue;
    }

    if (turn instanceof vscode.ChatResponseTurn) {
      const text = trimHistoryText(markdownTextFromResponse(turn.response));
      if (text) {
        lines.push(`Assistant: ${text}`);
      }
    }
  }

  if (lines.length === 0) {
    return prompt;
  }

  return [
    'Previous messages in this VS Code chat session:',
    lines.join('\n'),
    '',
    'Current user request:',
    prompt,
  ].join('\n');
}

function markdownTextFromResponse(response: readonly vscode.ChatResponsePart[]): string {
  return response
    .filter((part): part is vscode.ChatResponseMarkdownPart => part instanceof vscode.ChatResponseMarkdownPart)
    .map((part) => part.value.value)
    .join('\n');
}

function trimHistoryText(text: string): string {
  const compact = text.replace(/\s+/gu, ' ').trim();
  return compact.length > 1200 ? `${compact.slice(0, 1200)}…` : compact;
}

function buildPromptArgs(
  prompt: string,
  config: vscode.WorkspaceConfiguration,
  route?: ModelRouteState,
  attachmentPaths: string[] = []
): string[] {
  const args: string[] = [];

  const model = route?.model?.trim() || config.get<string>('defaultModel', 'sonnet');
  const permissionMode = config.get<string>('defaultPermissionMode', 'read-only');

  if (model) {
    args.push('--model', model);
  }

  if (permissionMode) {
    args.push('--permission-mode', permissionMode);
  }

  for (const filePath of attachmentPaths) {
    args.push('--file', filePath);
  }

  args.push('prompt', prompt);
  return args;
}

export function extractAttachmentPaths(references: readonly vscode.ChatPromptReference[]): string[] {
  const rawPaths: string[] = [];
  const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

  for (const reference of references) {
    const rawPath = extractReferencePathCandidate(reference.value);
    if (rawPath) {
      rawPaths.push(rawPath);
    }
  }

  return prepareAttachmentDescriptors(rawPaths, workspaceFolder, 'reference').paths;
}

function buildModelEnv(modelBackend: string, ollamaBaseUrl: string, route?: ModelRouteState): NodeJS.ProcessEnv | undefined {
  const routeBackend = (route?.modelBackend || '').toLowerCase();
  if (modelBackend.toLowerCase() !== 'ollama' && routeBackend !== 'local' && routeBackend !== 'cloud' && routeBackend !== 'ollama') {
    return undefined;
  }

  if (routeBackend === 'cloud' || modelBackend.toLowerCase() === 'cloud') {
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

function renderWelcomeMarkdown(): string {
  return [
    '## Himalaya is ready',
    '',
    'Try one of these quick starts:',
    '',
    '- `summarize this repository`',
    '- `review the selected code`',
    '- `show the current status`',
    '- `run the doctor health check`',
    '',
    'You can also use the built-in quick actions after a response.'
  ].join('\n');
}
