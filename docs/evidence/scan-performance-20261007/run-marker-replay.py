import sqlite3,shutil,time,json,random,sys
from pathlib import Path
root=Path('/tmp/3dam-issue-implementation/marker-replay');root.mkdir(exist_ok=True)
fixture=Path('/mnt/Rusty/3dam-scan-sweep-20261007/generated-100k-after/library.db')
reports=[]
def io():
    return {a:int(b) for a,b in (line.split(':') for line in Path('/proc/self/io').read_text().splitlines())}
for strategy in ['legacy_marker','private_spool']:
  for order in ['insertion','shuffled_seed_42']:
    db=root/(strategy+'-'+order+'.db');shutil.copyfile(fixture,db)
    spool=root/(strategy+'-'+order+'-spool.db')
    if spool.exists():spool.unlink()
    c=sqlite3.connect(db,isolation_level=None)
    c.execute('PRAGMA journal_mode=WAL');c.execute('PRAGMA synchronous=NORMAL')
    c.execute('ATTACH DATABASE ? AS scan_spool',(str(spool),))
    c.executescript('PRAGMA scan_spool.journal_mode=OFF; PRAGMA scan_spool.synchronous=OFF; PRAGMA scan_spool.cache_size=-1024; CREATE TABLE scan_spool.observed(source_id BLOB NOT NULL,generation INTEGER NOT NULL,asset_rowid INTEGER NOT NULL,PRIMARY KEY(source_id,generation,asset_rowid)) WITHOUT ROWID;')
    sid,generation=c.execute('SELECT id,scan_generation+1 FROM source LIMIT 1').fetchone()
    c.execute('UPDATE source SET scan_generation=? WHERE id=?',(generation,sid))
    rows=c.execute('SELECT rowid,path FROM asset WHERE source_id=? AND (flags & 1)=0 ORDER BY rowid LIMIT 20000',(sid,)).fetchall()
    assert len(rows)==20000
    if order.startswith('shuffled'):random.Random(42).shuffle(rows)
    c.execute('PRAGMA wal_checkpoint(TRUNCATE)'); before=io();start=time.perf_counter();changes=c.total_changes
    commits=0
    for offset in range(0,len(rows),128):
      c.execute('BEGIN IMMEDIATE')
      for rowid,path in rows[offset:offset+128]:
        if strategy=='legacy_marker':
          c.execute('UPDATE asset SET seen_generation=?3,flags=flags & -2 WHERE source_id=?1 AND path=?2 AND seen_generation<=?3',(sid,path,generation))
        else:
          found=c.execute('SELECT rowid,size_bytes,source_modified_at,flags FROM asset WHERE source_id=?1 AND path=?2 AND ?3=(SELECT scan_generation FROM source WHERE id=?1)',(sid,path,generation)).fetchone()
          assert found and found[3]&1==0
          c.execute('INSERT OR IGNORE INTO scan_spool.observed(source_id,generation,asset_rowid) VALUES(?,?,?)',(sid,generation,found[0]))
      c.execute('COMMIT');commits+=1
    elapsed=(time.perf_counter()-start)*1000;after=io();wal=Path(str(db)+'-wal');wal_bytes=wal.stat().st_size
    busy,frames,checkpointed=c.execute('PRAGMA wal_checkpoint(PASSIVE)').fetchone();page=c.execute('PRAGMA page_size').fetchone()[0]
    reports.append(dict(strategy=strategy,order=order,rows=len(rows),explicit_commits=commits,asset_rows_updated=len(rows) if strategy=='legacy_marker' else 0,elapsed_ms=elapsed,sql_total_changes=c.total_changes-changes,process_io={k:after[k]-before[k] for k in before},main_wal_end_bytes=wal_bytes,main_checkpoint_frames=frames,main_checkpoint_logical_bytes=checkpointed*page,spool_file_bytes=spool.stat().st_size))
    c.close()
result=dict(revision='9b43439226d6e89ab06b9bd2c1ec24a6d4305a0b',sqlite_version=sqlite3.sqlite_version,scope='Production observation SQL replay; 20k verified rows copied from the 100k production fixture under current schema V29. Excludes scan walking, progress and final missing reconciliation. Legacy SQL is the old asset marker, current SQL is the private observation spool. Warm NVMe-backed /tmp, WAL/NORMAL, default 1000-page autocheckpoint, chunks128, shuffled seed42. Not physical HDD.',profiles=reports)
Path('/tmp/3dam-issue-implementation/marker-replay.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
