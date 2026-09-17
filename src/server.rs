use crate::{
    p2p::{Pending, Tip, CAP},
    wallet, Config,
};
use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{watch, Mutex};
use zakura_chain::chain_tip::ChainTip;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    txid: String,
    raw_tx: String,
    fee: u64,
    receipt: Value,
    status: String,
    inclusion: Option<Tip>,
    failure_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Payout {
    block: Tip,
    address: String,
    amount: u64,
    txids: Vec<String>,
    raw_tx: Option<String>,
    txid: Option<String>,
    status: String,
    reason: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Ledger {
    tip: Option<Tip>,
    records: Vec<Record>,
    payouts: Vec<Payout>,
}
struct Store {
    db: rusqlite::Connection,
    ledger: Ledger,
    ready: bool,
    recovering: bool,
}
struct App {
    config: Arc<Config>,
    node: zakurad::node::NodeServices,
    store: Mutex<Store>,
    published: watch::Sender<Option<Pending>>,
    key: SigningKey,
}
impl Store {
    fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = rusqlite::Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS ledger(id INTEGER PRIMARY KEY CHECK(id=1),json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY, time TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),kind TEXT NOT NULL,data TEXT NOT NULL);")?;
        use rusqlite::OptionalExtension;
        let ledger = db
            .query_row("SELECT json FROM ledger WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            db,
            ledger,
            ready: false,
            recovering: true,
        })
    }
    fn save(&mut self, kind: &str, data: Value) -> Result<()> {
        let json = serde_json::to_string(&self.ledger)?;
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO ledger VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            [json],
        )?;
        tx.execute(
            "INSERT INTO events(kind,data) VALUES(?1,?2)",
            rusqlite::params![kind, data.to_string()],
        )?;
        tx.commit()?;
        tracing::info!(event=kind,%data,"preconf.event");
        Ok(())
    }
}
impl App {
    fn pending(&self, ledger: &Ledger) -> Option<Pending> {
        Some(Pending {
            chain_id: self.config.chain_id.clone(),
            tip: ledger.tip.clone()?,
            transactions: ledger
                .records
                .iter()
                .filter(|r| r.status == "pending")
                .map(|r| r.raw_tx.clone())
                .collect(),
        })
    }
    fn publish(&self, ledger: &Ledger) {
        self.published.send_replace(self.pending(ledger));
    }
}
fn result(record: &Record, tip: Option<&Tip>) -> Value {
    let inclusion=record.inclusion.as_ref().map(|b|json!({"block_hash":b.hash,"height":b.height,"confirmations":tip.map_or(0,|t|t.height.saturating_sub(b.height)+1)}));
    json!({"txid":record.txid,"status":record.status,"receipt":record.receipt,"inclusion":inclusion,"failure_reason":record.failure_reason})
}
fn error(id: Value, code: i32, reason: &str, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":{"reason":reason}}})
}
async fn api(State(app): State<Arc<App>>, body: axum::body::Bytes) -> Json<Value> {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return Json(error(Value::Null, -32700, "parse_error", "invalid JSON")),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || request.get("id").is_none()
        || !request.get("params").is_some_and(Value::is_object)
    {
        return Json(error(
            id,
            -32600,
            "invalid_request",
            "JSON-RPC 2.0 request with named params and id required",
        ));
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match dispatch(&app, method, &request["params"]).await {
        Ok(value) => Json(json!({"jsonrpc":"2.0","id":id,"result":value})),
        Err((code, reason, message)) => {
            tracing::info!(%method,%reason,%message,"preconf.rpc_rejected");
            Json(error(id, code, reason, &message))
        }
    }
}
type ApiError = (i32, &'static str, String);
fn internal(error: impl std::fmt::Display) -> ApiError {
    (-32603, "internal", error.to_string())
}
async fn dispatch(app: &App, method: &str, params: &Value) -> std::result::Result<Value, ApiError> {
    let mut store = app.store.lock().await;
    match method {
        "preconf_info" => {
            if params.as_object().is_none_or(|p| !p.is_empty()) {
                return Err((-32602, "invalid_params", "no parameters expected".into()));
            }
            Ok(
                json!({"protocol_version":1,"chain_id":app.config.chain_id,"service_pubkey":hex::encode(app.key.verifying_key().to_bytes()),"fee_address":wallet::address(&app.config.seed,&crate::network()).map_err(internal)?,"min_fee_zat":app.config.min_fee_zat.to_string(),"ready":store.ready,"tip":store.ledger.tip}),
            )
        }
        "preconf_get" => {
            let txid = params
                .get("txid")
                .and_then(Value::as_str)
                .filter(|_| params.as_object().is_some_and(|p| p.len() == 1))
                .ok_or((-32602, "invalid_params", "txid required".into()))?;
            let record = store
                .ledger
                .records
                .iter()
                .find(|r| r.txid == txid)
                .ok_or((-32006, "not_found", "unknown txid".into()))?;
            Ok(result(record, store.ledger.tip.as_ref()))
        }
        "preconf_submit" => {
            let raw = params
                .get("raw_tx")
                .and_then(Value::as_str)
                .filter(|_| params.as_object().is_some_and(|p| p.len() == 1))
                .ok_or((-32602, "invalid_params", "raw_tx required".into()))?;
            let tx = wallet::raw_tx(raw).map_err(|e| (-32002, "invalid_tx", e.to_string()))?;
            let txid = tx.hash().to_string();
            if let Some(record) = store.ledger.records.iter().find(|r| r.txid == txid) {
                return Ok(result(record, store.ledger.tip.as_ref()));
            }
            if !store.ready
                || store.ledger.tip.as_ref().map(|t| t.hash.clone())
                    != app
                        .node
                        .latest_chain_tip
                        .best_tip_hash()
                        .map(|h| h.to_string())
            {
                return Err((-32001, "not_ready", "chain reconciliation pending".into()));
            }
            let tip =
                store
                    .ledger
                    .tip
                    .clone()
                    .ok_or((-32001, "not_ready", "chain not ready".into()))?;
            let nfs: HashSet<_> = tx.ironwood_nullifiers().cloned().collect();
            for record in store
                .ledger
                .records
                .iter()
                .filter(|r| r.status == "pending")
            {
                let old = wallet::raw_tx(&record.raw_tx).map_err(internal)?;
                if old.ironwood_nullifiers().any(|nf| nfs.contains(nf)) {
                    return Err((-32004, "conflict", "spend already reserved".into()));
                }
            }
            crate::p2p::verify(&app.node, raw)
                .await
                .map_err(|e| (-32002, "invalid_tx", e.to_string()))?;
            if app
                .node
                .latest_chain_tip
                .best_tip_hash()
                .map(|h| h.to_string())
                != Some(tip.hash.clone())
            {
                return Err((-32001, "not_ready", "tip changed during validation".into()));
            }
            let fee = wallet::received(&tx, &app.config.seed, &crate::network(), tip.height + 1)
                .map_err(internal)?;
            if fee < app.config.min_fee_zat {
                return Err((-32003, "fee_too_low", "insufficient service fee".into()));
            }
            let mut pending = app
                .pending(&store.ledger)
                .ok_or_else(|| internal("missing tip"))?;
            pending.transactions.push(raw.to_string());
            if serde_json::to_vec(&pending).map_err(internal)?.len() > CAP {
                return Err((-32005, "pending_full", "pending set at capacity".into()));
            }
            let payload=serde_json::to_vec(&json!({"chain_id":app.config.chain_id,"txid":txid,"service_fee_zat":fee.to_string()})).map_err(internal)?;
            let mut signing = b"zcash-preconf-demo/v1\n".to_vec();
            signing.extend(&payload);
            let receipt = json!({"payload":base64::engine::general_purpose::STANDARD.encode(payload),"signature":hex::encode(app.key.sign(&signing).to_bytes())});
            let record = Record {
                txid: txid.clone(),
                raw_tx: raw.to_string(),
                fee,
                receipt,
                status: "pending".into(),
                inclusion: None,
                failure_reason: None,
            };
            let response = result(&record, Some(&tip));
            store.ledger.records.push(record);
            if let Err(e) = store.save("accepted", json!({"txid":txid,"fee_zat":fee})) {
                store.ledger.records.pop();
                store.ready = false;
                return Err(internal(e));
            }
            app.publish(&store.ledger);
            Ok(response)
        }
        _ => Err((-32601, "method_not_found", "unknown method".into())),
    }
}
async fn metrics(State(app): State<Arc<App>>) -> String {
    let store = app.store.lock().await;
    let mut text = format!(
        "preconf_ready {}\npreconf_chain_height {}\n",
        u8::from(store.ready),
        store.ledger.tip.as_ref().map_or(0, |t| t.height)
    );
    for status in ["pending", "included", "failed"] {
        text += &format!(
            "preconf_transactions{{status=\"{status}\"}} {}\n",
            store
                .ledger
                .records
                .iter()
                .filter(|r| r.status == status)
                .count()
        );
    }
    for status in ["waiting", "broadcast", "confirmed", "blocked"] {
        text += &format!(
            "preconf_payouts{{status=\"{status}\"}} {}\n",
            store
                .ledger
                .payouts
                .iter()
                .filter(|p| p.status == status)
                .count()
        );
    }
    text
}
async fn state(State(app): State<Arc<App>>) -> Json<Value> {
    let store = app.store.lock().await;
    Json(json!({"ready":store.ready,"ledger":store.ledger}))
}
pub async fn run(
    config: Arc<Config>,
    node: zakurad::node::NodeServices,
    published: watch::Sender<Option<Pending>>,
) -> Result<()> {
    let key = SigningKey::from_bytes(
        &hex::decode(&config.seed)?
            .try_into()
            .map_err(|_| anyhow!("seed length"))?,
    );
    let app = Arc::new(App {
        store: Mutex::new(Store::open(&config.database)?),
        node,
        published,
        key,
        config,
    });
    let listener = tokio::net::TcpListener::bind(&app.config.api).await?;
    let router = Router::new()
        .route("/", post(api))
        .route("/metrics", get(metrics))
        .route("/state", get(state))
        .layer(DefaultBodyLimit::max(CAP))
        .with_state(app.clone());
    let follower = async {
        loop {
            if let Err(error) = reconcile(&app).await {
                app.store.lock().await.ready = false;
                tracing::error!(%error,"preconf.reconcile_error");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {result=axum::serve(listener,router)=>result?,result=follower=>result?};
    Ok(())
}
async fn reconcile(app: &App) -> Result<()> {
    let Some(tip_hash) = app.node.latest_chain_tip.best_tip_hash() else {
        return Ok(());
    };
    {
        let store = app.store.lock().await;
        if store.recovering
            && store.ledger.tip.as_ref().is_some_and(|t| {
                app.node
                    .latest_chain_tip
                    .best_tip_height()
                    .is_none_or(|h| h.0 < t.height)
            })
        {
            return Ok(());
        }
        if store.ready
            && store
                .ledger
                .tip
                .as_ref()
                .is_some_and(|t| t.hash == tip_hash.to_string())
            && !store
                .ledger
                .payouts
                .iter()
                .any(|p| p.status == "waiting" || p.status == "broadcast")
        {
            return Ok(());
        }
    }
    let blocks = wallet::chain(&app.config.node_rpc).await?;
    let tip_block = blocks.last().context("empty chain")?;
    let tip = Tip {
        height: tip_block.coinbase_height().context("height")?.0,
        hash: tip_block.hash().to_string(),
    };
    let mut store = app.store.lock().await;
    anyhow::ensure!(
        app.node
            .latest_chain_tip
            .best_tip_hash()
            .map(|h| h.to_string())
            == Some(tip.hash.clone()),
        "tip changed before reconciliation"
    );
    store.ready = false;
    let mut inclusions = HashMap::new();
    let mut spent = HashMap::new();
    let mut block_map = HashMap::new();
    for block in blocks.iter() {
        let b = Tip {
            height: block.coinbase_height().context("height")?.0,
            hash: block.hash().to_string(),
        };
        block_map.insert(b.hash.clone(), block);
        for tx in &block.transactions {
            let id = tx.hash().to_string();
            inclusions.insert(id.clone(), b.clone());
            for nf in tx.ironwood_nullifiers() {
                spent.insert(hex::encode(<[u8; 32]>::from(*nf)), id.clone());
            }
        }
    }
    for payout in &mut store.ledger.payouts {
        if payout.status == "blocked" {
            continue;
        }
        if !block_map.contains_key(&payout.block.hash) {
            if payout.raw_tx.is_some() {
                payout.status = "blocked".into();
                payout.reason =
                    Some("inclusion block orphaned after signing; inspect before retry".into());
            }
        } else if payout
            .txid
            .as_ref()
            .is_some_and(|id| inclusions.contains_key(id))
        {
            payout.status = "confirmed".into();
        } else if payout.status == "confirmed" {
            payout.status = "blocked".into();
            payout.reason = Some("payout confirmation orphaned; inspect before retry".into());
        }
    }
    store
        .ledger
        .payouts
        .retain(|p| p.raw_tx.is_some() || block_map.contains_key(&p.block.hash));
    for record in &mut store.ledger.records {
        let old = record.status.clone();
        if let Some(block) = inclusions.get(&record.txid) {
            record.status = "included".into();
            record.inclusion = Some(block.clone());
            record.failure_reason = None;
        } else {
            record.inclusion = None;
            let tx = wallet::raw_tx(&record.raw_tx)?;
            let conflict = tx
                .ironwood_nullifiers()
                .any(|nf| spent.contains_key(&hex::encode(<[u8; 32]>::from(*nf))));
            if conflict {
                record.status = "failed".into();
                record.failure_reason = Some("conflict".into());
            } else {
                match crate::p2p::verify(&app.node, &record.raw_tx).await {
                    Ok(_) => {
                        record.status = "pending".into();
                        record.failure_reason = None;
                    }
                    Err(e) => {
                        record.status = "failed".into();
                        record.failure_reason = Some(
                            if tx
                                .expiry_height()
                                .is_some_and(|h| h.0 != 0 && h.0 < tip.height + 1)
                            {
                                "expired"
                            } else {
                                "invalidated"
                            }
                            .into(),
                        );
                        tracing::info!(txid=%record.txid,%e,"preconf.invalidated");
                    }
                }
            }
        }
        if record.status != old {
            tracing::info!(txid=%record.txid,status=%record.status,block=?record.inclusion,"preconf.status_changed");
        }
    }
    store.ledger.tip = Some(tip.clone());
    let assigned: HashSet<_> = store
        .ledger
        .payouts
        .iter()
        .flat_map(|p| p.txids.iter().cloned())
        .collect();
    let mut groups: HashMap<String, Vec<Record>> = HashMap::new();
    for r in &store.ledger.records {
        if let Some(b) = &r.inclusion {
            if !assigned.contains(&r.txid) {
                groups.entry(b.hash.clone()).or_default().push(r.clone());
            }
        }
    }
    for (hash, records) in groups {
        let b = records[0].inclusion.clone().context("inclusion")?;
        let recipients = wallet::coinbase_receivers(block_map[&hash], &crate::network())?;
        let miners: Vec<_> = app
            .config
            .miners
            .iter()
            .filter(|a| {
                wallet::receiver(a, &crate::network())
                    .is_ok_and(|r| recipients.contains(&hex::encode(r.to_raw_address_bytes())))
            })
            .collect();
        if miners.len() != 1 {
            tracing::warn!(%hash,"preconf.unattributed");
            continue;
        }
        let fees = records
            .iter()
            .try_fold(0u64, |n, r| n.checked_add(r.fee).context("fee overflow"))?;
        store.ledger.payouts.push(Payout {
            block: b,
            address: miners[0].clone(),
            amount: fees.checked_mul(98).context("amount overflow")? / 100,
            txids: records.iter().map(|r| r.txid.clone()).collect(),
            raw_tx: None,
            txid: None,
            status: "waiting".into(),
            reason: None,
        });
    }
    store.save("chain_reconciled", json!({"tip":tip}))?;
    store.ready = true;
    store.recovering = false;
    app.publish(&store.ledger);
    for i in 0..store.ledger.payouts.len() {
        let payout = store.ledger.payouts[i].clone();
        if !matches!(payout.status.as_str(), "waiting" | "broadcast")
            || tip.height < payout.block.height + 1
        {
            continue;
        }
        if payout.raw_tx.is_none() {
            let mut reserved = HashSet::new();
            for other in &store.ledger.payouts {
                if let Some(raw) = &other.raw_tx {
                    for nf in wallet::raw_tx(raw)?.ironwood_nullifiers() {
                        reserved.insert(hex::encode(<[u8; 32]>::from(*nf)));
                    }
                }
            }
            let blocks = blocks.clone();
            let seed = app.config.seed.clone();
            let outputs = vec![(payout.address.clone(), payout.amount)];
            tracing::info!(block=%payout.block.hash,amount=payout.amount,"preconf.payout_building");
            // Cold wallet recovery and proving must not block readiness or ledger reads.
            // Reconciliation is serial; RPC submissions do not change payout indices.
            drop(store);
            let built = tokio::task::spawn_blocking(move || {
                wallet::create(
                    &blocks,
                    &seed,
                    &outputs,
                    20000,
                    None,
                    &reserved,
                    &crate::network(),
                )
            })
            .await?;
            store = app.store.lock().await;
            match built {
                Ok(built) => {
                    store.ledger.payouts[i].raw_tx =
                        Some(built["raw_tx"].as_str().context("raw")?.to_string());
                    store.ledger.payouts[i].txid =
                        Some(built["txid"].as_str().context("id")?.to_string());
                    store.ledger.payouts[i].reason = None;
                    store.save("payout_signed",json!({"block_hash":payout.block.hash,"txid":built["txid"],"amount_zat":payout.amount}))?;
                }
                Err(error) => {
                    store.ledger.payouts[i].reason = Some(error.to_string());
                    tracing::warn!(%error,"preconf.payout_waiting");
                    continue;
                }
            }
        }
        // Recheck the credited block after proving and before broadcasting.
        if wallet::rpc(
            &app.config.node_rpc,
            "getblockhash",
            json!([payout.block.height]),
        )
        .await?
        .as_str()
            != Some(&payout.block.hash)
        {
            store.ledger.payouts[i].status = "blocked".into();
            store.save("payout_blocked", json!({"block_hash":payout.block.hash}))?;
            continue;
        }
        let raw = store.ledger.payouts[i]
            .raw_tx
            .clone()
            .context("signed payout")?;
        if wallet::raw_tx(&raw)?
            .expiry_height()
            .is_some_and(|height| height.0 != 0 && height.0 <= tip.height)
        {
            store.ledger.payouts[i].status = "blocked".into();
            store.ledger.payouts[i].reason =
                Some("signed payout expired; inspect before retry".into());
            store.save("payout_blocked", json!({"block_hash":payout.block.hash}))?;
            continue;
        }
        let mempool = wallet::rpc(&app.config.node_rpc, "getrawmempool", json!([])).await?;
        if mempool
            .as_array()
            .context("mempool array")?
            .iter()
            .any(|id| id.as_str() == store.ledger.payouts[i].txid.as_deref())
        {
            if store.ledger.payouts[i].status != "broadcast" {
                store.ledger.payouts[i].status = "broadcast".into();
                store.save(
                    "payout_broadcast_recovered",
                    json!({"block_hash":payout.block.hash}),
                )?;
            }
            continue;
        }
        match wallet::rpc(&app.config.node_rpc, "sendrawtransaction", json!([raw])).await {
            Ok(_) => {
                store.ledger.payouts[i].status = "broadcast".into();
                let payout_txid = store.ledger.payouts[i].txid.clone();
                store.save(
                    "payout_broadcast",
                    json!({"block_hash":payout.block.hash,"txid":payout_txid}),
                )?;
            }
            Err(error) => {
                tracing::warn!(%error,"preconf.payout_broadcast_retry");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_preserves_receipt_and_signed_payout_assignment() {
        let path = std::env::temp_dir().join(format!(
            "preconf-ledger-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let receipt = json!({"payload":"exact-bytes","signature":"immutable"});
        let block = Tip {
            height: 2,
            hash: "b".repeat(64),
        };
        {
            let mut store = Store::open(&path).unwrap();
            store.ledger.records.push(Record {
                txid: "tx".into(),
                raw_tx: "00".into(),
                fee: 10000,
                receipt: receipt.clone(),
                status: "included".into(),
                inclusion: Some(block.clone()),
                failure_reason: None,
            });
            store.ledger.payouts.push(Payout {
                block,
                address: "miner".into(),
                amount: 9800,
                txids: vec!["tx".into()],
                raw_tx: Some("signed".into()),
                txid: Some("payout".into()),
                status: "waiting".into(),
                reason: None,
            });
            store.save("payout_signed", json!({})).unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert!(!store.ready, "restart must reconcile before accepting");
            assert_eq!(store.ledger.records[0].receipt, receipt);
            assert_eq!(store.ledger.payouts[0].raw_tx.as_deref(), Some("signed"));
            assert_eq!(store.ledger.payouts[0].txids, vec!["tx"]);
            assert_eq!(
                store
                    .db
                    .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
