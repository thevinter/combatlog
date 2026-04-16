#!/usr/bin/env python3

import io
import json
import os
import queue
import random
import re
import string
import subprocess
import tempfile
import threading
import time
import uuid
import zipfile

from curl_cffi import requests as cffi_requests
from flask import Flask, request, Response, send_from_directory

app = Flask(__name__)
app.config['MAX_CONTENT_LENGTH'] = 1024 * 1024 * 1024  # 1 GB

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PARSER_HARNESS = os.path.join(SCRIPT_DIR, 'parser-harness.js')
BATCH_SIZE = 5000
BASE_URL = 'https://www.warcraftlogs.com'
CLIENT_VERSION = os.environ.get('WCL_CLIENT_VERSION', '9.0.1')
CHROME_VERSION = os.environ.get('WCL_CHROME_VERSION', '134.0.6998.205')
ELECTRON_VERSION = os.environ.get('WCL_ELECTRON_VERSION', '37.7.0')
PARSER_VERSION = 59

MAX_RETRIES = 3
RETRY_BASE_DELAY = 1.0


def _jitter_sleep():
    time.sleep(random.uniform(0.05, 0.25))


def _random_boundary():
    return '----WebKitFormBoundary' + ''.join(
        random.choices(string.ascii_letters + string.digits, k=16))


def _user_agent():
    return (f'Mozilla/5.0 (Windows NT 10.0; Win64; x64) '
            f'AppleWebKit/537.36 (KHTML, like Gecko) '
            f'ArchonApp/{CLIENT_VERSION} Chrome/{CHROME_VERSION} '
            f'Electron/{ELECTRON_VERSION} Safari/537.36')

jobs = {}


def fetch_parser_code(session):
    ts = int(time.time() * 1000)
    url = (f'{BASE_URL}/desktop-client/parser?id=1&ts={ts}'
           '&gameContentDetectionEnabled=false&metersEnabled=false'
           '&liveFightDataEnabled=false')
    _jitter_sleep()
    resp = session.request('GET', url, headers={'User-Agent': _user_agent()})
    html = resp.text

    m = re.search(
        r'<script[^>]*>(.*?window\.gameContentTypes.*?)</script>', html, re.DOTALL)
    gamedata_code = m.group(1).strip() if m else ''

    m2 = re.search(r'src="(https://assets\.rpglogs\.com/js/parser-warcraft[^"]+)"', html)
    if not m2:
        raise RuntimeError('Could not find parser-warcraft JS URL in parser page')
    parser_url = m2.group(1)
    _jitter_sleep()
    parser_code = session.get(parser_url, headers={'User-Agent': _user_agent()}).text

    m3 = re.search(r'const parserVersion\s*=\s*(\d+)', html)
    pv = int(m3.group(1)) if m3 else PARSER_VERSION

    return gamedata_code, parser_code, pv


class WCLSession:
    def __init__(self):
        self.session = cffi_requests.Session(impersonate="chrome")
        self.user = None

    def _request(self, method, url, **kwargs):
        kwargs.setdefault('headers', {})
        kwargs['headers'].setdefault('User-Agent', _user_agent())
        for attempt in range(MAX_RETRIES + 1):
            resp = self.session.request(method, url, **kwargs)
            if resp.status_code < 400:
                return resp
            if resp.status_code in (429,) or resp.status_code >= 500:
                if attempt < MAX_RETRIES:
                    delay = RETRY_BASE_DELAY * (2 ** attempt) + random.uniform(0, 1)
                    time.sleep(delay)
                    continue
            resp.raise_for_status()
        resp.raise_for_status()

    def login(self, email, password):
        _jitter_sleep()
        resp = self._request('POST', f'{BASE_URL}/desktop-client/log-in',
            json={'email': email, 'password': password, 'version': CLIENT_VERSION},
            headers={'Content-Type': 'application/json', 'User-Agent': _user_agent()},
        )
        result = resp.json()
        self.user = result.get('user')
        return result

    def create_report(self, filename, start_time, end_time, region, visibility, guild_id, parser_version=PARSER_VERSION):
        _jitter_sleep()
        resp = self._request('POST', f'{BASE_URL}/desktop-client/create-report',
            json={
                'clientVersion': CLIENT_VERSION, 'parserVersion': parser_version,
                'startTime': start_time, 'endTime': end_time,
                'guildId': guild_id, 'fileName': os.path.basename(filename),
                'serverOrRegion': region, 'visibility': visibility,
                'reportTagId': None, 'description': '',
            },
            headers={'Content-Type': 'application/json', 'User-Agent': _user_agent()},
        )
        return resp.json()['code']

    def _multipart(self, url, fields, files):
        boundary = _random_boundary()
        body = bytearray()
        for name, value in fields:
            body.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
        for name, fname, ctype, data in files:
            body.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{fname}"\r\nContent-Type: {ctype}\r\n\r\n'.encode())
            body.extend(data)
            body.extend(b'\r\n')
        body.extend(f'--{boundary}--\r\n'.encode())
        _jitter_sleep()
        return self._request('POST', url,
            data=bytes(body),
            headers={
                'Content-Type': f'multipart/form-data; boundary={boundary}',
                'User-Agent': _user_agent(),
            },
        )

    def set_master_table(self, code, seg_id, zip_bytes):
        self._multipart(
            f'{BASE_URL}/desktop-client/set-report-master-table/{code}',
            [('segmentId', str(seg_id)), ('isRealTime', 'false')],
            [('logfile', 'blob', 'application/zip', zip_bytes)],
        )

    def add_segment(self, code, seg_id, start_time, end_time, mythic, zip_bytes):
        params = json.dumps({
            'startTime': start_time, 'endTime': end_time, 'mythic': mythic,
            'isLiveLog': False, 'isRealTime': False,
            'inProgressEventCount': 0, 'segmentId': seg_id,
        })
        resp = self._multipart(
            f'{BASE_URL}/desktop-client/add-report-segment/{code}',
            [('parameters', params)],
            [('logfile', 'blob', 'application/zip', zip_bytes)],
        )
        return resp.json().get('nextSegmentId', seg_id + 1)

    def terminate_report(self, code):
        _jitter_sleep()
        self._request('POST', f'{BASE_URL}/desktop-client/terminate-report/{code}',
            headers={'User-Agent': _user_agent()},
        )


class Parser:
    def __init__(self, gamedata_code, parser_code):
        """Start parser with dynamically fetched code injected via stdin."""
        self.proc = subprocess.Popen(
            ['node', PARSER_HARNESS],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, bufsize=1,
            encoding='utf-8',
        )
        payload = json.dumps({'gamedataCode': gamedata_code,
                              'parserCode': parser_code})
        self.proc.stdin.write(payload + '\n')
        self.proc.stdin.flush()
        ready = self._read()
        if not ready.get('ready'):
            raise RuntimeError(f"Parser failed: {ready}")

    def _send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + '\n')
        self.proc.stdin.flush()
        return self._read()

    def _read(self):
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(f"Parser died: {self.proc.stderr.read()}")
        return json.loads(line)

    def clear_state(self):          return self._send({'action': 'clear-state'})
    def set_start_date(self, d):    return self._send({'action': 'set-start-date', 'startDate': d})
    def parse_lines(self, lines, region=2):
        return self._send({'action': 'parse-lines', 'lines': lines, 'selectedRegion': region})
    def collect_fights(self):
        return self._send({'action': 'collect-fights', 'pushFightIfNeeded': True, 'scanningOnly': False})
    def collect_master_info(self):  return self._send({'action': 'collect-master-info'})
    def clear_fights(self):         return self._send({'action': 'clear-fights'})
    def close(self):
        try: self.proc.stdin.close(); self.proc.wait(timeout=5)
        except: self.proc.kill()


def make_zip(s):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        zf.writestr('log.txt', s)
    return buf.getvalue()


def build_master_string(m, lv, gv):
    parts = [f"{lv}|{gv}|"]
    for key, skey in [('lastAssignedActorID','actorsString'),
                      ('lastAssignedAbilityID','abilitiesString'),
                      ('lastAssignedTupleID','tuplesString'),
                      ('lastAssignedPetID','petsString')]:
        parts.append(str(m[key]))
        if m[skey]:
            parts.append(m[skey].rstrip('\n'))
    return '\n'.join(parts) + '\n'


def build_fights_string(fd):
    total = sum(f['eventCount'] for f in fd['fights'])
    evts = ''.join(f['eventsString'] for f in fd['fights'])
    return f"{fd['logVersion']}|{fd['gameVersion']}\n{total}\n{evts}"


def parse_start_date(filename):
    m = re.search(r'WoWCombatLog-(\d{2})(\d{2})(\d{2})_', filename)
    if m:
        mm, dd, yy = m.groups()
        return f"{int(mm)}/{int(dd)}/{2000+int(yy)}"
    return None



def upload_worker(job_id, filepath, filename, email, password, region, visibility, guild_id):
    q = jobs[job_id]

    def emit(event, data):
        q.put(f"event: {event}\ndata: {json.dumps(data)}\n\n")

    try:
        with open(filepath, 'r', encoding='utf-8') as f:
            all_lines = [l.rstrip('\r\n') for l in f]
        total = len(all_lines)
        emit('progress', {'step': 'read', 'message': f'Read {total:,} lines', 'pct': 2})

        session = WCLSession()
        login_result = session.login(email, password)
        username = login_result.get('user', {}).get('userName', '?')
        emit('progress', {'step': 'login', 'message': f'Logged in as {username}', 'pct': 5})

        emit('progress', {'step': 'fetch-parser', 'message': 'Fetching latest parser...', 'pct': 6})
        gamedata_code, parser_code, parser_version = fetch_parser_code(session.session)
        emit('progress', {'step': 'fetch-parser', 'message': f'Parser v{parser_version} loaded', 'pct': 7})

        parser = Parser(gamedata_code=gamedata_code, parser_code=parser_code)
        parser.clear_state()
        sd = parse_start_date(filename)
        if sd:
            parser.set_start_date(sd)
        emit('progress', {'step': 'parser', 'message': 'Parser ready', 'pct': 8})

        segment_id = 1
        report_code = None
        total_batches = (total + BATCH_SIZE - 1) // BATCH_SIZE

        for batch_idx, batch_start in enumerate(range(0, total, BATCH_SIZE)):
            batch = all_lines[batch_start:batch_start + BATCH_SIZE]
            batch_num = batch_idx + 1
            pct = 10 + int(80 * batch_num / total_batches)

            r = parser.parse_lines(batch, region=region)
            if not r.get('ok'):
                emit('error', {'message': f"Parse error: {r.get('error', '?')}"})
                return

            fd = parser.collect_fights()
            if not fd.get('ok') or not fd['fights']:
                emit('progress', {'step': 'parse', 'message': f'Batch {batch_num}/{total_batches} — no fights yet', 'pct': pct})
                continue

            if report_code is None:
                report_code = session.create_report(filename, fd['startTime'], fd['endTime'], region, visibility, guild_id, parser_version)
                emit('progress', {'step': 'report', 'message': f'Report created: {report_code}', 'pct': pct})

            mi = parser.collect_master_info()
            session.set_master_table(report_code, segment_id, make_zip(build_master_string(mi, fd['logVersion'], fd['gameVersion'])))
            evts = sum(f['eventCount'] for f in fd['fights'])
            segment_id = session.add_segment(report_code, segment_id, fd['startTime'], fd['endTime'], fd['mythic'], make_zip(build_fights_string(fd)))
            parser.clear_fights()
            emit('progress', {'step': 'upload', 'message': f'Segment {batch_num}/{total_batches} — {evts:,} events', 'pct': pct})

        if report_code:
            session.terminate_report(report_code)
            url = f'https://www.warcraftlogs.com/reports/{report_code}'
            emit('done', {'url': url, 'code': report_code})
        else:
            emit('error', {'message': 'No fights found in log file.'})

        parser.close()

    except Exception as e:
        emit('error', {'message': str(e)})
    finally:
        q.put(None)  # sentinel
        try: os.unlink(filepath)
        except: pass


@app.route('/')
def index():
    return INDEX_HTML


@app.route('/upload', methods=['POST'])
def upload():
    f = request.files.get('logfile')
    if not f:
        return 'No file', 400

    email = request.form.get('email', '')
    password = request.form.get('password', '')
    region = int(request.form.get('region', 2))
    visibility = int(request.form.get('visibility', 2))
    guild_id_str = request.form.get('guild_id', '')
    guild_id = int(guild_id_str) if guild_id_str else None

    tmp = tempfile.NamedTemporaryFile(delete=False, suffix='.txt')
    f.save(tmp)
    tmp.close()

    job_id = uuid.uuid4().hex[:12]
    jobs[job_id] = queue.Queue()

    t = threading.Thread(target=upload_worker, daemon=True,
                         args=(job_id, tmp.name, f.filename, email, password, region, visibility, guild_id))
    t.start()

    return json.dumps({'jobId': job_id}), 200, {'Content-Type': 'application/json'}


@app.route('/events/<job_id>')
def events(job_id):
    q = jobs.get(job_id)
    if not q:
        return 'Unknown job', 404

    def stream():
        while True:
            msg = q.get()
            if msg is None:
                break
            yield msg
        del jobs[job_id]

    return Response(stream(), mimetype='text/event-stream',
                    headers={'Cache-Control': 'no-cache', 'X-Accel-Buffering': 'no'})



INDEX_HTML = r'''<!DOCTYPE html>
<html lang="en" data-theme="light">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>combatlog.dev</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Lexend:wght@300;400;500;600&family=Fira+Code:wght@400;500&display=swap" rel="stylesheet">
<style>
  /* ── light theme (default) ── */
  :root, [data-theme="light"] {
    --bg: #f4f4f6;
    --surface: #ffffff;
    --input-bg: #f4f4f6;
    --border: #d2d2d4;
    --accent: #ff634a;
    --accent-hover: #d94a34;
    --accent-glow: rgba(255,99,74,.15);
    --accent-glow-strong: rgba(255,99,74,.3);
    --accent-tint: rgba(255,99,74,.04);
    --accent-tint-md: rgba(255,99,74,.08);
    --text: #2a2a2c;
    --text-bright: #111113;
    --text-dim: #8a8a8e;
    --log-bg: #e7e7e7;
    --error: #d93636;
    --error-bg: #fff0f0;
    --error-border: rgba(179,58,58,.25);
    --success: #2e8a3e;
    --success-bg: rgba(58,138,74,.06);
    --success-border: rgba(58,138,74,.2);
    --success-glow: rgba(58,138,74,.3);
    --shadow: 0 2px 24px rgba(0,0,0,.06), 0 0 0 1px rgba(210,210,212,.5);
    --toggle-bg: #d2d2d4;
  }

  /* ── dark theme ── */
  [data-theme="dark"] {
    --bg: #101014;
    --surface: #1a1a20;
    --input-bg: #101014;
    --border: #2c2c34;
    --accent: #ff634a;
    --accent-hover: #ff7d68;
    --accent-glow: rgba(255,99,74,.12);
    --accent-glow-strong: rgba(255,99,74,.25);
    --accent-tint: rgba(255,99,74,.03);
    --accent-tint-md: rgba(255,99,74,.06);
    --text: #c8c8cc;
    --text-bright: #ededf0;
    --text-dim: #5e5e66;
    --log-bg: #141418;
    --error: #e04444;
    --error-bg: #1e1012;
    --error-border: rgba(224,68,68,.25);
    --success: #3ea64e;
    --success-bg: rgba(62,166,78,.08);
    --success-border: rgba(62,166,78,.2);
    --success-glow: rgba(62,166,78,.25);
    --shadow: 0 2px 32px rgba(0,0,0,.3), 0 0 0 1px rgba(255,255,255,.04);
    --toggle-bg: #2c2c34;
  }

  * { box-sizing: border-box; margin: 0; padding: 0; }
  html { scrollbar-gutter: stable; overflow-y: scroll; }

  body {
    font-family: 'Lexend', sans-serif;
    font-weight: 400;
    background: var(--bg);
    color: var(--text);
    min-height: 100vh;
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding: 48px 20px 60px;
    transition: background .25s, color .25s;
  }

  /* ── staggered entrance ── */
  @keyframes rise {
    from { opacity: 0; transform: translateY(18px); }
    to   { opacity: 1; transform: translateY(0); }
  }
  .card { animation: rise .5s cubic-bezier(.22,1,.36,1) both; }
  .field:nth-child(1) { animation: rise .45s cubic-bezier(.22,1,.36,1) .08s both; }
  .field:nth-child(2) { animation: rise .45s cubic-bezier(.22,1,.36,1) .14s both; }
  .field:nth-child(3) { animation: rise .45s cubic-bezier(.22,1,.36,1) .20s both; }
  .field:nth-child(4) { animation: rise .45s cubic-bezier(.22,1,.36,1) .26s both; }
  .field:nth-child(5) { animation: rise .45s cubic-bezier(.22,1,.36,1) .32s both; }
  .field:nth-child(6) { animation: rise .45s cubic-bezier(.22,1,.36,1) .38s both; }

  /* ── card ── */
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 4px;
    width: 100%;
    max-width: 480px;
    box-shadow: var(--shadow);
    overflow: hidden;
    transition: background .25s, border-color .25s, box-shadow .25s;
  }

  .card-body { padding: 36px 32px 32px; }

  /* ── header ── */
  .header {
    text-align: center;
    margin-bottom: 28px;
  }

  .header h1 {
    font-family: 'Lexend', sans-serif;
    font-weight: 600;
    font-size: 1.45rem;
    letter-spacing: .02em;
    color: var(--text-bright);
  }

  .header h1 em {
    font-style: normal;
    color: var(--accent);
  }

  .header .sub {
    font-size: .75rem;
    font-weight: 300;
    color: var(--text-dim);
    letter-spacing: .08em;
    text-transform: uppercase;
    margin-top: 6px;
  }

  /* ── theme toggle ── */
  .theme-toggle {
    position: fixed;
    top: 16px;
    right: 16px;
    z-index: 10;
    width: 36px;
    height: 20px;
    border-radius: 10px;
    background: var(--toggle-bg);
    border: none;
    cursor: pointer;
    padding: 0;
    transition: background .25s;
    display: flex;
    align-items: center;
  }

  .theme-toggle::after {
    content: '';
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--surface);
    box-shadow: 0 1px 3px rgba(0,0,0,.15);
    margin-left: 2px;
    transition: transform .25s, background .25s;
  }

  [data-theme="dark"] .theme-toggle {
    background: var(--accent);
  }

  [data-theme="dark"] .theme-toggle::after {
    transform: translateX(16px);
  }

  .theme-toggle-icon {
    position: absolute;
    font-size: 11px;
    line-height: 1;
    pointer-events: none;
  }

  .theme-toggle-icon.sun  { left: 4px; }
  .theme-toggle-icon.moon { right: 4px; }

  [data-theme="light"] .theme-toggle-icon.sun  { opacity: .6; }
  [data-theme="light"] .theme-toggle-icon.moon { opacity: .25; }
  [data-theme="dark"]  .theme-toggle-icon.sun  { opacity: .3; }
  [data-theme="dark"]  .theme-toggle-icon.moon { opacity: .9; }

  /* ── form fields ── */
  .field { margin-bottom: 16px; }

  .field label {
    display: block;
    font-size: .7rem;
    font-weight: 500;
    color: var(--text-dim);
    letter-spacing: .06em;
    text-transform: uppercase;
    margin-bottom: 6px;
  }

  .field input,
  .field select {
    width: 100%;
    padding: 10px 14px;
    border: 1px solid var(--border);
    border-radius: 3px;
    background: var(--input-bg);
    color: var(--text);
    font-family: 'Lexend', sans-serif;
    font-size: .88rem;
    font-weight: 400;
    transition: border-color .2s, box-shadow .2s, background .25s, color .25s;
    -webkit-appearance: none;
    appearance: none;
  }

  .field select {
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath d='M0 0l5 6 5-6z' fill='%238a8a8e'/%3E%3C/svg%3E");
    background-repeat: no-repeat;
    background-position: right 12px center;
    padding-right: 32px;
  }

  .field input:focus,
  .field select:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-glow);
  }

  .field input::placeholder { color: var(--text-dim); font-weight: 300; }

  .row { display: flex; gap: 12px; }
  .row > .field { flex: 1; margin-bottom: 0; }

  /* ── remember me ── */
  .remember {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
    margin-bottom: 16px;
    cursor: pointer;
    font-size: .78rem;
    color: var(--text-dim);
    user-select: none;
  }

  .remember input[type="checkbox"] {
    width: 14px; height: 14px;
    accent-color: var(--accent);
    cursor: pointer;
  }

  /* ── drop zone ── */
  .dropzone {
    position: relative;
    border: 1.5px dashed var(--border);
    border-radius: 3px;
    padding: 28px 16px;
    text-align: center;
    cursor: pointer;
    transition: border-color .25s, background .25s;
    background: var(--accent-tint);
  }

  .dropzone:hover {
    border-color: var(--accent);
    background: var(--accent-tint-md);
  }

  .dropzone.dragover {
    border-color: var(--accent);
    background: var(--accent-tint-md);
  }

  .dropzone.has-file {
    border-style: solid;
    border-color: var(--accent);
    background: var(--accent-tint-md);
  }

  .dropzone-icon {
    width: 32px; height: 32px;
    margin: 0 auto 10px;
    opacity: .35;
    transition: opacity .25s;
  }

  .dropzone:hover .dropzone-icon,
  .dropzone.dragover .dropzone-icon { opacity: .6; }
  .dropzone.has-file .dropzone-icon { opacity: .7; }

  .dropzone-text {
    font-size: .82rem;
    font-weight: 300;
    color: var(--text-dim);
    line-height: 1.5;
  }

  .dropzone-text strong {
    font-weight: 500;
    color: var(--text);
  }

  .dropzone .fileinfo {
    margin-top: 8px;
    font-family: 'Fira Code', monospace;
    font-size: .78rem;
    color: var(--accent);
    word-break: break-all;
  }

  .dropzone .fileinfo .size {
    color: var(--text-dim);
    font-weight: 400;
  }

  .dropzone input[type="file"] { display: none; }

  /* ── submit button ── */
  .btn {
    width: 100%;
    padding: 13px 20px;
    border: none;
    border-radius: 3px;
    background: var(--accent);
    color: #fff;
    font-family: 'Lexend', sans-serif;
    font-size: .88rem;
    font-weight: 500;
    letter-spacing: .04em;
    cursor: pointer;
    margin-top: 20px;
    transition: all .2s;
  }

  .btn:hover:not(:disabled) {
    background: var(--accent-hover);
    box-shadow: 0 4px 16px var(--accent-glow-strong);
  }

  .btn:active:not(:disabled) { transform: scale(.99); }

  .btn:disabled { opacity: .4; cursor: not-allowed; }

  /* ── progress — reserve space to prevent layout shift ── */
  #progress {
    display: none;
    margin-top: 24px;
    padding-top: 20px;
    border-top: 1px solid var(--border);
    min-height: 180px;
  }

  @keyframes barGlow {
    0%, 100% { box-shadow: 0 0 6px var(--accent-glow); }
    50%      { box-shadow: 0 0 12px var(--accent-glow-strong); }
  }

  .bar-track {
    height: 4px;
    background: var(--border);
    border-radius: 2px;
    overflow: hidden;
  }

  .bar-fill {
    height: 100%;
    width: 0%;
    border-radius: 2px;
    background: var(--accent);
    transition: width .4s cubic-bezier(.22,1,.36,1);
    animation: barGlow 2s ease-in-out infinite;
  }

  .bar-fill.done {
    animation: none;
    background: var(--success);
    box-shadow: 0 0 10px var(--success-glow);
  }

  .bar-fill.error {
    animation: none;
    background: var(--error);
  }

  #status {
    font-size: .78rem;
    font-weight: 400;
    color: var(--text-dim);
    margin-top: 10px;
    min-height: 1.4em;
  }

  #log {
    margin-top: 10px;
    height: 120px;
    overflow-y: auto;
    font-family: 'Fira Code', monospace;
    font-size: .68rem;
    line-height: 1.6;
    color: var(--text-dim);
    padding: 10px 12px;
    background: var(--log-bg);
    border: 1px solid var(--border);
    border-radius: 3px;
    white-space: pre-wrap;
    word-break: break-all;
    transition: background .25s, border-color .25s, color .25s;
  }

  /* scrollbar */
  #log::-webkit-scrollbar { width: 4px; }
  #log::-webkit-scrollbar-track { background: transparent; }
  #log::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }

  /* ── result ── */
  @keyframes fadeScale {
    from { opacity: 0; transform: scale(.96); }
    to   { opacity: 1; transform: scale(1); }
  }

  #result {
    display: none;
    margin-top: 20px;
    padding: 20px;
    background: var(--success-bg);
    border: 1px solid var(--success-border);
    border-radius: 3px;
    text-align: center;
    animation: fadeScale .35s cubic-bezier(.22,1,.36,1) both;
  }

  #result .label {
    font-size: .68rem;
    font-weight: 500;
    text-transform: uppercase;
    letter-spacing: .1em;
    color: var(--success);
    margin-bottom: 8px;
  }

  #result a {
    display: block;
    font-family: 'Fira Code', monospace;
    font-size: .82rem;
    color: var(--accent);
    text-decoration: none;
    word-break: break-all;
    transition: color .2s;
  }

  #result a:hover { color: var(--accent-hover); }

  .copy-btn {
    display: inline-block;
    margin-top: 10px;
    padding: 5px 14px;
    border: 1px solid var(--border);
    border-radius: 3px;
    background: transparent;
    color: var(--text-dim);
    font-family: 'Lexend', sans-serif;
    font-size: .72rem;
    cursor: pointer;
    transition: all .15s;
  }

  .copy-btn:hover { border-color: var(--text-dim); color: var(--text); }
  .copy-btn.copied { border-color: var(--success); color: var(--success); }

  /* ── error ── */
  #error-msg {
    display: none;
    margin-top: 16px;
    padding: 14px 16px;
    background: var(--error-bg);
    border: 1px solid var(--error-border);
    border-radius: 3px;
    font-size: .82rem;
    color: var(--error);
    line-height: 1.5;
    animation: fadeScale .3s cubic-bezier(.22,1,.36,1) both;
  }
</style>
</head>
<body>
<button class="theme-toggle" id="themeToggle" type="button" aria-label="Toggle dark mode">
  <span class="theme-toggle-icon sun">&#9728;</span>
  <span class="theme-toggle-icon moon">&#9790;</span>
</button>
<div class="card">
  <div class="card-body">
    <div class="header">
      <h1>combatlog<em>.dev</em></h1>
      <div class="sub">a privacy conscious uploader for warcraftlogs</div>
    </div>

    <form id="form" autocomplete="on">
      <div class="field">
        <label for="email">Email</label>
        <input id="email" name="email" type="email" required autocomplete="email" placeholder="you@example.com">
      </div>

      <div class="field">
        <label for="password">Password</label>
        <input id="password" name="password" type="password" required autocomplete="current-password">
      </div>

      <label class="remember">
        <input type="checkbox" id="rememberMe" checked> Remember credentials
      </label>

      <div class="row field">
        <div class="field">
          <label for="region">Region</label>
          <select id="region" name="region">
            <option value="1">US</option>
            <option value="2" selected>EU</option>
            <option value="3">KR</option>
            <option value="4">TW</option>
            <option value="5">CN</option>
          </select>
        </div>
        <div class="field">
          <label for="visibility">Visibility</label>
          <select id="visibility" name="visibility">
            <option value="0">Public</option>
            <option value="1">Private</option>
            <option value="2" selected>Unlisted</option>
          </select>
        </div>
      </div>

      <div class="field">
        <label>Combat Log</label>
        <div class="dropzone" id="dropzone">
          <svg class="dropzone-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z"/>
            <polyline points="14 2 14 8 20 8"/>
            <line x1="12" y1="18" x2="12" y2="12"/>
            <line x1="9" y1="15" x2="12" y2="12"/>
            <line x1="15" y1="15" x2="12" y2="12"/>
          </svg>
          <div class="dropzone-text">
            Drop your <strong>WoWCombatLog</strong> here<br>or click to browse
          </div>
          <div class="fileinfo" id="fileinfo"></div>
          <input type="file" id="logfile" accept=".txt">
        </div>
      </div>

      <button type="submit" id="btn" class="btn">Upload Log</button>
    </form>

    <div id="progress">
      <div class="bar-track"><div class="bar-fill" id="bar"></div></div>
      <div id="status"></div>
      <div id="log"></div>
    </div>

    <div id="result">
      <div class="label">Upload complete</div>
      <a id="report-link" href="#" target="_blank"></a>
      <button class="copy-btn" id="copyBtn" type="button">Copy URL</button>
    </div>

    <div id="error-msg"></div>
  </div>
</div>

<script>
/* ── theme toggle ── */
const THEME_KEY = 'wcl_theme';
function applyTheme(t) {
  document.documentElement.setAttribute('data-theme', t);
  localStorage.setItem(THEME_KEY, t);
}
// init: respect saved pref, else match OS
const saved = localStorage.getItem(THEME_KEY);
if (saved) {
  applyTheme(saved);
} else if (window.matchMedia('(prefers-color-scheme: dark)').matches) {
  applyTheme('dark');
}

document.getElementById('themeToggle').addEventListener('click', () => {
  const cur = document.documentElement.getAttribute('data-theme') || 'light';
  applyTheme(cur === 'light' ? 'dark' : 'light');
});

/* ── credentials persistence ── */
const STORE_KEY = 'wcl_upload_creds';

function loadCreds() {
  try {
    const d = JSON.parse(localStorage.getItem(STORE_KEY));
    if (d) {
      document.getElementById('email').value = d.email || '';
      document.getElementById('password').value = d.password || '';
      if (d.region) document.getElementById('region').value = d.region;
      if (d.visibility) document.getElementById('visibility').value = d.visibility;
    }
  } catch {}
}

function saveCreds() {
  if (!document.getElementById('rememberMe').checked) return;
  localStorage.setItem(STORE_KEY, JSON.stringify({
    email: document.getElementById('email').value,
    password: document.getElementById('password').value,
    region: document.getElementById('region').value,
    visibility: document.getElementById('visibility').value,
  }));
}

loadCreds();

/* ── file size formatting ── */
function fmtSize(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1048576) return (bytes/1024).toFixed(1) + ' KB';
  return (bytes/1048576).toFixed(1) + ' MB';
}

/* ── drop zone ── */
const dz = document.getElementById('dropzone');
const fi = document.getElementById('logfile');
const fileinfo = document.getElementById('fileinfo');

dz.addEventListener('click', () => fi.click());
dz.addEventListener('dragover', e => { e.preventDefault(); dz.classList.add('dragover'); });
dz.addEventListener('dragleave', () => dz.classList.remove('dragover'));
dz.addEventListener('drop', e => {
  e.preventDefault(); dz.classList.remove('dragover');
  if (e.dataTransfer.files.length) { fi.files = e.dataTransfer.files; showFile(); }
});
fi.addEventListener('change', showFile);

function showFile() {
  const f = fi.files[0];
  if (!f) return;
  dz.classList.add('has-file');
  fileinfo.innerHTML = f.name + ' <span class="size">' + fmtSize(f.size) + '</span>';
}

/* ── copy button ── */
document.getElementById('copyBtn').addEventListener('click', function() {
  const url = document.getElementById('report-link').href;
  navigator.clipboard.writeText(url).then(() => {
    this.textContent = 'Copied';
    this.classList.add('copied');
    setTimeout(() => { this.textContent = 'Copy URL'; this.classList.remove('copied'); }, 1500);
  });
});

/* ── upload ── */
document.getElementById('form').addEventListener('submit', async e => {
  e.preventDefault();
  const file = fi.files[0];
  if (!file) { dz.style.borderColor = 'var(--error)'; setTimeout(() => dz.style.borderColor = '', 1000); return; }

  saveCreds();

  const btn   = document.getElementById('btn');
  const prog  = document.getElementById('progress');
  const bar   = document.getElementById('bar');
  const status = document.getElementById('status');
  const log   = document.getElementById('log');
  const result = document.getElementById('result');
  const errMsg = document.getElementById('error-msg');

  btn.disabled = true;
  btn.textContent = 'Uploading...';
  prog.style.display = 'block';
  result.style.display = 'none';
  errMsg.style.display = 'none';
  log.textContent = '';
  status.textContent = 'Sending file to local server...';
  bar.style.width = '1%';
  bar.className = 'bar-fill';

  const fd = new FormData();
  fd.append('logfile', file);
  fd.append('email', document.getElementById('email').value);
  fd.append('password', document.getElementById('password').value);
  fd.append('region', document.getElementById('region').value);
  fd.append('visibility', document.getElementById('visibility').value);

  try {
    const resp = await fetch('/upload', { method: 'POST', body: fd });
    if (!resp.ok) throw new Error(await resp.text());
    const { jobId } = await resp.json();
    const es = new EventSource('/events/' + jobId);

    es.addEventListener('progress', e => {
      const d = JSON.parse(e.data);
      bar.style.width = d.pct + '%';
      status.textContent = d.message;
      log.textContent += d.message + '\n';
      log.scrollTop = log.scrollHeight;
    });

    es.addEventListener('done', e => {
      const d = JSON.parse(e.data);
      bar.style.width = '100%';
      bar.classList.add('done');
      status.textContent = '';
      result.style.display = 'block';
      const link = document.getElementById('report-link');
      link.href = d.url;
      link.textContent = d.url;
      es.close();
      btn.disabled = false;
      btn.textContent = 'Upload Log';
    });

    es.addEventListener('error', e => {
      try {
        const d = JSON.parse(e.data);
        errMsg.textContent = d.message;
        errMsg.style.display = 'block';
        bar.classList.add('error');
        status.textContent = 'Upload failed';
      } catch {}
      es.close();
      btn.disabled = false;
      btn.textContent = 'Upload Log';
    });

  } catch (err) {
    errMsg.textContent = err.message;
    errMsg.style.display = 'block';
    btn.disabled = false;
    btn.textContent = 'Upload Log';
  }
});
</script>
</body>
</html>
'''

if __name__ == '__main__':
    print('Starting WarcraftLogs Uploader on http://localhost:5050')
    app.run(host='0.0.0.0', port=5050, debug=False)
