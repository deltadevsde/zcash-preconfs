# PreconfScan: live Ironwood demo

A running Zcash regtest explorer, singleton preconfirmation service, two participating
miners, and automated shielded payments. Uses real Zakura v2 QUIC, Ironwood proofs,
block validation, and miner payouts. The explorer follows ShieldedScan's terminal-green
visual style with independent demo branding; it does not reuse its assets or code.

## Run locally

```sh
./live.sh
```

Open **http://127.0.0.1:8080**. This builds the Rust binary and starts a persistent
regtest network under `data/live/`. Ctrl-C stops the processes. Run the same command
to resume the same chain and explorer history.

The workload targets **20–40 preconfirmed payments per workload block**, averaging
30, with randomized amounts, service fees, miner selection, and submission timing.
About 10% of payments get a higher-fee conflicting spend sent to both miners first;
the service rejects that second spend and the miners prioritize the accepted payment.
Blocks are paced at a minimum of 30 seconds; proving or propagation can extend that
interval. Bootstrap/funding blocks and settlement-only test blocks have fewer payments.
Payouts run independently of payment generation, at two total confirmations.

```sh
./live.sh --payments 30 --jitter 5 --block-seconds 30
./live.sh --run-dir data/another-demo --base-port 29400 --explorer-port 8081
```

Rust 1.97+, Python 3.10+, C/C++, Clang, CMake, and Zakura's native build dependencies
are required. Keep this repo beside `../zakura` with the `demo/preconf` changes.
Use Docker below to avoid setting up the build toolchain manually.

## Explorer

- `/`: live block/transaction tables and pending preconfirmations.
- `/blocks`, `/block/HEIGHT`, `/block/HASH`: blocks, transactions, miner attribution,
  collected service fees, and linked payouts.
- `/txs`, `/tx/TXID`: shielded transactions, receipts, confirmation counts, and payout links.
- `/pending`: the current accepted pending set.
- `/miners`, `/miner/miner1`: block production and revenue by miner.
- `/protocol`: illustrated explanation of acceptance, miner priority, payouts, and trust assumptions.
- `/activity`: batch progress and rejected double-spend attempts.
- Search accepts block heights and block/transaction hashes. Every detail page is linkable.

The page polls every three seconds and labels unavailable/stalled data. `Seen` is
when the demo observed the block; block details also show the actual regtest header
time, which can differ from wall-clock time. Shielded payment values stay hidden.
Service fees and miner payouts are deliberately disclosed by the demo service.

Only read-only explorer routes are exposed. There is no arbitrary node RPC proxy.
Explorer `/api/*` endpoints provide the displayed metadata and `/healthz` reports
indexer availability. Node RPCs, merchant submission, and `/state` remain private.

## Host with Docker Compose

See **[deploy/HOSTING.md](deploy/HOSTING.md)** for the complete procedure, including
packaging both repos with the custom Zakura changes, HTTPS, persistence, updates,
and backups. Quick local container start:

```sh
docker compose up -d --build
```

For a public domain, set `DEMO_DOMAIN` in `.env`, point DNS to your server, and run:

```sh
docker compose --profile public up -d --build
```

The public profile adds Caddy on ports 80/443. No blockchain/node RPC port is
published. This is a test-coin demonstration, not a mainnet service.

## Verify

```sh
# Original focused scenarios: receipts, priorities, reorg, outsider conflict, restart.
./demo.sh --once

# Fresh randomized workload, followed by confirmation of every assigned payout.
./live.sh --run-dir runs/batch-check --blocks 3 --payments 30 --jitter 10

cargo test --release --locked
python3 -m unittest discover -s tests -v
```

The original scenario driver writes `result.json` after all ten assertions pass.
The workload writes `workload-result.json` after each verified batch and
`workload-ledger.json` after final payout verification in a bounded run. Its
`workload_passed` event confirms completion. Both leave logs after shutdown.
The outsider-win scenario is confined to the original test driver; the normal
hosted workload uses participating miners.

## Files and observations

| File / endpoint | Purpose |
| --- | --- |
| `manifest.json` | Chain ID, private RPC endpoints, configured fixture addresses |
| `driver.jsonl` | Batch, mining, and conflict events |
| `service.jsonl`, `miner1.jsonl`, `miner2.jsonl` | Structured node/service logs |
| `wallet.jsonl`, `explorer.jsonl` | Proof worker and indexer diagnostics |
| `service.sqlite` | Durable acceptance, receipt, and payout ledger plus events |
| `explorer.sqlite` | Incremental block and transaction index |
| Private service `/metrics` | Prometheus readiness, height, transaction/payout counts |

Wallets sync new blocks and update unspent-note witnesses incrementally. Reorgs
rebuild affected wallet state; the old 2,000-block cutoff is removed. Chain data is
cached in memory and rebuilt from the persisted node chain on process restart.
The demo ledger still uses a simple SQLite JSON snapshot; this is not a production
indexer. Monitor memory and disk for long runs. Keys are fixed regtest fixtures.

The miner receives `floor(sum(included service fees) * 98 / 100)` once per inclusion
block, at two total confirmations. Payout network fees are paid separately from
the prefunded service wallet. Coinbase attribution recovers configured Ironwood
receivers with the public zero outgoing viewing key. Reorgs after signing a payout
are blocked for inspection; automatic deep-reorg settlement is outside this demo.

Core files: `src/server.rs` (service), `src/p2p.rs` (native transport),
`src/wallet.rs` (wallet), `scripts/live.py` (supervision/traffic),
`scripts/explorer.py` (read-only index/API), and `web/` (frontend).
See [docs/protocol.md](docs/protocol.md) for the merchant and P2P contract.
