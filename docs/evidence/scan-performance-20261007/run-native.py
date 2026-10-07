import subprocess,os,json,time,sys
from pathlib import Path
name=sys.argv[1]; data=Path(sys.argv[2]); placement=sys.argv[3]
source=Path('/mnt/Henry/3dam-scan-sweep-20261007/source')
log=Path('/tmp/3dam-issue-implementation/validation')/(name+'.log')
output=Path('/tmp/3dam-issue-implementation')/(name+'.json')
args=['rtk','/mnt/Rusty/cargo-target-3dam-performance/release/examples/scan_performance','--backend','local','--source-root',str(source),'--data-dir',str(data),'--paths','4000','--placement',placement,'--topology','detected','--io-mib','0','--transfer-mib','0','--pressure-policy','production','--background','true','--foreground-writes','true','--co-tenant-cache','evict','--pause-before-change','true','--changed','1','--removed','1','--cancel-after-ms','50','--timeout-secs','600','--foreground-budget-ms','60','--output',str(output)]
if len(sys.argv)>4 and sys.argv[4]=='unchanged':
    args[args.index('--pause-before-change')+1]='false'
    args[args.index('--changed')+1]='0'
    args[args.index('--removed')+1]='0'
if len(sys.argv)>4 and sys.argv[4]=='trace':
    args=['rtk','strace','-f','-ttt','-T','-yy','-o',str(log.with_suffix('.strace')),'-e','trace=fsync,fdatasync,futex']+args[1:]
proc=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,bufsize=1)
with log.open('w') as f:
    f.write('COMMAND '+json.dumps(args)+'\n');f.flush()
    for line in proc.stdout:
        f.write(line);f.write('MARK '+str(time.time())+' '+line);f.flush()
        if 'Full baseline is complete.' in line:
            edit=source/'0/asset-00000.png'
            with edit.open('ab') as asset:
                asset.write(b'changed fixture revision\n');asset.flush();os.fsync(asset.fileno())
            missing=source/'0/asset-00002.png'
            if missing.exists():missing.unlink()
            proc.stdin.write('\n');proc.stdin.flush()
rc=proc.wait()
print(name,'exit',rc,'evidence',output,flush=True)
raise SystemExit(rc)
