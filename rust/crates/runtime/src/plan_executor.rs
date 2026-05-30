use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{PlanDag, PlanDagEdgeKind, PlanNodeKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanNodeStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanExecutionEventKind {
    NodeReady,
    NodeStarted,
    NodeSucceeded,
    NodeFailed,
    NodeSkipped,
    ExecutionBlocked,
    ExecutionFinished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanExecutionEvent {
    pub seq: u64,
    pub task_id: String,
    pub node_id: String,
    pub kind: PlanExecutionEventKind,
    pub status: PlanNodeStatus,
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_gate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeVerificationGate {
    pub node_id: String,
    pub command: String,
    pub required: bool,
    pub last_result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodeExecution {
    pub node_id: String,
    pub status: PlanNodeStatus,
    pub candidate_tools: Vec<String>,
    pub output_summary: Option<String>,
    pub failure_class: Option<String>,
    pub retry_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_gate: Option<NodeVerificationGate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanExecution {
    pub task_id: String,
    pub root_id: String,
    pub nodes: BTreeMap<String, PlanNodeExecution>,
    pub events: Vec<PlanExecutionEvent>,
    seq: u64,
}

impl PlanExecution {
    #[must_use]
    pub fn new(dag: &PlanDag) -> Self {
        let dependencies = dependency_map(dag);
        let mut nodes = BTreeMap::new();
        let mut execution = Self {
            task_id: dag.task_id.clone(),
            root_id: dag.root_id.clone(),
            nodes: BTreeMap::new(),
            events: Vec::new(),
            seq: 0,
        };

        for node in &dag.nodes {
            let status = if node.kind == PlanNodeKind::Task || dependencies[&node.id].is_empty() {
                PlanNodeStatus::Ready
            } else {
                PlanNodeStatus::Pending
            };
            nodes.insert(
                node.id.clone(),
                PlanNodeExecution {
                    node_id: node.id.clone(),
                    status,
                    candidate_tools: node.candidate_tools.clone(),
                    output_summary: None,
                    failure_class: None,
                    retry_count: 0,
                    verification_gate: None,
                    worker_id: None,
                },
            );
            if status == PlanNodeStatus::Ready {
                execution.push_event(
                    &node.id,
                    PlanExecutionEventKind::NodeReady,
                    status,
                    Some("node has no unsatisfied dependencies".to_string()),
                );
            }
        }

        execution.nodes = nodes;
        execution
    }

    #[must_use]
    pub fn ready_nodes(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| node.status == PlanNodeStatus::Ready)
            .map(|node| node.node_id.clone())
            .collect()
    }

    #[must_use]
    pub fn resumable_nodes(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| {
                matches!(
                    node.status,
                    PlanNodeStatus::Ready | PlanNodeStatus::Running | PlanNodeStatus::Failed
                )
            })
            .map(|node| node.node_id.clone())
            .collect()
    }

    #[must_use]
    pub fn completed_nodes(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| node.status == PlanNodeStatus::Succeeded)
            .map(|node| node.node_id.clone())
            .collect()
    }

    pub fn retry_node(&mut self, dag: &PlanDag, node_id: &str) -> Result<(), String> {
        let dependencies = dependency_map(dag);
        let succeeded = self
            .nodes
            .values()
            .filter(|node| node.status == PlanNodeStatus::Succeeded)
            .map(|node| node.node_id.clone())
            .collect::<BTreeSet<_>>();
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        if !matches!(
            node.status,
            PlanNodeStatus::Failed | PlanNodeStatus::Skipped | PlanNodeStatus::Running
        ) {
            return Err(format!(
                "plan node {node_id} cannot retry from status {:?}",
                node.status
            ));
        }
        if !dependencies
            .get(node_id)
            .into_iter()
            .flatten()
            .all(|dependency| succeeded.contains(dependency))
        {
            return Err(format!(
                "plan node {node_id} cannot retry before dependencies succeed"
            ));
        }
        node.status = PlanNodeStatus::Ready;
        node.retry_count = node.retry_count.saturating_add(1);
        node.failure_class = None;
        node.worker_id = None;
        self.reset_skipped_dependents(dag, node_id);
        self.push_event(
            node_id,
            PlanExecutionEventKind::NodeReady,
            PlanNodeStatus::Ready,
            Some("node retry scheduled".to_string()),
        );
        Ok(())
    }

    pub fn attach_verification_gate(
        &mut self,
        node_id: &str,
        command: impl Into<String>,
        required: bool,
    ) -> Result<(), String> {
        let command = command.into();
        let status = {
            let node = self
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("plan node not found: {node_id}"))?;
            node.verification_gate = Some(NodeVerificationGate {
                node_id: node_id.to_string(),
                command: command.clone(),
                required,
                last_result: None,
            });
            node.status
        };
        self.push_event(
            node_id,
            PlanExecutionEventKind::ExecutionBlocked,
            status,
            Some(format!("verification gate attached: {command}")),
        );
        if let Some(event) = self.events.last_mut() {
            event.verification_gate = Some(command);
        }
        Ok(())
    }

    pub fn record_node_verification(
        &mut self,
        node_id: &str,
        passed: bool,
        summary: impl Into<String>,
    ) -> Result<(), String> {
        let summary = summary.into();
        let status = {
            let node = self
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("plan node not found: {node_id}"))?;
            if let Some(gate) = node.verification_gate.as_mut() {
                gate.last_result = Some(summary.clone());
            }
            if !passed {
                node.status = PlanNodeStatus::Failed;
                node.worker_id = None;
                node.failure_class = Some("node_verification".to_string());
            }
            node.status
        };
        self.push_event(
            node_id,
            if passed {
                PlanExecutionEventKind::ExecutionFinished
            } else {
                PlanExecutionEventKind::NodeFailed
            },
            status,
            Some(summary),
        );
        Ok(())
    }

    pub fn start_node(&mut self, node_id: &str) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        if node.status != PlanNodeStatus::Ready {
            return Err(format!(
                "plan node {node_id} is not ready; current status: {:?}",
                node.status
            ));
        }
        node.status = PlanNodeStatus::Running;
        self.push_event(
            node_id,
            PlanExecutionEventKind::NodeStarted,
            PlanNodeStatus::Running,
            None,
        );
        Ok(())
    }

    pub fn assign_worker(
        &mut self,
        node_id: &str,
        worker_id: impl Into<String>,
    ) -> Result<(), String> {
        let worker_id = worker_id.into();
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        if node.status != PlanNodeStatus::Running {
            return Err(format!(
                "plan node {node_id} cannot bind worker from status {:?}",
                node.status
            ));
        }
        node.worker_id = Some(worker_id.clone());
        self.push_event(
            node_id,
            PlanExecutionEventKind::NodeStarted,
            PlanNodeStatus::Running,
            Some(format!("worker assigned: {worker_id}")),
        );
        Ok(())
    }

    pub fn clear_worker(&mut self, node_id: &str) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        node.worker_id = None;
        Ok(())
    }

    pub fn succeed_node(
        &mut self,
        dag: &PlanDag,
        node_id: &str,
        output_summary: Option<String>,
    ) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        if node.status != PlanNodeStatus::Running && node.status != PlanNodeStatus::Ready {
            return Err(format!(
                "plan node {node_id} cannot succeed from status {:?}",
                node.status
            ));
        }
        node.status = PlanNodeStatus::Succeeded;
        node.worker_id = None;
        node.output_summary = output_summary.clone();
        self.push_event(
            node_id,
            PlanExecutionEventKind::NodeSucceeded,
            PlanNodeStatus::Succeeded,
            output_summary,
        );
        self.refresh_ready_nodes(dag);
        Ok(())
    }

    pub fn fail_node(
        &mut self,
        dag: &PlanDag,
        node_id: &str,
        failure_class: impl Into<String>,
    ) -> Result<(), String> {
        let failure_class = failure_class.into();
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;
        node.status = PlanNodeStatus::Failed;
        node.worker_id = None;
        node.failure_class = Some(failure_class.clone());
        self.push_event(
            node_id,
            PlanExecutionEventKind::NodeFailed,
            PlanNodeStatus::Failed,
            Some(failure_class),
        );
        self.skip_dependents(dag, node_id);
        Ok(())
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.nodes.values().all(|node| {
            matches!(
                node.status,
                PlanNodeStatus::Succeeded | PlanNodeStatus::Failed | PlanNodeStatus::Skipped
            )
        })
    }

    #[must_use]
    pub fn events_since(&self, offset: usize) -> &[PlanExecutionEvent] {
        self.events.get(offset..).unwrap_or(&[])
    }

    fn refresh_ready_nodes(&mut self, dag: &PlanDag) {
        let dependencies = dependency_map(dag);
        let succeeded = self
            .nodes
            .values()
            .filter(|node| node.status == PlanNodeStatus::Succeeded)
            .map(|node| node.node_id.clone())
            .collect::<BTreeSet<_>>();
        let pending = self
            .nodes
            .values()
            .filter(|node| node.status == PlanNodeStatus::Pending)
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();

        for node_id in pending {
            if dependencies[&node_id]
                .iter()
                .all(|dependency| succeeded.contains(dependency))
            {
                if let Some(node) = self.nodes.get_mut(&node_id) {
                    node.status = PlanNodeStatus::Ready;
                }
                self.push_event(
                    &node_id,
                    PlanExecutionEventKind::NodeReady,
                    PlanNodeStatus::Ready,
                    Some("all dependencies satisfied".to_string()),
                );
            }
        }
    }

    fn skip_dependents(&mut self, dag: &PlanDag, failed_node_id: &str) {
        let reverse = reverse_dependency_map(dag);
        let mut stack = reverse.get(failed_node_id).cloned().unwrap_or_default();
        let mut visited = BTreeSet::new();
        while let Some(node_id) = stack.pop() {
            if !visited.insert(node_id.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get_mut(&node_id) {
                if matches!(node.status, PlanNodeStatus::Pending | PlanNodeStatus::Ready) {
                    node.status = PlanNodeStatus::Skipped;
                    self.push_event(
                        &node_id,
                        PlanExecutionEventKind::NodeSkipped,
                        PlanNodeStatus::Skipped,
                        Some(format!("blocked by failed dependency {failed_node_id}")),
                    );
                }
            }
            stack.extend(reverse.get(&node_id).cloned().unwrap_or_default());
        }
    }

    fn reset_skipped_dependents(&mut self, dag: &PlanDag, retried_node_id: &str) {
        let reverse = reverse_dependency_map(dag);
        let mut stack = reverse.get(retried_node_id).cloned().unwrap_or_default();
        let mut visited = BTreeSet::new();
        while let Some(node_id) = stack.pop() {
            if !visited.insert(node_id.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get_mut(&node_id) {
                if node.status == PlanNodeStatus::Skipped {
                    node.status = PlanNodeStatus::Pending;
                    node.failure_class = None;
                }
            }
            stack.extend(reverse.get(&node_id).cloned().unwrap_or_default());
        }
    }

    fn push_event(
        &mut self,
        node_id: &str,
        kind: PlanExecutionEventKind,
        status: PlanNodeStatus,
        message: Option<String>,
    ) {
        self.seq += 1;
        self.events.push(PlanExecutionEvent {
            seq: self.seq,
            task_id: self.task_id.clone(),
            node_id: node_id.to_string(),
            kind,
            status,
            message,
            attempt: self
                .nodes
                .get(node_id)
                .map(|node| node.retry_count.saturating_add(1)),
            dependencies: Vec::new(),
            blocking_reason: None,
            verification_gate: None,
        });
    }
}

#[must_use]
pub fn dependency_map(dag: &PlanDag) -> BTreeMap<String, BTreeSet<String>> {
    let mut dependencies = dag
        .nodes
        .iter()
        .map(|node| (node.id.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in &dag.edges {
        if edge.kind == PlanDagEdgeKind::DependsOn {
            dependencies
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
        }
    }
    dependencies
}

#[must_use]
pub fn reverse_dependency_map(dag: &PlanDag) -> BTreeMap<String, Vec<String>> {
    let mut reverse = BTreeMap::new();
    for edge in &dag.edges {
        if edge.kind == PlanDagEdgeKind::DependsOn {
            reverse
                .entry(edge.from.clone())
                .or_insert_with(Vec::new)
                .push(edge.to.clone());
        }
    }
    reverse
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanDagEdge, PlanDagNode};

    fn sample_dag() -> PlanDag {
        PlanDag {
            task_id: "task-1".to_string(),
            root_id: "task-1".to_string(),
            nodes: vec![
                PlanDagNode {
                    kind: PlanNodeKind::Task,
                    id: "task-1".to_string(),
                    title: "Do work".to_string(),
                    parallelizable: false,
                    estimated_effort: 2,
                    candidate_tools: vec!["read_file".to_string()],
                    notes: Vec::new(),
                },
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "analyze".to_string(),
                    title: "Analyze".to_string(),
                    parallelizable: true,
                    estimated_effort: 1,
                    candidate_tools: vec!["read_file".to_string()],
                    notes: Vec::new(),
                },
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "edit".to_string(),
                    title: "Edit".to_string(),
                    parallelizable: false,
                    estimated_effort: 1,
                    candidate_tools: vec!["edit_file".to_string()],
                    notes: Vec::new(),
                },
            ],
            edges: vec![
                PlanDagEdge {
                    from: "task-1".to_string(),
                    to: "analyze".to_string(),
                    kind: PlanDagEdgeKind::Contains,
                },
                PlanDagEdge {
                    from: "analyze".to_string(),
                    to: "edit".to_string(),
                    kind: PlanDagEdgeKind::DependsOn,
                },
            ],
        }
    }

    #[test]
    fn contains_edges_do_not_block_initial_readiness() {
        let dag = sample_dag();
        let execution = PlanExecution::new(&dag);
        assert!(execution.ready_nodes().contains(&"task-1".to_string()));
        assert!(execution.ready_nodes().contains(&"analyze".to_string()));
        assert_eq!(execution.nodes["edit"].status, PlanNodeStatus::Pending);
    }

    #[test]
    fn succeeding_dependency_releases_downstream_node() {
        let dag = sample_dag();
        let mut execution = PlanExecution::new(&dag);
        execution.start_node("analyze").expect("start");
        execution
            .succeed_node(&dag, "analyze", Some("done".to_string()))
            .expect("succeed");
        assert_eq!(execution.nodes["edit"].status, PlanNodeStatus::Ready);
    }

    #[test]
    fn failing_dependency_skips_downstream_node() {
        let dag = sample_dag();
        let mut execution = PlanExecution::new(&dag);
        execution
            .fail_node(&dag, "analyze", "test_failure")
            .expect("fail");
        assert_eq!(execution.nodes["edit"].status, PlanNodeStatus::Skipped);
        assert!(execution
            .events
            .iter()
            .any(|event| event.kind == PlanExecutionEventKind::NodeSkipped));
    }

    #[test]
    fn retry_failed_node_reopens_skipped_dependents() {
        let dag = sample_dag();
        let mut execution = PlanExecution::new(&dag);
        execution.start_node("analyze").expect("node should start");
        execution
            .fail_node(&dag, "analyze", "tool_error")
            .expect("node should fail");
        assert_eq!(execution.nodes["edit"].status, PlanNodeStatus::Skipped);

        execution
            .retry_node(&dag, "analyze")
            .expect("node should retry");
        execution
            .start_node("analyze")
            .expect("node should restart");
        execution
            .succeed_node(&dag, "analyze", Some("done".to_string()))
            .expect("node should succeed");

        assert_eq!(execution.nodes["edit"].status, PlanNodeStatus::Ready);
    }

    #[test]
    fn retry_failed_node_updates_resume_state_and_verification_gate() {
        let dag = sample_dag();
        let mut execution = PlanExecution::new(&dag);
        execution.start_node("analyze").expect("node should start");
        execution
            .fail_node(&dag, "analyze", "tool_error")
            .expect("node should fail");
        execution
            .retry_node(&dag, "analyze")
            .expect("node should retry");
        execution
            .attach_verification_gate("analyze", "cargo test", true)
            .expect("gate should attach");
        execution
            .record_node_verification("analyze", false, "verification failed")
            .expect("verification should record");

        assert!(execution.resumable_nodes().contains(&"analyze".to_string()));
        assert_eq!(execution.nodes["analyze"].retry_count, 1);
        assert!(execution.nodes["analyze"].verification_gate.is_some());
        assert_eq!(execution.nodes["analyze"].status, PlanNodeStatus::Failed);
    }
}
