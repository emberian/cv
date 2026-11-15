#!/usr/bin/env python3
"""Real Antigravity writer, deterministic local model responses, no credentials.

macOS only: an OS sandbox confines writes to the temporary directory and network
access to loopback, including descendants. The CLI owns all session serialization.
"""
import argparse
import base64
import datetime
import hashlib
import http.server
import json
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time

PIXEL = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==")


def capture(executable, base):
    for name in ('home', 'config', 'data', 'state', 'cache', 'tmp', 'project'):
        (base / name).mkdir()
    (base / 'project/pixel.png').write_bytes(PIXEL)
    profile = base / 'sandbox.sb'
    profile.write_text(
        f'(version 1)(allow default)(deny file-write*)'
        f'(allow file-write* (subpath {json.dumps(str(base))})(literal "/dev/null")'
        '(literal "/dev/ptmx")(regex #"^/dev/ttys[0-9]+$"))'
        '(deny network*)(allow network* (remote ip "localhost:*")(local ip "localhost:*"))')
    env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(base / 'home'),
           'TMPDIR': str(base / 'tmp'), 'TERM': 'dumb',
           'ANTIGRAVITY_APP_DATA_DIR': str(base / 'home/.gemini/antigravity-cli'),
           'ANTIGRAVITY_GEMINI_DIR': str(base / 'home/.gemini'),
           'AGY_CLI_DISABLE_AUTO_UPDATE': '1', 'AGY_CLI_MODEL_API_MAX_RETRIES': '0'}
    env.update({f'XDG_{kind}_HOME': str(base / name)
                for kind, name in [('CONFIG', 'config'), ('DATA', 'data'), ('STATE', 'state'), ('CACHE', 'cache')]})
    command = ['/usr/bin/sandbox-exec', '-f', str(profile), str(executable)]
    version = subprocess.run([*command, '--version'], env=env, cwd=base / 'project',
                             capture_output=True, check=True, timeout=10).stdout.decode().strip()
    if version != '1.5.9':
        raise RuntimeError('gateway and isolation controls verified only for CLI 1.5.9')
    calls = []

    class Mock(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            size = int(self.headers.get('Content-Length', '0'))
            if size > 4 * 1024 * 1024 or len(calls) >= 6:
                self.send_error(400, 'fixture request budget exceeded')
                return
            request = json.loads(self.rfile.read(size))
            if 'contents' not in request:
                self.send_error(400, 'expected Gemini request')
                return
            calls.append(self.path)
            if len(calls) == 1:
                parts = [
                    {'functionCall': {'name': 'run_command', 'args': {
                        'CommandLine': "sleep 1; printf 'background fixture done\\n'",
                        'WaitMsBeforeAsync': 500, 'toolSummary': 'Fixture background command'}}},
                    {'functionCall': {'name': 'read_file', 'args': {'Path': str(base / 'project/pixel.png')}}}]
            else:
                # Let the harness deliver its genuine background-completion event.
                time.sleep(2)
                parts = [{'text': 'Fixture background and image complete.'}]
            reply = {'candidates': [{'content': {'role': 'model', 'parts': parts}, 'finishReason': 'STOP'}]}
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            self.wfile.write(('data: ' + json.dumps(reply) + '\n\n').encode())

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Mock)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    env.update(AGY_LLM_GATEWAY_URL=f'http://127.0.0.1:{server.server_port}',
               AGY_LLM_GATEWAY_API_KEY='corpus-dummy')
    proc = subprocess.Popen(
        [*command, '--dangerously-skip-permissions', '--print',
         'Run the tiny fixture command in the background and read pixel.png.',
         '--print-timeout', '15s', '--output-format', 'stream-json', '--log-file', str(base / 'cli.log')],
        env=env, cwd=base / 'project', stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        output, error = proc.communicate(timeout=25)
        if proc.returncode:
            raise RuntimeError(f'isolated CLI failed: {error.decode()[-1000:]}')
    except subprocess.TimeoutExpired:
        # Stop only this owned process group, including its background command.
        import os
        os.killpg(proc.pid, signal.SIGKILL)
        proc.communicate()
        raise RuntimeError('isolated CLI timed out; no fixture published') from None
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    events = [json.loads(line) for line in output.splitlines()]
    result = next(e['result'] for e in events if e['event'] == 'result')
    if result['status'] != 'SUCCESS':
        raise RuntimeError('CLI did not complete the scenario')
    root = base / 'home/.gemini/antigravity-cli'
    conversation = result['conversation_id']
    logs = root / 'brain' / conversation / '.system_generated/logs'
    rows = [json.loads(line) for line in (logs / 'transcript_full.jsonl').read_text().splitlines()]
    if not any('background task with task id' in r.get('content', '') for r in rows):
        raise RuntimeError('command never entered background state')
    if not any(r['type'] == 'SYSTEM_MESSAGE' and 'background fixture done' in r.get('content', '') for r in rows):
        raise RuntimeError('harness did not write the background completion message')
    if not any(m.get('mime_type') == 'image/png' for r in rows for m in r.get('media', [])):
        raise RuntimeError('harness did not write image media')
    return root / 'conversations' / f'{conversation}.db', len(calls)


def scrub_native(source, destination, base):
    """Select actual writer steps; preserve protobuf lengths without knowing its schema.

    Request metadata tables contain full system prompts and are outside this fixture's
    scope. Native byte cells receive only equal-width path substitutions; anything
    needing a different scrub must fail rather than corrupt nested protobuf lengths.
    """
    destination.parent.mkdir(parents=True)
    with sqlite3.connect(source.as_uri() + '?mode=ro', uri=True) as src, sqlite3.connect(destination) as dst:
        src.backup(dst)
        for (table,) in dst.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall():
            if table not in ('steps', 'trajectory_meta'):
                dst.execute(f'DELETE FROM "{table}"')
        old = str(base).encode()
        replacement = b'/home/corpus/' + b'_' * (len(old) - len(b'/home/corpus/'))
        assert len(old) == len(replacement)
        for row in dst.execute('SELECT idx,metadata,error_details,permissions,task_details,render_info,step_payload FROM steps').fetchall():
            values = [v.replace(old, replacement) if isinstance(v, bytes) else v for v in row[1:]]
            assert all(not isinstance(a, bytes) or len(a) == len(b) for a, b in zip(row[1:], values))
            dst.execute('UPDATE steps SET metadata=?,error_details=?,permissions=?,task_details=?,render_info=?,step_payload=? WHERE idx=?', [*values, row[0]])
        dst.commit()
        dst.execute('PRAGMA journal_mode=DELETE')
        dst.execute('PRAGMA page_size=4096')
        dst.execute('VACUUM')
    data = destination.read_bytes()
    for forbidden in (str(base).encode(), b'/Users/', b'corpus-dummy'):
        if forbidden in data:
            raise RuntimeError('native capture requires additional protocol-aware scrub; refusing')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if args.out.exists():
        parser.error('refusing to regenerate an existing corpus version')
    if not Path('/usr/bin/sandbox-exec').is_file():
        parser.error('requires macOS sandbox-exec; no unsandboxed fallback')
    executable = args.bin.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix='cv-corpus.agy-') as directory:
        base = Path(directory).resolve()
        source, calls = capture(executable, base)
        # Stage all validation first; a failed capture never becomes a committed fixture.
        staged = base / 'fixture'
        destination = staged / '.gemini/antigravity-cli/conversations' / source.name
        scrub_native(source, destination, base)
        digest = hashlib.sha256(executable.read_bytes()).hexdigest()
        (staged / 'MANIFEST.toml').write_text(
            '# Real CLI-written native steps; never regenerate a committed version.\n'
            'harness = "antigravity"\nversion = "1.5.9-native-background-media"\n'
            f'generated = "{datetime.date.today()}"\nwriter_version = "1.5.9"\nwriter_sha256 = "{digest}"\n'
            f'model_calls = 0\nmock_calls = {calls}\ncli_invocations = 1\n'
            'scenarios = ["native_only", "background_command", "background_completion", "image_tool_result"]\n'
            'source = "Installed CLI writer against deterministic loopback Gemini SSE responses; no real credentials or upstream requests."\n'
            'isolation = "Fresh HOME/app-data/XDG paths; OS sandbox denies non-loopback network and writes outside scratch except PTY devices."\n'
            'selection = "SQLite backup; steps and trajectory_meta retained, other tables emptied; exported logs intentionally omitted."\n'
            'scrub = "Equal-width scratch-path replacement preserves every protobuf length; no synthetic step encoding. Image bytes remain native."\n')
        args.out.parent.mkdir(parents=True, exist_ok=True)
        import shutil
        shutil.copytree(staged, args.out)
        print(f'Captured real writer fixture with {calls} local mock responses: {args.out}')


if __name__ == '__main__':
    main()
