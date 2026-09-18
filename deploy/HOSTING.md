# Hosting PreconfScan

The stack is one demo container plus optional Caddy for HTTPS. The demo container
runs three Zakura nodes, a wallet worker, traffic generation, and the explorer.
All node and merchant RPCs bind to the container's loopback interface. Only the
read-only explorer is reachable through Caddy.

## Archived observer (current mode)

Mining and traffic generation are stopped. The public site serves a frozen snapshot
at block 1290 containing 38,489 included preconfirmations and 41,065 total transactions.
The snapshot is in `/data/archive` inside the existing Docker volume. Original chain
and service data remain in `/data/live`; nothing was deleted or reset.

Start only the observer with:

```sh
docker compose -f compose.yaml -f compose.archive.yaml up -d --build
```

The deployed computer's ignored `.env` sets
`COMPOSE_FILE=compose.yaml:compose.archive.yaml`, so plain `docker compose up`
also keeps archive mode. No miners, proof workers, or node RPCs run in this mode.
The explorer opens its saved database read-only, serves existing pages, and labels
the site as archived. Confirmation counts and payout statuses stay at the saved tip.
It does not imply that unsettled payouts have completed.

The archive was made after stopping the demo, using SQLite's backup API on
`/data/live/explorer.sqlite` and copying the driver log and the manifest's public
chain ID and target payment count. Before serving, the observer verifies that the
saved ledger tip matches the indexed block tip. Keep this volume in backups.

## Current deployment: Cloudflare Tunnel

Public URL: https://preconfs.deltadevs.xyz

The website and API both run on this computer at `127.0.0.1:8080`.
Cloudflare Tunnel `preconf-demo` forwards the public hostname directly to that
port; no separate public server or Tailscale connection is needed by visitors.
The DNS route and HTTPS are configured through Cloudflare.

The connector runs as the user service `preconf-tunnel.service`. User lingering
is enabled, so it starts at boot without an interactive login. Manage it with:

```sh
systemctl --user status preconf-tunnel.service
systemctl --user restart preconf-tunnel.service
journalctl --user -u preconf-tunnel.service -f
curl -f https://preconfs.deltadevs.xyz/healthz
```

Its private configuration and credentials are under `~/.cloudflared/` and are
not included in this repository or source archives. Keep this computer online
and the demo Docker container running. The other deployment options below are
alternatives, not additional required services.

## 1. Transfer the current source

### Separate frontend, with the demo on zinc

The live API is available at `https://zinc.curl-vimba.ts.net/api/*` through
Tailscale Serve. To host the public website on a different box:

1. Join that box to the tailnet and allow it to reach zinc on HTTPS port 443.
2. Point `preconfs.deltadevs.xyz` at that box and allow public ports 80/443.
3. Export the frontend: `python3 scripts/export-web.py /tmp/preconf-web`.
4. Copy the contents of `/tmp/preconf-web/` into `/srv/preconf/` on that box.
5. Use `deploy/Caddyfile.frontend` as that box's Caddy configuration (adjust the
   domain if needed), then reload Caddy.

Run Caddy on the tailnet-connected host, or provide its container with access to
the host's tailnet routing and DNS. This setup runs no Zakura nodes on the public
box. Do not start the demo Compose stack there for this deployment mode.

Caddy serves the static frontend locally and forwards `/api/*` and `/healthz`
to zinc over HTTPS, preserving their paths and query strings. The browser uses
same-origin API URLs, so visitors need neither Tailscale nor CORS configuration.
The exporter embeds the protocol article so `/protocol` also works on the static
host. Re-export and copy the files when updating the frontend.

Verify from the public box with `curl -f https://zinc.curl-vimba.ts.net/api/summary`,
then through `https://preconfs.deltadevs.xyz/api/summary`. Zinc must stay running
for live data; static pages remain available if its connection goes down.

The remaining instructions cover hosting the whole demo on a single server.

Only this repository is needed. Cargo downloads the custom Zakura fork from
`https://github.com/deltadevsde/zakura` at the commit pinned in `Cargo.toml`.
From `preconf/`:

```sh
./scripts/package.sh /tmp/preconf-hosting.tar.gz
scp /tmp/preconf-hosting.tar.gz user@YOUR_SERVER:/tmp/
```

On your server:

```sh
mkdir -p ~/preconf-demo
cd ~/preconf-demo
tar -xzf /tmp/preconf-hosting.tar.gz
cd preconf
```

The archive excludes build targets, Git directories, `.env`, runtime data and logs.
It contains source, not an image or private hosting credentials. The included
regtest fixture keys are public test keys. The source directory layout is:

```text
preconf-demo/
└── preconf/
```

## 2. Configure and start

Install Docker Engine with the Compose and Buildx plugins using
[Docker's installation instructions](https://docs.docker.com/engine/install/).
The Dockerfile builds the custom Rust binary, so the server does not need Rust.
Use an x86-64 Linux server for the tested configuration. Allow substantial disk
space for the Rust build cache; building from source is more demanding than running
the resulting image. Resource sizing depends on desired proof throughput; the
build uses four jobs and each process limits its worker pools to four threads.

For localhost-only access on the server:

```sh
docker compose up -d --build
```

Then use an SSH tunnel from your computer:

```sh
ssh -L 8080:127.0.0.1:8080 user@YOUR_SERVER
```

Open `http://127.0.0.1:8080`.

For a public forum link:

1. Point your domain's A record (and AAAA only if IPv6 is configured) to the server.
2. Allow inbound TCP 80 and 443.
3. Configure and start:

```sh
cp .env.example .env
# Edit DEMO_DOMAIN to your actual domain, without https:// or a path.
nano .env
docker compose --profile public up -d --build
```

Caddy provisions and renews the certificate. Open `https://YOUR_DOMAIN` and use
that URL in the forum post. Deep links such as `/block/42` and `/tx/HASH` work.
There is no need to expose miner P2P ports or RPC ports to the internet.
A domain/certificate has not been provisioned by this repository; that happens
on your server when you run the public profile.

## 3. Operation

```sh
docker compose ps
docker compose logs -f --tail 100 demo
docker compose logs --tail 100 web
curl -f http://127.0.0.1:8080/healthz
```

Readiness can take several minutes on a fresh chain. The first five blocks fund
independent payer and service notes; normal blocks then carry randomized batches.
`BLOCK_SECONDS` is minimum pacing, not a proof-throughput guarantee. Change
`PAYMENTS`, `PAYMENT_JITTER`, or `BLOCK_SECONDS` in `.env` and rerun `up -d`.
The supported payment range is 1–40 per batch. Defaults are 30 ± 10.

Compose restarts the demo if its supervisor exits. A restart reuses the named data
volume, the same chain ID, receipts, payout assignments, and explorer history.
Pending accepted transactions are drained before generating a new batch, so wallet
inputs are not accidentally reused. Use `docker compose stop` for a clean shutdown;
allow the configured two-minute grace period.

Updates: replace the source while preserving `.env`, then run
`docker compose --profile public up -d --build`. Back up first. Do not change the
regtest consensus configuration or fixture keys in an existing data volume.

## Backup and reset

For a consistent backup, stop writers first:

```sh
docker compose stop demo
docker compose cp demo:/data ./preconf-backup
docker compose start demo
```

The backup includes chain databases, service ledger, explorer index, and logs.
Monitor the volume's disk use: chain history and event logs are intentionally
retained. Docker's own container output has bounded rotation. The JSON ledger
and in-memory wallet caches favor simplicity over indefinite production scale.

To reset deliberately, `docker compose --profile public down -v` removes both
chain data and Caddy certificate volumes. This destroys existing explorer history
and breaks old transaction links; avoid resetting after sharing the forum URL.
