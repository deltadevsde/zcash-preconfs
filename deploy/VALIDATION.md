# Verified locally — 17 September 2026

- Release build and both Rust service/transport unit tests passed.
- Three explorer tests passed: public-field filtering and reopening, reorg rollback,
  and rejecting inconsistent chain/ledger snapshots. The numeric-only block-hash
  test also covers hash-versus-height lookup.
- The original ten end-to-end scenarios passed after the wallet changes:
  `runs/20260917-170057-115699/result.json`.
- A fresh batch run included 24, 20, and 22 payments (66 total), rejected the
  scripted higher-fee conflicts, and confirmed all three payouts at exactly 98%:
  `runs/live-explorer-final/workload-ledger.json` and `driver.jsonl`.
- An earlier batch run independently verified blocks with 32, 34, and 28 payments.
- Docker image built from source; Compose started successfully and reported healthy.
- Caddy configuration validation passed. No public domain/certificate was provisioned.
- Restarted the Compose container while accepted payments were pending. The chain ID
  and an existing signed receipt were unchanged; the chain advanced from height 6
  to height 7 and continued generating batches.
- Browser checks covered live home/block views, transaction-hash search, receipt
  and payout details, miner navigation, and a 390px mobile viewport without page
  overflow. Frontend JavaScript syntax and console checks passed.

The local container is `preconf-demo-1`, published only at `127.0.0.1:8080`.
Docker Compose/Buildx were absent on this workstation, so validation used official
binaries in `/tmp/preconf-docker-tools` without changing the installed Docker CLI.
On this workstation, the equivalent management command is:

```sh
DOCKER_CONFIG=/tmp/preconf-docker-tools docker compose ps
```

For deployment, install Docker with its normal Compose and Buildx plugins as
explained in HOSTING.md. No temporary tooling is included in the source archive.
Runtime memory was approximately 1 GiB early in the test; this is an observation,
not a sizing guarantee for long chains or a busy public endpoint.
