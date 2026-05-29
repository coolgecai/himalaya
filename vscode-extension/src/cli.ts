import * as cp from 'child_process';
import * as fs from 'fs';
import * as https from 'https';
import * as path from 'path';
import * as os from 'os';
import * as vscode from 'vscode';

export interface HimalayaRunOptions {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  silent?: boolean;
  onStdout?: (chunk: string) => void;
  onStderr?: (chunk: string) => void;
  signal?: AbortSignal;
}

export interface HimalayaReplHandle {
  ready: Promise<void>;
  send: (cmd: string) => void;
  close: () => void;
  kill: () => void;
  child: import('child_process').ChildProcess;
}

export interface HimalayaRunResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export type BinarySource = 'setting' | 'workspace' | 'path' | 'bundled' | 'downloaded';

export interface ResolvedBinary {
  path: string;
  source: BinarySource;
}

export class HimalayaCli {
  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly output: vscode.OutputChannel
  ) {}

  resolveBinary(): ResolvedBinary | null {
    const config = vscode.workspace.getConfiguration('himalayaCode');
    const configured = config.get<string>('binaryPath', '').trim();

    if (configured) {
      const resolved = this.resolveExplicitPath(configured);
      if (resolved) {
        return { path: resolved, source: 'setting' };
      }
    }

    for (const candidate of this.workspaceCandidates()) {
      if (candidate && fs.existsSync(candidate) && fs.statSync(candidate).isFile()) {
        return { path: candidate, source: 'workspace' };
      }
    }

    // Support running the Python development entrypoint if present in the workspace
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const pyEntry = path.join(folder.uri.fsPath, 'src', 'main.py');
      if (fs.existsSync(pyEntry) && fs.statSync(pyEntry).isFile()) {
        return { path: pyEntry, source: 'workspace' };
      }
    }

    const fromPath = this.resolveOnPath();
    if (fromPath) {
      return { path: fromPath, source: 'path' };
    }

    const bundled = path.join(this.context.extensionPath, 'bin', platformAssetName() ?? '');
    if (platformAssetName() && fs.existsSync(bundled) && fs.statSync(bundled).isFile()) {
      return { path: bundled, source: 'bundled' };
    }

    const downloaded = this.downloadedBinaryPath();
    if (downloaded && fs.existsSync(downloaded) && fs.statSync(downloaded).isFile()) {
      return { path: downloaded, source: 'downloaded' };
    }

    return null;
  }

  async ensureBinary(): Promise<ResolvedBinary> {
    const existing = this.resolveBinary();
    if (existing) {
      return existing;
    }

    const asset = platformAssetName();
    if (!asset) {
      throw new Error(
        `No Himalaya binary available for ${process.platform}/${process.arch}. ` +
        `Set himalayaCode.binaryPath manually.`
      );
    }

    const dest = this.downloadedBinaryPath()!;
    const url = `${RELEASE_BASE_URL}/${asset}`;
    this.output.appendLine(`Downloading Himalaya binary from ${url}`);

    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: 'Himalaya: downloading binary…', cancellable: false },
      () => this.downloadFile(url, dest)
    );

    if (process.platform !== 'win32') {
      fs.chmodSync(dest, 0o755);
    }

    this.output.appendLine(`Himalaya binary saved to ${dest}`);
    return { path: dest, source: 'downloaded' };
  }

  /// Start a persistent REPL process for long-lived sessions.
  /// Returns a handle that supports sending prompts and receiving stream-json events.
  async startRepl(options: {
    model: string;
    permissionMode?: string;
    cwd?: string;
    resumeTarget?: string;
    allowBroadCwd?: boolean;
    env?: NodeJS.ProcessEnv;
    onEvent?: (event: any) => void;
    onStderr?: (chunk: string) => void;
    onExit?: (code: number | null, signal: NodeJS.Signals | null) => void;
  }): Promise<HimalayaReplHandle> {
    const binary = await this.ensureBinary();
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      NO_COLOR: '1',
      TERM: 'dumb',
      ...options.env
    };
    const cwd = options.cwd ?? this.defaultWorkspaceCwd();
    const args = [
      '--repl',
      '--model', options.model,
    ];
    if (options.resumeTarget) {
      args.push('--resume', options.resumeTarget);
    }
    if (options.allowBroadCwd) {
      args.push('--allow-broad-cwd');
    }
    if (options.permissionMode) {
      args.push('--permission-mode', options.permissionMode);
    }

    const child = cp.spawn(binary.path, args, {
      cwd,
      env,
      shell: false,
      windowsHide: true,
      stdio: ['pipe', 'pipe', 'pipe']
    });

    let lineBuf = '';
    let settled = false;
    let readySettled = false;
    let readyResolve: () => void;
    let readyReject: (error: Error) => void;
    const readyPromise = new Promise<void>((resolve, reject) => {
      readyResolve = () => {
        readySettled = true;
        resolve();
      };
      readyReject = (error: Error) => {
        readySettled = true;
        reject(error);
      };
    });

    child.stdout!.setEncoding('utf8');
    child.stdout!.on('data', (chunk: string) => {
      const parts = chunk.split('\n');
      lineBuf += parts[0];
      for (let i = 1; i < parts.length; i++) {
        const line = lineBuf.trim();
        lineBuf = parts[i];
        if (!line) { continue; }
        try {
          const event = JSON.parse(line);
          options.onEvent?.(event);
        } catch { /* ignore malformed lines */ }
      }
    });

    child.stderr!.setEncoding('utf8');
    child.stderr!.on('data', (chunk: string) => {
      const clean = chunk.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\x1b[()][AB012]/g, '');
      if (clean.includes('[repl] ready')) {
        readyResolve();
      } else if (options.onStderr) {
        options.onStderr(clean);
      }
    });

    child.on('error', (error) => {
      settled = true;
      if (!readySettled) {
        readyReject(error instanceof Error ? error : new Error(String(error)));
      }
    });

    child.on('exit', (code, signal) => {
      settled = true;
      if (!readySettled) {
        readyReject(new Error(`Himalaya REPL exited before it was ready (code=${String(code)}, signal=${String(signal)})`));
      }
      options.onExit?.(code, signal);
    });

    const handle: HimalayaReplHandle = {
      ready: readyPromise,
      send: (cmd: string) => {
        if (!settled) {
          child.stdin!.write(cmd + '\n');
        }
      },
      close: () => {
        if (!settled) {
          settled = true;
          child.stdin!.write(JSON.stringify({ type: 'exit' }) + '\n');
          setTimeout(() => { try { child.kill(); } catch (_) {} }, 3000);
        }
      },
      kill: () => {
        if (!settled) {
          settled = true;
          try { child.kill('SIGTERM'); } catch (_) {}
          setTimeout(() => { try { child.kill('SIGKILL'); } catch (_) {} }, 2000);
        }
      },
      child,
    };

    return handle;
  }

  async run(args: string[], options: HimalayaRunOptions = {}): Promise<HimalayaRunResult> {
    const binary = await this.ensureBinary();

    const env: NodeJS.ProcessEnv = {
      ...process.env,
      NO_COLOR: '1',
      TERM: 'dumb',
      ...options.env
    };

    const cwd = options.cwd ?? this.defaultWorkspaceCwd();
    const stdoutParts: string[] = [];
    const stderrParts: string[] = [];

    const invocation = this.buildExecInvocation(binary.path, args);

    if (!options.silent) {
      this.output.appendLine(`> ${this.describeCommand(invocation.cmd, invocation.args, cwd)}`);
    }

    return await new Promise<HimalayaRunResult>((resolve, reject) => {
      const child = cp.spawn(invocation.cmd, invocation.args, {
        cwd,
        env,
        shell: false,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe']
      });

      let settled = false;

      // Handle abort signal
      const onAbort = () => {
        if (!settled) {
          settled = true;
          try { child.kill('SIGTERM'); } catch (_) {}
          // Give it 2s then force kill
          setTimeout(() => { try { child.kill('SIGKILL'); } catch (_) {} }, 2000);
          resolve({
            exitCode: -1,
            stdout: stdoutParts.join(''),
            stderr: 'Operation cancelled.'
          });
        }
      };

      if (options.signal) {
        if (options.signal.aborted) {
          onAbort();
          return;
        }
        options.signal.addEventListener('abort', onAbort, { once: true });
      }

      child.stdout!.setEncoding('utf8');
      child.stderr!.setEncoding('utf8');

      child.stdout!.on('data', (chunk: string) => {
        stdoutParts.push(chunk);
        options.onStdout?.(chunk);
        if (!options.silent) { this.output.append(chunk); }
      });

      child.stderr!.on('data', (chunk: string) => {
        stderrParts.push(chunk);
        options.onStderr?.(chunk);
        if (!options.silent) { this.output.append(chunk); }
      });

      child.on('error', (error) => {
        if (settled) { return; }
        settled = true;
        reject(error);
      });

      child.on('close', (exitCode) => {
        if (settled) {
          return;
        }
        settled = true;

        resolve({
          exitCode: exitCode ?? -1,
          stdout: stdoutParts.join(''),
          stderr: stderrParts.join('')
        });
      });
    });
  }

  async runToOutputChannel(args: string[], title: string): Promise<void> {
    if (this.shouldShowOutputChannel()) {
      this.output.show(true);
    }
    this.output.appendLine('');
    this.output.appendLine(`==> ${title}`);
    const result = await this.run(args);
    if (result.exitCode !== 0) {
      this.output.appendLine('');
      this.output.appendLine(`[exit ${result.exitCode}]`);
      throw new Error(`Himalaya exited with code ${result.exitCode}`);
    }
  }

  async listLocalModels(): Promise<string[]> {
    const modelsViaHttp = await this.listLocalModelsViaHttp();
    if (modelsViaHttp.length > 0) {
      return modelsViaHttp;
    }

    return await this.listLocalModelsViaCli();
  }

  private resolveExplicitPath(candidate: string): string | null {
    const expanded = candidate.startsWith('~') ? path.join(os.homedir(), candidate.slice(1)) : candidate;
    if (fs.existsSync(expanded) && fs.statSync(expanded).isFile()) {
      return expanded;
    }

    const withExe = process.platform === 'win32' && !expanded.toLowerCase().endsWith('.exe')
      ? `${expanded}.exe`
      : expanded;

    if (fs.existsSync(withExe) && fs.statSync(withExe).isFile()) {
      return withExe;
    }

    return null;
  }

  private workspaceCandidates(): string[] {
    const folders = vscode.workspace.workspaceFolders ?? [];
    const candidates: string[] = [];

    for (const folder of folders) {
      candidates.push(path.join(folder.uri.fsPath, 'rust', 'target', 'release', this.binaryName()));
      candidates.push(path.join(folder.uri.fsPath, 'rust', 'target', 'debug', this.binaryName()));
      candidates.push(path.join(folder.uri.fsPath, 'target', 'release', this.binaryName()));
      candidates.push(path.join(folder.uri.fsPath, 'target', 'debug', this.binaryName()));
    }

    return candidates;
  }

  private resolveOnPath(): string | null {
    const pathEntries = (process.env.PATH ?? '').split(path.delimiter).filter(Boolean);
    for (const entry of pathEntries) {
      const candidate = path.join(entry, this.binaryName());
      if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) {
        return candidate;
      }
    }

    return null;
  }

  private binaryName(): string {
    return process.platform === 'win32' ? 'Himalaya.exe' : 'Himalaya';
  }

  private defaultWorkspaceCwd(): string | undefined {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  }

  shouldShowOutputChannel(): boolean {
    return vscode.workspace.getConfiguration('himalayaCode').get<boolean>('showOutputChannelByDefault', true);
  }

  private describeCommand(binaryPath: string, args: string[], cwd?: string): string {
    const shown = [binaryPath, ...args].map((part) => this.quote(part)).join(' ');
    return cwd ? `(cwd: ${cwd}) ${shown}` : shown;
  }

  private quote(value: string): string {
    return /\s|["']/u.test(value) ? JSON.stringify(value) : value;
  }

  private buildExecInvocation(binaryPath: string, args: string[]): { cmd: string; args: string[] } {
    const lower = binaryPath.toLowerCase();

    // If the resolved path looks like a Python script, run via python interpreter
    if (lower.endsWith('.py')) {
      const python = process.env.PYTHON || process.env.PYTHON3 || 'python3';
      return { cmd: python, args: [binaryPath, ...args] };
    }

    // Default: run the binary directly
    return { cmd: binaryPath, args };
  }

  private downloadedBinaryPath(): string | null {
    const asset = platformAssetName();
    if (!asset) { return null; }
    const dir = this.context.globalStorageUri.fsPath;
    fs.mkdirSync(dir, { recursive: true });
    return path.join(dir, asset);
  }

  private downloadFile(url: string, dest: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const follow = (currentUrl: string, hops: number) => {
        if (hops > 5) { reject(new Error('Too many redirects')); return; }
        https.get(currentUrl, (res) => {
          const loc = res.headers.location;
          if ((res.statusCode === 301 || res.statusCode === 302 || res.statusCode === 307 || res.statusCode === 308) && loc) {
            res.resume();
            follow(loc, hops + 1);
            return;
          }
          if (res.statusCode !== 200) {
            res.resume();
            reject(new Error(`HTTP ${res.statusCode} downloading Himalaya binary`));
            return;
          }
          const tmp = `${dest}.tmp`;
          const file = fs.createWriteStream(tmp);
          res.pipe(file);
          file.on('finish', () => file.close(() => { fs.renameSync(tmp, dest); resolve(); }));
          file.on('error', (err) => { fs.unlink(tmp, () => {}); reject(err); });
        }).on('error', reject);
      };
      follow(url, 0);
    });
  }

  private async listLocalModelsViaHttp(): Promise<string[]> {
    const fetchFn = globalThis.fetch;
    if (!fetchFn) {
      return [];
    }

    try {
      const response = await fetchFn('http://127.0.0.1:11434/api/tags');
      if (!response.ok) {
        return [];
      }

      const body = await response.text();
      const matches = [...body.matchAll(/"name"\s*:\s*"([^"]+)"/gu)];
      return [...new Set(matches.map((match) => match[1]).filter(Boolean))];
    } catch {
      return [];
    }
  }

  private async listLocalModelsViaCli(): Promise<string[]> {
    try {
      return await new Promise<string[]>((resolve) => {
        cp.execFile('ollama', ['list'], { timeout: 5000, maxBuffer: 1024 * 1024 }, (error, stdout) => {
          if (error) {
            resolve([]);
            return;
          }

          const models = stdout
            .split('\n')
            .slice(1)
            .map((line) => line.trim().split(/\s+/u)[0] ?? '')
            .filter(Boolean);

          resolve([...new Set(models)]);
        });
      });
    } catch {
      return [];
    }
  }
}

const RELEASE_VERSION = '0.1.0';
const RELEASE_BASE_URL = `https://github.com/ultraworkers/Himalaya-code/releases/download/v${RELEASE_VERSION}`;

function platformAssetName(): string | null {
  const p = process.platform;
  const a = process.arch;
  if (p === 'linux'  && a === 'x64')   { return 'Himalaya-linux-x64'; }
  if (p === 'linux'  && a === 'arm64') { return 'Himalaya-linux-arm64'; }
  if (p === 'darwin' && a === 'x64')   { return 'Himalaya-darwin-x64'; }
  if (p === 'darwin' && a === 'arm64') { return 'Himalaya-darwin-arm64'; }
  if (p === 'win32'  && a === 'x64')   { return 'Himalaya-win32-x64.exe'; }
  return null;
}