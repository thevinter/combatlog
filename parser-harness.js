// Usage:
//   node parser-harness.js    — reads JSON {gamedataCode, parserCode} from first stdin line,
//                                then switches to the command loop.
//
// Supports both parser generations served by RPGLogs sites:
//   - legacy (warcraft): the bundle defines loose globals (parseLogLine, logFights,
//     clearParserState, buildActorsString, ...)
//   - class-based (fellowship, and other sites' log-parsers/ bundles): the bundle
//     defines only window.LogParser; we instantiate and drive it the same way the
//     site's /desktop-client/parser page glue does.

const readline = require("readline");
const vm = require("vm");

global.window = global;
global.document = {};
global.navigator = { userAgent: "" };
global.URLSearchParams = class {
  constructor() {}
  get(key) {
    if (key === "gameContentDetectionEnabled") return "false";
    if (key === "metersEnabled") return "false";
    if (key === "liveFightDataEnabled") return "false";
    if (key === "id") return "1";
    return null;
  }
};
global.location = { search: "" };
window.gameContentDetectionEnabled = false;
window.metersEnabled = false;
window.liveFightDataEnabled = false;
window.setWarningText = (text) => {
  process.stderr.write(`[WARN] ${text}\n`);
};
window.setErrorText = (text) => {
  process.stderr.write(`[ERROR] ${text}\n`);
};
window.sendLogMessage = (...args) => {
  process.stderr.write(`[LOG] ${args.join(" ")}\n`);
};
window.sendEventMessage = (event) => {};
window.sendToHost = () => {};
window.addEventListener = () => {};
window.postMessage = () => {};

function respond(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function errorMessage(e) {
  if (e && e.message) return e.message;
  const s = String(e);
  return s === "[object Object]" ? "unknown parser error" : s;
}

let classParser = null;

function newClassParser() {
  return new window.LogParser(
    0,
    false,
    [],
    false,
    window.gameContentDetectionEnabled,
    window.metersEnabled,
    window.gameContentTypes || null,
  );
}

function handleClassCommand(cmd) {
  switch (cmd.action) {
    case "clear-state":
      classParser = newClassParser();
      parsedLineCount = 0;
      respond({ ok: true });
      break;
    case "set-start-date": {
      const d = new Date(cmd.startDate);
      d.setHours(0, 0, 0, 0);
      classParser.setLogStartDate(d);
      respond({ ok: true });
      break;
    }
    case "set-report-code":
      respond({ ok: true });
      break;
    // Live logging was built against the legacy warcraft bundle's globals.
    // The new bundles haven't been yet verified so livelogging is not yet supproted.
    case "set-live-logging-start-time":
    case "collect-in-progress-fight":
      respond({
        ok: false,
        error: `live logging is not supported by this game's parser (${cmd.action})`,
      });
      break;
    case "parse-lines": {
      classParser.prepareToParseLines(
        cmd.scanning || false,
        cmd.selectedRegion || 2,
        cmd.raidsToUpload || [],
        cmd.logFilePosition || null,
        window.gameContentDetectionEnabled,
        window.metersEnabled,
        window.gameContentTypes || null,
      );
      for (let i = 0; i < cmd.lines.length; i++) {
        parsedLineCount++;
        try {
          classParser.parseLine(cmd.lines[i]);
        } catch (e) {
          respond({
            ok: false,
            error: errorMessage(e),
            line: cmd.lines[i],
            parsedLineCount,
          });
          return;
        }
      }
      respond({ ok: true, parsedLineCount });
      break;
    }
    case "collect-fights": {
      const lf =
        classParser.collectCommittedFights(cmd.pushFightIfNeeded || false) ||
        {};
      const fights = (lf.fights || []).map((f) => ({
        eventCount: f.eventCount,
        eventsString: f.eventsString,
      }));
      respond({
        ok: true,
        logVersion: lf.logVersion,
        gameVersion: lf.gameVersion,
        mythic: lf.mythic,
        startTime: lf.startTime,
        endTime: lf.endTime,
        fights,
      });
      break;
    }
    case "collect-master-info": {
      const mt = classParser.collectMasterTable() || {};
      respond(Object.assign({ ok: true }, mt));
      break;
    }
    case "clear-fights":
      classParser.clearCommittedFightsAndScannedRaids();
      respond({ ok: true });
      break;
    case "get-parser-version":
      respond({
        ok: true,
        parserVersion:
          typeof parserVersion !== "undefined" ? parserVersion : "unknown",
      });
      break;
    case "ping":
      respond({ ok: true, pong: true });
      break;
    default:
      respond({ ok: false, error: `Unknown action: ${cmd.action}` });
  }
}

function handleLegacyCommand(cmd) {
  switch (cmd.action) {
    case "clear-state":
      clearParserState();
      parsedLineCount = 0;
      respond({ ok: true });
      break;
    case "set-start-date":
      logStartDate = logCurrDate = cmd.startDate;
      respond({ ok: true });
      break;
    case "set-report-code":
      respond({ ok: true });
      break;
    case "set-live-logging-start-time":
      liveLoggingStartTime = cmd.startTime;
      respond({ ok: true });
      break;
    case "parse-lines":
      for (let i = 0; i < cmd.lines.length; i++) {
        parsedLineCount++;
        try {
          parseLogLine(
            cmd.lines[i],
            cmd.scanning || false,
            cmd.selectedRegion || 2,
            cmd.raidsToUpload || [],
            cmd.logFilePosition || null,
          );
        } catch (e) {
          respond({
            ok: false,
            error: errorMessage(e),
            line: cmd.lines[i],
            parsedLineCount,
          });
          return;
        }
      }
      respond({ ok: true, parsedLineCount });
      break;
    case "collect-fights":
      if (cmd.pushFightIfNeeded) pushLogFight(cmd.scanningOnly || false);
      logFights.logVersion = logVersion;
      logFights.gameVersion = gameVersion;
      logFights.mythic = mythic;
      logFights.startTime = startTime;
      logFights.endTime = endTime;
      const fights = logFights.fights.map((f) => ({
        eventCount: f.eventCount,
        eventsString: f.eventsString,
      }));
      respond({
        ok: true,
        logVersion,
        gameVersion,
        mythic,
        startTime,
        endTime,
        fights,
      });
      break;
    case "collect-in-progress-fight": {
      // same guards as ipcCollectInProgressFight in the site's parser-page glue
      const pending = lastAssignedEventID - currentEventIndex;
      const inProgress =
        inCombat && pending > 1000
          ? [
              {
                eventCount: pending,
                eventsString,
              },
            ]
          : [];
      respond({
        ok: true,
        logVersion,
        gameVersion,
        mythic,
        startTime,
        endTime,
        fights: inProgress,
      });
      break;
    }
    case "collect-master-info":
      buildActorsString();
      if (typeof buildAbilitiesStringIfNeeded === "function")
        buildAbilitiesStringIfNeeded();
      buildPetsString();
      respond({
        ok: true,
        lastAssignedActorID,
        actorsString,
        lastAssignedAbilityID,
        abilitiesString,
        lastAssignedTupleID,
        tuplesString,
        lastAssignedPetID,
        petsString,
      });
      break;
    case "clear-fights":
      logFights = { fights: [] };
      scannedRaids = [];
      respond({ ok: true });
      break;
    case "get-parser-version":
      respond({
        ok: true,
        parserVersion:
          typeof parserVersion !== "undefined" ? parserVersion : "unknown",
      });
      break;
    case "ping":
      respond({ ok: true, pong: true });
      break;
    default:
      respond({ ok: false, error: `Unknown action: ${cmd.action}` });
  }
}

function startCommandLoop() {
  const isClassParser = typeof window.LogParser === "function";
  if (isClassParser) {
    global.parsedLineCount = 0;
    classParser = newClassParser();
  }
  const handle = isClassParser ? handleClassCommand : handleLegacyCommand;
  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  rl.on("line", (line) => {
    try {
      handle(JSON.parse(line));
    } catch (e) {
      respond({ ok: false, error: e.message, stack: e.stack });
    }
  });
}

let buf = "";
process.stdin.setEncoding("utf-8");
process.stdin.on("readable", function onReadable() {
  let chunk;
  while ((chunk = process.stdin.read()) !== null) {
    buf += chunk;
    const nl = buf.indexOf("\n");
    if (nl !== -1) {
      const firstLine = buf.slice(0, nl);
      const remainder = buf.slice(nl + 1);
      process.stdin.removeListener("readable", onReadable);
      process.stdin.pause();

      try {
        const payload = JSON.parse(firstLine);
        if (payload.gamedataCode) vm.runInThisContext(payload.gamedataCode);
        if (payload.parserCode) vm.runInThisContext(payload.parserCode);
      } catch (e) {
        respond({ ready: false, error: e.message });
        process.exit(1);
      }
      respond({
        ready: true,
        parserVersion:
          typeof parserVersion !== "undefined" ? parserVersion : "unknown",
      });

      if (remainder) process.stdin.unshift(Buffer.from(remainder, "utf-8"));
      startCommandLoop();
      return;
    }
  }
});
