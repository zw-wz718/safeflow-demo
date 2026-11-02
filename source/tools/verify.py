"""Verify every published file against the repository's public manifest."""
from pathlib import Path, PurePosixPath
import hashlib
import json

ROOT = Path(__file__).resolve().parents[2]
manifest = json.loads((ROOT / 'PUBLIC-MANIFEST.json').read_text('utf8'))
for row in manifest['files']:
    rel = PurePosixPath(row['path'])
    if rel.is_absolute() or '..' in rel.parts:
        raise ValueError('unsafe manifest path')
    p = ROOT / rel
    data = p.read_bytes()
    if p.is_symlink() or len(data) != row['bytes'] or hashlib.sha256(data).hexdigest() != row['sha256']:
        raise ValueError(f'file mismatch: {rel}')
print(json.dumps({'verified_files': len(manifest['files']), 'status': 'PASS'}))
