export const STREAM_PROTOCOL_VERSION = 1;

export type KnownStreamEventType =
  | 'text_delta'
  | 'tool_use'
  | 'tool_result'
  | 'done'
  | 'message_start'
  | 'message_stop'
  | 'command_match'
  | 'tool_match'
  | 'permission_denial'
  | 'reasoning_step'
  | 'decisioning_event';


export type StreamEvent = {
  type: string;
  reasoning_step?: ReasoningStep;
  decisioning_event?: DecisioningEvent;
  text?: string;
  name?: string;
  input?: unknown;
  output?: string;
  is_error?: boolean;
  iterations?: number;
  protocol_version?: number;
  schema_version?: number;
  [key: string]: unknown;
};
 

export type ReasoningStepType = 'analysis' | 'planning' | 'reflection' | 'decision';

export type ReasoningStep = {
  step_type: ReasoningStepType;
  content?: string;
  confidence?: number;
  plan?: string;
  steps?: string[];
  critique?: string;
  adjustment?: string;
  choice?: string;
  reasoning?: string;
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
  details?: string[];
};

export type ParsedStreamEvent =
  | { ok: true; event: StreamEvent }
  | { ok: false; reason: 'not-json' | 'invalid-shape' };

const KNOWN_EVENT_TYPES: ReadonlySet<KnownStreamEventType> = new Set([
  'text_delta',
  'tool_use',
  'tool_result',
  'done',
  'message_start',
  'message_stop',
  'command_match',
  'tool_match',
  'permission_denial',
  'reasoning_step',
  'decisioning_event',
]);

export function parseStreamEventLine(line: string): ParsedStreamEvent {
  try {
    const parsed = JSON.parse(line) as { type?: unknown; [key: string]: unknown };
    if (!parsed || typeof parsed !== 'object' || typeof parsed.type !== 'string') {
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
