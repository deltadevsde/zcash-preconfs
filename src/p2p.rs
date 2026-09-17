use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tower::ServiceExt;
use zakura_chain::{chain_tip::ChainTip, transaction::UnminedTx};
use zakura_network::zakura::{
    CustomService, Frame, Peer, RequestResponseService, Service, SinkReject, Stream, StreamMode,
    ZakuraConnId, ZakuraPeerId, ZakuraServiceId,
};

pub const CAP: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tip {
    pub height: u32,
    pub hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub chain_id: String,
    pub tip: Tip,
    pub transactions: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetPending {
    chain_id: String,
}
#[derive(Debug)]
pub struct PendingService {
    pub feed: watch::Receiver<Option<Pending>>,
    pub chain_id: String,
}
const STREAM: Stream = Stream {
    kind: 65,
    version: 1,
    frame_cap: 2 * 1024 * 1024 + 8,
    capability: 1 << 17,
    mode: StreamMode::RequestResponse,
};
impl Service for PendingService {
    fn name(&self) -> &'static str {
        "zakura.preconf.v1"
    }
    fn streams(&self) -> &[Stream] {
        &[STREAM]
    }
    fn message_types(&self, _: Stream) -> Option<&'static [u16]> {
        Some(&[0, 1, 2])
    }
    fn message_payload_limits(&self, _: Stream) -> &'static [(u16, usize)] {
        &[(0, 128), (1, CAP), (2, 128)]
    }
    fn add_peer(&self, _: Peer) {}
    fn remove_peer(&self, _: &ZakuraPeerId, _: ZakuraConnId) {}
    fn as_request_response(&self) -> Option<&dyn RequestResponseService> {
        Some(self)
    }
}
impl RequestResponseService for PendingService {
    fn request_frame<'a>(
        &'a self,
        _: ZakuraPeerId,
        _: u16,
        _: u64,
        max_frame: u32,
        max_message: u32,
        frame: Frame,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Frame>, SinkReject>> + Send + 'a>,
    > {
        Box::pin(async move {
            if frame.message_type != 0 || frame.flags != 0 {
                return Err(SinkReject::protocol("invalid preconf request"));
            }
            let req: GetPending =
                serde_json::from_slice(&frame.payload).map_err(SinkReject::protocol)?;
            let current = self.feed.borrow().clone();
            let (mut kind, mut payload) = if req.chain_id != self.chain_id {
                (2, br#"{"reason":"wrong_chain"}"#.to_vec())
            } else if let Some(pending) = current {
                (
                    1,
                    serde_json::to_vec(&pending).map_err(SinkReject::protocol)?,
                )
            } else {
                (2, br#"{"reason":"not_ready"}"#.to_vec())
            };
            let capacity = (max_frame.min(max_message) as usize).saturating_sub(8);
            if payload.len() > capacity {
                kind = 2;
                payload = br#"{"reason":"capacity"}"#.to_vec();
            }
            if payload.len() > capacity {
                return Err(SinkReject::protocol("frame capacity"));
            }
            tracing::debug!(count = payload.len(), "preconf.pending_fetch");
            Ok(vec![Frame {
                message_type: kind,
                flags: 0,
                payload,
            }])
        })
    }
}
pub fn custom(
    feed: watch::Receiver<Option<Pending>>,
    chain_id: String,
    server: bool,
) -> Result<CustomService> {
    let id = ZakuraServiceId::new("zakura.preconf.v1").map_err(|e| anyhow!(e.to_string()))?;
    Ok(CustomService {
        service: Arc::new(PendingService { feed, chain_id }),
        provides: if server { vec![id.clone()] } else { vec![] },
        seeks: if server { vec![] } else { vec![id] },
    })
}
pub async fn verify(
    node: &zakurad::node::NodeServices,
    raw: &str,
) -> Result<zakura_chain::transaction::VerifiedUnminedTx> {
    let tx = crate::wallet::raw_tx(raw)?;
    anyhow::ensure!(
        matches!(tx, zakura_chain::transaction::Transaction::V6 { .. })
            && !tx.is_coinbase()
            && tx.inputs().is_empty()
            && tx.outputs().is_empty()
            && tx.sapling_outputs().next().is_none()
            && tx.sapling_nullifiers().next().is_none()
            && tx.orchard_shielded_data().is_none()
            && tx.ironwood_shielded_data().is_some(),
        "Ironwood-only v6 transaction required"
    );
    let height = node
        .latest_chain_tip
        .best_tip_height()
        .context("chain not ready")?
        .next()?;
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        node.transaction_verifier.clone().oneshot(
            zakura_consensus::transaction::Request::Mempool {
                transaction: UnminedTx::from(tx),
                height,
            },
        ),
    )
    .await?
    .map_err(|e| anyhow!(e.to_string()))?;
    match response {
        zakura_consensus::transaction::Response::Mempool { transaction, .. } => Ok(transaction),
        _ => Err(anyhow!("unexpected verification response")),
    }
}
pub async fn miner(
    node: zakurad::node::NodeServices,
    chain_id: String,
    server_id: String,
) -> Result<()> {
    let endpoint = node
        .zakura_endpoint
        .clone()
        .context("native P2P required")?;
    let mut last: Option<Pending> = None;
    let mut request_id = 0u64;
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut changed = false;
        tick += 1;
        let peers = endpoint.supervisor().outbound_peer_handles().await;
        if tick % 20 == 1 {
            tracing::info!(peers=?peers.iter().map(|p|hex::encode(p.peer_id().as_bytes())).collect::<Vec<_>>(),expected=%server_id,"preconf.peer_status");
        }
        for peer in peers {
            if hex::encode(peer.peer_id().as_bytes()) != server_id {
                continue;
            }
            request_id += 1;
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                peer.request(
                    65,
                    request_id,
                    0,
                    0,
                    serde_json::to_vec(&serde_json::json!({"chain_id":chain_id}))?,
                ),
            )
            .await;
            match response {
                Ok(Ok(frames))
                    if frames.len() == 1
                        && frames[0].message_type == 1
                        && frames[0].flags == 0
                        && frames[0].payload.len() <= CAP =>
                {
                    let pending: Pending = serde_json::from_slice(&frames[0].payload)?;
                    anyhow::ensure!(
                        pending.chain_id == chain_id,
                        "wrong chain in pending response"
                    );
                    if last.as_ref() != Some(&pending) {
                        changed = true;
                        tracing::info!(
                            count = pending.transactions.len(),
                            height = pending.tip.height,
                            "preconf.pending_received"
                        );
                    }
                    last = Some(pending);
                }
                other => tracing::warn!(?other, "preconf.fetch_unavailable"),
            }
        }
        let Some(pending) = &last else { continue };
        let Some(parent) = node.latest_chain_tip.best_tip_hash() else {
            continue;
        };
        let old = node.preconf.borrow().clone();
        if !changed && old.as_ref().is_some_and(|p| p.parent == parent) {
            continue;
        }
        // Revalidate independently, including chain-spent nullifiers and anchors.
        let mut transactions = Vec::new();
        let mut reserved = std::collections::HashSet::new();
        for raw in &pending.transactions {
            match verify(&node, raw).await {
                Ok(tx) => {
                    let nfs: Vec<_> = tx
                        .transaction
                        .transaction()
                        .ironwood_nullifiers()
                        .cloned()
                        .collect();
                    if nfs.iter().any(|nf| reserved.contains(nf)) {
                        continue;
                    }
                    reserved.extend(nfs);
                    transactions.push(tx);
                }
                Err(error) => tracing::info!(%error,"preconf.template_skip"),
            }
        }
        if node.latest_chain_tip.best_tip_hash() != Some(parent) {
            continue;
        }
        node.preconf
            .send_replace(Some(zakura_rpc::methods::PreconfTransactions {
                parent,
                transactions,
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wrong_chain_and_insufficient_capacity_never_return_pending_transactions() {
        let pending = Pending {
            chain_id: "a".repeat(64),
            tip: Tip {
                height: 1,
                hash: "b".repeat(64),
            },
            transactions: vec!["ff".repeat(200)],
        };
        let (_, rx) = watch::channel(Some(pending.clone()));
        let service = PendingService {
            feed: rx,
            chain_id: pending.chain_id.clone(),
        };
        let peer = ZakuraPeerId::new(vec![1; 32]).unwrap();
        let frame = |chain_id: &str| Frame {
            message_type: 0,
            flags: 0,
            payload: serde_json::to_vec(&serde_json::json!({"chain_id":chain_id})).unwrap(),
        };
        let wrong = service
            .request_frame(peer.clone(), 65, 1, 1024, 1024, frame(&"c".repeat(64)))
            .await
            .unwrap();
        assert_eq!(wrong[0].message_type, 2);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&wrong[0].payload).unwrap()["reason"],
            "wrong_chain"
        );
        let small = service
            .request_frame(peer.clone(), 65, 2, 128, 128, frame(&pending.chain_id))
            .await
            .unwrap();
        assert_eq!(small[0].message_type, 2);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&small[0].payload).unwrap()["reason"],
            "capacity"
        );
        let full = service
            .request_frame(peer, 65, 3, 1024, 1024, frame(&pending.chain_id))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Pending>(&full[0].payload).unwrap(),
            pending
        );
    }
}
