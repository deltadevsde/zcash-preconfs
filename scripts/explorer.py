#!/usr/bin/env python3
"""Read-only, incrementally indexed explorer. No public node RPC proxy."""
import argparse
import json
import re
import sqlite3
import threading
import time
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse, parse_qs
from demo import request, get

WEB = Path(__file__).resolve().parents[1] / 'web'

class Index:
    def __init__(self, run):
        self.run = Path(run)
        self.lock = threading.RLock()
        self.db = sqlite3.connect(self.run/'explorer.sqlite', check_same_thread=False)
        self.db.executescript('''PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS blocks(height INTEGER PRIMARY KEY, hash TEXT UNIQUE, data TEXT);
          CREATE TABLE IF NOT EXISTS transactions(txid TEXT PRIMARY KEY, height INTEGER, position INTEGER);
          CREATE INDEX IF NOT EXISTS tx_height ON transactions(height,position);
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, data TEXT);''')
        self.state = {'records': [], 'payouts': []}
        row = self.db.execute("SELECT data FROM metadata WHERE key='ledger'").fetchone()
        if row: self.state=json.loads(row[0])
        self.last_ok = 0
        self.last_block = time.time()
        self.error = 'Connecting to demo'
        self.manifest = {}
        self.events = []
        self.event_offset = 0
        self.miners = {}
        self.observed = {}

    def read_events(self):
        path=self.run/'driver.jsonl'
        if not path.exists(): return
        with path.open() as f:
            f.seek(self.event_offset)
            for line in f:
                event=json.loads(line)
                if event.get('event')=='mined':
                    self.miners[event['block_hash']]=event['miner']
                    self.observed[event['block_hash']]=event['time']
                # Explicitly allowlist fields: never expose raw transactions or wallet fixtures.
                clean={k:event[k] for k in ['time','event','txid','miner','height','block_hash','count','accepted','conflicts','reason','phase'] if k in event}
                self.events.append(clean)
            self.event_offset=f.tell()
        self.events=self.events[-200:]

    def update(self):
        manifest=json.loads((self.run/'manifest.json').read_text())
        url=manifest['nodes']['service']
        ledger=get(manifest['api']+'/state')['ledger']
        tip=request(url,'getblockcount')
        with self.lock:
            self.manifest=manifest
            self.read_events()
            # Roll back indexed orphan blocks before inserting replacements.
            while row:=self.db.execute('SELECT height,hash FROM blocks ORDER BY height DESC LIMIT 1').fetchone():
                h,hash_=row
                if h<=tip and request(url,'getblockhash',[h])==hash_: break
                self.db.execute('DELETE FROM transactions WHERE height=?',(h,))
                self.db.execute('DELETE FROM blocks WHERE height=?',(h,))
            start=self.db.execute('SELECT COALESCE(MAX(height),-1)+1 FROM blocks').fetchone()[0]
            for h in range(start,tip+1):
                hash_=request(url,'getblockhash',[h])
                b=request(url,'getblock',[hash_,1])
                txids=[t if isinstance(t,str) else t['txid'] for t in b['tx']]
                clean={'height':h,'hash':hash_,'time':b.get('time'), 'size':b.get('size'), 'txids':txids, 'previous':b.get('previousblockhash')}
                self.db.execute('INSERT INTO blocks VALUES(?,?,?)',(h,hash_,json.dumps(clean)))
                self.db.executemany('INSERT OR REPLACE INTO transactions VALUES(?,?,?)',[(txid,h,i) for i,txid in enumerate(txids)])
                self.last_block=time.time()
            indexed_tip=self.db.execute('SELECT hash FROM blocks WHERE height=?',(tip,)).fetchone()[0]
            if request(url,'getbestblockhash')!=indexed_tip or ledger['tip']['hash']!=indexed_tip:
                raise RuntimeError('chain view changed during indexing')
            self.state={'tip':ledger['tip'],'records':[{k:r[k] for k in ['txid','status','fee','receipt','inclusion','failure_reason']} for r in ledger['records']], 'payouts':[{k:p[k] for k in ['block','amount','txids','txid','status','reason']} for p in ledger['payouts']]}
            self.db.execute("INSERT OR REPLACE INTO metadata VALUES('ledger',?)",(json.dumps(self.state),))
            self.db.commit()
            self.last_ok=time.time()
            self.error=None

    def loop(self):
        while True:
            try: self.update()
            except Exception as e:
                with self.lock:
                    self.db.rollback()
                    self.error='Demo unavailable; showing last indexed state'
                print(json.dumps({'event':'explorer.index_error','error':str(e)}),flush=True)
            time.sleep(2)

    def maps(self):
        return ({r['txid']:r for r in self.state['records']}, {tx:p for p in self.state['payouts'] for tx in p['txids']}, {p['txid']:p for p in self.state['payouts'] if p['txid']})

    def tx(self, txid):
        records,assigned,payouts=self.maps()
        row=self.db.execute('SELECT height,position FROM transactions WHERE txid=?',(txid,)).fetchone()
        r=records.get(txid)
        if row is None and r is None and txid not in payouts: return None
        h,pos=row if row else (None,None)
        block=json.loads(self.db.execute('SELECT data FROM blocks WHERE height=?',(h,)).fetchone()[0]) if h is not None else None
        kind='preconf' if r else ('payout' if txid in payouts else ('coinbase' if pos==0 else 'shielded'))
        tip=self.db.execute('SELECT COALESCE(MAX(height),0) FROM blocks').fetchone()[0]
        return {'txid':txid,'height':h,'block_hash':block['hash'] if block else None,'confirmations':tip-h+1 if h is not None else 0,'kind':kind,'status':r['status'] if r else ('included' if row else 'pending'),'miner':self.miners.get(block['hash']) if block else None,'preconf':r,'payout':assigned.get(txid) or payouts.get(txid)}

    def block(self, value):
        is_height=value.isdigit() and len(value)<=10
        row=self.db.execute('SELECT data FROM blocks WHERE height=?' if is_height else 'SELECT data FROM blocks WHERE hash=?',(int(value) if is_height else value,)).fetchone()
        if not row:return None
        b=json.loads(row[0]);records,_,_=self.maps()
        b['miner']=self.miners.get(b['hash'],'genesis' if b['height']==0 else 'unknown')
        b['observed_at']=self.observed.get(b['hash'])
        b['preconf_count']=sum(t in records for t in b['txids'])
        b['service_fees']=sum(records[t]['fee'] for t in b['txids'] if t in records)
        b['payouts']=[p for p in self.state['payouts'] if p['block']['hash']==b['hash']]
        b['confirmations']=self.db.execute('SELECT MAX(height) FROM blocks').fetchone()[0]-b['height']+1
        return b

    def api(self,path,query):
        with self.lock:
            page=max(0,min(int(query.get('page',['0'])[0]),1000000))
            tip=self.db.execute('SELECT COALESCE(MAX(height),0) FROM blocks').fetchone()[0]
            if path=='/api/summary':
                records=self.state['records'];payouts=self.state['payouts']
                return {'height':tip,'chain_id':self.manifest.get('chain_id'),'indexed_at':self.last_ok,'last_block_at':self.last_block,'error':self.error,'stalled':time.time()-self.last_block>180,'records':len(records),'pending':sum(r['status']=='pending' for r in records),'included':sum(r['status']=='included' for r in records),'failed':sum(r['status']=='failed' for r in records),'paid':sum(p['amount'] for p in payouts if p['status']=='confirmed'),'transactions':self.db.execute('SELECT COUNT(*) FROM transactions').fetchone()[0],'target_payments':self.manifest.get('target_payments',30)}
            if path=='/api/blocks':
                rows=self.db.execute('SELECT height FROM blocks ORDER BY height DESC LIMIT 25 OFFSET ?',(page*25,)).fetchall()
                return {'items':[self.block(str(r[0])) for r in rows],'page':page,'has_more':tip+1>(page+1)*25}
            if path.startswith('/api/block/'):return self.block(path.rsplit('/',1)[1])
            if path.startswith('/api/tx/'):return self.tx(path.rsplit('/',1)[1])
            if path=='/api/txs':
                mode=query.get('filter',['all'])[0]
                if mode=='pending': ids=[r['txid'] for r in reversed(self.state['records']) if r['status']=='pending']
                elif mode=='preconf': ids=[r['txid'] for r in reversed(self.state['records'])]
                else: ids=[r[0] for r in self.db.execute('SELECT txid FROM transactions ORDER BY height DESC,position DESC LIMIT 26 OFFSET ?',(page*25,))]
                if mode!='all':ids=ids[page*25:(page+1)*25+1]
                return {'items':[self.tx(t) for t in ids[:25]],'page':page,'has_more':len(ids)>25}
            if path=='/api/miners':
                items=[]
                for miner in ['miner1','miner2','service','outsider']:
                    blocks=[self.block(str(r[0])) for r in self.db.execute('SELECT height FROM blocks')]
                    blocks=[b for b in blocks if b['miner']==miner]
                    payouts=[p for b in blocks for p in b['payouts']]
                    items.append({'name':miner,'blocks':len(blocks),'payments':sum(b['preconf_count'] for b in blocks),'earned':sum(p['amount'] for p in payouts),'paid':sum(p['amount'] for p in payouts if p['status']=='confirmed'),'recent_blocks':[b['height'] for b in blocks[-10:]][::-1]})
                return {'items':items}
            if path=='/api/activity':return {'items':list(reversed(self.events[-60:]))}
            return None

def serve(run,host,port):
    index=Index(run)
    threading.Thread(target=index.loop,daemon=True).start()
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            parsed=urlparse(self.path);path=parsed.path
            try:
                if path=='/healthz':
                    healthy=index.last_ok>0 and time.time()-index.last_ok<30
                    return self.send(200 if healthy else 503,json.dumps({'healthy':healthy}).encode(),'application/json')
                if path.startswith('/api/'):
                    value=index.api(path,parse_qs(parsed.query))
                    return self.send(200 if value is not None else 404,json.dumps(value if value is not None else {'error':'Not found'}).encode(),'application/json')
                assets={'/app.js':('app.js','text/javascript'),'/style.css':('style.css','text/css')}
                if path in assets:
                    name,mime=assets[path];return self.send(200,(WEB/name).read_bytes(),mime)
                if re.fullmatch(r'/(?:|blocks|txs|pending|miners|activity|protocol|block/(?:[0-9]+|[a-f0-9]{64})|tx/[a-f0-9]{64}|miner/(?:miner1|miner2|service|outsider))',path):
                    html=(WEB/'index.html').read_text()
                    if path=='/protocol':
                        html=html.replace('<p class="empty">Loading chain data…</p>',(WEB/'protocol.html').read_text())
                    return self.send(200,html.encode(),'text/html')
                self.send(404,b'Not found','text/plain')
            except (ValueError,OverflowError):self.send(400,b'Invalid request','text/plain')
            except Exception as e:
                print(json.dumps({'event':'explorer.request_error','error':str(e)}),flush=True)
                self.send(503,b'Explorer temporarily unavailable','text/plain')
        def send(self,status,body,mime):
            self.send_response(status)
            self.send_header('Content-Type',mime+'; charset=utf-8')
            self.send_header('Content-Length',str(len(body)))
            self.send_header('Cache-Control','no-store')
            self.send_header('X-Content-Type-Options','nosniff')
            self.send_header('Content-Security-Policy',"default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'")
            self.end_headers();self.wfile.write(body)
        def log_message(self,*args):pass
    server=ThreadingHTTPServer((host,port),Handler)
    server.daemon_threads=True
    print(f'Explorer: http://{host}:{port}',flush=True)
    server.serve_forever()

if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--run',required=True)
    parser.add_argument('--host',default='127.0.0.1')
    parser.add_argument('--port',type=int,default=8080)
    args=parser.parse_args();serve(args.run,args.host,args.port)
