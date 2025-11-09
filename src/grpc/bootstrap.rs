use crate::grpc::server::raft_proto::{
    raft_service_client::RaftServiceClient, DiscoveryRequest,
};

/// Information about a discovered peer node
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub node_id: u64,
    pub address: String,
    pub highest_known_node_id: u64,
    pub management_leader_node_id: u64,
    pub management_leader_address: String,
    pub should_join_as_voter: bool,
    pub current_voter_count: u64,
    pub max_voters: u64,
}

/// Discover information from a peer node
pub async fn discover_peer(address: &str) -> Result<DiscoveredPeer, Box<dyn std::error::Error>> {
    let mut client = RaftServiceClient::connect(format!("http://{}", address)).await?;

    let response = client.discover(DiscoveryRequest {}).await?;
    let discovery = response.into_inner();

    Ok(DiscoveredPeer {
        node_id: discovery.node_id,
        address: discovery.address,
        highest_known_node_id: discovery.highest_known_node_id,
        management_leader_node_id: discovery.management_leader_node_id,
        management_leader_address: discovery.management_leader_address,
        should_join_as_voter: discovery.should_join_as_voter,
        current_voter_count: discovery.current_voter_count,
        max_voters: discovery.max_voters,
    })
}

/// Discover information from multiple peer nodes
/// Returns only successful discoveries
pub async fn discover_peers(addresses: Vec<String>) -> Vec<DiscoveredPeer> {
    let mut peers = Vec::new();

    for address in addresses {
        match discover_peer(&address).await {
            Ok(peer) => {
                println!("✓ Discovered peer at {}: node_id={}, highest_known={}, mgmt_leader={}",
                    address, peer.node_id, peer.highest_known_node_id, peer.management_leader_node_id);
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
                highest_known_node_id: 3,
                management_leader_node_id: 1,
                management_leader_address: "127.0.0.1:5001".to_string(),
                should_join_as_voter: true,
                current_voter_count: 3,
                max_voters: 5,
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
                highest_known_node_id: 3,
                management_leader_node_id: 1,
                management_leader_address: "127.0.0.1:5001".to_string(),
                should_join_as_voter: true,
                current_voter_count: 3,
                max_voters: 5,
            },
            DiscoveredPeer {
                node_id: 2,
                address: "127.0.0.1:5002".to_string(),
                highest_known_node_id: 5,
                management_leader_node_id: 1,
                management_leader_address: "127.0.0.1:5001".to_string(),
                should_join_as_voter: true,
                current_voter_count: 3,
                max_voters: 5,
            },
            DiscoveredPeer {
                node_id: 3,
                address: "127.0.0.1:5003".to_string(),
                highest_known_node_id: 5,
                management_leader_node_id: 1,
                management_leader_address: "127.0.0.1:5001".to_string(),
                should_join_as_voter: true,
                current_voter_count: 3,
                max_voters: 5,
            }
        ];
        // Should pick max highest_known_node_id (5) and add 1
        assert_eq!(next_node_id(&peers), 6);
    }
}
