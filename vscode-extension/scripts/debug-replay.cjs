const Module = require('module');
const path = require('path');

const root = path.resolve(__dirname, '..');
const oldLoad = Module._load;
Module._load = (request, parent, isMain) => {
  if (request === 'vscode') {
    return {
      workspace: {
        workspaceFolders: [{ uri: { fsPath: root } }],
        isTrusted: true,
        getConfiguration: () => ({ get: (_key, defaultValue) => defaultValue }),
      },
      window: {
        ViewColumn: { Beside: 2 },
        showOpenDialog: async () => [],
        showWarningMessage: async () => undefined,
        showErrorMessage: async () => undefined,
        showInformationMessage: async () => undefined,
      },
      Disposable: class {},
      EventEmitter: class { constructor() { this.event = () => undefined; } fire() {} },
      TreeItem: class {},
      TreeItemCollapsibleState: { None: 0 },
      commands: { executeCommand: async () => undefined },
      Uri: {
        file: (filePath) => ({ fsPath: filePath }),
        joinPath: (base, ...segments) => ({ fsPath: [base && base.fsPath, ...segments].filter(Boolean).join('/') }),
      },
    };
  }
  return oldLoad(request, parent, isMain);
};

(async () => {
  const { HimalayaChatPanel } = require('../out/chatPanel.js');
  const host = {
    webview: {
      onDidReceiveMessage: () => ({ dispose() {} }),
      postMessage: (m) => { console.log('POSTED:', m); posted.push(m); },
      cspSource: 'vscode-resource',
      asWebviewUri: (uri) => ({ toString: () => 'vscode-resource://' + ((uri && uri.fsPath) || '') }),
    },
    onDidDispose: () => ({ dispose() {} }),
  };
  const context = {
    workspaceState: { get: () => undefined, update: async () => undefined },
    globalState: { get: () => undefined, update: async () => undefined },
    extensionUri: { fsPath: root },
  };
  const Bootstrap = {
    trust: true,
    config: {
      defaultModel: 'sonnet',
      defaultPermissionMode: 'read-only',
      defaultModelBackend: 'auto',
      ollamaBaseUrl: '',
      allowUntrustedRuns: false,
      binaryPath: '',
    },
    history: { records: [], activeRecordId: null },
    modelCatalog: { recentModels: [], localModels: [] },
    sessions: { groups: [] },
    identity: {},
  };
  const posted = [];
  const historyMock = {
    appendMessage: async () => {},
    appendRecoveryEvidence: async () => {},
    createDraft: async (input) => ({ id: 'local-debug', title: input.title, createdAt: Date.now(), updatedAt: Date.now(), pinned: false, messages: [] }),
    records: () => []
  };
  const panel = new HimalayaChatPanel(context, {}, { appendLine() {} }, historyMock, host, Bootstrap);
  panel.initialize({ showReasoning: true });
  try {
    await panel.executePromptSubmission({ prompt: 'Replay stream', model: 'sonnet', permissionMode: 'read-only', cwd: root });
  } catch (e) {
    console.error('run failed', e);
  }
  console.log('Posted count:', posted.length);
})();
