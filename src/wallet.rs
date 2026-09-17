use anyhow::{anyhow, bail, Context, Result};
use incrementalmerkletree::{frontier::CommitmentTree, witness::IncrementalWitness};
use orchard::{
    keys::{FullViewingKey, PreparedIncomingViewingKey, Scope, SpendAuthorizingKey, SpendingKey},
    note_encryption::IronwoodDomain,
    tree::MerkleHashOrchard,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};
use zakura_chain::{
    block::Block,
    parameters::Network,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
    transaction::Transaction,
};
use zcash_keys::address::UnifiedAddress;
use zcash_primitives::transaction::{
    builder::{BuildConfig, Builder, BundlePadding},
    fees::fixed::FeeRule,
};
use zcash_protocol::{consensus::BlockHeight, memo::MemoBytes, value::Zatoshis};

pub fn key(seed: &str) -> Result<SpendingKey> {
    let bytes: [u8; 32] = hex::decode(seed)?
        .try_into()
        .map_err(|_| anyhow!("seed must be 32 bytes"))?;
    Option::from(SpendingKey::from_bytes(bytes)).ok_or_else(|| anyhow!("invalid spending key"))
}
pub fn address(seed: &str, net: &Network) -> Result<String> {
    let addr = FullViewingKey::from(&key(seed)?).address_at(0u32, Scope::External);
    Ok(UnifiedAddress::from_receivers(Some(addr), None, None)
        .context("receiver")?
        .encode(net))
}
pub fn receiver(address: &str, net: &Network) -> Result<orchard::Address> {
    match zcash_keys::address::Address::decode(net, address) {
        Some(zcash_keys::address::Address::Unified(ua)) => {
            ua.orchard().copied().context("Ironwood receiver required")
        }
        _ => bail!("unified address required"),
    }
}
pub async fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let response: Value = reqwest::Client::new()
        .post(url)
        .timeout(std::time::Duration::from_secs(180))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        bail!("{method}: {error}");
    }
    response
        .get("result")
        .cloned()
        .context("missing RPC result")
}
// Reuse downloaded blocks between updates, including across wallet requests.
pub async fn chain(url: &str) -> Result<Arc<Vec<Block>>> {
    static CHAINS: OnceLock<tokio::sync::Mutex<HashMap<String, Arc<Vec<Block>>>>> = OnceLock::new();
    let mut chains = CHAINS.get_or_init(Default::default).lock().await;
    let blocks = chains.entry(url.into()).or_default();
    let height = rpc(url, "getblockcount", json!([]))
        .await?
        .as_u64()
        .context("height")?;
    let data = Arc::make_mut(blocks);
    while let Some(last) = data.last() {
        let h = data.len() - 1;
        if h as u64 <= height
            && rpc(url, "getblockhash", json!([h])).await?.as_str()
                == Some(&last.hash().to_string())
        {
            break;
        }
        data.pop();
    }
    for h in data.len() as u64..=height {
        let hash = rpc(url, "getblockhash", json!([h])).await?;
        let raw = rpc(url, "getblock", json!([hash, 0])).await?;
        data.push(
            hex::decode(raw.as_str().context("block hex")?)?
                .as_slice()
                .zcash_deserialize_into()?,
        );
    }
    let current = rpc(url, "getbestblockhash", json!([])).await?;
    anyhow::ensure!(
        current.as_str() == data.last().map(|b| b.hash().to_string()).as_deref(),
        "chain changed while syncing wallet"
    );
    Ok(blocks.clone())
}
pub fn raw_tx(raw: &str) -> Result<Transaction> {
    let bytes = hex::decode(raw)?;
    let mut cursor = std::io::Cursor::new(&bytes);
    let tx = zakura_chain::serialization::ZcashDeserialize::zcash_deserialize(&mut cursor)?;
    anyhow::ensure!(
        cursor.position() == bytes.len() as u64,
        "trailing transaction bytes"
    );
    Ok(tx)
}
pub fn received(tx: &Transaction, seed: &str, net: &Network, height: u32) -> Result<u64> {
    let fvk = FullViewingKey::from(&key(seed)?);
    let ivk = PreparedIncomingViewingKey::new(&fvk.to_ivk(Scope::External));
    let tx = parse(tx, net, height)?;
    let mut total = 0u64;
    if let Some(bundle) = tx.ironwood_bundle() {
        for action in bundle.actions() {
            if let Some((note, _, _)) = zcash_note_encryption::try_note_decryption(
                &IronwoodDomain::for_action(action),
                &ivk,
                action,
            ) {
                total = total
                    .checked_add(note.value().inner())
                    .context("amount overflow")?;
            }
        }
    }
    Ok(total)
}
pub fn coinbase_receivers(block: &Block, net: &Network) -> Result<HashSet<String>> {
    let height = block.coinbase_height().context("height")?;
    let tx = parse(&block.transactions[0], net, height.0)?;
    let mut recipients = HashSet::new();
    if let Some(bundle) = tx.ironwood_bundle() {
        for action in bundle.actions() {
            if let Some((note, addr, _)) = zcash_note_encryption::try_output_recovery_with_ovk(
                &IronwoodDomain::for_action(action),
                &orchard::keys::OutgoingViewingKey::from([0u8; 32]),
                action,
                action.cv_net(),
                &action.encrypted_note().out_ciphertext,
            ) {
                if note.value().inner() > 0 {
                    recipients.insert(hex::encode(addr.to_raw_address_bytes()));
                }
            }
        }
    }
    Ok(recipients)
}
#[derive(Clone)]
struct OwnedNote {
    note: orchard::Note,
    witness: IncrementalWitness<MerkleHashOrchard, 32>,
    txid: String,
}
struct WalletState {
    min_value: u64,
    hashes: Vec<zakura_chain::block::Hash>,
    tree: CommitmentTree<MerkleHashOrchard, 32>,
    notes: Vec<OwnedNote>,
}
impl Default for WalletState {
    fn default() -> Self {
        Self {
            min_value: 0,
            hashes: Vec::new(),
            tree: CommitmentTree::empty(),
            notes: Vec::new(),
        }
    }
}
impl WalletState {
    fn sync(
        &mut self,
        blocks: &[Block],
        fvk: &FullViewingKey,
        net: &Network,
        min_value: u64,
    ) -> Result<()> {
        // A reorg rebuilds witnesses from the surviving canonical chain.
        if min_value < self.min_value
            || self.hashes.len() > blocks.len()
            || self
                .hashes
                .last()
                .is_some_and(|h| *h != blocks[self.hashes.len() - 1].hash())
        {
            *self = Self::default();
        }
        // This demo spends one input. Notes unable to cover its fee and any positive output cannot fund it;
        // keeping witnesses for every small service fee makes cold recovery quadratic.
        // Keep the lowest supported cutoff. A higher-fee conflict must not evict
        // notes and force the next ordinary payment to rescan the whole chain.
        if self.hashes.is_empty() {
            self.min_value = min_value;
        }
        let min_value = self.min_value;
        let ivk = PreparedIncomingViewingKey::new(&fvk.to_ivk(Scope::External));
        // Only unspent notes need witnesses at the requested tip. During cold
        // recovery, avoid rebuilding witnesses for notes spent later in this range.
        let spent: HashSet<_> = blocks[self.hashes.len()..]
            .iter()
            .flat_map(|b| &b.transactions)
            .flat_map(|tx| tx.ironwood_nullifiers())
            .map(|nf| hex::encode(<[u8; 32]>::from(*nf)))
            .collect();
        self.notes
            .retain(|n| !spent.contains(&hex::encode(n.note.nullifier(fvk).to_bytes())));
        for block in &blocks[self.hashes.len()..] {
            let height = block.coinbase_height().context("block height")?;
            for tx in &block.transactions {
                let parsed = parse(tx, net, height.0)?;
                if let Some(bundle) = parsed.ironwood_bundle() {
                    for action in bundle.actions() {
                        let cm = MerkleHashOrchard::from_cmx(action.cmx());
                        self.tree.append(cm).map_err(|_| anyhow!("tree full"))?;
                        for note in &mut self.notes {
                            note.witness
                                .append(cm)
                                .map_err(|_| anyhow!("witness full"))?;
                        }
                        if let Some((note, _, _)) = zcash_note_encryption::try_note_decryption(
                            &IronwoodDomain::for_action(action),
                            &ivk,
                            action,
                        ) {
                            if note.value().inner() > 0
                                && note.value().inner() >= min_value
                                && !spent.contains(&hex::encode(note.nullifier(fvk).to_bytes()))
                            {
                                self.notes.push(OwnedNote {
                                    note,
                                    witness: IncrementalWitness::from_tree(self.tree.clone())
                                        .context("witness")?,
                                    txid: tx.hash().to_string(),
                                });
                            }
                        }
                    }
                }
            }
            self.hashes.push(block.hash());
        }
        Ok(())
    }
}
pub fn create(
    blocks: &[Block],
    seed: &str,
    outputs: &[(String, u64)],
    fee: u64,
    input_txid: Option<&str>,
    reserved: &HashSet<String>,
    net: &Network,
) -> Result<Value> {
    static WALLETS: OnceLock<Mutex<HashMap<String, WalletState>>> = OnceLock::new();
    let sk = key(seed)?;
    let fvk = FullViewingKey::from(&sk);
    let mut wallets = WALLETS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow!("wallet lock"))?;
    let wallet = wallets.entry(seed.into()).or_default();
    let min_value = fee
        .checked_add(u64::from(outputs.iter().any(|(_, value)| *value > 0)))
        .context("amount overflow")?;
    wallet.sync(blocks, &fvk, net, min_value)?;
    let total = outputs.iter().try_fold(fee, |n, (_, value)| {
        n.checked_add(*value).context("amount overflow")
    })?;
    let owned = wallet
        .notes
        .iter()
        .find(|n| {
            n.note.value().inner() >= total
                && !reserved.contains(&hex::encode(n.note.nullifier(&fvk).to_bytes()))
                && input_txid.is_none_or(|id| {
                    id == n.txid
                        || id.strip_prefix("nf:")
                            == Some(&hex::encode(n.note.nullifier(&fvk).to_bytes()))
                })
        })
        .cloned()
        .context("no available confirmed note large enough")?;
    let root = wallet.tree.root();
    drop(wallets);
    let input_nf = hex::encode(owned.note.nullifier(&fvk).to_bytes());
    let height = blocks
        .last()
        .context("empty chain")?
        .coinbase_height()
        .context("height")?
        .0
        + 1;
    let mut builder = Builder::new(
        net,
        BlockHeight::from_u32(height),
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: None,
            ironwood_anchor: Some(orchard::Anchor::from(root)),
            orchard_padding: BundlePadding::DEFAULT,
            ironwood_padding: BundlePadding::DEFAULT,
        },
    );
    builder.add_ironwood_spend::<std::convert::Infallible>(
        fvk.clone(),
        owned.note,
        owned.witness.path().context("path")?.into(),
    )?;
    for (addr, value) in outputs {
        builder.add_ironwood_output::<std::convert::Infallible>(
            Some(fvk.to_ovk(Scope::External)),
            receiver(addr, net)?,
            Zatoshis::from_u64(*value)?,
            MemoBytes::empty(),
        )?;
    }
    let change = owned.note.value().inner() - total;
    if change > 0 {
        builder.add_ironwood_output::<std::convert::Infallible>(
            Some(fvk.to_ovk(Scope::External)),
            fvk.address_at(0u32, Scope::External),
            Zatoshis::from_u64(change)?,
            MemoBytes::empty(),
        )?;
    }
    let prover = zakura_consensus::sapling_prover();
    let built = builder.build(
        &Default::default(),
        &[],
        &[SpendAuthorizingKey::from(&sk)],
        rand::rng(),
        prover,
        prover,
        &FeeRule::non_standard(Zatoshis::from_u64(fee)?),
    )?;
    let tx = built.transaction();
    let mut raw = Vec::new();
    tx.write(&mut raw)?;
    Ok(
        json!({"raw_tx":hex::encode(raw),"txid":tx.txid().to_string(),"input_txid":owned.txid,"input_nf":input_nf}),
    )
}

fn parse(
    tx: &Transaction,
    net: &Network,
    height: u32,
) -> Result<zcash_primitives::transaction::Transaction> {
    Ok(zcash_primitives::transaction::Transaction::read(
        &tx.zcash_serialize_to_vec()?[..],
        zcash_protocol::consensus::BranchId::for_height(net, BlockHeight::from_u32(height)),
    )?)
}
