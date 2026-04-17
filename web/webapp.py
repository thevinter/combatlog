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
_LOCAL_HARNESS = os.path.join(SCRIPT_DIR, 'parser-harness.js')
_REPO_HARNESS = os.path.normpath(os.path.join(SCRIPT_DIR, '..', 'parser-harness.js'))
PARSER_HARNESS = _LOCAL_HARNESS if os.path.exists(_LOCAL_HARNESS) else _REPO_HARNESS

_LOCAL_INDEX = os.path.join(SCRIPT_DIR, 'index.html')
_REPO_INDEX = os.path.normpath(os.path.join(SCRIPT_DIR, '..', 'app', 'src', 'index.html'))
INDEX_PATH = _LOCAL_INDEX if os.path.exists(_LOCAL_INDEX) else _REPO_INDEX
with open(INDEX_PATH, 'r', encoding='utf-8') as _f:
    INDEX_HTML = _f.read()
BATCH_SIZE = 100000
BASE_URL = 'https://www.warcraftlogs.com'
FALLBACK_CLIENT_VERSION = '9.0.1'
CHROME_VERSION = os.environ.get('WCL_CHROME_VERSION', '134.0.6998.205')
ELECTRON_VERSION = os.environ.get('WCL_ELECTRON_VERSION', '37.7.0')
PARSER_VERSION = 59


def _fetch_latest_client_version():
    """Fetch the latest Archon client version from GitHub releases."""
    import urllib.request
    try:
        req = urllib.request.Request(
            'https://api.github.com/repos/RPGLogs/Uploaders-archon/releases/latest',
            headers={'Accept': 'application/vnd.github.v3+json', 'User-Agent': 'wcl-upload'},
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
        version = data.get('name', '').strip()
        if version:
            print(f'Fetched latest Archon version: {version}')
            return version
    except Exception as e:
        print(f'Warning: Could not fetch latest Archon version: {e}')
    print(f'Using fallback client version: {FALLBACK_CLIENT_VERSION}')
    return FALLBACK_CLIENT_VERSION


CLIENT_VERSION = os.environ.get('WCL_CLIENT_VERSION') or _fetch_latest_client_version()

MAX_RETRIES = 3
RETRY_BASE_DELAY = 1.0


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
    resp = session.request('GET', url, headers={'User-Agent': _user_agent()})
    html = resp.text

    m = re.search(
        r'<script[^>]*>(.*?window\.gameContentTypes.*?)</script>', html, re.DOTALL)
    gamedata_code = m.group(1).strip() if m else ''

    m2 = re.search(r'src="(https://assets\.rpglogs\.com/js/parser-warcraft[^"]+)"', html)
    if not m2:
        raise RuntimeError('Could not find parser-warcraft JS URL in parser page')
    parser_url = m2.group(1)
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
        resp = self._request('POST', f'{BASE_URL}/desktop-client/log-in',
            json={'email': email, 'password': password, 'version': CLIENT_VERSION},
            headers={'Content-Type': 'application/json', 'User-Agent': _user_agent()},
        )
        result = resp.json()
        self.user = result.get('user')
        return result

    def create_report(self, filename, start_time, end_time, region, visibility, guild_id, parser_version=PARSER_VERSION):
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
    with zipfile.ZipFile(buf, 'w', zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
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
        last_master_ids = None
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
            master_ids = (mi['lastAssignedActorID'], mi['lastAssignedAbilityID'],
                          mi['lastAssignedTupleID'], mi['lastAssignedPetID'])
            if master_ids != last_master_ids:
                session.set_master_table(report_code, segment_id, make_zip(build_master_string(mi, fd['logVersion'], fd['gameVersion'])))
                last_master_ids = master_ids
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


@app.errorhandler(413)
def file_too_large(e):
    return json.dumps({'error': 'File exceeds the 1 GB size limit.'}), 413, {'Content-Type': 'application/json'}


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


if __name__ == '__main__':
    print('Starting WarcraftLogs Uploader on http://localhost:5050')
    app.run(host='0.0.0.0', port=5050, debug=False)
