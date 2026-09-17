mod p2p;
mod server;
mod wallet;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Wallet {
        #[arg(long)]
        rpc: String,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    Address {
        #[arg(long)]
        seed: String,
    },
    Inspect {
        #[arg(long)]
        seed: String,
        #[arg(long)]
        raw_tx: String,
    },
    VerifyReceipt {
        #[arg(long)]
        key: String,
        #[arg(long)]
        receipt: String,
    },
    Create {
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        seed: String,
        #[arg(long)]
        outputs: String,
        #[arg(long, default_value_t = 20000)]
        fee: u64,
        #[arg(long)]
        input_txid: Option<String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    role: String,
    node_config: PathBuf,
    chain_id: String,
    server_peer: String,
    node_rpc: String,
    api: String,
    database: PathBuf,
    seed: String,
    miners: Vec<String>,
    #[serde(default = "min_fee")]
    min_fee_zat: u64,
}
fn min_fee() -> u64 {
    10000
}
fn network() -> zakura_chain::parameters::Network {
    use zakura_chain::parameters::{
        subsidy::FundingStreamReceiver,
        testnet::{
            ConfiguredActivationHeights, ConfiguredFundingStreamRecipient,
            ConfiguredFundingStreams, ConfiguredLockboxDisbursement, RegtestParameters,
        },
    };
    use zakura_chain::{amount::Amount, block::Height};
    zakura_chain::parameters::Network::new_regtest(RegtestParameters {
        activation_heights: ConfiguredActivationHeights {
            nu6_3: Some(1),
            ..Default::default()
        },
        funding_streams: Some(vec![ConfiguredFundingStreams {
            height_range: Some(Height(1)..Height(2001)),
            recipients: Some(vec![ConfiguredFundingStreamRecipient {
                receiver: FundingStreamReceiver::Deferred,
                numerator: 1,
                addresses: None,
            }]),
        }]),
        lockbox_disbursements: Some(vec![ConfiguredLockboxDisbursement {
            address: "t2RnBRiqrN1nW4ecZs1Fj3WWjNdnSs4kiX8".into(),
            amount: Amount::new(6_250_000),
        }]),
        ..Default::default()
    })
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Args::parse().command {
        Command::Wallet { rpc } => {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Some(line) = lines.next_line().await? {
                let result: Result<serde_json::Value> = async {
                    let requests: Vec<serde_json::Value> = serde_json::from_str(&line)?;
                    let blocks = wallet::chain(&rpc).await?;
                    tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                        let mut reserved = std::collections::HashSet::new();
                        let mut results = Vec::new();
                        for request in requests {
                            let seed = request["seed"].as_str().context("seed")?;
                            let outputs: Vec<(String, u64)> =
                                serde_json::from_value(request["outputs"].clone())?;
                            let fee = request["fee"].as_u64().unwrap_or(20000);
                            let tx = wallet::create(
                                &blocks,
                                seed,
                                &outputs,
                                fee,
                                None,
                                &reserved,
                                &network(),
                            )?;
                            let mut item = serde_json::json!({"payment":tx});
                            if request["conflict"].as_bool().unwrap_or(false) {
                                let input =
                                    format!("nf:{}", tx["input_nf"].as_str().context("input nf")?);
                                item["conflict"] = wallet::create(
                                    &blocks,
                                    seed,
                                    &outputs,
                                    fee + 100000,
                                    Some(&input),
                                    &Default::default(),
                                    &network(),
                                )?;
                            }
                            reserved
                                .insert(tx["input_nf"].as_str().context("input nf")?.to_string());
                            results.push(item);
                        }
                        Ok(serde_json::json!({"result":results}))
                    })
                    .await?
                }
                .await;
                let value = match result {
                    Ok(v) => v,
                    Err(e) => serde_json::json!({"error":e.to_string()}),
                };
                tokio::io::stdout()
                    .write_all(format!("{value}\n").as_bytes())
                    .await?;
                tokio::io::stdout().flush().await?;
            }
        }
        Command::Address { seed } => println!("{}", wallet::address(&seed, &network())?),
        Command::Inspect { seed, raw_tx } => {
            let tx = wallet::raw_tx(&raw_tx)?;
            println!(
                "{}",
                serde_json::json!({"txid":tx.hash().to_string(),"received_zat":wallet::received(&tx,&seed,&network(),1)?})
            );
        }
        Command::VerifyReceipt { key, receipt } => {
            use base64::Engine;
            let receipt: serde_json::Value = serde_json::from_str(&receipt)?;
            let payload = base64::engine::general_purpose::STANDARD
                .decode(receipt["payload"].as_str().context("payload")?)?;
            let signature = ed25519_dalek::Signature::from_slice(&hex::decode(
                receipt["signature"].as_str().context("signature")?,
            )?)?;
            let key = ed25519_dalek::VerifyingKey::from_bytes(
                &hex::decode(key)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("key length"))?,
            )?;
            let mut message = b"zcash-preconf-demo/v1\n".to_vec();
            message.extend(&payload);
            key.verify_strict(&message, &signature)?;
            println!("{}", std::str::from_utf8(&payload)?);
        }
        Command::Create {
            rpc,
            seed,
            outputs,
            fee,
            input_txid,
        } => {
            let outputs: Vec<(String, u64)> = serde_json::from_str(&outputs)?;
            let blocks = wallet::chain(&rpc).await?;
            let result = tokio::task::spawn_blocking(move || {
                wallet::create(
                    &blocks,
                    &seed,
                    &outputs,
                    fee,
                    input_txid.as_deref(),
                    &Default::default(),
                    &network(),
                )
            })
            .await??;
            println!("{result}");
        }
        Command::Run { config } => run(serde_json::from_slice(&std::fs::read(config)?)?).await?,
    }
    Ok(())
}
async fn run(config: Config) -> Result<()> {
    anyhow::ensure!(
        matches!(config.role.as_str(), "server" | "miner" | "plain"),
        "invalid role"
    );
    let mut node_config: zakurad::config::ZakuradConfig =
        toml::from_str(&std::fs::read_to_string(&config.node_config)?)?;
    anyhow::ensure!(
        node_config.network.network.is_regtest(),
        "this demo is regtest only"
    );
    node_config.network.network = network();
    let shutdown = CancellationToken::new();
    let (published, feed) = tokio::sync::watch::channel(None);
    let services = if config.role == "plain" {
        vec![]
    } else {
        vec![p2p::custom(
            feed,
            config.chain_id.clone(),
            config.role == "server",
        )?]
    };
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let token = shutdown.clone();
    let node = zakurad::node::run_with_services_ready(node_config, services, token, ready_tx);
    tokio::pin!(node);
    let handles = tokio::select! {
        ready=tokio::time::timeout(std::time::Duration::from_secs(180),ready_rx)=>ready?.context("node startup failed")?,
        result=&mut node=>{return Err(anyhow::anyhow!("node exited before ready: {result:?}"));}
    };
    let app = async move {
        match config.role.as_str() {
            "server" => server::run(Arc::new(config), handles, published).await,
            "miner" => p2p::miner(handles, config.chain_id, config.server_peer).await,
            _ => std::future::pending::<Result<()>>().await,
        }
    };
    tokio::pin!(app);
    tokio::select! {
        result=&mut node=> {shutdown.cancel();result.map_err(|e|anyhow::anyhow!(e.to_string()))?;}
        result=&mut app=>{shutdown.cancel();node.await.map_err(|e|anyhow::anyhow!(e.to_string()))?;result?;}
        _=tokio::signal::ctrl_c()=>{shutdown.cancel();node.await.map_err(|e|anyhow::anyhow!(e.to_string()))?;}
    }
    Ok(())
}
