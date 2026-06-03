export const STREAM_PROTOCOL_VERSION = 1;

// Stream-json v1 is NDJSON on stdout; Rust emits protocol_version, while schema_version remains a legacy VS Code fallback.
export type KnownStreamEventType =
  | 'text_delta'
  | 'tool_use'
  | 'tool_result'
  | 'done'
  | 'session_meta'
  | 'message_start'
  | 'message_stop'
  | 'command_match'
  | 'tool_match'
  | 'permission_denial'
  | 'permission_request'
  | 'reasoning_step'
  | 'decisioning_event'
  | 'plan_execution_event'
  | 'task_ledger_event'
  | 'model_route_event'
  | 'team_execution_event'
  | 'recovery_event'
  | 'recovery_action_event'
  | 'task_execution_event'
  | 'local_command'
  | 'recovery_suggestion'
  | 'task_list'
  | 'task_show'
  | 'task_execution'
  | 'task_recovery'
  | 'task_verification'
  | 'task_node_retry'
  | 'task_node_verification'
  | 'task_compacted'
  | 'task_cancelled'
  | 'task_packet_create'
  | 'task_packet_run'
  | 'task_packet_status'
  | 'task_scheduler_tick'
  | 'task_scheduler_queue'
  | 'task_scheduler_daemon_run'
  | 'task_scheduler_daemon_status'
  | 'task_scheduler_daemon_logs'
  | 'route_feedback_summary'
  | 'benchmark_suite'
  | 'benchmark_task'
  | 'benchmark_run'
  | 'worker_list'
  | 'worker_create'
  | 'worker_spawn'
  | 'worker_probe'
  | 'worker_observe'
  | 'worker_ready'
  | 'worker_resolve_trust'
  | 'worker_prompt'
  | 'worker_complete'
  | 'worker_restart'
  | 'worker_terminate'
  | 'worker_supervisor_tick'
  | 'error'
  | 'context_event';


export type StreamEvent = {
  type: string;
  reasoning_step?: ReasoningStep;
  decisioning_event?: DecisioningEvent;
  plan_execution_event?: PlanExecutionEvent;
  task_ledger_event?: TaskLedgerEvent;
  model_route_event?: ModelRouteDecision;
  team_execution_event?: TeamExecutionEvent;
  recovery_event?: RecoveryEvent;
  recovery_action_event?: RecoveryActionExecution;
  task_execution_event?: TaskExecutionOutcome;
  task?: unknown;
  tasks?: unknown[];
  ledger?: TaskLedgerEvent[];
  outcome?: TaskExecutionOutcome;
  execution?: RecoveryActionExecution;
  result?: unknown;
  text?: string;
  id?: string;
  name?: string;
  tool?: string;
  reason?: string;
  action?: string;
  suggestion?: string;
  source_event?: string;
  failure_class?: string;
  error?: string;
  input?: unknown;
  output?: string;
  is_error?: boolean;
  iterations?: number;
  session_id?: string;
  session_path?: string;
  model?: string;
  protocol_version?: number;
  schema_version?: number;
  [key: string]: unknown;
};
 

export type ReasoningStepType = 'analysis' | 'planning' | 'reflection' | 'decision' | 'redacted_thinking';

export type ReasoningStep = {
  step_type: ReasoningStepType;
  content?: string;
  confidence?: number;
  signature?: string;
  plan?: string;
  steps?: string[];
  critique?: string;
  adjustment?: string;
  choice?: string;
  reasoning?: string;
  data?: unknown;
};

export type DecisioningEventKind =
  | 'tool_selection'
  | 'task_decomposition'
  | 'parallelism_decision'
  | 'safety_assessment'
  | 'plan_adjustment';

export type DecisioningRiskLevel = 'low' | 'medium' | 'high';

export type DecisioningToolScore = {
  name: string;
  score: number;
  success_rate: number;
  latency_ms: number;
  cost: number;
  parallelizable: boolean;
  capabilities: string[];
  selected: boolean;
};

export type DecisioningPlanNodeKind = 'task' | 'step';

export type DecisioningPlanNode = {
  kind: DecisioningPlanNodeKind;
  id: string;
  title: string;
  parallelizable: boolean;
  estimated_effort: number;
  candidate_tools: string[];
  notes: string[];
  children: DecisioningPlanNode[];
};

export type DecisioningPlanDagEdgeKind = 'contains' | 'depends_on';

export type DecisioningPlanDagNode = {
  kind: DecisioningPlanNodeKind;
  id: string;
  title: string;
  parallelizable: boolean;
  estimated_effort: number;
  candidate_tools: string[];
  notes: string[];
};

export type DecisioningPlanDagEdge = {
  from: string;
  to: string;
  kind: DecisioningPlanDagEdgeKind;
};

export type DecisioningPlanDag = {
  task_id: string;
  root_id: string;
  nodes: DecisioningPlanDagNode[];
  edges: DecisioningPlanDagEdge[];
};

export type DecisioningEvent = {
  kind: DecisioningEventKind;
  title: string;
  summary: string;
  task_id?: string;
  confidence?: number;
  risk_score?: number;
  risk_level?: DecisioningRiskLevel;
  selected_tools?: string[];
  parallelizable?: boolean;
  action?: 'allow' | 'review' | 'deny';
  tool_scores?: DecisioningToolScore[];
  plan_tree?: DecisioningPlanNode;
  plan_dag?: DecisioningPlanDag;
  details?: string[];
};

export type TaskResumeCursor = {
  node_id?: string;
  completed_nodes: string[];
  resumable_nodes: string[];
  updated_at: number;
};

export type NodeVerificationGate = {
  node_id: string;
  command: string;
  required: boolean;
  last_result?: string;
};
export type PlanNodeStatus = 'pending' | 'ready' | 'running' | 'succeeded' | 'failed' | 'skipped';

export type PlanExecutionEventKind =
  | 'node_ready'
  | 'node_started'
  | 'node_succeeded'
  | 'node_failed'
  | 'node_skipped'
  | 'execution_blocked'
  | 'execution_finished';

export type PlanExecutionEvent = {
  seq: number;
  task_id: string;
  node_id: string;
  kind: PlanExecutionEventKind;
  status: PlanNodeStatus;
  message?: string;
  attempt?: number;
  dependencies?: string[];
  blocking_reason?: string;
  verification_gate?: string;
};

export type TaskStatus =
  | 'created'
  | 'planning'
  | 'running'
  | 'waiting_for_permission'
  | 'waiting_for_verification'
  | 'recovering'
  | 'blocked'
  | 'completed'
  | 'failed'
  | 'stopped'
  | 'cancelled';

export type TaskLedgerEvent = {
  seq: number;
  task_id: string;
  event: string;
  status: TaskStatus;
  message?: string;
  timestamp?: number;
};

export type ModelRoutePhase = 'planning' | 'coding' | 'verification' | 'summarization' | 'vision' | 'local_fast';

export type ModelRouteDecision = {
  phase: ModelRoutePhase;
  model: string;
  provider?: string;
  reason: string;
  confidence?: number;
  fallback_model?: string;
};

export type TeamRole = 'planner' | 'implementer' | 'verifier' | 'reviewer' | 'summarizer';

export type TeamExecutionEventKind =
  | 'task_assigned'
  | 'node_started'
  | 'node_finished'
  | 'verification_failed'
  | 'verification_passed'
  | 'summary_ready';

export type TeamExecutionEvent = {
  seq: number;
  team_id: string;
  task_id: string;
  role: TeamRole;
  kind: TeamExecutionEventKind;
  model_route?: ModelRouteDecision;
  message?: string;
};

export type RecoveryActionRisk = 'safe' | 'needs_workspace_write' | 'needs_danger_full_access' | 'needs_human';

export type RecoveryActionKind =
  | 'rerun_verification'
  | 'retry_node'
  | 'request_permission'
  | 'switch_model'
  | 'restart_plugin'
  | 'retry_mcp_handshake'
  | 'mark_blocked'
  | 'escalate';

export type RecoveryAction = {
  kind: RecoveryActionKind;
  scenario: string;
  risk: RecoveryActionRisk;
  node_id?: string;
  message: string;
};

export type RecoveryActionResult = {
  action: RecoveryAction;
  executed: boolean;
  blocked: boolean;
  reason: string;
};

export type RecoveryActionExecution = {
  task_id: string;
  results: RecoveryActionResult[];
};

export type TaskExecutionStepKind = 'resume_node' | 'run_node_verification' | 'complete_task' | 'blocked';

export type TaskExecutionStep = {
  task_id: string;
  node_id?: string;
  kind: TaskExecutionStepKind;
  message: string;
};

export type TaskExecutionOutcome = {
  task_id: string;
  steps: TaskExecutionStep[];
  completed: boolean;
  blocked: boolean;
  message: string;
};

export type RouteFeedbackSummary = {
  phase: ModelRoutePhase;
  model: string;
  total: number;
  failures: number;
  recovery_triggered: number;
  success_rate: number;
};

export type RecoveryResult =
  | { recovered: { steps_taken: number } }
  | { partial_recovery: { recovered: unknown[]; remaining: unknown[] } }
  | { escalation_required: { reason: string } }
  | Record<string, unknown>;

export type RecoveryRecipe = {
  scenario?: string;
  steps?: unknown[];
  max_attempts?: number;
  escalation_policy?: string;
  [key: string]: unknown;
};

export type RecoveryEvent =
  | 'recovery_succeeded'
  | 'recovery_failed'
  | 'escalated'
  | {
      recovery_attempted: {
        scenario?: string;
        recipe?: RecoveryRecipe;
        result?: RecoveryResult;
        [key: string]: unknown;
      };
    }
  | { recovery_succeeded: null }
  | { recovery_failed: null }
  | { escalated: null }
  | Record<string, unknown>;

export type ParsedStreamEvent =
  | { ok: true; event: StreamEvent }
  | { ok: false; reason: 'not-json' | 'invalid-shape' };

export const KNOWN_STREAM_EVENT_TYPES: readonly KnownStreamEventType[] = [
  'text_delta',
  'tool_use',
  'tool_result',
  'done',
  'session_meta',
  'message_start',
  'message_stop',
  'command_match',
  'tool_match',
  'permission_denial',
  'permission_request',
  'reasoning_step',
  'decisioning_event',
  'plan_execution_event',
  'task_ledger_event',
  'model_route_event',
  'team_execution_event',
  'recovery_event',
  'recovery_action_event',
  'task_execution_event',
  'local_command',
  'recovery_suggestion',
  'task_list',
  'task_show',
  'task_execution',
  'task_recovery',
  'task_verification',
  'task_node_retry',
  'task_node_verification',
  'task_compacted',
  'task_cancelled',
  'task_packet_create',
  'task_packet_run',
  'task_packet_status',
  'task_scheduler_tick',
  'task_scheduler_queue',
  'task_scheduler_daemon_run',
  'task_scheduler_daemon_status',
  'task_scheduler_daemon_logs',
  'route_feedback_summary',
  'benchmark_suite',
  'benchmark_task',
  'benchmark_run',
  'worker_list',
  'worker_create',
  'worker_spawn',
  'worker_probe',
  'worker_observe',
  'worker_ready',
  'worker_resolve_trust',
  'worker_prompt',
  'worker_complete',
  'worker_restart',
  'worker_terminate',
  'worker_supervisor_tick',
  'error',
  'context_event',
];

const KNOWN_EVENT_TYPES: ReadonlySet<KnownStreamEventType> = new Set(KNOWN_STREAM_EVENT_TYPES);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function hasString(record: Record<string, unknown>, key: string): boolean {
  return typeof record[key] === 'string';
}

function hasNumber(record: Record<string, unknown>, key: string): boolean {
  return typeof record[key] === 'number' && Number.isFinite(record[key]);
}

function hasBoolean(record: Record<string, unknown>, key: string): boolean {
  return typeof record[key] === 'boolean';
}

function hasArray(record: Record<string, unknown>, key: string): boolean {
  return Array.isArray(record[key]);
}

function hasObject(record: Record<string, unknown>, key: string): boolean {
  return isRecord(record[key]);
}

function hasOptionalString(record: Record<string, unknown>, key: string): boolean {
  return !(key in record) || typeof record[key] === 'string';
}

function hasOptionalNumber(record: Record<string, unknown>, key: string): boolean {
  return !(key in record) || (typeof record[key] === 'number' && Number.isFinite(record[key]));
}

function hasOptionalStringArray(record: Record<string, unknown>, key: string): boolean {
  return !(key in record) || (Array.isArray(record[key]) && record[key].every((value) => typeof value === 'string'));
}

function validateReasoningStep(event: Record<string, unknown>): boolean {
  if (!hasObject(event, 'reasoning_step')) {
    return false;
  }
  const step = event.reasoning_step as Record<string, unknown>;
  if (!hasString(step, 'step_type')) {
    return false;
  }
  if (step.step_type === 'redacted_thinking' && !('data' in step)) {
    return false;
  }
  return hasOptionalString(step, 'content') &&
    hasOptionalString(step, 'signature') &&
    hasOptionalNumber(step, 'confidence') &&
    hasOptionalString(step, 'plan') &&
    hasOptionalString(step, 'critique') &&
    hasOptionalString(step, 'adjustment') &&
    hasOptionalString(step, 'choice') &&
    hasOptionalString(step, 'reasoning') &&
    hasOptionalStringArray(step, 'steps');
}

function validateDecisioningEvent(event: Record<string, unknown>): boolean {
  if (!hasObject(event, 'decisioning_event')) {
    return false;
  }
  const decision = event.decisioning_event as Record<string, unknown>;
  return hasString(decision, 'kind') &&
    hasString(decision, 'title') &&
    hasString(decision, 'summary') &&
    hasOptionalString(decision, 'task_id') &&
    hasOptionalNumber(decision, 'confidence') &&
    hasOptionalNumber(decision, 'risk_score') &&
    hasOptionalString(decision, 'risk_level') &&
    hasOptionalString(decision, 'action') &&
    hasOptionalStringArray(decision, 'selected_tools');
}

function validateStreamEventShape(event: Record<string, unknown>): boolean {
  switch (event.type) {
    case 'message_start':
    case 'message_stop':
      return true;
    case 'text_delta':
      return hasString(event, 'text');
    case 'tool_use':
      return hasString(event, 'id') && hasString(event, 'name') && 'input' in event;
    case 'tool_result':
      return hasString(event, 'name') && hasString(event, 'output') && hasBoolean(event, 'is_error');
    case 'done':
      return hasNumber(event, 'iterations');
    case 'session_meta':
      return hasString(event, 'session_id') && hasString(event, 'session_path') && hasString(event, 'model');
    case 'command_match':
      return hasString(event, 'command');
    case 'tool_match':
      return hasString(event, 'tool');
    case 'permission_denial':
      return hasString(event, 'tool') && hasString(event, 'reason');
    case 'permission_request':
      return hasString(event, 'tool') && 'input' in event && hasString(event, 'current_mode') && hasString(event, 'required_mode') && hasString(event, 'reason');
    case 'reasoning_step':
      return validateReasoningStep(event);
    case 'decisioning_event':
      return validateDecisioningEvent(event);
    case 'plan_execution_event': {
      if (!hasObject(event, 'plan_execution_event')) { return false; }
      const plan = event.plan_execution_event as Record<string, unknown>;
      return hasNumber(plan, 'seq') && hasString(plan, 'task_id') && hasString(plan, 'node_id') && hasString(plan, 'kind') && hasString(plan, 'status');
    }
    case 'task_ledger_event': {
      if (!hasObject(event, 'task_ledger_event')) { return false; }
      const ledger = event.task_ledger_event as Record<string, unknown>;
      return hasNumber(ledger, 'seq') && hasString(ledger, 'task_id') && hasString(ledger, 'event') && hasString(ledger, 'status') && hasOptionalNumber(ledger, 'timestamp') && hasOptionalString(ledger, 'message');
    }
    case 'model_route_event': {
      if (!hasObject(event, 'model_route_event')) { return false; }
      const route = event.model_route_event as Record<string, unknown>;
      return hasString(route, 'phase') && hasString(route, 'model') && hasString(route, 'reason') && hasOptionalString(route, 'provider') && hasOptionalNumber(route, 'confidence') && hasOptionalString(route, 'fallback_model');
    }
    case 'team_execution_event': {
      if (!hasObject(event, 'team_execution_event')) { return false; }
      const team = event.team_execution_event as Record<string, unknown>;
      return hasNumber(team, 'seq') && hasString(team, 'team_id') && hasString(team, 'task_id') && hasString(team, 'role') && hasString(team, 'kind');
    }
    case 'recovery_event':
      return typeof event.recovery_event === 'string' || isRecord(event.recovery_event);
    case 'recovery_action_event': {
      if (!hasObject(event, 'recovery_action_event')) { return false; }
      const recovery = event.recovery_action_event as Record<string, unknown>;
      return hasString(recovery, 'task_id') && hasArray(recovery, 'results');
    }
    case 'task_execution_event': {
      if (!hasObject(event, 'task_execution_event')) { return false; }
      const outcome = event.task_execution_event as Record<string, unknown>;
      return hasString(outcome, 'task_id') && hasArray(outcome, 'steps') && hasBoolean(outcome, 'completed') && hasBoolean(outcome, 'blocked') && hasString(outcome, 'message');
    }
    case 'local_command':
      return hasString(event, 'command') && hasString(event, 'status') && hasString(event, 'summary');
    case 'recovery_suggestion':
      return hasString(event, 'source_event') && hasString(event, 'failure_class') && hasString(event, 'tool') && hasString(event, 'reason') && hasString(event, 'action') && hasString(event, 'suggestion');
    case 'task_list':
      return hasArray(event, 'tasks');
    case 'task_show':
      return hasObject(event, 'task') && hasArray(event, 'ledger');
    case 'task_execution':
      return hasObject(event, 'outcome');
    case 'task_recovery':
      return hasObject(event, 'execution');
    case 'task_verification':
      return hasObject(event, 'result');
    case 'task_node_retry':
      return hasObject(event, 'task') && hasString(event, 'node_id');
    case 'task_node_verification':
      return hasObject(event, 'task') && hasString(event, 'node_id') && hasString(event, 'command');
    case 'task_compacted':
      return hasObject(event, 'task') && hasNumber(event, 'keep_last');
    case 'task_cancelled':
      return hasObject(event, 'task');
    case 'task_packet_create':
    case 'task_packet_run':
    case 'task_packet_status':
      return hasObject(event, 'task') && hasArray(event, 'ledger') && hasObject(event, 'verification_handoff');
    case 'task_scheduler_tick':
      return hasObject(event, 'tick');
    case 'task_scheduler_queue':
      return hasArray(event, 'queue');
    case 'task_scheduler_daemon_run':
      return hasArray(event, 'runs') && ('state' in event);
    case 'task_scheduler_daemon_status':
      return 'state' in event && hasString(event, 'state_path') && hasString(event, 'events_path');
    case 'task_scheduler_daemon_logs':
      return hasArray(event, 'events') && hasString(event, 'events_path');
    case 'route_feedback_summary':
      return hasArray(event, 'summaries') && hasNumber(event, 'feedback_count');
    case 'benchmark_suite':
      return hasString(event, 'suite_id') && hasString(event, 'version') && hasArray(event, 'tasks');
    case 'benchmark_task':
      return hasObject(event, 'task');
    case 'benchmark_run':
      return hasObject(event, 'run');
    case 'worker_list':
      return hasArray(event, 'workers');
    case 'worker_create':
    case 'worker_spawn':
    case 'worker_probe':
    case 'worker_observe':
    case 'worker_resolve_trust':
    case 'worker_prompt':
    case 'worker_complete':
    case 'worker_restart':
    case 'worker_terminate':
      return hasObject(event, 'worker');
    case 'worker_ready':
      return hasObject(event, 'ready');
    case 'worker_supervisor_tick':
      return hasObject(event, 'tick');
    case 'error':
      return hasString(event, 'error');
    case 'context_event':
      return hasString(event, 'kind');
    default:
      return true;
  }
}

export function parseStreamEventLine(line: string): ParsedStreamEvent {
  try {
    const parsed = JSON.parse(line) as { type?: unknown; [key: string]: unknown };
    if (!parsed || typeof parsed !== 'object' || typeof parsed.type !== 'string') {
      return { ok: false, reason: 'invalid-shape' };
    }
    if (
      ('protocol_version' in parsed && typeof parsed.protocol_version !== 'number') ||
      ('schema_version' in parsed && typeof parsed.schema_version !== 'number')
    ) {
      return { ok: false, reason: 'invalid-shape' };
    }
    if (!validateStreamEventShape(parsed)) {
      return { ok: false, reason: 'invalid-shape' };
    }
    return { ok: true, event: parsed as StreamEvent };
  } catch {
    return { ok: false, reason: 'not-json' };
  }
}

export function isKnownStreamEventType(type: string): type is KnownStreamEventType {
  return KNOWN_EVENT_TYPES.has(type as KnownStreamEventType);
}

export function readProtocolVersion(event: StreamEvent): number | undefined {
  const schema = event.schema_version;
  const protocol = event.protocol_version;
  if (typeof protocol === 'number') {
    return protocol;
  }
  if (typeof schema === 'number') {
    return schema;
  }
  return undefined;
}

export function classifyProtocolVersion(
  version: number | undefined,
  expected: number = STREAM_PROTOCOL_VERSION,
): 'missing' | 'match' | 'mismatch' {
  if (version === undefined) {
    return 'missing';
  }
  return version === expected ? 'match' : 'mismatch';
}
