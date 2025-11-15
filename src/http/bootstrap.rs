//! Bootstrap functionality for discovering and joining existing clusters via HTTP

use crate::http::messages::{PeerInfo, RegisterNodeRequest, RegisterNodeResponse};
use serde::{Deserialize, Serialize};
use slog::{info, warn, Logger};

/// Discovery response from /discovery/peers endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryResponse {
    node_id: u64,
    highest_known_node_id: u64,
    address: String,
    management_leader_node_id: u64,
    management_leader_address: String,
    should_join_as_voter: bool,
    current_voter_count: u64,
    max_voters: u64,
}

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

/// Discover information from a peer node via HTTP
pub async fn discover_peer(address: &str) -> Result<DiscoveredPeer, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();

    // Call the discovery endpoint
    let url = format!("http://{}/discovery/peers", address);

    let response: DiscoveryResponse = client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;

    Ok(DiscoveredPeer {
        node_id: response.node_id,
        address: response.address,
        highest_known_node_id: response.highest_known_node_id,
        management_leader_node_id: response.management_leader_node_id,
        management_leader_address: response.management_leader_address,
        should_join_as_voter: response.should_join_as_voter,
        current_voter_count: response.current_voter_count,
        max_voters: response.max_voters,
    })
}

/// Discover information from multiple peer nodes
/// Returns only successful discoveries
pub async fn discover_peers(addresses: Vec<String>) -> Vec<DiscoveredPeer> {
    let mut peers = Vec::new();

    for address in addresses {
        match discover_peer(&address).await {
            Ok(peer) => {
                println!("✓ Discovered peer at {}: highest_known={}, mgmt_leader={}",
                    address, peer.highest_known_node_id, peer.management_leader_node_id);
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

/// Register this node with a seed peer
pub async fn register_with_peer(
    seed_address: &str,
    our_address: &str,
    node_id: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://{}/discovery/register", seed_address);

    let request = RegisterNodeRequest {
        address: our_address.to_string(),
        node_id: Some(node_id),
    };

    let response: RegisterNodeResponse = client
        .post(&url)
        .json(&request)
        .send()
        .await?
        .json()
        .await?;

    Ok(response.node_id)
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
    fn test_next_node_id_with_peers() {
        let peers = vec![
            DiscoveredPeer {
                node_id: 1,
                address: "addr1".to_string(),
                highest_known_node_id: 3,
                management_leader_node_id: 1,
                management_leader_address: "addr1".to_string(),
                should_join_as_voter: true,
                current_voter_count: 2,
                max_voters: 5,
            },
            DiscoveredPeer {
                node_id: 2,
                address: "addr2".to_string(),
                highest_known_node_id: 5,
                management_leader_node_id: 1,
                management_leader_address: "addr1".to_string(),
                should_join_as_voter: true,
                current_voter_count: 2,
                max_voters: 5,
            },
        ];
        assert_eq!(next_node_id(&peers), 6);
    }
}
