use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Manages workflow ownership assignment across cluster nodes
///
/// Each workflow is assigned to exactly one "owner" node that is responsible
/// for executing the workflow logic (creating checkpoints, completing workflows).
/// The Raft leader manages the ownership map and can reassign workflows when
/// nodes fail or for load balancing.
#[derive(Clone, Debug)]
pub struct WorkflowOwnershipMap {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl WorkflowOwnershipMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the owner for a workflow
    pub fn set_owner(&self, workflow_id: String, owner_node_id: u64) {
        self.inner.lock().unwrap().insert(workflow_id, owner_node_id);
    }

    /// Get the owner node ID for a workflow
    pub fn get_owner(&self, workflow_id: &str) -> Option<u64> {
        self.inner.lock().unwrap().get(workflow_id).copied()
    }

    /// Remove ownership entry (when workflow completes)
    pub fn remove_owner(&self, workflow_id: &str) {
        self.inner.lock().unwrap().remove(workflow_id);
    }

    /// Get all workflows owned by a specific node
    /// Used for reassignment when a node fails
    pub fn get_workflows_owned_by(&self, node_id: u64) -> Vec<String> {
        self.inner.lock().unwrap()
            .iter()
            .filter(|&(_, &owner)| owner == node_id)
            .map(|(wf_id, _)| wf_id.clone())
            .collect()
    }

    /// Get total count of workflows with owners
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

impl Default for WorkflowOwnershipMap {
    fn default() -> Self {
        Self::new()
    }
}
