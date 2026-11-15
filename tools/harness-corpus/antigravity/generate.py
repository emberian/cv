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
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time

PIXEL_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="
PIXEL = base64.b64decode(PIXEL_B64)
UUID = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}')
LOGS = '.system_generated/logs/transcript_full.jsonl'


def call(tool, **args):
    return {'functionCall': {'name': tool, 'args': args}}


def command(line, summary, wait=5000):
    return call('run_command', CommandLine=line, WaitMsBeforeAsync=wait, toolSummary=summary)


def answered(request):
    return any('functionResponse' in part for part in request['contents'][-1].get('parts', []))


def last_request(request):
    for content in reversed(request['contents']):
        for part in content.get('parts', []):
            if content['role'] == 'user' and '<USER_REQUEST>' in part.get('text', ''):
                return part['text']
    return ''


class Sandbox:
    """One fresh HOME, one loopback Gemini endpoint, any number of CLI invocations."""

    def __init__(self, executable, base, budget):
        self.base, self.budget, self.calls, self.invocations = base, budget, 0, 0
        self.script, self.counts, self.lock = None, {}, threading.Lock()
        for name in ('home', 'config', 'data', 'state', 'cache', 'tmp', 'project'):
            (base / name).mkdir()
        profile = base / 'sandbox.sb'
        profile.write_text(
            f'(version 1)(allow default)(deny file-write*)'
            f'(allow file-write* (subpath {json.dumps(str(base))})(literal "/dev/null")'
            '(literal "/dev/ptmx")(regex #"^/dev/ttys[0-9]+$"))'
            '(deny network*)(allow network* (remote ip "localhost:*")(local ip "localhost:*"))')
        self.env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(base / 'home'),
                    'TMPDIR': str(base / 'tmp'), 'TERM': 'dumb',
                    'ANTIGRAVITY_APP_DATA_DIR': str(base / 'home/.gemini/antigravity-cli'),
                    'ANTIGRAVITY_GEMINI_DIR': str(base / 'home/.gemini'),
                    'AGY_CLI_DISABLE_AUTO_UPDATE': '1', 'AGY_CLI_MODEL_API_MAX_RETRIES': '0'}
        self.env.update({f'XDG_{kind}_HOME': str(base / name)
                         for kind, name in [('CONFIG', 'config'), ('DATA', 'data'), ('STATE', 'state'), ('CACHE', 'cache')]})
        self.command = ['/usr/bin/sandbox-exec', '-f', str(profile), str(executable)]
        self.project = base / 'project'
        self.root = base / 'home/.gemini/antigravity-cli'
        version = subprocess.run([*self.command, '--version'], env=self.env, cwd=self.project,
                                 capture_output=True, check=True, timeout=10).stdout.decode().strip()
        if version != '1.5.9':
            raise RuntimeError('gateway and isolation controls verified only for CLI 1.5.9')
        sandbox = self

        class Mock(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def stream(self, parts, finish=True):
                reply = {'candidates': [{'content': {'role': 'model', 'parts': parts},
                                         **({'finishReason': 'STOP'} if finish else {})}]}
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                self.wfile.write(('data: ' + json.dumps(reply) + '\n\n').encode())
                self.wfile.flush()

            def do_POST(self):
                size = int(self.headers.get('Content-Length', '0'))
                if size > 4 * 1024 * 1024:
                    self.send_error(400, 'fixture request budget exceeded')
                    return
                request = json.loads(self.rfile.read(size))
                if 'contents' not in request:
                    self.send_error(400, 'expected Gemini request')
                    return
                if 'title generator' in json.dumps(request.get('systemInstruction', '')):
                    self.stream([{'text': 'Fixture session'}])
                    return
                with sandbox.lock:
                    sandbox.calls += 1
                    over = sandbox.calls > sandbox.budget
                if over:
                    self.send_error(400, 'fixture request budget exceeded')
                    return
                out = sandbox.script(request)
                if out[0] == 'error':
                    body = json.dumps({'error': {'code': out[1], 'message': 'fixture error', 'status': 'UNAVAILABLE'}})
                    self.send_response(out[1])
                    self.send_header('Content-Type', 'application/json')
                    self.end_headers()
                    self.wfile.write(body.encode())
                elif out[0] == 'drop':
                    self.stream(out[1], finish=False)
                    self.connection.shutdown(2)
                elif out[0] == 'hang':
                    self.stream(out[1], finish=False)
                    time.sleep(out[2])
                else:
                    self.stream(out)

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Mock)
        self.server.daemon_threads = True
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.env.update(AGY_LLM_GATEWAY_URL=f'http://127.0.0.1:{self.server.server_port}',
                        AGY_LLM_GATEWAY_API_KEY='corpus-dummy')

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()

    def turn(self, who):
        """The 1-based count of scripted replies this agent has received."""
        with self.lock:
            self.counts[who] = self.counts.get(who, 0) + 1
            return self.counts[who]

    def spawn(self, args, stdin=subprocess.DEVNULL):
        self.invocations += 1
        return subprocess.Popen([*self.command, '--log-file', str(self.base / f'cli{self.invocations}.log'), *args],
                                env=self.env, cwd=self.project, stdin=stdin,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)

    def print_mode(self, prompt, *flags, permissions=True, timeout=30, allow_empty=False):
        """One `--print` invocation; returns its result event."""
        skip = ['--dangerously-skip-permissions'] if permissions else []
        proc = self.spawn([*skip, *flags, '--print', prompt, '--print-timeout', f'{timeout - 10}s',
                           '--output-format', 'stream-json'])
        try:
            output, error = proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Stop only this owned process group, including its background commands.
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            raise RuntimeError('isolated CLI timed out; no fixture published') from None
        if proc.returncode:
            raise RuntimeError(f'isolated CLI failed: {error.decode()[-1000:]}')
        events = [json.loads(line) for line in output.splitlines()]
        result = next(e['result'] for e in events if e['event'] == 'result')
        if result['status'] != 'SUCCESS' or not (allow_empty or result.get('response')):
            raise RuntimeError(f'CLI did not complete the scenario: {error.decode()[-500:]}')
        return result

    def acp(self, decide=lambda tool: 'allow_once'):
        return Acp(self, decide)

    def database(self, conversation):
        return self.root / 'conversations' / f'{conversation}.db'

    def statuses(self, conversation):
        """CORTEX_STEP_STATUS of each stored step: 3 DONE, 6 CANCELED, 7 ERROR."""
        with sqlite3.connect(self.database(conversation).as_uri() + '?mode=ro', uri=True) as db:
            return [status for (status,) in db.execute('SELECT status FROM steps ORDER BY idx')]

    def rows(self, conversation):
        log = self.root / 'brain' / conversation / LOGS
        return [json.loads(line) for line in log.read_text().splitlines()]


class Acp:
    """The CLI's Agent Client Protocol server: the non-interactive channel for permission
    answers, cancellation, prompt media and a session that outlives one turn."""

    def __init__(self, sandbox, decide):
        self.proc = sandbox.spawn(['acp'], stdin=subprocess.PIPE)
        self.decide, self.next, self.results, self.updates = decide, 1, {}, []
        self.permissions = []
        self.changed = threading.Condition()
        threading.Thread(target=self.read, daemon=True).start()
        self.request('initialize', {'protocolVersion': 1, 'clientCapabilities': {}})
        self.session = self.request('session/new', {'cwd': str(sandbox.project), 'mcpServers': []})['sessionId']

    def send(self, message):
        self.proc.stdin.write((json.dumps({'jsonrpc': '2.0', **message}) + '\n').encode())
        self.proc.stdin.flush()

    def read(self):
        for line in self.proc.stdout:
            message = json.loads(line)
            if 'method' in message and 'id' in message:
                result = {}
                if message['method'] == 'session/request_permission':
                    params = message['params']
                    kind = self.decide(params['toolCall'])
                    self.permissions.append((params['toolCall'].get('toolCallId'), kind))
                    option = next(o for o in params['options'] if o['kind'] == kind)
                    result = {'outcome': {'outcome': 'selected', 'optionId': option['optionId']}}
                self.send({'id': message['id'], 'result': result})
                continue
            with self.changed:
                if 'method' in message:
                    self.updates.append(message['params'].get('update', {}))
                else:
                    self.results[message['id']] = message
                self.changed.notify_all()

    def until(self, ready, timeout):
        deadline = time.time() + timeout
        with self.changed:
            while not (found := ready()):
                left = deadline - time.time()
                if left <= 0 or self.proc.poll() is not None:
                    raise RuntimeError('isolated ACP session did not reach the scripted state')
                self.changed.wait(min(left, 0.5))
            return found

    def start(self, method, params):
        index, self.next = self.next, self.next + 1
        self.send({'id': index, 'method': method, 'params': params})
        return index

    def result(self, index, timeout=30):
        reply = self.until(lambda: self.results.get(index), timeout)
        if 'error' in reply:
            raise RuntimeError(f'ACP request failed: {reply["error"]}')
        return reply['result']

    def request(self, method, params, timeout=30):
        return self.result(self.start(method, params), timeout)

    def prompt(self, *blocks):
        blocks = [{'type': 'text', 'text': b} if isinstance(b, str) else b for b in blocks]
        return self.start('session/prompt', {'sessionId': self.session, 'prompt': blocks})

    def ask(self, *blocks, timeout=30):
        return self.result(self.prompt(*blocks), timeout)['stopReason']

    def update(self, ready, timeout=20):
        return self.until(lambda: next((u for u in self.updates if ready(u)), None), timeout)

    def cancel(self):
        self.send({'method': 'session/cancel', 'params': {'sessionId': self.session}})

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()


def expect(condition, message):
    if not condition:
        raise RuntimeError(message)


def background_media(box):
    (box.project / 'pixel.png').write_bytes(PIXEL)

    def script(request):
        if box.turn('root') == 1:
            return [command("sleep 1; printf 'background fixture done\\n'", 'Fixture background command', 500),
                    call('read_file', Path=str(box.project / 'pixel.png'))]
        # Let the harness deliver its genuine background-completion event.
        time.sleep(2)
        return [{'text': 'Fixture background and image complete.'}]
    box.script = script
    conversation = box.print_mode('Run the tiny fixture command in the background and read pixel.png.',
                                  timeout=25)['conversation_id']
    rows = box.rows(conversation)
    expect(any('background task with task id' in r.get('content', '') for r in rows),
           'command never entered background state')
    expect(any(r['type'] == 'SYSTEM_MESSAGE' and 'background fixture done' in r.get('content', '') for r in rows),
           'harness did not write the background completion message')
    expect(any(m.get('mime_type') == 'image/png' for r in rows for m in r.get('media', [])),
           'harness did not write image media')
    return {conversation: []}


def permissions(box):
    """An approved and a denied command through ACP, then the headless automatic denial."""
    def script(request):
        if 'headless' in last_request(request):
            return [{'text': 'unreachable'}] if answered(request) else [command("printf 'headless fixture\\n'", 'Headless fixture command')]
        turn = box.turn('root')
        if turn == 1:
            return [{'text': 'Running the first command.'}, command("printf 'approved fixture\\n'", 'Approved fixture command')]
        if turn == 2:
            return [command("printf 'denied fixture\\n'", 'Denied fixture command')]
        return [{'text': 'unreachable'}]
    box.script = script
    acp = box.acp(lambda tool: 'reject_once' if 'denied' in json.dumps(tool.get('rawInput')) else 'allow_once')
    expect(acp.ask('Run two gated fixture commands.') == 'end_turn', 'permission turn did not end')
    # The turn ends before the writer's last revision of the denied step is stored.
    deadline = time.time() + 10
    while box.statuses(acp.session)[-1] != 7 and time.time() < deadline:
        time.sleep(0.2)
    acp.close()
    expect(box.statuses(acp.session) == [3, 3, 3, 3, 7], 'writer did not record the denial as an error')
    expect([kind for _, kind in acp.permissions] == ['allow_once', 'reject_once'], 'expected one approval and one denial')
    # Print mode cannot prompt: the writer cancels the gated step itself.
    headless = box.print_mode('headless: run the gated fixture command.', permissions=False, allow_empty=True)
    expect(headless.get('denied_actions'), 'headless run did not record an automatic denial')
    return {acp.session: [], headless['conversation_id']: []}


def interruption(box):
    """A cancelled running tool and its retry; an upstream error, a dropped stream and a
    cancelled stream, each followed by a later successful turn."""
    def script(request):
        text, done = last_request(request), answered(request)
        if 'interrupt-tool' in text:
            return [{'text': 'unreachable'}] if done else [command("sleep 8; printf 'interrupted fixture finished\\n'", 'Interrupted fixture command', 10000)]
        if 'retry-tool' in text:
            return [{'text': 'Retry finished.'}] if done else [command("printf 'retried fixture finished\\n'", 'Retried fixture command', 10000)]
        if 'stream-error' in text:
            return ('error', 503)
        if 'stream-drop' in text:
            return ('drop', [{'text': 'partial reply before the stream '}])
        if 'stream-hang' in text:
            return ('hang', [{'text': 'partial reply before interruption '}], 20)
        return [{'text': 'Stream retry succeeded.'}]
    box.script = script
    acp = box.acp()
    turn = acp.prompt('interrupt-tool: run the slow fixture command')
    acp.update(lambda u: u.get('sessionUpdate') == 'tool_call_update' and u.get('status') == 'in_progress')
    time.sleep(1)
    acp.cancel()
    expect(acp.result(turn)['stopReason'] == 'cancelled', 'tool turn was not cancelled')
    expect(acp.ask('retry-tool: run it again') == 'end_turn', 'retry did not finish')
    acp.ask('stream-error: this turn fails upstream')
    acp.ask('stream-drop: this turn loses its stream')
    turn = acp.prompt('stream-hang: this turn is interrupted while streaming')
    acp.update(lambda u: u.get('sessionUpdate') == 'agent_message_chunk' and 'before interruption' in json.dumps(u))
    time.sleep(0.5)
    acp.cancel()
    expect(acp.result(turn)['stopReason'] == 'cancelled', 'stream turn was not cancelled')
    expect(acp.ask('stream-retry: answer normally') == 'end_turn', 'stream retry did not finish')
    acp.close()
    exported = {r['step_index'] for r in box.rows(acp.session)}
    native = box.statuses(acp.session)
    expect(native[2] == 6 and set(range(len(native))) - exported == {2, 8},
           'writer did not keep the cancelled tool and the upstream error out of its export')
    return {acp.session: [LOGS]}


def background_tasks(box):
    """Three background commands that finish out of launch order; the slowest is killed."""
    def script(request):
        turn = box.turn('root')
        if turn == 1:
            return [command("sleep 4; printf 'slow fixture done\\n'", 'Slow fixture task', 500),
                    command("sleep 2; printf 'fast fixture done\\n'", 'Fast fixture task', 500),
                    command("sleep 60; printf 'never printed\\n'", 'Doomed fixture task', 500)]
        if turn == 2:
            return [call('manage_task', Action='list')]
        if turn == 3:
            tasks = sorted(set(re.findall(UUID.pattern + r'/task-\d+', json.dumps(request['contents']))))
            return [call('manage_task', Action='kill', TaskId=tasks[2]), call('manage_task', Action='status', TaskId=tasks[0])]
        if turn == 4:
            return [{'text': 'Waiting for the remaining fixture tasks.'}]
        return [{'text': f'Noted fixture notification {turn - 4}.'}]
    box.script = script
    conversation = box.print_mode('Start three fixture tasks, kill the doomed one, wait for the others.',
                                  timeout=40)['conversation_id']
    messages = [r['content'] for r in box.rows(conversation) if r['type'] == 'SYSTEM_MESSAGE']
    order = [re.search(r'/(task-\d+)" (finished|was canceled)', m).groups() for m in messages]
    expect([state for _, state in order] == ['was canceled', 'finished', 'finished'] and order[1][0] > order[2][0],
           f'tasks did not finish in the scripted interleaving: {order}')
    return {conversation: [LOGS]}


def swarm(box):
    """A coordinator, two workers and a helper one worker adds; board posts, a parked
    worker, a kill. Members are routed by the fixture marker in their own brief."""
    def script(request):
        opening = json.dumps(request['contents'][:2])
        who = next((n for n in ('gamma', 'alpha', 'beta') if f'{n}-fixture' in opening and 'coordinate-fixture' not in opening), 'coordinator')
        turn = box.turn(who)
        if who == 'coordinator':
            if turn == 1:
                return [call('swarm', action='title', title='Fixture swarm'),
                        call('swarm', action='add', name='alpha', brief='alpha-fixture: post [done] alpha to the board and stop.'),
                        call('swarm', action='add', name='beta', brief='beta-fixture: add a helper named gamma, then wait.')]
            if turn == 2:
                return [{'text': 'Fixture swarm started.'}]
            if turn == 3:
                return [call('read_board', since=0), call('swarm', action='status')]
            if turn == 4:
                # Let beta integrate its helper before the helper is removed.
                time.sleep(3)
                return [call('swarm', action='kill', name='gamma'), call('swarm', action='status')]
            return [{'text': 'Fixture swarm finished.' if turn == 5 else f'Coordinator noted event {turn}.'}]
        if who == 'alpha':
            return [call('post_message', text='[done] alpha fixture result @coordinator')] if turn == 1 else [{'text': 'alpha final reply'}]
        if who == 'gamma':
            return [call('post_message', text='[done] gamma fixture result @beta')] if turn == 1 else [{'text': 'gamma final reply'}]
        if turn == 1:
            return [call('swarm', action='add', name='gamma', brief='gamma-fixture: post [done] gamma to the board and stop.')]
        if turn == 2:
            return [call('swarm_wait', since=3)]
        if turn == 3:
            return [call('post_message', text='beta integrated gamma'), call('swarm_wait', since=5)]
        return [{'text': f'beta reply {turn}'}]
    box.script = script
    acp = box.acp()
    expect(acp.ask('/swarm coordinate-fixture: run the fixture swarm') == 'end_turn', 'coordinator did not park')
    acp.until(lambda: box.counts.get('coordinator', 0) >= 5 and box.counts.get('beta', 0) >= 3, 40)
    time.sleep(3)
    acp.close()
    registry = box.root / 'brain' / acp.session / '.system_generated'
    members = json.loads((registry / 'swarm/members.json').read_text())['members']
    expect(members['gamma'].get('added_by') == 'beta' and members['gamma']['killed'], 'helper was not added by a worker and killed')
    board = [json.loads(line) for line in (registry / 'swarm/board.jsonl').read_text().splitlines()]
    expect([m['agent'] for m in board if m.get('kind') != 'brief'] == ['alpha', 'gamma', 'beta'], 'board posts differ from the script')
    files = ['.system_generated/swarm/members.json', '.system_generated/swarm/board.jsonl', '.system_generated/swarm/swarm.json',
             *(f'.system_generated/subagents/{p.name}' for p in sorted((registry / 'subagents').iterdir())), LOGS]
    return {acp.session: files, **{m['conversation_id']: [LOGS] for name, m in members.items() if name != 'coordinator'}}


def subagents(box):
    """Two invoke_subagent children: one returns at once, one after a parent message."""
    def script(request):
        opening = json.dumps(request['contents'][:2])
        who = next((n for n in ('child-one', 'child-two') if f'{n}-fixture' in opening and 'parent-fixture' not in opening), 'parent')
        turn = box.turn(who)
        if who == 'child-one':
            return [{'text': 'child one final result'}]
        if who == 'child-two':
            if turn == 1:
                time.sleep(2)
            return [{'text': 'child two first reply' if turn == 1 else 'child two final result'}]
        if turn == 1:
            return [call('invoke_subagent', TypeName='self', Role='First fixture child', Prompt='child-one-fixture: reply with your result.'),
                    call('invoke_subagent', TypeName='self', Role='Second fixture child', Prompt='child-two-fixture: wait for a message, then reply.')]
        if turn == 2:
            second = UUID.findall(json.dumps(request['contents'][-1]))[-1]
            return [call('manage_subagents', Action='list'), call('send_message', Recipient=second, Message='parent-fixture message to child two')]
        return [{'text': 'Waiting for fixture children.' if turn == 3 else f'Parent noted child event {turn - 3}.'}]
    box.script = script
    conversation = box.print_mode('parent-fixture: start two fixture children', timeout=45)['conversation_id']
    rows = box.rows(conversation)
    idle = [r['content'] for r in rows if r['type'] == 'SYSTEM_MESSAGE' and 'has gone idle' in r.get('content', '')]
    expect(any('child one final result' in m for m in idle) and any('child two' in m for m in idle),
           'children did not both return to the parent')
    registry = box.root / 'brain' / conversation / '.system_generated/subagents'
    children = sorted(p.stem for p in registry.iterdir())
    expect(len(children) == 2, 'expected two child descriptors')
    return {conversation: [LOGS, *(f'.system_generated/subagents/{c}.json' for c in children)], **{c: [LOGS] for c in children}}


def prompt_media(box):
    """An inline prompt image and linked files through ACP. The writer copies the image
    beside the conversation; the fixture omits that copy, as a native-only store would."""
    (box.project / 'linked.png').write_bytes(PIXEL)
    (box.project / 'notes.txt').write_text('linked fixture notes\n')
    box.script = lambda request: [{'text': f'Media reply {box.turn("root")}.'}]
    acp = box.acp()
    link = lambda name, mime: {'type': 'resource_link', 'uri': f'file://{box.project / name}', 'name': name, 'mimeType': mime}
    acp.ask('inline-image fixture prompt', {'type': 'image', 'data': PIXEL_B64, 'mimeType': 'image/png'})
    acp.ask('linked-attachment fixture prompt', link('linked.png', 'image/png'), link('notes.txt', 'text/plain'))
    # The ACP server advertises audio=false; it accepts this prompt and records no media.
    acp.ask('audio fixture prompt', {'type': 'audio', 'data': base64.b64encode(b'RIFF').decode(), 'mimeType': 'audio/wav'})
    acp.close()
    uploads = list((box.root / 'brain' / acp.session / '.user_uploaded').iterdir())
    expect(len(uploads) == 1 and uploads[0].read_bytes() == PIXEL, 'writer did not store the prompt image')
    return {acp.session: []}


def compaction_resume(box):
    """A turn, /compact on resume, then the exported logs disappear and the conversation is
    resumed twice from its native store alone."""
    def script(request):
        if box.invocations == 2:
            return [{'text': 'FIXTURE SUMMARY: the user ran one pre-compaction fixture command successfully.'}]
        label = re.search(r'resume-fixture: (\w+)', last_request(request)).group(1)
        if answered(request):
            return [{'text': f'{label}: done.'}]
        return [{'text': f'{label}: running the fixture command.'}, command(f"printf '{label} fixture output\\n'", f'{label} command')]
    box.script = script
    conversation = box.print_mode('resume-fixture: first turn')['conversation_id']
    resume = ('--conversation', conversation)
    box.print_mode('/compact', *resume, allow_empty=True)
    expect(any(r['type'] == 'CHECKPOINT' for r in box.rows(conversation)), 'writer did not record a compaction checkpoint')
    shutil.rmtree(box.root / 'brain' / conversation)
    for label in ('second', 'third'):
        expect(box.print_mode(f'resume-fixture: {label} turn', *resume)['conversation_id'] == conversation,
               'resume did not keep the conversation')
    expect(min(r['step_index'] for r in box.rows(conversation)) > 0, 'expected the re-created export to start after the resume')
    return {conversation: [LOGS]}


SCENARIOS = {
    'native-background-media': (background_media, 6, ['native_only', 'background_command', 'background_completion', 'image_tool_result']),
    'permissions': (permissions, 6, ['native_only', 'permission_approved', 'permission_denied', 'headless_auto_denial']),
    'interruption': (interruption, 10, ['cancelled_tool', 'tool_retry', 'upstream_error', 'dropped_stream', 'cancelled_stream', 'steps_absent_from_export']),
    'background-tasks': (background_tasks, 10, ['three_background_tasks', 'interleaved_completion', 'killed_task', 'task_status_poll']),
    'swarm': (swarm, 24, ['swarm_coordinator', 'two_workers', 'worker_added_helper', 'board_posts', 'parked_worker', 'killed_member']),
    'subagents': (subagents, 12, ['two_invoked_children', 'parent_message_to_child', 'idle_child_returns']),
    'prompt-media': (prompt_media, 6, ['native_only', 'inline_prompt_image', 'linked_files', 'stored_upload_absent', 'audio_prompt_not_recorded']),
    'compaction-resume': (compaction_resume, 10, ['manual_compaction', 'resume_by_conversation', 'native_only_resume', 'export_restarts_after_resume']),
}


def replace_paths(data, base):
    """Equal-width, so a protobuf length prefix or a JSON string stays valid."""
    old = str(base).encode()
    replacement = b'/home/corpus/' + b'_' * (len(old) - len(b'/home/corpus/'))
    assert len(old) == len(replacement)
    return data.replace(old, replacement)


def refuse_residue(data, base):
    for forbidden in (str(base).encode(), b'/Users/', b'corpus-dummy'):
        if forbidden in data:
            raise RuntimeError('capture requires additional protocol-aware scrub; refusing')


# TrajectoryMetadata fields the reader takes lineage from: workspace(1), created(2),
# trajectory id(3), parent(5), root(6), workspace uri(7), agent type and role(8).
LINEAGE_FIELDS = (1, 2, 3, 5, 6, 7, 8)


def top_level_fields(blob, keep):
    """Whole top-level fields of one protobuf message, by number. A top-level field has no
    enclosing length prefix, so dropping its siblings leaves every retained byte valid."""
    def varint(at):
        value = shift = 0
        while True:
            byte = blob[at]
            at += 1
            value |= (byte & 127) << shift
            shift += 7
            if byte < 128:
                return value, at
    out, at = bytearray(), 0
    while at < len(blob):
        start = at
        key, at = varint(at)
        wire = key & 7
        if wire == 0:
            _, at = varint(at)
        elif wire == 1:
            at += 8
        elif wire == 5:
            at += 4
        elif wire == 2:
            size, at = varint(at)
            at += size
        else:
            raise RuntimeError('unexpected protobuf wire type; refusing')
        if at > len(blob):
            raise RuntimeError('truncated protobuf field; refusing')
        if key >> 3 in keep:
            if at - start > 1024:
                raise RuntimeError('lineage field is larger than lineage; refusing')
            out += blob[start:at]
    return bytes(out)


def scrub_native(source, destination, base):
    """Select actual writer steps; preserve protobuf lengths without knowing its schema.

    Request metadata tables contain full system prompts and are outside this fixture's
    scope; of the trajectory metadata only whole lineage fields are kept. Native byte
    cells receive only equal-width path substitutions; anything needing a different
    scrub must fail rather than corrupt nested protobuf lengths.
    """
    destination.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(source.as_uri() + '?mode=ro', uri=True) as src, sqlite3.connect(destination) as dst:
        src.backup(dst)
        tables = [name for (name,) in dst.execute("SELECT name FROM sqlite_master WHERE type='table'")]
        for table in tables:
            if table not in ('steps', 'trajectory_meta', 'trajectory_metadata_blob'):
                dst.execute(f'DELETE FROM "{table}"')
        if 'trajectory_metadata_blob' in tables:
            for key, data in dst.execute('SELECT id,data FROM trajectory_metadata_blob').fetchall():
                dst.execute('UPDATE trajectory_metadata_blob SET data=? WHERE id=?',
                            [replace_paths(top_level_fields(data, LINEAGE_FIELDS), base), key])
        for row in dst.execute('SELECT idx,metadata,error_details,permissions,task_details,render_info,step_payload FROM steps').fetchall():
            values = [replace_paths(v, base) if isinstance(v, bytes) else v for v in row[1:]]
            assert all(not isinstance(a, bytes) or len(a) == len(b) for a, b in zip(row[1:], values))
            dst.execute('UPDATE steps SET metadata=?,error_details=?,permissions=?,task_details=?,render_info=?,step_payload=? WHERE idx=?', [*values, row[0]])
        dst.commit()
        dst.execute('PRAGMA journal_mode=DELETE')
        dst.execute('PRAGMA page_size=4096')
        dst.execute('VACUUM')
    refuse_residue(destination.read_bytes(), base)


def scrub_text(source, destination, base):
    """Writer-owned JSON beside the store: the same equal-width path substitution."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    data = replace_paths(source.read_bytes(), base)
    refuse_residue(data, base)
    destination.write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--scenario', choices=sorted(SCENARIOS), default='native-background-media')
    args = parser.parse_args()
    if args.out.exists():
        parser.error('refusing to regenerate an existing corpus version')
    if not Path('/usr/bin/sandbox-exec').is_file():
        parser.error('requires macOS sandbox-exec; no unsandboxed fallback')
    executable = args.bin.resolve(strict=True)
    scenario, budget, labels = SCENARIOS[args.scenario]
    with tempfile.TemporaryDirectory(prefix='cv-corpus.agy-') as directory:
        base = Path(directory).resolve()
        box = Sandbox(executable, base, budget)
        try:
            captured = scenario(box)
        finally:
            box.close()
        # Stage all validation first; a failed capture never becomes a committed fixture.
        staged = base / 'fixture'
        app = staged / '.gemini/antigravity-cli'
        for conversation, files in captured.items():
            scrub_native(box.database(conversation), app / 'conversations' / f'{conversation}.db', base)
            for name in files:
                scrub_text(box.root / 'brain' / conversation / name, app / 'brain' / conversation / name, base)
        kept = sorted({name.rsplit('/', 1)[-1] for files in captured.values() for name in files})
        digest = hashlib.sha256(executable.read_bytes()).hexdigest()
        (staged / 'MANIFEST.toml').write_text(
            '# Real CLI-written native steps; never regenerate a committed version.\n'
            f'harness = "antigravity"\nversion = "1.5.9-{args.scenario}"\n'
            f'generated = "{datetime.date.today()}"\nwriter_version = "1.5.9"\nwriter_sha256 = "{digest}"\n'
            f'model_calls = 0\nmock_calls = {box.calls}\ncli_invocations = {box.invocations}\n'
            f'scenarios = {json.dumps(labels)}\n'
            'source = "Installed CLI writer against deterministic loopback Gemini SSE responses; no real credentials or upstream requests."\n'
            'isolation = "Fresh HOME/app-data/XDG paths; OS sandbox denies non-loopback network and writes outside scratch except PTY devices."\n'
            'selection = "SQLite backup; steps, trajectory_meta and the lineage fields of trajectory metadata retained, other tables emptied; '
            + (f'writer files kept beside the store: {", ".join(kept)}."\n' if kept else 'exported logs intentionally omitted."\n')
            + 'scrub = "Equal-width scratch-path replacement preserves every protobuf length; no synthetic step encoding. Image bytes remain native."\n')
        args.out.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(staged, args.out)
        print(f'Captured real writer fixture with {box.calls} local mock responses: {args.out}')


if __name__ == '__main__':
    main()
