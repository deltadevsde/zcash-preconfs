import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'scripts'))
from explorer import Index

class ExplorerTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.run=Path(self.tmp.name)
        (self.run/'manifest.json').write_text(json.dumps({'nodes':{'service':'node'},'api':'service','chain_id':'c'*64,'seed':'MUST-NOT-LEAK'}))
        self.hashes=['0'*64,'1'*64]
        self.ids=['a'*64,'b'*64]
        self.ledger={'tip':{'height':1,'hash':self.hashes[-1]},'records':[{'txid':self.ids[-1],'raw_tx':'MUST-NOT-LEAK','fee':10000,'receipt':{'payload':'signed','signature':'sig'},'status':'included','inclusion':{'height':1,'hash':self.hashes[-1]},'failure_reason':None}], 'payouts':[]}
        (self.run/'driver.jsonl').write_text(json.dumps({'time':100,'event':'mined','miner':'miner1','height':1,'block_hash':self.hashes[-1],'seed':'MUST-NOT-LEAK'})+'\n')
        self.index=Index(self.run)
    def tearDown(self):self.index.db.close();self.tmp.cleanup()
    def rpc(self,url,method,params=None):
        if method=='getblockcount':return len(self.hashes)-1
        if method=='getbestblockhash':return self.hashes[-1]
        if method=='getblockhash':return self.hashes[params[0]]
        if method=='getblock':
            h=self.hashes.index(params[0]);return {'tx':[self.ids[h]],'time':100+h,'size':1000}
        raise AssertionError(method)
    def update(self):
        with patch('explorer.request',side_effect=self.rpc),patch('explorer.get',return_value={'ledger':self.ledger}):self.index.update()
    def test_index_public_data_and_reopen(self):
        self.update()
        block=self.index.api('/api/block/1',{})
        self.assertEqual(block['preconf_count'],1)
        self.assertEqual(block['miner'],'miner1')
        self.assertEqual(block['observed_at'],100)
        serialized=json.dumps([self.index.api(p,{}) for p in ['/api/summary','/api/blocks','/api/txs','/api/activity','/api/tx/'+self.ids[-1]]])
        self.assertNotIn('MUST-NOT-LEAK',serialized)
        self.assertNotIn('raw_tx',serialized)
        self.assertIsNone(self.index.api('/api/sendrawtransaction',{}))
        reopened=Index(self.run)
        self.assertEqual(reopened.api('/api/tx/'+self.ids[-1],{})['height'],1)
        reopened.db.close()
    def test_archive_serves_saved_history_without_network_or_writes(self):
        self.update()
        with patch('explorer.request',side_effect=AssertionError('RPC forbidden')), patch('explorer.get',side_effect=AssertionError('HTTP forbidden')):
            archived=Index(self.run,archived=True)
            archived.update()
            summary=archived.api('/api/summary',{})
            self.assertTrue(summary['archived'])
            self.assertFalse(summary['stalled'])
            self.assertIsNone(summary['error'])
            self.assertEqual(summary['chain_id'],'c'*64)
            self.assertEqual(archived.api('/api/block/1',{})['miner'],'miner1')
            self.assertEqual(archived.api('/api/tx/'+self.ids[-1],{})['confirmations'],1)
            self.assertTrue(archived.api('/api/miners',{})['items'])
            with self.assertRaisesRegex(Exception,'readonly'):
                archived.db.execute('DELETE FROM blocks')
            archived.db.close()

    def test_reorg_removes_old_block_and_requeues_receipt(self):
        self.update();old=self.hashes[-1];old_tx=self.ids[-1]
        self.hashes[-1]='2'*64;self.ids[-1]='d'*64
        self.ledger['tip']['hash']=self.hashes[-1]
        self.ledger['records'][0].update(status='pending',inclusion=None)
        self.update()
        self.assertIsNone(self.index.api('/api/block/'+old,{}))
        tx=self.index.api('/api/tx/'+old_tx,{})
        self.assertEqual(tx['status'],'pending');self.assertIsNone(tx['height']);self.assertEqual(tx['confirmations'],0)
        self.assertEqual(self.index.api('/api/block/1',{})['hash'],self.hashes[-1])
    def test_inconsistent_snapshot_does_not_commit_new_blocks(self):
        self.update()
        self.hashes.append('3'*64);self.ids.append('e'*64)
        with self.assertRaisesRegex(RuntimeError,'chain view changed'):self.update()
        self.index.db.rollback()
        self.assertIsNone(self.index.api('/api/block/2',{}))

if __name__=='__main__':unittest.main()
