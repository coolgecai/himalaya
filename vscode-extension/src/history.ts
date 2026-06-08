import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import type { AttachmentDescriptor, AttachmentSource } from './attachmentPaths';

export type HistoryRole = 'user' | 'assistant' | 'system' | 'error';

export interface HistoryMessage {
  role: HistoryRole;
  text: string;
  createdAt: number;
  attachments?: AttachmentDescriptor[];
  truncated?: boolean;
}

export interface RecoveryEvidence {
  sourceEvent?: string;
  failureClass?: string;
  tool?: string;
  action?: string;
  reason?: string;
  suggestion?: string;
  createdAt: number;
}

export interface ChatHistoryRecord {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  pinned: boolean;
  model?: string;
  modelBackend?: string;
  permissionMode?: string;
  resumeTarget?: string;
  cwd?: string;
  messages: HistoryMessage[];
  recoveryEvidence?: RecoveryEvidence[];
  messageCount?: number;
  recoveryEvidenceCount?: number;
  lastMessagePreview?: string;
  storageVersion?: number;
  bodyStorage?: 'globalStorage';
}

export interface ChatHistorySnapshot {
  records: ChatHistoryRecord[];
  activeRecordId: string | null;
}

const HISTORY_STORAGE_VERSION = 2;
const MAX_HISTORY_RECORDS = 80;
const MAX_MESSAGES_PER_RECORD = 80;
const MAX_MESSAGE_TEXT_CHARS = 24_000;
const MAX_TITLE_CHARS = 140;
const MAX_PREVIEW_CHARS = 180;
const MAX_OPTIONAL_FIELD_CHARS = 2_000;
const MAX_ATTACHMENT_PATH_CHARS = 2_000;
const MAX_ATTACHMENT_DESCRIPTORS_PER_MESSAGE = 8;
const MAX_RECOVERY_EVIDENCE_PER_RECORD = 32;
const MAX_RECOVERY_FIELD_CHARS = 2_000;

interface ChatHistoryRecordBody {
  messages?: HistoryMessage[];
  recoveryEvidence?: RecoveryEvidence[];
}

export class HimalayaHistoryStore {
  private readonly stateKey = 'himalayaCode.history.v1';
  private readonly activeKey = 'himalayaCode.history.active.v1';
  private readonly changeEmitter = new vscode.EventEmitter<void>();

  readonly onDidChange = this.changeEmitter.event;

  constructor(private readonly context: vscode.ExtensionContext) {}

  snapshot(): ChatHistorySnapshot {
    const activeRecordId = this.activeRecordId();
    return {
      records: this.records().map((record) => this.snapshotRecord(record, activeRecordId)),
      activeRecordId
    };
  }

  records(): ChatHistoryRecord[] {
    return this.normalizeRecords(
      this.context.globalState.get<ChatHistoryRecord[]>(this.stateKey, []),
      this.activeRecordId(),
      { includeBodies: true }
    );
  }

  activeRecordId(): string | null {
    return this.context.globalState.get<string | null>(this.activeKey, null);
  }

  async setActiveRecord(id: string | null): Promise<void> {
    await this.context.globalState.update(this.activeKey, id);
    this.changeEmitter.fire();
  }

  async upsert(record: ChatHistoryRecord): Promise<void> {
    const records = this.records();
    const existingIndex = records.findIndex((item) => item.id === record.id);

    if (existingIndex >= 0) {
      records[existingIndex] = {
        ...records[existingIndex],
        ...record,
        updatedAt: Date.now()
      };
    } else {
      records.push(record);
    }

    await this.persist(records, record.id);
    await this.setActiveRecord(record.id);
  }

  async appendMessage(recordId: string, message: HistoryMessage): Promise<void> {
    const records = this.records();
    const record = records.find((item) => item.id === recordId);
    if (!record) {
      return;
    }

    record.messages.push(message);
    record.updatedAt = Date.now();
    await this.persist(records, recordId);
    this.changeEmitter.fire();
  }

  async replaceAssistantTail(recordId: string, text: string): Promise<void> {
    const records = this.records();
    const record = records.find((item) => item.id === recordId);
    if (!record) {
      return;
    }

    const tail = record.messages[record.messages.length - 1];
    if (tail && tail.role === 'assistant') {
      tail.text = text;
    }

    record.updatedAt = Date.now();
    await this.persist(records, recordId);
    this.changeEmitter.fire();
  }

  async appendRecoveryEvidence(recordId: string, evidence: RecoveryEvidence): Promise<void> {
    const records = this.records();
    const record = records.find((item) => item.id === recordId);
    if (!record) {
      return;
    }

    record.recoveryEvidence = [...(record.recoveryEvidence ?? []), evidence];
    record.updatedAt = Date.now();
    await this.persist(records, recordId);
    this.changeEmitter.fire();
  }

  async rename(recordId: string, title: string): Promise<void> {
    const records = this.records();
    const record = records.find((item) => item.id === recordId);
    if (!record) {
      return;
    }

    record.title = title.trim() || record.title;
    record.updatedAt = Date.now();
    await this.persist(records, recordId);
    this.changeEmitter.fire();
  }

  async pin(recordId: string, pinned: boolean): Promise<void> {
    const records = this.records();
    const record = records.find((item) => item.id === recordId);
    if (!record) {
      return;
    }

    record.pinned = pinned;
    record.updatedAt = Date.now();
    await this.persist(records, recordId);
    this.changeEmitter.fire();
  }

  async remove(recordId: string): Promise<void> {
    const activeId = this.activeRecordId();
    const records = this.records().filter((item) => item.id !== recordId);
    this.deleteRecordBody(recordId);
    await this.persist(records, activeId === recordId ? null : activeId);

    if (activeId === recordId) {
      await this.setActiveRecord(null);
    } else {
      this.changeEmitter.fire();
    }
  }

  async createDraft(input: { title: string; model?: string; modelBackend?: string; permissionMode?: string; resumeTarget?: string; cwd?: string }): Promise<ChatHistoryRecord> {
    const record: ChatHistoryRecord = {
      id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      title: input.title,
      createdAt: Date.now(),
      updatedAt: Date.now(),
      pinned: false,
      model: input.model,
      modelBackend: input.modelBackend,
      permissionMode: input.permissionMode,
      resumeTarget: input.resumeTarget,
      cwd: input.cwd,
      messages: []
    };

    await this.upsert(record);
    return record;
  }

  private snapshotRecord(record: ChatHistoryRecord, activeRecordId: string | null): ChatHistoryRecord {
    if (record.id === activeRecordId) {
      return record;
    }

    return {
      ...record,
      messages: [],
      recoveryEvidence: [],
    };
  }

  private async persist(records: ChatHistoryRecord[], activeRecordId: string | null): Promise<void> {
    const normalized = this.normalizeRecords(records, activeRecordId, { includeBodies: true });
    for (const record of normalized) {
      this.writeRecordBody(record.id, {
        messages: record.messages,
        recoveryEvidence: record.recoveryEvidence,
      });
    }

    const indexRecords = normalized.map((record) => ({
      ...record,
      messages: [],
      recoveryEvidence: [],
      bodyStorage: 'globalStorage' as const,
    }));
    await this.context.globalState.update(this.stateKey, indexRecords);
    if (activeRecordId && !normalized.some((record) => record.id === activeRecordId)) {
      await this.context.globalState.update(this.activeKey, null);
    }
  }

  private normalizeRecords(records: ChatHistoryRecord[], activeRecordId: string | null, options: { includeBodies: boolean }): ChatHistoryRecord[] {
    const normalized = (Array.isArray(records) ? records : [])
      .map((record) => this.normalizeRecord(record, options.includeBodies))
      .filter((record): record is ChatHistoryRecord => Boolean(record));

    normalized.sort(compareRecords);
    const capped = normalized.slice(0, MAX_HISTORY_RECORDS);
    if (activeRecordId && !capped.some((record) => record.id === activeRecordId)) {
      const activeRecord = normalized.find((record) => record.id === activeRecordId);
      if (activeRecord) {
        capped.splice(Math.max(0, MAX_HISTORY_RECORDS - 1), 1, activeRecord);
        capped.sort(compareRecords);
      }
    }

    return capped;
  }

  private normalizeRecord(record: ChatHistoryRecord | undefined, includeBody: boolean): ChatHistoryRecord | null {
    if (!record || typeof record.id !== 'string' || !record.id.trim()) {
      return null;
    }

    const body: ChatHistoryRecordBody = includeBody ? this.readRecordBody(record.id) : {};
    const rawMessages = Array.isArray(record.messages) && record.messages.length > 0
      ? record.messages
      : body.messages;
    const rawEvidence = Array.isArray(record.recoveryEvidence) && record.recoveryEvidence.length > 0
      ? record.recoveryEvidence
      : body.recoveryEvidence;
    const messages = Array.isArray(rawMessages) ? rawMessages : [];
    const normalizedMessages = messages
      .slice(-MAX_MESSAGES_PER_RECORD)
      .map((message) => normalizeMessage(message))
      .filter((message): message is HistoryMessage => Boolean(message));
    const evidence = Array.isArray(rawEvidence) ? rawEvidence : [];
    const normalizedEvidence = evidence
      .slice(-MAX_RECOVERY_EVIDENCE_PER_RECORD)
      .map((item) => normalizeRecoveryEvidence(item))
      .filter((item): item is RecoveryEvidence => Boolean(item));
    const lastMessage = [...normalizedMessages].reverse().find((message) => message.text.trim()) ?? normalizedMessages[normalizedMessages.length - 1];

    return {
      id: trimText(record.id, MAX_OPTIONAL_FIELD_CHARS).text,
      title: trimText(record.title || 'Untitled', MAX_TITLE_CHARS).text || 'Untitled',
      createdAt: finiteNumber(record.createdAt, Date.now()),
      updatedAt: finiteNumber(record.updatedAt, finiteNumber(record.createdAt, Date.now())),
      pinned: Boolean(record.pinned),
      model: normalizeOptionalString(record.model),
      modelBackend: normalizeOptionalString(record.modelBackend),
      permissionMode: normalizeOptionalString(record.permissionMode),
      resumeTarget: normalizeOptionalString(record.resumeTarget),
      cwd: normalizeOptionalString(record.cwd),
      messages: normalizedMessages,
      recoveryEvidence: normalizedEvidence,
      messageCount: messages.length,
      recoveryEvidenceCount: evidence.length,
      lastMessagePreview: previewText(lastMessage?.text ?? record.lastMessagePreview ?? ''),
      storageVersion: HISTORY_STORAGE_VERSION,
      bodyStorage: 'globalStorage',
    };
  }

  private recordBodyPath(recordId: string): string {
    return path.join(this.context.globalStorageUri.fsPath, 'history', `${safeRecordId(recordId)}.json`);
  }

  private readRecordBody(recordId: string): ChatHistoryRecordBody {
    try {
      const filePath = this.recordBodyPath(recordId);
      if (!fs.existsSync(filePath)) {
        return {};
      }
      const parsed = JSON.parse(fs.readFileSync(filePath, 'utf8')) as ChatHistoryRecordBody;
      return parsed && typeof parsed === 'object' ? parsed : {};
    } catch {
      return {};
    }
  }

  private writeRecordBody(recordId: string, body: ChatHistoryRecordBody): void {
    try {
      const filePath = this.recordBodyPath(recordId);
      fs.mkdirSync(path.dirname(filePath), { recursive: true });
      fs.writeFileSync(filePath, JSON.stringify(body), 'utf8');
    } catch {
      // History indexing should not block chat execution if disk persistence fails.
    }
  }

  private deleteRecordBody(recordId: string): void {
    try {
      fs.rmSync(this.recordBodyPath(recordId), { force: true });
    } catch {
      // Best-effort cleanup: the history index remains authoritative.
    }
  }
}

function normalizeMessage(message: HistoryMessage | undefined): HistoryMessage | null {
  if (!message || !isHistoryRole(message.role)) {
    return null;
  }

  const trimmed = trimText(message.text, MAX_MESSAGE_TEXT_CHARS);
  const attachments = Array.isArray(message.attachments)
    ? message.attachments
      .slice(0, MAX_ATTACHMENT_DESCRIPTORS_PER_MESSAGE)
      .map((attachment) => normalizeAttachment(attachment))
      .filter((attachment): attachment is AttachmentDescriptor => Boolean(attachment))
    : undefined;

  return {
    role: message.role,
    text: trimmed.text,
    createdAt: finiteNumber(message.createdAt, Date.now()),
    attachments: attachments && attachments.length > 0 ? attachments : undefined,
    truncated: trimmed.truncated || message.truncated || undefined,
  };
}

function normalizeRecoveryEvidence(evidence: RecoveryEvidence | undefined): RecoveryEvidence | null {
  if (!evidence) {
    return null;
  }

  return {
    sourceEvent: normalizeOptionalString(evidence.sourceEvent, MAX_RECOVERY_FIELD_CHARS),
    failureClass: normalizeOptionalString(evidence.failureClass, MAX_RECOVERY_FIELD_CHARS),
    tool: normalizeOptionalString(evidence.tool, MAX_RECOVERY_FIELD_CHARS),
    action: normalizeOptionalString(evidence.action, MAX_RECOVERY_FIELD_CHARS),
    reason: normalizeOptionalString(evidence.reason, MAX_RECOVERY_FIELD_CHARS),
    suggestion: normalizeOptionalString(evidence.suggestion, MAX_RECOVERY_FIELD_CHARS),
    createdAt: finiteNumber(evidence.createdAt, Date.now()),
  };
}

function normalizeAttachment(attachment: AttachmentDescriptor | undefined): AttachmentDescriptor | null {
  if (!attachment || typeof attachment !== 'object') {
    return null;
  }

  return {
    path: trimText(String(attachment.path || ''), MAX_ATTACHMENT_PATH_CHARS).text,
    displayName: trimText(String(attachment.displayName || ''), MAX_TITLE_CHARS).text,
    sizeBytes: finiteNumber(attachment.sizeBytes, 0),
    modifiedMs: finiteNumber(attachment.modifiedMs, 0),
    extension: trimText(String(attachment.extension || ''), 32).text,
    source: normalizeAttachmentSource(attachment.source),
    mediaKind: normalizeMediaKind(attachment.mediaKind),
  };
}

function normalizeOptionalString(value: unknown, limit = MAX_OPTIONAL_FIELD_CHARS): string | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const text = trimText(value, limit).text.trim();
  return text || undefined;
}

function trimText(value: unknown, limit: number): { text: string; truncated: boolean } {
  const text = String(value ?? '');
  if (text.length <= limit) {
    return { text, truncated: false };
  }

  const marker = `\n\n[History truncated ${text.length - limit} characters]\n\n`;
  const head = Math.max(0, Math.floor((limit - marker.length) * 0.65));
  const tail = Math.max(0, limit - marker.length - head);
  return {
    text: `${text.slice(0, head)}${marker}${text.slice(text.length - tail)}`,
    truncated: true,
  };
}

function previewText(value: string): string {
  return trimText(value.replace(/\s+/gu, ' ').trim(), MAX_PREVIEW_CHARS).text;
}

function finiteNumber(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function isHistoryRole(value: unknown): value is HistoryRole {
  return value === 'user' || value === 'assistant' || value === 'system' || value === 'error';
}

function normalizeAttachmentSource(value: unknown): AttachmentSource {
  return value === 'picker' || value === 'reference' || value === 'typed' ? value : 'typed';
}

function normalizeMediaKind(value: unknown): AttachmentDescriptor['mediaKind'] {
  return value === 'image' || value === 'pdf' || value === 'text' || value === 'binary' ? value : 'binary';
}

function safeRecordId(recordId: string): string {
  return recordId.replace(/[^a-z0-9._-]/giu, '_').slice(0, 180) || 'record';
}

function compareRecords(left: ChatHistoryRecord, right: ChatHistoryRecord): number {
  if (left.pinned !== right.pinned) {
    return left.pinned ? -1 : 1;
  }

  return right.updatedAt - left.updatedAt;
}
