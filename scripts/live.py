#!/usr/bin/env python3
"""Persistent local devnet with randomized batches and a read-only explorer."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import random
import selectors
import signal
import subprocess
import sys
import time
import fcntl
from demo import ROOT, BINARY, PROCESSES, request, get, wait_for


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--run-dir',default=str(ROOT/'data/live'))
    parser.add_argument('--base-port',type=int,default=28400)
    parser.add_argument('--explorer-port',type=int,default=8080)
    parser.add_argument('--host',default='127.0.0.1')
    parser.add_argument('--payments',type=int,default=30)
    parser.add_argument('--jitter',type=int,default=10)
    parser.add_argument('--block-seconds',type=float,default=30)
    parser.add_argument('--blocks',type=int,default=0,help='stop after N workload blocks; 0 keeps running')
    args=parser.parse_args()
    if not 1<=args.payments-args.jitter<=args.payments+args.jitter<=40:parser.error('payment range must be within 1..40')
    if args.block_seconds<1:parser.error('block-seconds must be positive')
    run=Path(args.run_dir).resolve();run.mkdir(parents=True,exist_ok=True)
    lock=(run/'run.lock').open('w');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    env={**os.environ,'RAYON_NUM_THREADS':'4','TOKIO_WORKER_THREADS':'4','RUST_LOG':'info'}
    def event(name,**fields):
        d={'time':time.time(),'event':name,**fields}
        with (run/'driver.jsonl').open('a') as f:f.write(json.dumps(d)+'\n')
        print(json.dumps(d),flush=True)
    seeds={n:f'{i:02x}'*32 for i,n in enumerate(['service','miner1','miner2','outsider','merchant'],1)}
    addresses={n:subprocess.check_output([str(BINARY),'address','--seed',s],text=True).strip() for n,s in seeds.items()}
    saved=json.loads((run/'manifest.json').read_text()) if (run/'manifest.json').exists() else {}
    chain_id=saved.get('chain_id') or os.urandom(32).hex()
    urls={n:f'http://127.0.0.1:{args.base_port+i*10}' for i,n in enumerate(['service','miner1','miner2'])}
    api=f'http://127.0.0.1:{args.base_port+50}'
    peer='8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c'
    for i,name in enumerate(urls):
        port=args.base_port+i*10
        peers=[] if i==0 else [f'{peer}@127.0.0.1:{args.base_port+2}']
        node=run/f'{name}.toml'
        node.write_text(f'''[network]
network = "Regtest"
listen_addr = "127.0.0.1:{port+1}"
p2p_stack = "zakura"
cache_dir = false
initial_testnet_peers = []
max_connections_per_ip = 16
zakura_node_secret_key = "{seeds[name]}"
[network.zakura]
listen_addr = "127.0.0.1:{port+2}"
bootstrap_peers = {json.dumps(peers)}
trace_dir = "{run/(name+'-traces')}"
[network.zakura.header_sync]
status_refresh_interval = "1s"
[network.zakura.block_sync]
status_refresh_interval = "1s"
[state]
cache_dir = "{run/(name+'-chain')}"
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
        config={'role':'server' if i==0 else 'miner','node_config':str(node),'chain_id':chain_id,'server_peer':peer,'node_rpc':urls[name],'api':f'127.0.0.1:{args.base_port+50}','database':str(run/'service.sqlite'),'seed':seeds[name],'miners':[addresses['miner1'],addresses['miner2']],'min_fee_zat':10000}
        path=run/f'{name}.json';path.write_text(json.dumps(config))
        log=(run/f'{name}.jsonl').open('a')
        PROCESSES.append(subprocess.Popen([str(BINARY),'run','--config',str(path)],stdout=log,stderr=log,env=env))
    manifest={'run':str(run),'api':api,'nodes':urls,'addresses':addresses,'chain_id':chain_id,'pids':[p.pid for p in PROCESSES],'target_payments':args.payments}
    (run/'manifest.json').write_text(json.dumps(manifest,indent=2))
    explorer=subprocess.Popen([sys.executable,str(ROOT/'scripts/explorer.py'),'--run',str(run),'--host',args.host,'--port',str(args.explorer_port)],stdout=(run/'explorer.jsonl').open('a'),stderr=subprocess.STDOUT)
    PROCESSES.append(explorer)
    event('starting',phase='connecting')
    for url in urls.values():wait_for('node RPC ready',lambda url=url:request(url,'getblockcount')>=0)
    wait_for('service ready',lambda:request(api,'preconf_info',{})['ready'],timeout=1800)
    def sync(height):
        for url in urls.values():wait_for('chain synchronization',lambda url=url:request(url,'getblockcount')==height)
        wait_for('service reconciliation',lambda:(s:=request(api,'preconf_info',{}))['ready'] and s['tip']['height']==height)
    def mine(miner):
        hash_=request(urls[miner],'generate',[1])[0]
        height=request(urls[miner],'getblockcount')
        event('mined',miner=miner,height=height,block_hash=hash_)
        sync(height)
        return hash_,height
    wallet=subprocess.Popen([str(BINARY),'wallet','--rpc',urls['service']],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=(run/'wallet.jsonl').open('a'),text=True,bufsize=1,env=env)
    PROCESSES.append(wallet)
    def build(specs):
        wallet.stdin.write(json.dumps(specs)+'\n');wallet.stdin.flush()
        with selectors.DefaultSelector() as sel:
            sel.register(wallet.stdout,selectors.EVENT_READ)
            if not sel.select(600):raise RuntimeError('wallet proof generation timed out')
        response=json.loads(wallet.stdout.readline())
        if 'error' in response:raise RuntimeError(response['error'])
        return response['result']
    def fund_notes(owner="miner1"):
        event('funding',phase='splitting wallet notes')
        tx=build([{'seed':seeds[owner],'outputs':[(addresses[owner],2000000)]*60,'fee':500000}])[0]['payment']
        request(urls[owner],'sendrawtransaction',[tx['raw_tx']])
        wait_for('funding in mempool',lambda:tx['txid'] in request(urls[owner],'getrawmempool'))
        mine(owner)
    if request(urls['service'],'getblockcount')==0:
        mine('service');mine('miner1');mine('miner1');fund_notes();fund_notes('service')
    else:
        sync(max(request(u,'getblockcount') for u in urls.values()))
        # Drain any accepted payments left by an interrupted workload before reserving new notes.
        pending=[r['txid'] for r in get(api+'/state')['ledger']['records'] if r['status']=='pending']
        if pending:
            wait_for('recovered pending set',lambda:all(t in json.dumps(request(urls['miner1'],'getblocktemplate',[{}])) for t in pending))
            mine('miner1')
    completed=0
    while not args.blocks or completed<args.blocks:
        start=time.monotonic();count=random.randint(args.payments-args.jitter,args.payments+args.jitter)
        event('building_batch',count=count,phase='proving Ironwood payments')
        specs=[{'seed':seeds['miner1'],'outputs':[(addresses['merchant'],random.randint(50000,200000)),(addresses['service'],random.choice([10000,15000,20000]))],'fee':20000,'conflict':random.random()<.1} for _ in range(count)]
        try: batch=build(specs)
        except RuntimeError as e:
            if 'no available confirmed note' not in str(e):raise
            fund_notes();batch=build(specs)
        miner=random.choice(['miner1','miner2'])
        def submit(item):
            time.sleep(random.uniform(0,.8))
            tx=item['payment'];conflict=item.get('conflict')
            if conflict:
                for url in [urls['miner1'],urls['miner2']]:
                    reply=request(url,'sendrawtransaction',[conflict['raw_tx']],raw=True)
                    error=reply.get('error')
                    if error and not any(word in error.get('message','') for word in ['already queued','already exists']):
                        raise RuntimeError(str(error))
                for url in [urls['miner1'],urls['miner2']]:wait_for('conflict in mempool',lambda url=url:conflict['txid'] in request(url,'getrawmempool'))
            result=request(api,'preconf_submit',{'raw_tx':tx['raw_tx']})
            assert result['status']=='pending',result
            if conflict:
                rejection=request(api,'preconf_submit',{'raw_tx':conflict['raw_tx']},raw=True)
                assert rejection.get('error',{}).get('data',{}).get('reason')=='conflict',rejection
                event('double_spend_rejected',txid=tx['txid'],reason='reserved nullifier')
            return tx['txid']
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:ids=list(pool.map(submit,batch))
        event('batch_accepted',count=len(ids),conflicts=sum('conflict' in i for i in batch))
        def template_ready():
            template=request(urls[miner],'getblocktemplate',[{}])
            hashes={t['hash'] for t in template['transactions']}
            return all(t in hashes for t in ids)
        wait_for('miner priority template contains batch',template_ready,timeout=300)
        time.sleep(max(0,args.block_seconds-(time.monotonic()-start)))
        hash_,height=mine(miner)
        included=request(urls['service'],'getblock',[hash_,1])['tx']
        assert all(t in included for t in ids),'accepted transaction missing from block'
        event('batch_verified',count=len(ids),miner=miner,height=height,block_hash=hash_)
        completed+=1
        (run/'workload-result.json').write_text(json.dumps({'passed':True,'blocks_verified':completed,'last_height':height,'last_count':len(ids),'chain_id':chain_id},indent=2))
    # Finish outstanding revenue-share transactions for bounded verification runs.
    if args.blocks:
        deadline=time.monotonic()+600
        while not all(p['status']=='confirmed' for p in get(api+'/state')['ledger']['payouts']):
            if time.monotonic()>deadline:raise RuntimeError('payout settlement timed out')
            mine('miner1')
            time.sleep(3)
        ledger=get(api+'/state')['ledger']
        assert all(p['amount']==sum(r['fee'] for r in ledger['records'] if r['txid'] in p['txids'])*98//100 for p in ledger['payouts'])
        event('workload_passed',count=sum(r['status']=='included' for r in ledger['records']))
        (run/'workload-ledger.json').write_text(json.dumps(ledger))

if __name__=='__main__':
    signal.signal(signal.SIGTERM,lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    try:main()
    except KeyboardInterrupt:print('Stopping demo; persistent chain and explorer data retained.',flush=True)
    finally:
        for p in reversed(PROCESSES):
            if p.poll() is None:p.send_signal(signal.SIGINT)
        for p in PROCESSES:
            try:p.wait(timeout=45)
            except subprocess.TimeoutExpired:p.kill();p.wait()
