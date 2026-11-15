#!/usr/bin/env python3
"""No CLI, network or credentials: selection and protobuf-safe path scrub."""
import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('agy_generate', Path(__file__).with_name('generate.py'))
generate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generate)


class NativeScrub(unittest.TestCase):
    def test_preserves_blob_lengths_and_discards_unrelated_metadata(self):
        with tempfile.TemporaryDirectory(prefix='cv-agy-scrub-') as directory:
            base = Path(directory).resolve()
            source, destination = base / 'source.db', base / 'output/store.db'
            body = str(base / 'image.png').encode()
            # A real protobuf string's length remains correct after replacement.
            length, value = bytearray(), len(body)
            while value >= 128:
                length.append((value & 127) | 128)
                value >>= 7
            length.append(value)
            header = b'\x0a' + length
            blob = header + body
            with sqlite3.connect(source) as db:
                db.executescript('CREATE TABLE steps(idx INTEGER,metadata BLOB,error_details BLOB,permissions BLOB,task_details BLOB,render_info BLOB,step_payload BLOB);'
                                 'CREATE TABLE trajectory_meta(trajectory_id TEXT);'
                                 'CREATE TABLE trajectory_metadata_blob(id TEXT, data BLOB);'
                                 'CREATE TABLE gen_metadata(data BLOB);')
                db.execute('INSERT INTO steps VALUES(0,NULL,NULL,NULL,NULL,NULL,?)', [blob])
                # parent(5) kept whole; the agent configuration(4) with its prompts dropped.
                lineage = b'\x2a\x06parent'
                db.execute('INSERT INTO trajectory_metadata_blob VALUES(?,?)',
                           ['main', b'\x22\x0dsystem prompt' + lineage + b'\x48\x01'])
                db.execute('INSERT INTO gen_metadata VALUES(?)', [b'private system prompt'])
            generate.scrub_native(source, destination, base)
            with sqlite3.connect(destination) as db:
                scrubbed = db.execute('SELECT step_payload FROM steps').fetchone()[0]
                self.assertEqual(len(scrubbed), len(blob))
                self.assertEqual(scrubbed[:len(header)], header)
                self.assertNotIn(str(base).encode(), scrubbed)
                self.assertEqual(db.execute('SELECT count(*) FROM gen_metadata').fetchone()[0], 0)
                self.assertEqual(db.execute('SELECT data FROM trajectory_metadata_blob').fetchone()[0], lineage)
            with sqlite3.connect(source) as db:
                self.assertEqual(db.execute('SELECT step_payload FROM steps').fetchone()[0], blob)
                self.assertEqual(db.execute('SELECT count(*) FROM gen_metadata').fetchone()[0], 1)


if __name__ == '__main__':
    unittest.main()
