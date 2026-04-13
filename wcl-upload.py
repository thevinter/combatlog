#!/usr/bin/env python3
import argparse
import io
import json
import os
import random
import string
import subprocess
import sys
import time
import zipfile

from curl_cffi import requests as cffi_requests

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
                    print(f"  HTTP {resp.status_code}, retrying in {delay:.1f}s "
                          f"(attempt {attempt+1}/{MAX_RETRIES})...")
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
        print(f"Logged in as {self.user['userName']}")
        return result

    def create_report(self, filename, start_time, end_time, region=2, visibility=2, guild_id=None, parser_version=PARSER_VERSION):
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

    def set_master_table(self, report_code, segment_id, master_zip_bytes):
        self._multipart(
            f'{BASE_URL}/desktop-client/set-report-master-table/{report_code}',
            [('segmentId', str(segment_id)), ('isRealTime', 'false')],
            [('logfile', 'blob', 'application/zip', master_zip_bytes)],
        )

    def add_segment(self, report_code, segment_id, start_time, end_time,
                    mythic, fights_zip_bytes, is_real_time=False):
        params = json.dumps({
            'startTime': start_time, 'endTime': end_time, 'mythic': mythic,
            'isLiveLog': False, 'isRealTime': is_real_time,
            'inProgressEventCount': 0, 'segmentId': segment_id,
        })
        resp = self._multipart(
            f'{BASE_URL}/desktop-client/add-report-segment/{report_code}',
            [('parameters', params)],
            [('logfile', 'blob', 'application/zip', fights_zip_bytes)],
        )
        return resp.json().get('nextSegmentId', segment_id + 1)

    def terminate_report(self, report_code):
        _jitter_sleep()
        self._request('POST', f'{BASE_URL}/desktop-client/terminate-report/{report_code}',
            headers={'User-Agent': _user_agent()},
        )


def fetch_parser_code(session):
    """Fetch the latest parser JS + game data from WCL (requires authed session).
    Returns (gamedata_code, parser_code, parser_version)."""
    import re
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


class Parser:
    def __init__(self, gamedata_code, parser_code):
        """Start parser with dynamically fetched code injected via stdin."""
        self.proc = subprocess.Popen(
            ['node', PARSER_HARNESS],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        payload = json.dumps({'gamedataCode': gamedata_code,
                              'parserCode': parser_code})
        self.proc.stdin.write(payload + '\n')
        self.proc.stdin.flush()
        ready = self._read_response()
        if not ready.get('ready'):
            raise RuntimeError(f"Parser failed to start: {ready}")

    def _send(self, obj):
        line = json.dumps(obj)
        self.proc.stdin.write(line + '\n')
        self.proc.stdin.flush()
        return self._read_response()

    def _read_response(self):
        line = self.proc.stdout.readline()
        if not line:
            stderr = self.proc.stderr.read()
            raise RuntimeError(f"Parser process died. stderr: {stderr}")
        return json.loads(line)

    def clear_state(self):
        return self._send({'action': 'clear-state'})

    def set_start_date(self, start_date):
        return self._send({'action': 'set-start-date', 'startDate': start_date})

    def parse_lines(self, lines, selected_region=2):
        return self._send({
            'action': 'parse-lines',
            'lines': lines,
            'selectedRegion': selected_region,
        })

    def collect_fights(self, push_fight_if_needed=True):
        return self._send({
            'action': 'collect-fights',
            'pushFightIfNeeded': push_fight_if_needed,
            'scanningOnly': False,
        })

    def collect_master_info(self):
        return self._send({'action': 'collect-master-info'})

    def clear_fights(self):
        return self._send({'action': 'clear-fights'})

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def make_zip(content_str):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        zf.writestr('log.txt', content_str)
    return buf.getvalue()


def build_master_table_string(master_info, log_version, game_version, log_file_details=''):
    parts = []
    parts.append(f"{log_version}|{game_version}|{log_file_details}")
    parts.append(str(master_info['lastAssignedActorID']))
    if master_info['actorsString']:
        parts.append(master_info['actorsString'].rstrip('\n'))
    parts.append(str(master_info['lastAssignedAbilityID']))
    if master_info['abilitiesString']:
        parts.append(master_info['abilitiesString'].rstrip('\n'))
    parts.append(str(master_info['lastAssignedTupleID']))
    if master_info['tuplesString']:
        parts.append(master_info['tuplesString'].rstrip('\n'))
    parts.append(str(master_info['lastAssignedPetID']))
    if master_info['petsString']:
        parts.append(master_info['petsString'].rstrip('\n'))
    return '\n'.join(parts) + '\n'


def build_fights_string(fights_data):
    total_events = sum(f['eventCount'] for f in fights_data['fights'])
    events_combined = ''.join(f['eventsString'] for f in fights_data['fights'])
    log_version = fights_data['logVersion']
    game_version = fights_data['gameVersion']
    return f"{log_version}|{game_version}\n{total_events}\n{events_combined}"


def parse_start_date_from_filename(filename):
    """Format: WoWCombatLog-MMDDYY_HHMMSS.txt"""
    import re
    match = re.search(r'WoWCombatLog-(\d{2})(\d{2})(\d{2})_(\d{2})(\d{2})(\d{2})', filename)
    if match:
        mm, dd, yy, hh, mi, ss = match.groups()
        year = 2000 + int(yy)
        month = int(mm)
        day = int(dd)
        return f"{month}/{day}/{year}"
    return None


def upload_log(filepath, email, password, region=2, visibility=2, guild_id=None):
    """Main upload flow."""
    filename = os.path.basename(filepath)

    print(f"Reading {filepath}...")
    with open(filepath, 'r', encoding='utf-8') as f:
        all_lines = [line.rstrip('\n').rstrip('\r') for line in f]
    total_lines = len(all_lines)
    print(f"  {total_lines} lines")

    # Login first (required to fetch parser)
    print("Logging in to WarcraftLogs...")
    session = WCLSession()
    session.login(email, password)

    # Fetch parser code dynamically from WCL
    print("Fetching latest parser from WarcraftLogs...")
    gamedata_code, parser_code, parser_version = fetch_parser_code(session.session)
    print(f"  Parser v{parser_version} loaded")

    # Start parser with fresh code
    print("Starting parser...")
    parser = Parser(gamedata_code, parser_code)
    parser.clear_state()

    start_date = parse_start_date_from_filename(filename)
    if start_date:
        parser.set_start_date(start_date)

    segment_id = 1
    report_code = None

    try:
        for batch_start in range(0, total_lines, BATCH_SIZE):
            batch_end = min(batch_start + BATCH_SIZE, total_lines)
            batch = all_lines[batch_start:batch_end]
            print(f"  Parsing lines {batch_start+1}-{batch_end}/{total_lines}...")

            result = parser.parse_lines(batch, selected_region=region)
            if not result.get('ok'):
                print(f"  Parse error: {result}")
                return None

            # Collect fights
            fights_data = parser.collect_fights(push_fight_if_needed=True)
            if not fights_data.get('ok'):
                print(f"  Collect fights error: {fights_data}")
                return None

            if not fights_data['fights']:
                continue

            # Create report on first segment
            if report_code is None:
                start_time = fights_data['startTime']
                end_time = fights_data['endTime']
                print(f"  Creating report (startTime={start_time})...")
                report_code = session.create_report(
                    filepath, start_time, end_time,
                    region=region, visibility=visibility, guild_id=guild_id,
                    parser_version=parser_version,
                )
                print(f"  Report code: {report_code}")

            # Collect and upload master table
            master_info = parser.collect_master_info()
            if not master_info.get('ok'):
                print(f"  Master info error: {master_info}")
                return None

            master_str = build_master_table_string(
                master_info,
                fights_data['logVersion'],
                fights_data['gameVersion'],
            )
            master_zip = make_zip(master_str)
            print(f"  Uploading master table (segment {segment_id}, {len(master_zip)} bytes)...")
            session.set_master_table(report_code, segment_id, master_zip)

            # Build and upload fights segment
            fights_str = build_fights_string(fights_data)
            fights_zip = make_zip(fights_str)
            total_events = sum(f['eventCount'] for f in fights_data['fights'])
            print(f"  Uploading segment {segment_id} ({total_events} events, {len(fights_zip)} bytes)...")
            segment_id = session.add_segment(
                report_code, segment_id,
                fights_data['startTime'],
                fights_data['endTime'],
                fights_data['mythic'],
                fights_zip,
            )
            print(f"  Next segment: {segment_id}")

            parser.clear_fights()

        if report_code:
            print(f"Terminating report {report_code}...")
            session.terminate_report(report_code)
            url = f"https://www.warcraftlogs.com/reports/{report_code}"
            print(f"\nUpload complete! Report URL: {url}")
            return url
        else:
            print("No fights found in log file.")
            return None

    finally:
        parser.close()


def main():
    p = argparse.ArgumentParser(description='Upload WoW combat logs to WarcraftLogs')
    p.add_argument('logfile', help='Path to WoWCombatLog*.txt file')
    p.add_argument('--email', required=True, help='WarcraftLogs email')
    p.add_argument('--password', required=True, help='WarcraftLogs password')
    p.add_argument('--region', type=int, default=2, help='Region (1=US, 2=EU, 3=KR, 4=TW, 5=CN)')
    p.add_argument('--visibility', type=int, default=2,
                   help='Visibility (0=Public, 1=Private, 2=Unlisted)')
    p.add_argument('--guild-id', type=int, default=None, help='Guild ID (optional)')
    args = p.parse_args()

    if not os.path.exists(args.logfile):
        print(f"Error: File not found: {args.logfile}")
        sys.exit(1)

    url = upload_log(
        args.logfile,
        args.email,
        args.password,
        region=args.region,
        visibility=args.visibility,
        guild_id=args.guild_id,
    )
    if not url:
        sys.exit(1)


if __name__ == '__main__':
    main()
