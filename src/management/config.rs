//! Management Cluster Configuration
//!
//! Configuration for the management cluster, including voter selection strategy
//! to enable scaling to large numbers of nodes by limiting the voter set.

/// Configuration for the management cluster
#[derive(Debug, Clone)]
pub struct ManagementClusterConfig {
    /// Maximum number of voter nodes in the management cluster (default: 5)
    ///
    /// Limiting the number of voters reduces consensus overhead and enables
    /// scaling to hundreds/thousands of nodes. Additional nodes join as learners
    /// which receive log updates but don't participate in voting.
    pub max_voters: usize,

    /// Strategy for selecting which nodes should be voters
    pub voter_selection: VoterSelectionStrategy,
}

impl Default for ManagementClusterConfig {
    fn default() -> Self {
        Self {
            max_voters: 5,
            voter_selection: VoterSelectionStrategy::FirstJoin,
        }
    }
}

/// Strategy for selecting voter nodes in the management cluster
#[derive(Debug, Clone)]
pub enum VoterSelectionStrategy {
    /// First N nodes to join become voters
    ///
    /// Simple strategy where the first `max_voters` nodes become voters,
    /// and all subsequent nodes join as learners. The bootstrap node is
    /// always a voter.
    FirstJoin,

    /// Manually configured voter node IDs
    ///
    /// Explicitly specify which node IDs should be voters. This allows
    /// for precise control over voter placement (e.g., spreading voters
    /// across availability zones).
    Manual(Vec<u64>),

    // Future enhancements:
    // Geographic - Select voters based on geographic distribution
    // LoadBased - Dynamically adjust voters based on load
}

impl ManagementClusterConfig {
    /// Create a new management cluster config with all defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a config with a specific max voter count
    pub fn with_max_voters(max_voters: usize) -> Self {
        Self {
            max_voters,
            ..Default::default()
        }
    }

    /// Create a config with manual voter selection
    pub fn with_manual_voters(voter_ids: Vec<u64>) -> Self {
        Self {
            max_voters: voter_ids.len(),
            voter_selection: VoterSelectionStrategy::Manual(voter_ids),
        }
    }

    /// Check if a node should join as a voter based on current cluster state
    ///
    /// # Arguments
    /// * `node_id` - ID of the joining node
    /// * `current_voter_count` - Number of existing voters in the cluster
    /// * `existing_voters` - IDs of existing voter nodes (for Manual strategy)
    ///
    /// # Returns
    /// * `true` if the node should join as a voter
    /// * `false` if the node should join as a learner
    pub fn should_join_as_voter(
        &self,
        node_id: u64,
        current_voter_count: usize,
        existing_voters: &[u64],
    ) -> bool {
        match &self.voter_selection {
            VoterSelectionStrategy::FirstJoin => {
                // Join as voter if we haven't reached max_voters yet
                current_voter_count < self.max_voters
            }
            VoterSelectionStrategy::Manual(voter_ids) => {
                // Join as voter if this node ID is in the manual list
                // and we haven't exceeded max_voters
                voter_ids.contains(&node_id) && current_voter_count < self.max_voters
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ManagementClusterConfig::default();
        assert_eq!(config.max_voters, 5);
        assert!(matches!(config.voter_selection, VoterSelectionStrategy::FirstJoin));
    }

    #[test]
    fn test_with_max_voters() {
        let config = ManagementClusterConfig::with_max_voters(3);
        assert_eq!(config.max_voters, 3);
    }

    #[test]
    fn test_with_manual_voters() {
        let config = ManagementClusterConfig::with_manual_voters(vec![1, 2, 3]);
        assert_eq!(config.max_voters, 3);
        assert!(matches!(config.voter_selection, VoterSelectionStrategy::Manual(_)));
    }

    #[test]
    fn test_should_join_as_voter_first_join() {
        let config = ManagementClusterConfig::with_max_voters(3);

        // First 3 nodes should join as voters
        assert!(config.should_join_as_voter(1, 0, &[]));
        assert!(config.should_join_as_voter(2, 1, &[1]));
        assert!(config.should_join_as_voter(3, 2, &[1, 2]));

        // 4th node should join as learner
        assert!(!config.should_join_as_voter(4, 3, &[1, 2, 3]));
    }

    #[test]
    fn test_should_join_as_voter_manual() {
        let config = ManagementClusterConfig::with_manual_voters(vec![1, 3, 5]);

        // Nodes in the manual list should join as voters
        assert!(config.should_join_as_voter(1, 0, &[]));
        assert!(config.should_join_as_voter(3, 1, &[1]));
        assert!(config.should_join_as_voter(5, 2, &[1, 3]));

        // Nodes not in the manual list should join as learners
        assert!(!config.should_join_as_voter(2, 0, &[]));
        assert!(!config.should_join_as_voter(4, 2, &[1, 3]));

        // Even if in manual list, can't exceed max_voters
        assert!(!config.should_join_as_voter(5, 3, &[1, 3, 7]));
    }
}
