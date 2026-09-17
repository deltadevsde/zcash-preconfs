#!/usr/bin/env python3
"""Launch real regtest nodes, exercise preconfirmation, and retain evidence."""
import argparse
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT.parent / 'zakura/target/release/preconf-demo'
PROCESSES = []


def request(url, method, params=None, raw=False):
    data = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': method,
                       'params': [] if params is None else params}).encode()
    req = urllib.request.Request(url, data, {'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=240) as response:
        value = json.load(response)
    if raw:
        return value
    if value.get('error'):
        raise RuntimeError(f'{method}: {value["error"]}')
    return value['result']


def get(url):
    with urllib.request.urlopen(url, timeout=240) as response:
        return json.load(response)


def wait_for(label, check, timeout=240):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        for proc in PROCESSES:
            if proc.poll() is not None:
                raise RuntimeError(f'node exited ({proc.returncode}); inspect logs')
        try:
            value = check()
            if value:
                return value
        except (OSError, RuntimeError, KeyError) as error:
            last = error
        time.sleep(.5)
    raise RuntimeError(f'timed out: {label}; last error: {last}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--once', action='store_true', help='stop nodes after passing scenarios')
    parser.add_argument('--base-port', type=int, default=28400)
    args = parser.parse_args()
    run = ROOT / 'runs' / (time.strftime('%Y%m%d-%H%M%S') + '-' + secrets.token_hex(3))
    run.mkdir(parents=True)
    evidence = []

    def event(name, **fields):
        item = {'time': time.time(), 'event': name, **fields}
        evidence.append(item)
        with (run / 'driver.jsonl').open('a') as f:
            f.write(json.dumps(item) + '\n')
        print(json.dumps(item), flush=True)

    seeds = {name: f'{i:02x}' * 32 for i, name in enumerate(['service', 'miner1', 'miner2', 'outsider', 'merchant'], 1)}
    addresses = {name: subprocess.check_output([str(BINARY), 'address', '--seed', seed], text=True).strip()
                 for name, seed in seeds.items()}
    chain_id = secrets.token_hex(32)
    server_peer = '8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c'
    node_urls = {}
    api = f'http://127.0.0.1:{args.base_port + 50}'
    for index, name in enumerate(['service', 'miner1', 'miner2', 'outsider']):
        port = args.base_port + index * 10
        node_urls[name] = f'http://127.0.0.1:{port}'
        role = 'server' if name == 'service' else ('plain' if name == 'outsider' else 'miner')
        bootstrap = [] if index == 0 else [f'{server_peer}@127.0.0.1:{args.base_port+2}']
        node_config = run / f'{name}.toml'
        node_config.write_text(f'''
[network]
network = "Regtest"
listen_addr = "127.0.0.1:{port+1}"
p2p_stack = "zakura"
cache_dir = false
initial_testnet_peers = []
max_connections_per_ip = 16
zakura_node_secret_key = "{seeds[name]}"
[network.zakura]
listen_addr = "127.0.0.1:{port+2}"
bootstrap_peers = {json.dumps(bootstrap)}
trace_dir = "{run / (name+'-traces')}"
[network.zakura.header_sync]
status_refresh_interval = "1s"
[network.zakura.block_sync]
status_refresh_interval = "1s"
[state]
cache_dir = "{run / (name+'-chain')}"
[rpc]
listen_addr = "127.0.0.1:{port}"
enable_cookie_auth = false
debug_force_finished_sync = true
[mempool]
debug_enable_at_height = 0
[mining]
miner_address = "{addresses[name]}"
optimistic_block_inventory = false
[tracing]
filter = "info"
''')
        config = {'role': role, 'node_config': str(node_config), 'chain_id': chain_id,
                  'server_peer': server_peer, 'node_rpc': node_urls[name],
                  'api': f'127.0.0.1:{args.base_port+50}', 'database': str(run/'service.sqlite'),
                  'seed': seeds[name], 'miners': [addresses['miner1'], addresses['miner2']], 'min_fee_zat': 10000}
        config_path = run / f'{name}.json'
        config_path.write_text(json.dumps(config, indent=2))
        log = (run / f'{name}.jsonl').open('w')
        proc = subprocess.Popen([str(BINARY), 'run', '--config', str(config_path)], stdout=log, stderr=log,
                                env={**os.environ, 'RAYON_NUM_THREADS': '4', 'TOKIO_WORKER_THREADS': '4', 'RUST_LOG': 'info,zakura_rpc=debug,zakura_consensus=debug'})
        PROCESSES.append(proc)
    manifest = {'run': str(run), 'api': api, 'nodes': node_urls, 'addresses': addresses,
                'chain_id': chain_id, 'pids': [p.pid for p in PROCESSES]}
    (run / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    event('starting', **manifest)
    for name, url in node_urls.items():
        wait_for(f'{name} ready', lambda url=url: request(url, 'getblockcount') == 0)
    wait_for('service ready', lambda: request(api, 'preconf_info', {})['ready'])

    def sync(height):
        for name, url in node_urls.items():
            wait_for(f'{name} at height {height}', lambda url=url: request(url, 'getblockcount') == height)
        wait_for('service reconciliation', lambda: (info := request(api, 'preconf_info', {}))['ready'] and info['tip']['height'] == height)

    def mine(name):
        event('mining', miner=name)
        hash_ = request(node_urls[name], 'generate', [1])[0]
        height = request(node_urls[name], 'getblockcount')
        sync(height)
        event('mined', miner=name, height=height, block_hash=hash_)
        return hash_

    def create(seed_name, outputs, fee=20000, input_txid=None, label='payment'):
        event('building_transaction', label=label)
        command = [str(BINARY), 'create', '--rpc', node_urls['service'], '--seed', seeds[seed_name],
                   '--outputs', json.dumps(outputs), '--fee', str(fee)]
        if input_txid:
            command += ['--input-txid', input_txid]
        with (run / f'{label}-build.log').open('w') as log:
            tx = json.loads(subprocess.check_output(command, text=True, stderr=log,
                            env={**os.environ, 'RAYON_NUM_THREADS': '4'}, timeout=600))
        (run / f'{label}.json').write_text(json.dumps(tx))
        event('transaction_built', label=label, txid=tx['txid'])
        return tx

    def submit(tx):
        result = request(api, 'preconf_submit', {'raw_tx': tx['raw_tx']})
        assert result['status'] == 'pending', result
        retry = request(api, 'preconf_submit', {'raw_tx': tx['raw_tx']})
        assert result['receipt'] == retry['receipt']
        key = request(api, 'preconf_info', {})['service_pubkey']
        verified = json.loads(subprocess.check_output([str(BINARY), 'verify-receipt', '--key', key, '--receipt', json.dumps(result['receipt'])], text=True))
        assert verified['txid'] == tx['txid'] and verified['chain_id'] == chain_id
        received = json.loads(subprocess.check_output([str(BINARY), 'inspect', '--seed', seeds['merchant'], '--raw-tx', tx['raw_tx']], text=True))
        assert received['received_zat'] == 100000
        # Ensure native fetch and miner validation occurred before mining.
        for miner in ['miner1', 'miner2']:
            wait_for(f'{miner} fetched payment', lambda miner=miner: tx['txid'] in json.dumps(request(node_urls[miner], 'getblocktemplate', [{}])))
        event('preconfirmed', txid=tx['txid'], receipt=result['receipt'])
        return result

    def settle(tx, miner, reorg=False):
        inclusion = mine(miner)
        status = request(api, 'preconf_get', {'txid': tx['txid']})
        assert status['status'] == 'included' and status['inclusion']['confirmations'] == 1, status
        state = get(api + '/state')['ledger']
        payout = next(p for p in state['payouts'] if tx['txid'] in p['txids'])
        assert payout['raw_tx'] is None, 'paid before two confirmations'
        if reorg:
            old_height = status['inclusion']['height']
            for name in ['miner1', 'miner2', 'outsider', 'service']:
                request(node_urls[name], 'invalidateblock', [inclusion])
            sync(old_height - 1)
            reverted = request(api, 'preconf_get', {'txid': tx['txid']})
            assert reverted['status'] == 'pending', reverted
            assert not any(p['block']['hash'] == inclusion for p in get(api+'/state')['ledger']['payouts'])
            event('reorg_recovered', orphaned_block=inclusion, txid=tx['txid'])
            submit(tx)  # Wait for the requeued transaction to reach miner templates.
            inclusion = mine(miner)
            assert request(api, 'preconf_get', {'txid': tx['txid']})['status'] == 'included'
        mine('service')
        def paid():
            return next((p for p in get(api+'/state')['ledger']['payouts'] if tx['txid'] in p['txids'] and p['status']=='broadcast'), None)
        payout = wait_for('98% payout broadcast', paid, timeout=600)
        assert payout['amount'] == 9800 and payout['address'] == addresses[miner], payout
        received = json.loads(subprocess.check_output([str(BINARY), 'inspect', '--seed', seeds[miner], '--raw-tx', payout['raw_tx']], text=True))
        assert received['received_zat'] == 9800
        wait_for('payout propagated', lambda: payout['txid'] in request(node_urls[miner], 'getrawmempool'))
        mine(miner)
        wait_for('payout confirmed', lambda: any(p['txid']==payout['txid'] and p['status']=='confirmed' for p in get(api+'/state')['ledger']['payouts']))
        event('payout_verified', payment_txid=tx['txid'], block_hash=inclusion, payout_txid=payout['txid'], miner=miner, amount_zat=9800)

    # Shielded coinbase funds mature through the ordinary Ironwood anchor rules.
    mine('service')
    mine('miner1')
    mine('miner1')
    normal = create('miner1', [(addresses['merchant'], 100000), (addresses['service'], 10000)], label='normal')
    submit(normal)
    settle(normal, 'miner1', reorg=True)

    accepted = create('miner1', [(addresses['merchant'], 100000), (addresses['service'], 10000)], label='accepted-conflict')
    attack = create('miner1', [(addresses['outsider'], 100000)], fee=100000, input_txid=accepted['input_txid'], label='higher-fee-double-spend')
    request(node_urls['miner1'], 'sendrawtransaction', [attack['raw_tx']])
    for miner in ['miner1', 'miner2']:
        wait_for(f'attack in {miner} mempool', lambda miner=miner: attack['txid'] in request(node_urls[miner], 'getrawmempool'))
    submit(accepted)
    rejection = request(api, 'preconf_submit', {'raw_tx': attack['raw_tx']}, raw=True)
    assert rejection['error']['data']['reason']=='conflict', rejection
    settle(accepted, 'miner2')
    assert request(api, 'preconf_get', {'txid': accepted['txid']})['status']=='included'
    doomed = create('miner1', [(addresses['merchant'], 100000), (addresses['service'], 10000)], label='nonparticipating-payment')
    outside_attack = create('miner1', [(addresses['outsider'], 100000)], fee=100000, input_txid=doomed['input_txid'], label='nonparticipating-double-spend')
    request(node_urls['outsider'], 'sendrawtransaction', [outside_attack['raw_tx']])
    wait_for('outsider has conflicting transaction', lambda: outside_attack['txid'] in request(node_urls['outsider'], 'getrawmempool'))
    submit(doomed)
    mine('outsider')
    failed = request(api, 'preconf_get', {'txid': doomed['txid']})
    assert failed['status'] == 'failed' and failed['failure_reason'] == 'conflict', failed
    event('nonparticipating_conflict_verified', txid=doomed['txid'], conflicting_txid=outside_attack['txid'])

    # Restart the singleton service; live miners retain and resupply the chain.
    old_receipt = request(api, 'preconf_get', {'txid': normal['txid']})['receipt']
    old_payouts = get(api+'/state')['ledger']['payouts']
    old_height = request(node_urls['miner1'], 'getblockcount')
    service = PROCESSES.pop(0)
    service.send_signal(signal.SIGINT)
    service.wait(timeout=60)
    log = (run / 'service.jsonl').open('a')
    restarted = subprocess.Popen([str(BINARY), 'run', '--config', str(run/'service.json')], stdout=log, stderr=log, env={**os.environ, 'RAYON_NUM_THREADS':'4', 'TOKIO_WORKER_THREADS':'4', 'RUST_LOG':'info,zakura_rpc=debug,zakura_consensus=debug'})
    PROCESSES.insert(0, restarted)
    sync(old_height)
    wait_for('service recovered after restart', lambda: request(api, 'preconf_info', {})['ready'])
    assert request(api, 'preconf_submit', {'raw_tx': normal['raw_tx']})['receipt'] == old_receipt
    recovered_payouts = get(api+'/state')['ledger']['payouts']
    assert [p['txid'] for p in recovered_payouts] == [p['txid'] for p in old_payouts]
    manifest['pids'] = [p.pid for p in PROCESSES]
    (run / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    event('restart_verified', payout_txids=[p['txid'] for p in recovered_payouts])
    ledger = get(api+'/state')['ledger']
    assigned = [txid for p in ledger['payouts'] for txid in p['txids']]
    assert len(assigned) == len(set(assigned)) == 2
    (run / 'result.json').write_text(json.dumps({'passed': True, 'scenarios': ['normal-payment', 'duplicate-submit', 'higher-fee-conflict-first', 'service-conflict-rejection', 'two-confirmation-payout', 'both-miners-paid', 'no-double-payment', 'reorg-before-payout', 'nonparticipating-miner-conflict', 'restart-recovery'], 'ledger': ledger}, indent=2))
    event('demo_passed', evidence=str(run/'result.json'), metrics=api+'/metrics')
    if not args.once:
        print('Demo continues generating payments and double-spend attempts. Ctrl-C stops its nodes.', flush=True)
        cycle = 0
        while True:
            time.sleep(5)
            miner = 'miner1' if cycle % 2 == 0 else 'miner2'
            tx = create('miner1', [(addresses['merchant'], 100000), (addresses['service'], 10000)], label=f'live-{cycle}')
            if cycle % 3 == 0:
                conflict = create('miner1', [(addresses['outsider'], 100000)], fee=100000, input_txid=tx['input_txid'], label=f'live-conflict-{cycle}')
                request(node_urls[miner], 'sendrawtransaction', [conflict['raw_tx']])
                wait_for('live conflict in mempool', lambda: conflict['txid'] in request(node_urls[miner], 'getrawmempool'))
            submit(tx)
            settle(tx, miner)
            cycle += 1



if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        print('Demo stopped; run logs are retained.', flush=True)
    finally:
        for proc in PROCESSES:
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
        for proc in PROCESSES:
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
