const fs = require('fs');
const path = require('path');
const Module = require('module');
const root = path.resolve(__dirname, '..');

const vscodeMock = {
  workspace: {
    workspaceFolders: [{ uri: { fsPath: root } }],
    isTrusted: true,
    getConfiguration: () => ({ get: (_k, d) => d }),
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
  Uri: { file: (p)=> ({ fsPath: p }), joinPath: (base,...s) => ({ fsPath: [base && base.fsPath, ...s].filter(Boolean).join('/') }) },
  ViewColumn: { Beside: 2 },
};

const oldLoad = Module._load;
Module._load = (request, parent, isMain) => {
  if (request === 'vscode') return vscodeMock;
  return oldLoad(request, parent, isMain);
};

(async ()=>{
  try {
    const { HimalayaChatPanel } = require('../out/chatPanel.js');
    const context = {
      workspaceState: { get: (k,d)=>d, update: async ()=>{} },
      globalState: { get: (k,d)=>d, update: async ()=>{} },
      secrets: { get: async ()=> undefined, store: async ()=>{}, delete: async ()=>{} },
      extensionUri: { fsPath: root },
      extensionPath: root,
    };

    const posted = [];
    const host = {
      webview: { onDidReceiveMessage: ()=>({dispose(){}}), postMessage: (m)=>{ posted.push(m); return Promise.resolve(true); }, cspSource:'vscode-resource', asWebviewUri: (uri)=> ({ toString: ()=> 'vscode-resource://' + ((uri && uri.fsPath) || '')}) },
      onDidDispose: ()=>({dispose(){}}),
    };

    const transcript = fs.readFileSync(path.resolve(__dirname, '..', '..', 'protocol', 'stream-json-v1.golden.ndjson'), 'utf8');
    const runCalls = [];
    const cli = {
      run: async (args, options) => {
        runCalls.push(args);
        for (const chunk of [transcript.slice(0,73), transcript.slice(73,211), transcript.slice(211)]) {
          options.onStdout(chunk);
        }
        return { exitCode: 0, stdout: transcript, stderr: '' };
      },
      startRepl: async () => { throw new Error('unexpected repl start'); }
    };

    const records = [];
    const history = {
      records: () => records,
      createDraft: async (input)=> { const rec = { id: `r-${records.length+1}`, title: input.title, createdAt: Date.now(), updatedAt: Date.now(), pinned:false, model: input.model, modelBackend: input.modelBackend, permissionMode: input.permissionMode, resumeTarget: input.resumeTarget, cwd: input.cwd, messages: [], recoveryEvidence: [] }; records.push(rec); return rec; },
      setActiveRecord: async ()=>{}, upsert: async (r)=>{},
      appendMessage: async (recordId, message)=>{ const rec = records.find(r=>r.id===recordId); if(rec) rec.messages.push(message); },
      replaceAssistantTail: async ()=>{},
      appendRecoveryEvidence: async (recordId, evidence)=>{ const rec = records.find(r=>r.id===recordId); if(rec) rec.recoveryEvidence.push(evidence); }
    };

    const bootstrap = { trust:true, config: { defaultModel:'sonnet', defaultPermissionMode:'read-only', defaultModelBackend:'auto', ollamaBaseUrl:'', allowUntrustedRuns:false, binaryPath:'' }, history:{records:[], activeRecordId:null}, modelCatalog:{recentModels:[], localModels:[]}, sessions:{groups:[]}, identity:{} };

    const panel = new HimalayaChatPanel(context, cli, { appendLine: ()=>{} }, history, host, bootstrap);
    panel.initialize({ showReasoning: true });
    panel.replCanReuse = false;
    await panel.executePromptSubmission({ prompt: 'Replay stream', model: 'sonnet', permissionMode: 'read-only', cwd: root });

    console.log('Posted messages:');
    console.dir(posted, { depth: null });
    console.log('Records:', JSON.stringify(records, null, 2));
  } catch (e) {
    console.error('err', e);
  } finally {
    Module._load = oldLoad;
  }
})();
