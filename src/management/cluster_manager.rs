//! Cluster Management Logic
//!
//! Pure functional logic for deciding sub-cluster topology changes.
//! All functions are deterministic and side-effect free, taking state
//! snapshots and returning action lists.

use crate::management::state_machine::{ManagementStateMachine, SubClusterMetadata};

/// Policy configuration for cluster management
#[derive(Debug, Clone)]
pub struct ClusterManagerConfig {
    /// Target cluster size for new clusters (default: 3)
    pub target_cluster_size: usize,

    /// Minimum cluster size before triggering consolidation (default: 2)
    pub min_cluster_size: usize,

    /// Maximum cluster size before triggering split (default: 6)
    pub max_cluster_size: usize,

    /// Split size: how many nodes to move to new cluster (default: 3)
    pub split_size: usize,

    /// Ratio threshold for draining: clusters / total_nodes (default: 0.5)
    /// If clusters/nodes > threshold, start draining
    pub drain_threshold: f64,

    /// Minimum number of clusters to maintain (default: 1)
    /// Don't consolidate below this number
    pub min_cluster_count: usize,
}

impl Default for ClusterManagerConfig {
    fn default() -> Self {
        Self {
            target_cluster_size: 3,
            min_cluster_size: 2,
            max_cluster_size: 6,
            split_size: 3,
            drain_threshold: 0.5,
            min_cluster_count: 1,
        }
    }
}

/// Decision output from cluster manager
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterAction {
    /// Add node to specific cluster
    AddNodeToCluster { cluster_id: u32, node_id: u64 },

    /// Create new cluster with specific nodes
    CreateCluster { node_ids: Vec<u64> },

    /// Remove nodes from cluster (for splits/rebalancing)
    RemoveNodesFromCluster { cluster_id: u32, node_ids: Vec<u64> },

    /// Mark cluster for draining (no new work)
    DrainCluster { cluster_id: u32 },

    /// Destroy empty drained cluster
    DestroyCluster { cluster_id: u32 },
}

/// Pure logic cluster manager
#[derive(Clone)]
pub struct ClusterManager {
    config: ClusterManagerConfig,
}

impl ClusterManager {
    /// Create a new cluster manager with default configuration
    pub fn new(config: ClusterManagerConfig) -> Self {
        Self { config }
    }

    /// Decide which cluster(s) to add a new node to
    ///
    /// Strategy:
    /// 1. If no clusters exist, create first cluster with this node
    /// 2. Find smallest non-draining cluster
    /// 3. If smallest < target_size, add to it
    /// 4. Otherwise create new cluster with just this node
    pub fn decide_node_placement(
        &self,
        state: &ManagementStateMachine,
        node_id: u64,
    ) -> Vec<ClusterAction> {
        let clusters = state.get_all_sub_clusters();

        // If no clusters exist, create first one
        if clusters.is_empty() {
            return vec![ClusterAction::CreateCluster {
                node_ids: vec![node_id],
            }];
        }

        // Find smallest non-draining cluster
        let mut eligible_clusters: Vec<_> = clusters
            .iter()
            .filter(|(_, meta)| !Self::is_draining(meta))
            .collect();

        // Sort by size (smallest first)
        eligible_clusters.sort_by_key(|(_, meta)| meta.node_ids.len());

        if let Some((cluster_id, metadata)) = eligible_clusters.first() {
            // If smallest cluster is below target size, add to it
            if metadata.node_ids.len() < self.config.target_cluster_size {
                return vec![ClusterAction::AddNodeToCluster {
                    cluster_id: **cluster_id,
                    node_id,
                }];
            }
        }

        // All clusters at or above target size, create new cluster
        vec![ClusterAction::CreateCluster {
            node_ids: vec![node_id],
        }]
    }

    /// Check if any cluster needs splitting
    ///
    /// Strategy:
    /// 1. Find clusters with size >= max_cluster_size
    /// 2. For each: select split_size nodes (lowest IDs for determinism)
    /// 3. Create new cluster with selected nodes
    /// 4. Remove those nodes from original cluster
    pub fn decide_splits(&self, state: &ManagementStateMachine) -> Vec<ClusterAction> {
        let clusters = state.get_all_sub_clusters();
        let mut actions = Vec::new();

        for (cluster_id, metadata) in clusters {
            // Skip draining clusters
            if Self::is_draining(metadata) {
                continue;
            }

            // Check if cluster needs splitting
            if metadata.node_ids.len() >= self.config.max_cluster_size {
                // Select nodes to move (lowest N node IDs for determinism)
                let mut sorted_nodes = metadata.node_ids.clone();
                sorted_nodes.sort();
                let nodes_to_move: Vec<u64> = sorted_nodes
                    .into_iter()
                    .take(self.config.split_size)
                    .collect();

                // Create new cluster with selected nodes
                actions.push(ClusterAction::CreateCluster {
                    node_ids: nodes_to_move.clone(),
                });

                // Remove those nodes from original cluster
                actions.push(ClusterAction::RemoveNodesFromCluster {
                    cluster_id: *cluster_id,
                    node_ids: nodes_to_move,
                });
            }
        }

        actions
    }

    /// Rebalance after node removal
    ///
    /// Strategy:
    /// 1. Find clusters below min_cluster_size
    /// 2. For each undersized cluster: find a node from the largest cluster to add
    /// 3. Prefer nodes from largest non-draining clusters
    pub fn decide_rebalancing(
        &self,
        state: &ManagementStateMachine,
        _removed_node_id: u64,
    ) -> Vec<ClusterAction> {
        let clusters = state.get_all_sub_clusters();
        let mut actions = Vec::new();

        // Find undersized non-draining clusters
        let undersized: Vec<_> = clusters
            .iter()
            .filter(|(_, meta)| {
                !Self::is_draining(meta) && meta.node_ids.len() < self.config.min_cluster_size
            })
            .collect();

        // Find oversized non-draining clusters (source of nodes)
        let mut oversized: Vec<_> = clusters
            .iter()
            .filter(|(_, meta)| {
                !Self::is_draining(meta) && meta.node_ids.len() > self.config.target_cluster_size
            })
            .collect();

        // Sort oversized by size (largest first)
        oversized.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.node_ids.len()));

        // For each undersized cluster, try to add a node from an oversized cluster
        for (undersized_cluster_id, _) in undersized {
            if let Some((_source_cluster_id, source_metadata)) = oversized.first() {
                // Pick first node from source cluster (deterministic)
                if let Some(&node_id) = source_metadata.node_ids.first() {
                    actions.push(ClusterAction::AddNodeToCluster {
                        cluster_id: *undersized_cluster_id,
                        node_id,
                    });

                    // Note: We don't remove from source here - that will happen
                    // when the add succeeds and triggers another rebalancing check
                }
            }
        }

        actions
    }

    /// Identify clusters for draining/consolidation
    ///
    /// Strategy:
    /// 1. Calculate clusters/nodes ratio
    /// 2. If > drain_threshold, mark smallest clusters for draining
    /// 3. Return destroy actions for empty drained clusters
    /// 4. Never consolidate below min_cluster_count
    pub fn decide_consolidation(&self, state: &ManagementStateMachine) -> Vec<ClusterAction> {
        let clusters = state.get_all_sub_clusters();
        let mut actions = Vec::new();

        // Destroy any empty drained clusters
        for (cluster_id, metadata) in clusters.iter() {
            if Self::is_draining(metadata) && metadata.node_ids.is_empty() {
                actions.push(ClusterAction::DestroyCluster {
                    cluster_id: *cluster_id,
                });
            }
        }

        // Calculate total nodes across all clusters
        let total_nodes: usize = clusters.values().map(|m| m.node_ids.len()).sum();

        if total_nodes == 0 {
            return actions;
        }

        let cluster_count = clusters.len();

        // Don't consolidate if below minimum cluster count
        if cluster_count <= self.config.min_cluster_count {
            return actions;
        }

        // Check if we need to start draining
        let ratio = cluster_count as f64 / total_nodes as f64;

        if ratio > self.config.drain_threshold {
            // Find smallest non-draining cluster to drain
            let mut non_draining: Vec<_> = clusters
                .iter()
                .filter(|(_, meta)| !Self::is_draining(meta))
                .collect();

            // Sort by size (smallest first)
            non_draining.sort_by_key(|(_, meta)| meta.node_ids.len());

            // Only drain if we still have more than min_cluster_count after draining
            let non_draining_count = non_draining.len();
            if non_draining_count > self.config.min_cluster_count {
                if let Some((cluster_id, _)) = non_draining.first() {
                    actions.push(ClusterAction::DrainCluster {
                        cluster_id: **cluster_id,
                    });
                }
            }
        }

        actions
    }

    /// Helper to check if cluster is draining
    fn is_draining(metadata: &SubClusterMetadata) -> bool {
        metadata
            .metadata
            .get("draining")
            .map(|v| v == "true")
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Helper to create state with clusters
    fn create_state_with_clusters(clusters: Vec<(u32, Vec<u64>)>) -> ManagementStateMachine {
        let mut state = ManagementStateMachine::new();
        for (cluster_id, node_ids) in clusters {
            // Manually insert cluster metadata
            let metadata = SubClusterMetadata {
                node_ids,
                metadata: HashMap::new(),
            };
            state
                .get_all_sub_clusters_mut()
                .insert(cluster_id, metadata);
        }
        state
    }

    fn create_draining_cluster_state(
        cluster_id: u32,
        node_ids: Vec<u64>,
    ) -> ManagementStateMachine {
        let mut state = ManagementStateMachine::new();
        let mut metadata = SubClusterMetadata {
            node_ids,
            metadata: HashMap::new(),
        };
        metadata.metadata.insert("draining".to_string(), "true".to_string());
        state
            .get_all_sub_clusters_mut()
            .insert(cluster_id, metadata);
        state
    }

    // === Node Placement Tests ===

    #[test]
    fn test_node_placement_creates_first_cluster() {
        let manager = ClusterManager::new(Default::default());
        let state = ManagementStateMachine::new();
        let actions = manager.decide_node_placement(&state, 1);

        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::CreateCluster { node_ids: vec![1] }
        );
    }

    #[test]
    fn test_node_placement_adds_to_smallest_below_target() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            target_cluster_size: 3,
            ..Default::default()
        });

        // Two clusters: size 1 and size 2
        let state = create_state_with_clusters(vec![(1, vec![1]), (2, vec![2, 3])]);

        let actions = manager.decide_node_placement(&state, 4);

        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::AddNodeToCluster {
                cluster_id: 1,
                node_id: 4
            }
        );
    }

    #[test]
    fn test_node_placement_creates_new_when_all_at_target() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            target_cluster_size: 3,
            ..Default::default()
        });

        // Two clusters both at target size
        let state = create_state_with_clusters(vec![(1, vec![1, 2, 3]), (2, vec![4, 5, 6])]);

        let actions = manager.decide_node_placement(&state, 7);

        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::CreateCluster { node_ids: vec![7] }
        );
    }

    #[test]
    fn test_node_placement_skips_draining_clusters() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            target_cluster_size: 3,
            ..Default::default()
        });

        // Cluster 1 is draining with size 1, cluster 2 is at target
        let mut state = create_draining_cluster_state(1, vec![1]);
        state
            .get_all_sub_clusters_mut()
            .insert(2, SubClusterMetadata {
                node_ids: vec![2, 3, 4],
                metadata: HashMap::new(),
            });

        let actions = manager.decide_node_placement(&state, 5);

        // Should create new cluster instead of adding to draining cluster
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::CreateCluster { node_ids: vec![5] }
        );
    }

    // === Split Tests ===

    #[test]
    fn test_split_triggers_at_max_size() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            max_cluster_size: 6,
            split_size: 3,
            ..Default::default()
        });

        let state = create_state_with_clusters(vec![(1, vec![1, 2, 3, 4, 5, 6])]);

        let actions = manager.decide_splits(&state);

        assert_eq!(actions.len(), 2);
        // Should create new cluster with first 3 nodes
        assert_eq!(
            actions[0],
            ClusterAction::CreateCluster {
                node_ids: vec![1, 2, 3]
            }
        );
        // Should remove those nodes from original
        assert_eq!(
            actions[1],
            ClusterAction::RemoveNodesFromCluster {
                cluster_id: 1,
                node_ids: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn test_split_no_action_below_max() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            max_cluster_size: 6,
            ..Default::default()
        });

        let state = create_state_with_clusters(vec![(1, vec![1, 2, 3, 4, 5])]);

        let actions = manager.decide_splits(&state);

        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn test_split_multiple_clusters() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            max_cluster_size: 6,
            split_size: 3,
            ..Default::default()
        });

        let state = create_state_with_clusters(vec![
            (1, vec![1, 2, 3, 4, 5, 6]),
            (2, vec![7, 8, 9, 10, 11, 12]),
        ]);

        let actions = manager.decide_splits(&state);

        assert_eq!(actions.len(), 4); // 2 creates + 2 removes
    }

    #[test]
    fn test_split_skips_draining_clusters() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            max_cluster_size: 6,
            split_size: 3,
            ..Default::default()
        });

        let state = create_draining_cluster_state(1, vec![1, 2, 3, 4, 5, 6]);

        let actions = manager.decide_splits(&state);

        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn test_split_deterministic_node_selection() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            max_cluster_size: 6,
            split_size: 3,
            ..Default::default()
        });

        // Nodes in random order
        let state = create_state_with_clusters(vec![(1, vec![6, 2, 4, 1, 5, 3])]);

        let actions = manager.decide_splits(&state);

        // Should always select lowest node IDs
        assert_eq!(
            actions[0],
            ClusterAction::CreateCluster {
                node_ids: vec![1, 2, 3]
            }
        );
    }

    // === Rebalancing Tests ===

    #[test]
    fn test_rebalancing_adds_node_to_undersized() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            min_cluster_size: 2,
            target_cluster_size: 3,
            ..Default::default()
        });

        // Cluster 1 is undersized (1 node), cluster 2 is oversized (4 nodes)
        let state = create_state_with_clusters(vec![(1, vec![1]), (2, vec![2, 3, 4, 5])]);

        let actions = manager.decide_rebalancing(&state, 1);

        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::AddNodeToCluster {
                cluster_id: 1,
                node_id: 2 // First node from oversized cluster
            }
        );
    }

    #[test]
    fn test_rebalancing_no_action_when_balanced() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            min_cluster_size: 2,
            target_cluster_size: 3,
            ..Default::default()
        });

        let state = create_state_with_clusters(vec![(1, vec![1, 2]), (2, vec![3, 4])]);

        let actions = manager.decide_rebalancing(&state, 1);

        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn test_rebalancing_prefers_largest_source() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            min_cluster_size: 2,
            target_cluster_size: 3,
            ..Default::default()
        });

        // Cluster 1 undersized, clusters 2 and 3 both oversized (3 prefers larger)
        let state =
            create_state_with_clusters(vec![(1, vec![1]), (2, vec![2, 3, 4]), (3, vec![5, 6, 7, 8])]);

        let actions = manager.decide_rebalancing(&state, 1);

        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ClusterAction::AddNodeToCluster {
                cluster_id: 1,
                node_id: 5 // From largest cluster (3)
            }
        );
    }

    // === Consolidation Tests ===

    #[test]
    fn test_consolidation_destroys_empty_drained() {
        let manager = ClusterManager::new(Default::default());

        let state = create_draining_cluster_state(1, vec![]);

        let actions = manager.decide_consolidation(&state);

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], ClusterAction::DestroyCluster { cluster_id: 1 });
    }

    #[test]
    fn test_consolidation_drains_when_ratio_high() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            drain_threshold: 0.5,
            min_cluster_count: 1,
            ..Default::default()
        });

        // 3 clusters with 4 total nodes = 0.75 ratio (> 0.5 threshold)
        let state = create_state_with_clusters(vec![
            (1, vec![1]),
            (2, vec![2]),
            (3, vec![3, 4]),
        ]);

        let actions = manager.decide_consolidation(&state);

        assert_eq!(actions.len(), 1);
        // Should drain smallest cluster (1 or 2, both size 1)
        match &actions[0] {
            ClusterAction::DrainCluster { cluster_id } => {
                assert!(*cluster_id == 1 || *cluster_id == 2);
            }
            _ => panic!("Expected DrainCluster action"),
        }
    }

    #[test]
    fn test_consolidation_no_drain_below_min_count() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            drain_threshold: 0.5,
            min_cluster_count: 2,
            ..Default::default()
        });

        // 2 clusters with 3 nodes = 0.67 ratio (> threshold but at min count)
        let state = create_state_with_clusters(vec![(1, vec![1]), (2, vec![2, 3])]);

        let actions = manager.decide_consolidation(&state);

        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn test_consolidation_no_drain_below_threshold() {
        let manager = ClusterManager::new(ClusterManagerConfig {
            drain_threshold: 0.5,
            ..Default::default()
        });

        // 2 clusters with 6 nodes = 0.33 ratio (< threshold)
        let state = create_state_with_clusters(vec![(1, vec![1, 2, 3]), (2, vec![4, 5, 6])]);

        let actions = manager.decide_consolidation(&state);

        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn test_consolidation_handles_empty_state() {
        let manager = ClusterManager::new(Default::default());
        let state = ManagementStateMachine::new();

        let actions = manager.decide_consolidation(&state);

        assert_eq!(actions.len(), 0);
    }
}
