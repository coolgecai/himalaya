export class SeqDeduper {
  private lastSeq: Record<string, number>;
  constructor() { this.lastSeq = Object.create(null); }
  shouldForward(kind: string, ev: any): boolean {
    if (!ev || typeof ev !== 'object') { return true; }
    const seq = typeof ev.seq === 'number' ? ev.seq : null;
    if (seq === null) { return true; }
    const id = typeof ev.task_id === 'string' ? ev.task_id : (typeof ev.taskId === 'string' ? ev.taskId : (typeof ev.node_id === 'string' ? ev.node_id : (typeof ev.nodeId === 'string' ? ev.nodeId : '')));
    const key = `${kind}:${String(id || 'global')}`;
    const prev = typeof this.lastSeq[key] === 'number' ? this.lastSeq[key] : -Infinity;
    if (seq <= prev) { return false; }
    this.lastSeq[key] = seq;
    return true;
  }
}
