use serde::{Deserialize, Serialize};

use crate::{
    ModelRouteDecision, PlanDag, PlanNodeKind, TeamExecutionEvent, TeamExecutionEventKind, TeamRole,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAssignment {
    pub node_id: String,
    pub role: TeamRole,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCoordinationPlan {
    pub team_id: String,
    pub task_id: String,
    pub assignments: Vec<WorkerAssignment>,
}

#[derive(Debug, Clone)]
pub struct TeamCoordinator {
    team_id: String,
    seq: u64,
}

impl TeamCoordinator {
    #[must_use]
    pub fn new(team_id: impl Into<String>) -> Self {
        Self {
            team_id: team_id.into(),
            seq: 0,
        }
    }

    #[must_use]
    pub fn plan(&self, dag: &PlanDag) -> TeamCoordinationPlan {
        let assignments = dag
            .nodes
            .iter()
            .map(|node| WorkerAssignment {
                node_id: node.id.clone(),
                role: role_for_node(node.kind.clone(), &node.candidate_tools),
                reason: format!("assigned from {:?} node and tool hints", node.kind),
            })
            .collect();
        TeamCoordinationPlan {
            team_id: self.team_id.clone(),
            task_id: dag.task_id.clone(),
            assignments,
        }
    }

    pub fn event_for_assignment(
        &mut self,
        task_id: &str,
        assignment: &WorkerAssignment,
        model_route: Option<ModelRouteDecision>,
    ) -> TeamExecutionEvent {
        self.seq += 1;
        TeamExecutionEvent {
            seq: self.seq,
            team_id: self.team_id.clone(),
            task_id: task_id.to_string(),
            role: assignment.role,
            kind: TeamExecutionEventKind::TaskAssigned,
            model_route,
            message: Some(format!(
                "{} assigned to {:?}: {}",
                assignment.node_id, assignment.role, assignment.reason
            )),
        }
    }
}

fn role_for_node(kind: PlanNodeKind, tools: &[String]) -> TeamRole {
    if tools
        .iter()
        .any(|tool| tool.contains("grep") || tool.contains("read"))
    {
        return TeamRole::Planner;
    }
    if tools
        .iter()
        .any(|tool| tool.contains("write") || tool.contains("edit"))
    {
        return TeamRole::Implementer;
    }
    match kind {
        PlanNodeKind::Task => TeamRole::Planner,
        PlanNodeKind::Step => TeamRole::Implementer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanDagEdge, PlanDagNode};

    #[test]
    fn assigns_roles_from_tool_hints() {
        let dag = PlanDag {
            task_id: "task-1".to_string(),
            root_id: "task-1".to_string(),
            nodes: vec![PlanDagNode {
                kind: PlanNodeKind::Step,
                id: "edit".to_string(),
                title: "Edit".to_string(),
                parallelizable: false,
                estimated_effort: 1,
                candidate_tools: vec!["edit_file".to_string()],
                notes: Vec::new(),
            }],
            edges: Vec::<PlanDagEdge>::new(),
        };
        let coordinator = TeamCoordinator::new("team-1");
        let plan = coordinator.plan(&dag);

        assert_eq!(plan.assignments[0].role, TeamRole::Implementer);
    }
}
