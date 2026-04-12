// Usage:
//   node parser-harness.js                 — loads parser-warcraft.js + parser-gamedata.js from disk (fallback)
//   node parser-harness.js --from-stdin    — reads JSON {gamedataCode, parserCode} from first stdin line

const fs = require('fs');
const path = require('path');
const readline = require('readline');
const vm = require('vm');

global.window = global;
global.document = {};
global.navigator = { userAgent: '' };
global.URLSearchParams = class {
    constructor() {}
    get(key) {
        if (key === 'gameContentDetectionEnabled') return 'false';
        if (key === 'metersEnabled') return 'false';
        if (key === 'liveFightDataEnabled') return 'false';
        if (key === 'id') return '1';
        return null;
    }
};
global.location = { search: '' };
window.gameContentDetectionEnabled = false;
window.metersEnabled = false;
window.liveFightDataEnabled = false;
window.setWarningText = (text) => { process.stderr.write(`[WARN] ${text}\n`); };
window.setErrorText = (text) => { process.stderr.write(`[ERROR] ${text}\n`); };
window.sendLogMessage = (...args) => { process.stderr.write(`[LOG] ${args.join(' ')}\n`); };
window.sendEventMessage = (event) => {};
window.sendToHost = () => {};
window.addEventListener = () => {};
window.postMessage = () => {};

function respond(obj) {
    process.stdout.write(JSON.stringify(obj) + '\n');
}

function startCommandLoop() {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout, terminal: false });

    rl.on('line', (line) => {
        try {
            const cmd = JSON.parse(line);
            switch (cmd.action) {
                case 'clear-state':
                    clearParserState();
                    parsedLineCount = 0;
                    respond({ ok: true });
                    break;
                case 'set-start-date':
                    logStartDate = logCurrDate = cmd.startDate;
                    respond({ ok: true });
                    break;
                case 'set-report-code':
                    respond({ ok: true });
                    break;
                case 'parse-lines':
                    for (let i = 0; i < cmd.lines.length; i++) {
                        parsedLineCount++;
                        try {
                            parseLogLine(cmd.lines[i], cmd.scanning || false, cmd.selectedRegion || 2, cmd.raidsToUpload || [], cmd.logFilePosition || null);
                        } catch (e) {
                            respond({ ok: false, error: e.message, line: cmd.lines[i], parsedLineCount });
                            return;
                        }
                    }
                    respond({ ok: true, parsedLineCount });
                    break;
                case 'collect-fights':
                    if (cmd.pushFightIfNeeded)
                        pushLogFight(cmd.scanningOnly || false);
                    logFights.logVersion = logVersion;
                    logFights.gameVersion = gameVersion;
                    logFights.mythic = mythic;
                    logFights.startTime = startTime;
                    logFights.endTime = endTime;
                    const fights = logFights.fights.map(f => ({
                        eventCount: f.eventCount,
                        eventsString: f.eventsString
                    }));
                    respond({ ok: true, logVersion, gameVersion, mythic, startTime, endTime, fights });
                    break;
                case 'collect-master-info':
                    buildActorsString();
                    if (typeof buildAbilitiesStringIfNeeded === 'function')
                        buildAbilitiesStringIfNeeded();
                    buildPetsString();
                    respond({
                        ok: true,
                        lastAssignedActorID, actorsString,
                        lastAssignedAbilityID, abilitiesString,
                        lastAssignedTupleID, tuplesString,
                        lastAssignedPetID, petsString
                    });
                    break;
                case 'clear-fights':
                    logFights = { fights: [] };
                    scannedRaids = [];
                    respond({ ok: true });
                    break;
                case 'get-parser-version':
                    respond({ ok: true, parserVersion: typeof parserVersion !== 'undefined' ? parserVersion : 'unknown' });
                    break;
                case 'ping':
                    respond({ ok: true, pong: true });
                    break;
                default:
                    respond({ ok: false, error: `Unknown action: ${cmd.action}` });
            }
        } catch (e) {
            respond({ ok: false, error: e.message, stack: e.stack });
        }
    });
}

const fromStdin = process.argv.includes('--from-stdin');

if (fromStdin) {

    let buf = '';
    process.stdin.setEncoding('utf-8');
    process.stdin.on('readable', function onReadable() {
        let chunk;
        while ((chunk = process.stdin.read()) !== null) {
            buf += chunk;
            const nl = buf.indexOf('\n');
            if (nl !== -1) {
                const firstLine = buf.slice(0, nl);
                const remainder = buf.slice(nl + 1);
                process.stdin.removeListener('readable', onReadable);
                process.stdin.pause();

                try {
                    const payload = JSON.parse(firstLine);
                    if (payload.gamedataCode) vm.runInThisContext(payload.gamedataCode);
                    if (payload.parserCode) vm.runInThisContext(payload.parserCode);
                } catch (e) {
                    respond({ ready: false, error: e.message });
                    process.exit(1);
                }
                respond({ ready: true, parserVersion: typeof parserVersion !== 'undefined' ? parserVersion : 'unknown' });

                if (remainder) process.stdin.unshift(Buffer.from(remainder, 'utf-8'));
                startCommandLoop();
                return;
            }
        }
    });
} else {
    const gamedataPath = path.join(__dirname, 'parser-gamedata.js');
    if (fs.existsSync(gamedataPath)) eval(fs.readFileSync(gamedataPath, 'utf-8'));
    const parserPath = path.join(__dirname, 'parser-warcraft.js');
    eval(fs.readFileSync(parserPath, 'utf-8'));
    respond({ ready: true, parserVersion: typeof parserVersion !== 'undefined' ? parserVersion : 'unknown' });
    startCommandLoop();
}
