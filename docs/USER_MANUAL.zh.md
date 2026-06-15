# Himalaya Code 软件使用说明书

适用版本：`0.1.0`  
本文基于当前 release 构建 `Git SHA 66449aa` 与 `Himalaya --help` 输出整理。

## 1. 软件简介

Himalaya Code 是一个面向软件工程场景的高级 AI Agent 工具。它以 Claude Code 类交互为参考，提供命令行 CLI、VS Code 扩展、长期会话记忆、复杂任务分解、自动迭代执行、工具调用、权限控制、MCP/插件扩展、模型路由反馈与策略治理能力。

它的典型用途包括：

- 阅读、理解和总结代码仓库。
- 生成修复方案、修改代码、运行验证命令。
- 将复杂任务拆解为可执行步骤或持久任务。
- 在长程任务中记录检查点、恢复执行、审计策略变更。
- 在 VS Code 中通过聊天面板和会话树自然交互。
- 根据任务阶段在不同模型之间路由，例如规划、编码、验证、总结。

Himalaya Code 当前包含两个主要用户入口：

- CLI：本地命令行工具。源码构建出的二进制名为 `Himalaya`；deb 包安装后命令名为 `himalaya-code`。
- VS Code 扩展：`himalaya-code-vscode-0.1.0.vsix`，内置 Linux x64 CLI，也可指定外部 CLI 路径。

本文示例默认使用 `Himalaya` 作为 CLI 命令名。如果你通过 deb 安装且没有创建软链接，请将示例中的 `Himalaya` 替换为 `himalaya-code`。

## 2. 安装方式

### 2.1 使用 deb 安装 CLI

当前发布包：

```bash
dist/himalaya-code_0.1.0-1_amd64.deb
```

安装：

```bash
sudo dpkg -i dist/himalaya-code_0.1.0-1_amd64.deb
sudo apt-get install -f
```

验证：

```bash
himalaya-code --version
himalaya-code --help
```

deb 包会安装：

```text
/usr/bin/himalaya-code
/usr/share/doc/himalaya-code/README.md
/usr/share/himalaya-code/bundled/...
```

如果希望使用 `Himalaya` 作为命令名，可以自行创建软链接：

```bash
sudo ln -sf /usr/bin/himalaya-code /usr/local/bin/Himalaya
```

### 2.2 从源码构建 CLI

源码方式适合开发、测试或需要最新本地改动的场景。

```bash
cd rust
cargo build --release --package rusty-Himalaya-cli
./target/release/Himalaya --version
```

调试构建：

```bash
cd rust
cargo build --workspace
./target/debug/Himalaya --help
```

也可以使用仓库根目录的安装脚本：

```bash
./install.sh --release
```

### 2.3 安装 VS Code 扩展

当前发布包：

```bash
dist/himalaya-code-vscode-0.1.0.vsix
```

命令行安装：

```bash
code --install-extension dist/himalaya-code-vscode-0.1.0.vsix --force
```

图形界面安装：

1. 打开 VS Code。
2. 进入 Extensions 面板。
3. 点击右上角 `...`。
4. 选择 `Install from VSIX...`。
5. 选择 `dist/himalaya-code-vscode-0.1.0.vsix`。

## 3. 首次配置

### 3.1 选择认证方式

Himalaya 支持 Anthropic API Key、OAuth，以及 OpenAI-compatible provider。

Anthropic API Key：

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

OAuth 登录：

```bash
Himalaya login
Himalaya logout
```

如果是 deb 安装，请使用：

```bash
himalaya-code login
himalaya-code logout
```

OpenAI-compatible provider：

```bash
export OPENAI_API_KEY="sk-..."
export OPENAI_BASE_URL="https://api.openai.com/v1"
```

本地 Ollama：

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
unset OPENAI_API_KEY
```

DashScope/Qwen：

```bash
export DASHSCOPE_API_KEY="sk-..."
```

xAI：

```bash
export XAI_API_KEY="xai-..."
```

### 3.2 常用环境变量

| 环境变量 | 作用 |
|---|---|
| `ANTHROPIC_API_KEY` | Anthropic API Key，使用 `x-api-key` 认证 |
| `ANTHROPIC_AUTH_TOKEN` | OAuth/Bearer token，使用 `Authorization: Bearer` 认证 |
| `ANTHROPIC_BASE_URL` | Anthropic-compatible 服务地址 |
| `ANTHROPIC_MODEL` | 默认模型名或别名 |
| `OPENAI_API_KEY` | OpenAI-compatible API Key |
| `OPENAI_BASE_URL` | OpenAI-compatible 服务地址 |
| `DASHSCOPE_API_KEY` | DashScope/Qwen API Key |
| `DASHSCOPE_BASE_URL` | DashScope-compatible 自定义服务地址 |
| `XAI_API_KEY` | xAI API Key |
| `XAI_BASE_URL` | xAI-compatible 自定义服务地址 |
| `Himalaya_CONFIG_HOME` | 用户级配置目录，默认 `~/.Himalaya` |
| `RUSTY_Himalaya_PERMISSION_MODE` | 默认权限模式覆盖 |

注意：`ANTHROPIC_API_KEY` 和 `ANTHROPIC_AUTH_TOKEN` 不能混用。`sk-ant-*` 应放入 `ANTHROPIC_API_KEY`，不要放入 `ANTHROPIC_AUTH_TOKEN`。

### 3.3 运行健康检查

首次使用建议先运行：

```bash
Himalaya doctor
```

deb 安装：

```bash
himalaya-code doctor
```

`doctor` 会检查：

- 认证信息是否存在。
- 配置文件是否能解析。
- 当前目录是否为项目根。
- Git 状态。
- 沙箱状态。
- 系统平台与构建版本。

## 4. CLI 基础用法

### 4.1 交互式 REPL

源码构建：

```bash
./rust/target/release/Himalaya
```

deb 安装：

```bash
himalaya-code
```

进入 REPL 后可以直接输入自然语言任务，也可以使用 slash 命令，例如：

```text
/status
/doctor
/model sonnet
/permissions read-only
/diff
/review
```

### 4.2 单次 prompt

```bash
Himalaya prompt "总结这个仓库的主要模块"
```

也可以使用简写模式：

```bash
Himalaya "总结这个仓库的主要模块"
```

指定模型：

```bash
Himalaya --model sonnet prompt "审查当前 diff"
Himalaya --model openai/gpt-4.1-mini prompt "解释 src/main.rs"
Himalaya --model qwen-plus prompt "生成测试计划"
```

### 4.3 输出格式

文本输出：

```bash
Himalaya --output-format text prompt "总结当前项目"
```

JSON 输出：

```bash
Himalaya --output-format json status
Himalaya --output-format json prompt "输出 JSON 格式总结"
```

流式 JSON 输出，适合集成 VS Code 或外部自动化：

```bash
Himalaya --output-format stream-json prompt "逐步分析这个问题"
```

只输出最终文本，适合管道：

```bash
Himalaya --compact "总结 Cargo.toml" | wc -l
```

### 4.4 模型别名

内置别名：

| 别名 | 解析到 |
|---|---|
| `opus` | `Himalaya-opus-4-6` |
| `sonnet` | `Himalaya-sonnet-4-6` |
| `haiku` | `Himalaya-haiku-4-5-20251213` |

查看或切换模型：

```text
/model
/model sonnet
/model opus
```

也可以用配置文件定义自定义别名，见第 9 节。

### 4.5 权限模式

命令行设置：

```bash
Himalaya --permission-mode read-only prompt "只读分析这个仓库"
Himalaya --permission-mode workspace-write prompt "修复 README 中的问题"
Himalaya --permission-mode danger-full-access prompt "执行完整构建和修复"
```

REPL 中切换：

```text
/permissions
/permissions read-only
/permissions workspace-write
/permissions danger-full-access
```

常用模式说明：

| 模式 | 作用 |
|---|---|
| `prompt` | 默认交互式授权模式，读操作更宽松，写入/高风险操作会请求确认 |
| `read-only` 或 `plan` | 只读和规划模式，不写文件 |
| `workspace-write` 或 `acceptEdits` | 允许修改工作区内文件 |
| `danger-full-access` 或 `bypassPermissions` | 允许更高权限工具访问，需谨慎使用 |

也可使用：

```bash
Himalaya --dangerously-skip-permissions prompt "..."
```

该选项会跳过权限检查，只应在可信环境中使用。

### 4.6 限制可用工具

```bash
Himalaya --allowedTools read,glob prompt "只用读取和 glob 工具总结项目结构"
```

`--allowedTools` 支持重复传入，也支持逗号分隔。适合审计、只读分析、CI 自动化等场景。

### 4.7 顶层命令总览

| 命令 | 作用 |
|---|---|
| `Himalaya help` | 显示帮助，等同于 `--help` |
| `Himalaya version` | 显示版本，等同于 `--version` |
| `Himalaya status` | 显示当前模型、权限、用量、工作区和沙箱状态 |
| `Himalaya sandbox` | 显示沙箱隔离状态 |
| `Himalaya doctor` | 诊断认证、配置、工作区、沙箱和系统信息 |
| `Himalaya prompt TEXT` | 执行一次性 prompt |
| `Himalaya TEXT` | 一次性 prompt 简写 |
| `Himalaya tasks ...` | 管理长程任务、调度器和守护进程 |
| `Himalaya workers ...` | 管理本地 worker 生命周期 |
| `Himalaya routes ...` | 查看路由反馈、生成/应用/回滚路由策略提案 |
| `Himalaya policy ...` | 查看和治理自治策略账本 |
| `Himalaya benchmark ...` | 运行内置复杂编码任务 benchmark |
| `Himalaya maturity-matrix` | 输出工具和 slash command 成熟度矩阵 |
| `Himalaya plan TASK` | 本地生成任务分解，不调用 API |
| `Himalaya bootstrap-plan` | 输出启动规划信息 |
| `Himalaya agents` | 查看可用 agent |
| `Himalaya mcp` | 查看 MCP 配置 |
| `Himalaya skills` | 查看 skill |
| `Himalaya system-prompt` | 输出系统提示词，可指定 cwd/date |
| `Himalaya login` | OAuth 登录 |
| `Himalaya logout` | 登出 |
| `Himalaya init` | 创建项目 `Himalaya.md` |
| `Himalaya export` | 导出会话为 Markdown |

### 4.8 文档生成能力

Himalaya 的内置 `generate_file` 工具可以生成二进制办公文档：

- Word 报告：`docx`
- PowerPoint 演示：`pptx` / `ppt`
- 可打印文档：`pdf`
- Excel 工作簿：`xlsx` / `xls`

简单场景可以让 agent 使用 markdown 内容生成；复杂场景建议使用 `$document-generator` skill，让 agent 构造结构化 `document_spec`。`document_spec` 支持标题、段落、项目符号、表格、LaTeX 公式、图表数据、图片引用、Excel sheet、单元格公式等字段。生成成功后会在同目录写入质量清单，例如 `report.docx.manifest.json`，其中包含表格、公式、图表、图片数量以及潜在质量告警。

示例：

```bash
Himalaya prompt '使用 $document-generator 生成 docs/经营分析报告.docx，包含摘要、指标表、NPV 公式、收入趋势图和质量 manifest'
```

## 5. 会话、记忆与恢复

### 5.1 会话保存位置

REPL 会自动保存会话：

```text
.Himalaya/sessions/<session-id>.jsonl
```

用户目录下也可能存在：

```text
~/.Himalaya/sessions/
```

### 5.2 恢复最近会话

```bash
Himalaya --resume latest
```

查看最近会话状态：

```bash
Himalaya --resume latest /status
```

连续执行多个 resume-safe 命令：

```bash
Himalaya --resume latest /status /diff /export notes.md
```

### 5.3 会话管理命令

REPL 中可用：

```text
/session list
/session switch latest
/session fork experiment-branch
/session delete <session-id> --force
/history
/tokens
/cache
/compact
/clear --confirm
```

### 5.4 项目记忆

项目根目录可放置：

```text
Himalaya.md
```

该文件用于存放项目说明、约定、长期指令和工作习惯。初始化：

```bash
Himalaya init
```

查看加载的记忆：

```text
/memory
```

## 6. 长程任务与自动迭代

Himalaya 支持持久任务目录、任务包、调度器、守护进程、恢复、验证和回放。

### 6.1 查看任务

```bash
Himalaya tasks list
Himalaya tasks show <task-id>
Himalaya tasks status <task-id>
Himalaya tasks report <task-id>
Himalaya tasks review <task-id>
```

任务数据默认位于：

```text
.Himalaya/tasks/
```

### 6.2 执行与恢复任务

```bash
Himalaya tasks resume <task-id> "继续完成剩余修复"
Himalaya tasks execute <task-id>
Himalaya tasks verify <task-id>
Himalaya tasks recover <task-id>
Himalaya tasks cancel <task-id>
```

### 6.3 任务包

任务包适合将复杂工作显式描述为可复现 JSON 文件。

```bash
Himalaya tasks packet create packet.json
Himalaya tasks packet run packet.json
Himalaya tasks packet status <task-id>
```

### 6.4 调度器

```bash
Himalaya tasks scheduler queue
Himalaya tasks scheduler tick
Himalaya tasks scheduler explain <task-id>
Himalaya tasks scheduler run --once
Himalaya tasks scheduler run --max-ticks 5
Himalaya tasks scheduler status
```

### 6.5 守护进程

```bash
Himalaya tasks daemon start --once
Himalaya tasks daemon start --max-ticks 10
Himalaya tasks daemon status
Himalaya tasks daemon logs --limit 50
Himalaya tasks daemon report --limit 20 --max-ticks 3
Himalaya tasks daemon evaluate --limit 20
Himalaya tasks daemon replay --limit 20
Himalaya tasks daemon stop
```

### 6.6 Worker 管理

```bash
Himalaya workers list
Himalaya workers spawn --cwd . -- COMMAND...
Himalaya workers probe <worker-id>
Himalaya workers observe
Himalaya workers ready
Himalaya workers restart
Himalaya workers terminate
Himalaya workers cleanup --stale
Himalaya workers supervise
```

Worker 状态会写入工作区 `.Himalaya/worker-state.json`，便于外部观察和恢复。

## 7. MoE 模型路由与策略治理

Himalaya 当前实现的是任务阶段级的自适应模型路由，而不是 token-level MoE。它会根据规划、编码、验证、总结等阶段选择模型，并收集反馈。

### 7.1 查看路由反馈

```bash
Himalaya routes feedback summary
Himalaya routes summary
Himalaya --output-format json routes summary
```

反馈文件通常位于：

```text
.Himalaya/routes/feedback.json
```

### 7.2 优化、回放、提案和应用

```bash
Himalaya routes list
Himalaya routes optimize
Himalaya routes replay
Himalaya routes propose
Himalaya routes apply <proposal-id> --dry-run
Himalaya routes apply <proposal-id>
Himalaya routes rollback <proposal-id>
```

建议先使用 `--dry-run` 查看影响，再真正应用。

### 7.3 Policy 治理账本

策略治理入口：

```bash
Himalaya policy ledger --limit 20
Himalaya policy review --limit 20 --max-ticks 3
Himalaya policy replay --limit 20
Himalaya policy plan --limit 20 --max-ticks 3
Himalaya policy apply --dry-run --domain routing --proposal-id <id>
Himalaya policy apply --domain routing --proposal-id <id>
Himalaya policy rollback --domain routing --proposal-id <id>
```

可用 domain：

```text
routing
scheduler
memory
recovery
```

治理账本默认位置：

```text
.Himalaya/policy/ledger.jsonl
```

## 8. 常用 Slash 命令

### 8.1 会话与状态

```text
/help
/status
/sandbox
/doctor
/version
/cost
/stats
/history
/tokens
/cache
/compact
/clear --confirm
/resume <session-path>
```

### 8.2 工作区与 Git

```text
/diff
/review
/commit
/pr
/issue
/export notes.md
/workspace <path>
/init
/files
/branch
/release-notes
```

### 8.3 配置与模型

```text
/model
/model sonnet
/permissions
/permissions workspace-write
/config
/config env
/config hooks
/config model
/config plugins
/providers
/memory
```

### 8.4 自动化与诊断

```text
/plan <task>
/tasks list
/test
/lint
/build
/diagnostics <path>
/bughunter
/ultraplan <task>
/teleport <symbol-or-path>
```

### 8.5 扩展系统

```text
/agents list
/agents help
/skills list
/skills install <path>
/skills show <skill>
/skills doctor
/skills help
$skill args
/mcp list
/mcp show <server>
/plugin list
/plugin install <path>
/plugin enable <name>
/plugin disable <name>
/plugin uninstall <id>
/plugin update <id>
```

项目内置的 `$document-generator` skill 面向 Word、PPT、PDF、Excel 生成任务，会优先使用结构化 `document_spec`，适合包含高质量表格、公式、图表数据和质量 manifest 的报告、演示稿与工作簿。

## 9. 配置文件

### 9.1 配置加载顺序

Himalaya 会按优先级发现并合并以下配置：

```text
~/.Himalaya.json
~/.Himalaya/settings.json
<project>/.Himalaya.json
<project>/.Himalaya/settings.json
<project>/.Himalaya/settings.local.json
```

也可以通过 `Himalaya_CONFIG_HOME` 改变用户配置目录。

项目级配置建议放在：

```text
.Himalaya/settings.json
```

本机私有配置建议放在：

```text
.Himalaya/settings.local.json
```

### 9.2 基础配置示例

```json
{
  "model": "sonnet",
  "aliases": {
    "fast": "haiku",
    "smart": "opus",
    "local-coder": "qwen-plus"
  },
  "permissions": {
    "defaultMode": "workspace-write",
    "allow": ["Read", "Glob", "Grep"],
    "ask": ["Edit", "Write"],
    "deny": ["Bash(rm -rf)"]
  },
  "sandbox": {
    "enabled": true,
    "namespaceRestrictions": true,
    "networkIsolation": false,
    "filesystemMode": "workspace-only",
    "allowedMounts": []
  }
}
```

### 9.3 Decisioning 复杂任务分解

`decisioning` 控制任务分析、复杂度阈值、DAG 结构化执行、多角色收敛和安全策略。

```json
{
  "decisioning": {
    "enabled": true,
    "emitEvents": true,
    "maxParallelism": 2,
    "planningComplexityThreshold": 3,
    "structuredExecutionThreshold": 4,
    "teamConvergenceThreshold": 4,
    "safetyPolicy": {
      "reviewThresholdPercent": 35,
      "denyThresholdPercent": 70,
      "protectedTerms": ["delete", "secret", "credential", "network", "shell"]
    }
  }
}
```

字段说明：

| 字段 | 说明 |
|---|---|
| `enabled` | 是否启用 decisioning |
| `emitEvents` | 是否向前端/流输出发出结构化事件 |
| `maxParallelism` | 最大并行节点数 |
| `planningComplexityThreshold` | 复杂度达到该值时启用模型增强规划 |
| `structuredExecutionThreshold` | 复杂度达到该值时走 DAG 结构化执行 |
| `teamConvergenceThreshold` | 节点 effort 达到该值时启用 Architect/Executor/Reviewer 收敛 |

### 9.4 Model Routing 配置

```json
{
  "modelRouting": {
    "enabled": true,
    "minFeedbackSamples": 2,
    "switchFailureThresholdPercent": 50,
    "routes": [
      {
        "phase": "planning",
        "model": "opus",
        "provider": "anthropic",
        "capabilities": ["planning", "long_context"],
        "qualityWeight": 5,
        "latencyWeight": 2,
        "costWeight": 1,
        "maxTokens": 32000
      },
      {
        "phase": "coding",
        "model": "qwen-plus",
        "provider": "dashscope",
        "capabilities": ["coding", "refactor"],
        "qualityWeight": 4,
        "latencyWeight": 3,
        "costWeight": 2
      },
      {
        "phase": "verification",
        "model": "sonnet",
        "capabilities": ["verification", "test_generation"],
        "qualityWeight": 5,
        "latencyWeight": 2,
        "costWeight": 1
      }
    ]
  }
}
```

支持 phase：

```text
planning
coding
verification
summarization
vision
local_fast
```

支持 capability：

```text
planning
coding
refactor
test_generation
verification
summarization
vision
local_fast
cheap
long_context
```

### 9.5 Provider Fallbacks

```json
{
  "providerFallbacks": {
    "primary": "sonnet",
    "fallbacks": ["haiku", "openai/gpt-4.1-mini", "qwen-plus"]
  }
}
```

当主 provider 遇到可重试失败时，系统会按顺序尝试 fallback。

### 9.6 MCP 配置

stdio MCP：

```json
{
  "mcpServers": {
    "filesystem": {
      "type": "stdio",
      "command": "uvx",
      "args": ["mcp-server-filesystem", "."],
      "env": {
        "LOG_LEVEL": "info"
      },
      "toolCallTimeoutMs": 30000
    }
  }
}
```

HTTP MCP：

```json
{
  "mcpServers": {
    "remote-tools": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${TOKEN}"
      }
    }
  }
}
```

查看：

```bash
Himalaya mcp
Himalaya mcp show filesystem
```

REPL：

```text
/mcp list
/mcp show filesystem
```

### 9.7 插件配置

```json
{
  "plugins": {
    "enabled": {
      "sample-hooks": true,
      "tool-guard": false
    },
    "externalDirectories": ["./plugins"],
    "installRoot": "~/.Himalaya/plugins",
    "registryPath": "~/.Himalaya/plugins/registry.json",
    "bundledRoot": "/usr/share/himalaya-code/bundled",
    "maxOutputTokens": 4000
  }
}
```

常用命令：

```bash
Himalaya skills
Himalaya skills show <skill>
Himalaya skills doctor
Himalaya agents
```

Skill 目录结构：

```text
.Himalaya/skills/<name>/SKILL.md
```

`SKILL.md` 可包含 frontmatter：

```markdown
---
name: demo
description: Demo workflow guidance
---

# Demo
Follow this workflow when the user invokes the skill.
```

调用方式：

```text
$demo args
/skills demo args
demo args
```

优先级为项目级 skill 优先，其次是用户配置根，再到用户 home 兼容根；同名低优先级 skill 会在列表中显示为 shadowed。常用兼容根包括 `.Himalaya/skills`、`.omc/skills`、`.agents/skills`、`.codex/skills`、`~/.Himalaya/skills`、`~/.agents/skills`、`~/.config/opencode/skills` 和 legacy `commands`。

REPL：

```text
/plugin list
/plugin install <path>
/plugin enable <name>
/plugin disable <name>
```

### 9.8 Hooks

```json
{
  "hooks": {
    "PreToolUse": ["./scripts/pre-tool.sh"],
    "PostToolUse": ["./scripts/post-tool.sh"],
    "PostToolUseFailure": ["./scripts/tool-failed.sh"]
  }
}
```

Hooks 可用于审计、日志、二次确认、工具调用前后检查等场景。

## 10. VS Code 扩展使用

### 10.1 功能入口

安装扩展后，VS Code 会提供：

- Activity Bar 中的 Himalaya 图标。
- Secondary Sidebar 中的 Himalaya Chat 面板。
- Command Palette 命令。
- VS Code Chat Participant：`@himalaya`。
- 本地聊天历史。
- CLI session 文件发现和恢复。

### 10.2 Command Palette 命令

按 `Ctrl+Shift+P`，搜索 `Himalaya`：

| 命令 | 作用 |
|---|---|
| `Himalaya: Open Chat` | 打开聊天面板 |
| `Himalaya: Ask About Selection` | 将当前选中文本发送给 Himalaya |
| `Himalaya: Configure Model` | 配置模型 |
| `Himalaya: Status` | 执行 CLI `status` |
| `Himalaya: Doctor` | 执行 CLI `doctor` |
| `Himalaya: Login` | 登录 |
| `Himalaya: Logout` | 登出 |
| `Himalaya: Refresh Sessions` | 刷新 session 列表 |
| `Himalaya: Open Session` | 打开 session 文件 |
| `Himalaya: Resume Session` | 恢复 session |
| `Himalaya: Configure Binary Path` | 设置 CLI 路径 |

### 10.3 扩展配置项

在 VS Code settings 中搜索 `himalayaCode`。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `himalayaCode.binaryPath` | 空 | 指定 CLI 绝对路径 |
| `himalayaCode.defaultModel` | `sonnet` | 默认模型 |
| `himalayaCode.defaultPermissionMode` | `read-only` | 默认权限模式 |
| `himalayaCode.dangerousPermissionConfirmationPolicy` | `always` | danger 权限确认策略 |
| `himalayaCode.defaultModelBackend` | `auto` | `auto`、`cloud`、`ollama` |
| `himalayaCode.ollamaBaseUrl` | `http://127.0.0.1:11434/v1` | Ollama OpenAI-compatible 地址 |
| `himalayaCode.allowUntrustedRuns` | `false` | 是否允许未信任工作区运行 |
| `himalayaCode.showOutputChannelByDefault` | `true` | 命令开始时是否显示输出面板 |

如果使用 deb 包安装，可以设置：

```json
{
  "himalayaCode.binaryPath": "/usr/bin/himalaya-code"
}
```

如果使用源码构建，可以设置：

```json
{
  "himalayaCode.binaryPath": "/path/to/Himalaya-main/rust/target/release/Himalaya"
}
```

### 10.4 CLI 自动发现顺序

扩展按以下顺序查找 CLI：

1. `himalayaCode.binaryPath`
2. 工作区 `rust/target/release/Himalaya`
3. 工作区 `rust/target/debug/Himalaya`
4. `PATH` 中的 `Himalaya`
5. 扩展内置 `bin/Himalaya-linux-x64`
6. 下载缓存路径

如果扩展提示找不到 CLI，优先配置 `himalayaCode.binaryPath`。

### 10.5 会话恢复

扩展会扫描：

```text
<workspace>/.Himalaya/sessions/
~/.Himalaya/sessions/
```

可以在侧栏中打开或恢复 session。恢复后，聊天面板会将该 session id 作为 CLI resume target。

### 10.6 工作区信任

未信任工作区默认禁止执行 prompt。可选择：

- 在 VS Code 中信任当前工作区。
- 或设置 `himalayaCode.allowUntrustedRuns: true`。

建议只对可信代码库启用运行权限。

## 11. 常见工作流

### 11.1 初次分析一个项目

```bash
cd /path/to/project
Himalaya doctor
Himalaya init
Himalaya --permission-mode read-only prompt "分析这个项目的目录结构、主要模块和运行方式"
```

### 11.2 修复 bug

```bash
Himalaya --permission-mode workspace-write prompt "复现并修复当前测试失败，修改后运行相关测试"
```

REPL 中：

```text
/status
/diff
/review
/test
/commit
```

### 11.3 只做方案，不改代码

```bash
Himalaya --permission-mode read-only prompt "分析这个需求，给出实现方案，不要修改文件"
```

也可以：

```bash
Himalaya plan "实现用户登录失败重试机制"
```

### 11.4 长程任务自动推进

```bash
Himalaya tasks scheduler queue
Himalaya tasks scheduler run --max-ticks 5
Himalaya tasks daemon report --limit 20 --max-ticks 3
```

### 11.5 生成可审计发布前检查

```bash
Himalaya doctor
Himalaya status
Himalaya routes summary
Himalaya policy ledger --limit 20
```

## 12. 开发者与发布验证

### 12.1 本地 CI Gate

```bash
./scripts/ci-gate.sh
```

该脚本会执行：

- secrets/generated artifact 检查。
- Rust fmt。
- Rust clippy。
- workspace build/tests。
- stream-json 和 output-format contract。
- mock parity diff。
- VS Code regression tests。
- VS Code prepackage checks。

### 12.2 打包 deb

```bash
cd rust
cargo build --release --package rusty-Himalaya-cli
cargo deb --package rusty-Himalaya-cli --no-build
```

输出：

```text
rust/target/debian/himalaya-code_0.1.0-1_amd64.deb
```

检查：

```bash
dpkg-deb --info rust/target/debian/himalaya-code_0.1.0-1_amd64.deb
dpkg-deb --contents rust/target/debian/himalaya-code_0.1.0-1_amd64.deb
```

### 12.3 打包 VSIX

```bash
cd vscode-extension
npm run package:vsix
```

输出：

```text
vscode-extension/himalaya-code-vscode-0.1.0.vsix
```

检查：

```bash
unzip -t vscode-extension/himalaya-code-vscode-0.1.0.vsix
```

### 12.4 确认 VSIX 内嵌 CLI

```bash
unzip -p vscode-extension/himalaya-code-vscode-0.1.0.vsix extension/bin/Himalaya-linux-x64 > /tmp/himalaya-vsix-bin
chmod +x /tmp/himalaya-vsix-bin
/tmp/himalaya-vsix-bin --version
sha256sum /tmp/himalaya-vsix-bin rust/target/release/Himalaya
```

两个 hash 应一致。

## 13. 故障排查

### 13.1 无法认证

先运行：

```bash
Himalaya doctor
```

常见问题：

- `ANTHROPIC_API_KEY` 未设置。
- 将 `sk-ant-*` 错放到了 `ANTHROPIC_AUTH_TOKEN`。
- 使用 OpenAI-compatible provider 时没有设置 `OPENAI_BASE_URL`。
- 使用非 Anthropic 模型但没有正确指定模型前缀。

### 13.2 找不到命令

源码构建出的命令是：

```text
rust/target/release/Himalaya
```

deb 安装后的命令是：

```text
/usr/bin/himalaya-code
```

确认：

```bash
which Himalaya
which himalaya-code
```

### 13.3 VS Code 扩展找不到 CLI

在 VS Code 设置中配置：

```json
{
  "himalayaCode.binaryPath": "/usr/bin/himalaya-code"
}
```

或：

```json
{
  "himalayaCode.binaryPath": "/path/to/rust/target/release/Himalaya"
}
```

然后运行 `Himalaya: Doctor`。

### 13.4 配置文件无法加载

运行：

```bash
Himalaya doctor
Himalaya
```

进入 REPL 后运行：

```text
/config
```

如果已有保存会话，也可以直接执行：

```bash
Himalaya --resume latest /config
```

确认配置文件是 JSON object，而不是数组或空白之外的非法 JSON。常见路径：

```text
~/.Himalaya/settings.json
.Himalaya/settings.json
.Himalaya/settings.local.json
```

### 13.5 权限被拒绝

检查当前权限：

```text
/permissions
```

只读任务使用：

```bash
Himalaya --permission-mode read-only prompt "..."
```

需要改文件时使用：

```bash
Himalaya --permission-mode workspace-write prompt "..."
```

极高权限场景才使用：

```bash
Himalaya --permission-mode danger-full-access prompt "..."
```

### 13.6 沙箱问题

查看：

```bash
Himalaya sandbox
```

配置示例：

```json
{
  "sandbox": {
    "enabled": true,
    "filesystemMode": "workspace-only",
    "allowedMounts": []
  }
}
```

如果任务必须访问工作区外文件，应显式加入 `allowedMounts`，并确保你信任该路径。

### 13.7 本地模型不可用

Ollama：

```bash
ollama serve
ollama list
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
unset OPENAI_API_KEY
Himalaya --model llama3.2 prompt "hello"
```

OpenAI-compatible 服务：

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8000/v1"
export OPENAI_API_KEY="local-dev-token"
Himalaya --model qwen2.5-coder prompt "hello"
```

### 13.8 deb 安装依赖问题

```bash
sudo apt-get install -f
sudo dpkg -i dist/himalaya-code_0.1.0-1_amd64.deb
```

当前 deb 自动依赖主要为：

```text
libc6 (>= 2.34)
```

## 14. 安全建议

- 默认使用 `read-only` 或 `prompt` 模式探索陌生仓库。
- 只有在明确需要修改文件时才使用 `workspace-write`。
- 只有在完全可信环境下才使用 `danger-full-access` 或 `--dangerously-skip-permissions`。
- 不要把真实 API Key 写入仓库内可提交文件。
- 将个人密钥、私有 endpoint、本机路径放入 `.Himalaya/settings.local.json` 或 shell 环境变量。
- 对策略应用命令优先使用 `--dry-run`。
- 发布前运行 `doctor`、`status`、测试和打包完整性检查。

## 15. 快速命令索引

```bash
# 版本与帮助
Himalaya --version
Himalaya --help
Himalaya doctor
Himalaya status
Himalaya sandbox

# 交互与单次 prompt
Himalaya
Himalaya prompt "任务"
Himalaya "任务"
Himalaya --model sonnet prompt "任务"

# 输出格式
Himalaya --output-format json status
Himalaya --output-format stream-json prompt "任务"
Himalaya --compact "任务"

# 会话
Himalaya --resume latest
Himalaya --resume latest /status /diff
Himalaya export conversation.md

# 长程任务
Himalaya tasks list
Himalaya tasks scheduler status
Himalaya tasks daemon status

# 路由与策略
Himalaya routes summary
Himalaya routes propose
Himalaya policy ledger --limit 20
Himalaya policy apply --dry-run --domain routing --proposal-id <id>

# 扩展生态
Himalaya agents
Himalaya mcp
Himalaya skills
```
