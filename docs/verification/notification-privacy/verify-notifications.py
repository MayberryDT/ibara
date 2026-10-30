import pathlib,json
p=pathlib.Path.cwd();results=[]
for mode in ['false','true']:
 rows=[json.loads(x) for x in (p/f'notification-{mode}.jsonl').read_text().splitlines()]
 assert len(rows)==4,(mode,len(rows))
 for r in rows:
  args=' '.join(r['argv']);assert 'CANARY' not in args and 'private-site' not in args
 assert sorted(r['argv'][-2] for r in rows)==sorted(['ibara','ibara needs your approval','ibara needs your answer','Sign-in needed'])
 log=(p/f'notification-{mode}.log').read_text()
 for ref in ['att_privacy_a','att_privacy_q','att_privacy_l']:assert 'OPEN_REVIEW io.zet.ibara' in log and '"ref":"'+ref+'"' in log
 results.append({'request_popups':mode=='true','desktop_notifications':mode=='true','spawned_notifications':len(rows),'private_canaries_in_arguments':False,'open_routes_verified':3})
(p/'notification-results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(results))
