use crate::grpc::server::raft_proto::{
    raft_service_client::RaftServiceClient, DiscoveryRequest,
};

/// Information about a discovered peer node
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub node_id: u64,
    pub address: String,
    pub role: RaftRole,
    pub highest_known_node_id: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub first_entry_index: u64,
    pub first_entry_term: u64,
}

/// Raft role of a node
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRole {
    Follower,
    Candidate,
    Leader,
    Learner,
}

impl From<i32> for RaftRole {
    fn from(value: i32) -> Self {
        match value {
            0 => RaftRole::Follower,
            1 => RaftRole::Candidate,
            2 => RaftRole::Leader,
            3 => RaftRole::Learner,
            _ => RaftRole::Follower, // Default
        }
    }
}

/// Discover information from a peer node
pub async fn discover_peer(address: &str) -> Result<DiscoveredPeer, Box<dyn std::error::Error>> {
    let mut client = RaftServiceClient::connect(format!("http://{}", address)).await?;

    let response = client.discover(DiscoveryRequest {}).await?;
    let discovery = response.into_inner();

    Ok(DiscoveredPeer {
        node_id: discovery.node_id,
        address: discovery.address,
        role: RaftRole::from(discovery.role),
        highest_known_node_id: discovery.highest_known_node_id,
        voters: discovery.voters,
        learners: discovery.learners,
        first_entry_index: discovery.first_entry_index,
        first_entry_term: discovery.first_entry_term,
    })
}

/// Discover information from multiple peer nodes
/// Returns only successful discoveries
pub async fn discover_peers(addresses: Vec<String>) -> Vec<DiscoveredPeer> {
    let mut peers = Vec::new();

    for address in addresses {
        match discover_peer(&address).await {
            Ok(peer) => {
                println!("✓ Discovered peer at {}: node_id={}, role={:?}, highest_known={}",
                    address, peer.node_id, peer.role, peer.highest_known_node_id);
                peers.push(peer);
            }
            Err(e) => {
                println!("✗ Failed to discover peer at {}: {}", address, e);
            }
        }
    }

    peers
}

/// Determine the next available node ID based on discovered peers
pub fn next_node_id(discovered_peers: &[DiscoveredPeer]) -> u64 {
    discovered_peers
        .iter()
        .map(|p| p.highest_known_node_id)
        .max()
        .map(|max_id| max_id + 1)
        .unwrap_or(1) // Start at 1 if no peers discovered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_node_id_empty() {
        let peers = vec![];
        assert_eq!(next_node_id(&peers), 1);
    }

    #[test]
    fn test_next_node_id_single_peer() {
        let peers = vec![
            DiscoveredPeer {
                node_id: 1,
                address: "127.0.0.1:5001".to_string(),
                role: RaftRole::Leader,
                highest_known_node_id: 3,
                voters: vec![1, 2, 3],
                learners: vec![],
                first_entry_index: 1,
                first_entry_term: 1,
            }
        ];
        assert_eq!(next_node_id(&peers), 4);
    }

    #[test]
    fn test_next_node_id_multiple_peers() {
        let peers = vec![
            DiscoveredPeer {
                node_id: 1,
                address: "127.0.0.1:5001".to_string(),
                role: RaftRole::Leader,
                highest_known_node_id: 3,
                voters: vec![1, 2, 3],
                learners: vec![],
                first_entry_index: 1,
                first_entry_term: 1,
            },
            DiscoveredPeer {
                node_id: 2,
                address: "127.0.0.1:5002".to_string(),
                role: RaftRole::Follower,
                highest_known_node_id: 5,
                voters: vec![1, 2, 3],
                learners: vec![],
                first_entry_index: 1,
                first_entry_term: 1,
            },
            DiscoveredPeer {
                node_id: 3,
                address: "127.0.0.1:5003".to_string(),
                role: RaftRole::Follower,
                highest_known_node_id: 5,
                voters: vec![1, 2, 3],
                learners: vec![],
                first_entry_index: 1,
                first_entry_term: 1,
            }
        ];
        // Should pick max highest_known_node_id (5) and add 1
        assert_eq!(next_node_id(&peers), 6);
    }
}
