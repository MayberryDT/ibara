"""Real uploader process against a local archive: retain on failure, hash ack on success."""
import tempfile,pathlib,json,threading,http.server,subprocess,sys,hashlib,os
out=pathlib.Path(sys.argv[1]);out.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory() as tmp:
 p=pathlib.Path(tmp);spool=p/'facts';spool.mkdir();f=spool/'raw/v1/2026/10/24/proof.jsonl.gz';f.parent.mkdir(parents=True);f.write_bytes(b'archive-fixture')
 token=p/'token';token.write_text('fixture-token');token.chmod(0o600)
 class Archive(http.server.BaseHTTPRequestHandler):
  fail=True
  def do_PUT(self):
   body=self.rfile.read(int(self.headers['Content-Length']));self.send_response(503 if self.fail else 200);self.end_headers()
   if not self.fail:self.wfile.write(json.dumps({'saved':True,'sha256':hashlib.sha256(body).hexdigest()}).encode())
  def log_message(self,*args):pass
 server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Archive);threading.Thread(target=server.serve_forever,daemon=True).start()
 argv=[sys.executable,'scripts/facts-upload.py','--dir',str(spool),'--url',f'http://127.0.0.1:{server.server_port}','--token-file',str(token)]
 failed=subprocess.run(argv,capture_output=True,text=True);assert failed.returncode==1,failed.stderr;assert f.exists()
 status=json.loads((spool/'.upload-status.json').read_text());assert status['ok'] is False
 Archive.fail=False
 saved=subprocess.run(argv,capture_output=True,text=True);assert saved.returncode==0,saved.stderr;assert not f.exists()
 status=json.loads((spool/'.upload-status.json').read_text());assert status['ok'] is True and status['saved_objects']==1
 assert 'fixture-token' not in json.dumps(status)
 (out/'uploader.json').write_text(json.dumps({'failed':json.loads(failed.stdout),'saved':json.loads(saved.stdout),'status':status},indent=2));server.shutdown()
