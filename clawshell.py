#!/usr/bin/env python3
"""ClawShell — External Watchdog Daemon for OpenClaw

Single responsibility: monitor OpenClaw gateway process health from outside.
Detects hangs, zombies, and crashes that the process cannot self-detect.

State machine:
  UNKNOWN → HEALTHY (on first successful tick)
  HEALTHY → HEAVY_INFERENCE (HTTP down but I/O active)
  HEAVY_INFERENCE → POSSIBLE_HANG (I/O stalled >120s)
  POSSIBLE_HANG → CONFIRMED_HANG (I/O stalled >300s)
  Any → DOWN (PID gone)
  Any → ZOMBIE (/proc state = Z)
  DOWN/HANG/ZOMBIE → HEALTHY (recovery)

Does NOT do: Telegram directly, audit scanning, LKG promotion, snapshots.
Calls alert.sh for all notifications.
Writes /var/lib/occlawshell/audit/gateway-proc-latest.json every 30s.
"""

import ctypes
import ctypes.util
import json
import logging
import os
import queue
import select
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Optional

from urllib.request import Request, urlopen

# ── Configuration ──────────────────────────────────────────────

TICK_INTERVAL = 1          # Main loop tick (seconds)
COLLECT_INTERVAL = 30      # /proc collection interval (seconds)

GATEWAY_PORT = int(os.environ.get("GATEWAY_PORT", "18789"))
HEALTH_URL = f"http://127.0.0.1:{GATEWAY_PORT}/health"
HEALTH_TIMEOUT = 5         # HTTP timeout (seconds)

HANG_WARN_SECS = 120       # I/O stall → POSSIBLE_HANG
HANG_CRIT_SECS = 300       # I/O stall → CONFIRMED_HANG

IO_DELTA_THRESHOLD = 1_048_576   # 1MB I/O delta → active
CTX_SWITCH_THRESHOLD = 10        # Context switch delta → active

ALERT_SCRIPT = "/var/lib/occlawshell/bin/alert.sh"
PROC_JSON_PATH = "/var/lib/occlawshell/audit/gateway-proc-latest.json"
GATEWAY_SERVICE = "openclaw-gateway.service"
AUTO_RECOVER_SCRIPT = "/var/lib/occlawshell/bin/auto-recover.sh"

RESTART_SETTLE_WAIT = 90   # Seconds to wait after restart before trying LKG recovery
RECOVERY_COOLDOWN = 1800   # Don't retry recovery for 30 minutes after an attempt

# Config file monitoring
OPENCLAW_DIR = os.environ.get("OPENCLAW_DIR", "/home/ocagent/.openclaw")
WATCHED_FILES = [
    os.path.join(OPENCLAW_DIR, "workspace", "SOUL.md"),
    os.path.join(OPENCLAW_DIR, "workspace", "AGENTS.md"),
    os.path.join(OPENCLAW_DIR, "workspace", "USER.md"),
    os.path.join(OPENCLAW_DIR, "openclaw.json"),
]
CONFIG_REWATCH_INTERVAL = 300  # Re-check for deleted-then-recreated files every 5 min

# Telegram bot (two-way)
TG_TOKEN = os.environ.get("CLAWSHELL_TG_TOKEN", "")
TG_CHAT = os.environ.get("CLAWSHELL_TG_CHAT", "")

LOG_FORMAT = "%(asctime)s [%(levelname)s] %(message)s"


# ── State Machine ──────────────────────────────────────────────

class State(Enum):
    UNKNOWN = "UNKNOWN"
    HEALTHY = "HEALTHY"
    HEAVY_INFERENCE = "HEAVY_INFERENCE"
    POSSIBLE_HANG = "POSSIBLE_HANG"
    CONFIRMED_HANG = "CONFIRMED_HANG"
    ZOMBIE = "ZOMBIE"
    DOWN = "DOWN"


# ── Data Classes ───────────────────────────────────────────────

class ProcessMetrics:
    """Snapshot of /proc/[pid] data."""
    __slots__ = (
        "pid", "state", "read_bytes", "write_bytes",
        "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches",
        "threads", "rss_bytes", "fd_count", "timestamp",
    )

    def __init__(self):
        self.pid: int = 0
        self.state: str = ""
        self.read_bytes: int = 0
        self.write_bytes: int = 0
        self.voluntary_ctxt_switches: int = 0
        self.nonvoluntary_ctxt_switches: int = 0
        self.threads: int = 0
        self.rss_bytes: int = 0
        self.fd_count: int = 0
        self.timestamp: float = 0.0

    def to_dict(self) -> dict:
        return {
            "pid": self.pid,
            "proc_state": self.state,
            "read_bytes": self.read_bytes,
            "write_bytes": self.write_bytes,
            "voluntary_ctxt_switches": self.voluntary_ctxt_switches,
            "nonvoluntary_ctxt_switches": self.nonvoluntary_ctxt_switches,
            "threads": self.threads,
            "rss_bytes": self.rss_bytes,
            "fd_count": self.fd_count,
        }


# ── Helper Functions ───────────────────────────────────────────

# Pop NOTIFY_SOCKET from env so subprocesses don't inherit it
# (prevents "reception only permitted for main PID" warnings)
_NOTIFY_SOCKET = os.environ.pop("NOTIFY_SOCKET", None)


def sd_notify(state: str) -> None:
    """Send notification to systemd via NOTIFY_SOCKET."""
    if not _NOTIFY_SOCKET:
        return
    addr = _NOTIFY_SOCKET
    if addr[0] == "@":
        addr = "\0" + addr[1:]
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    try:
        sock.sendto(state.encode(), addr)
    finally:
        sock.close()


def find_gateway_pid() -> Optional[int]:
    """Find the main PID of openclaw-gateway.service via systemctl."""
    try:
        result = subprocess.run(
            ["systemctl", "show", GATEWAY_SERVICE,
             "--property=MainPID", "--value"],
            capture_output=True, text=True, timeout=5,
        )
        pid = int(result.stdout.strip())
        if pid > 0 and Path(f"/proc/{pid}").exists():
            return pid
    except (ValueError, subprocess.TimeoutExpired, OSError):
        pass
    return None


def collect_proc(pid: int) -> Optional[ProcessMetrics]:
    """Read /proc/[pid] metrics. Returns None if process is gone."""
    proc = Path(f"/proc/{pid}")
    if not proc.exists():
        return None

    m = ProcessMetrics()
    m.pid = pid
    m.timestamp = time.time()

    # /proc/[pid]/status
    try:
        status_text = (proc / "status").read_text()
        for line in status_text.splitlines():
            if line.startswith("State:"):
                m.state = line.split()[1]
            elif line.startswith("Threads:"):
                m.threads = int(line.split()[1])
            elif line.startswith("VmRSS:"):
                m.rss_bytes = int(line.split()[1]) * 1024
            elif line.startswith("voluntary_ctxt_switches:"):
                m.voluntary_ctxt_switches = int(line.split()[1])
            elif line.startswith("nonvoluntary_ctxt_switches:"):
                m.nonvoluntary_ctxt_switches = int(line.split()[1])
    except (OSError, ValueError, IndexError):
        return None

    # /proc/[pid]/io (may lack permission)
    try:
        io_text = (proc / "io").read_text()
        for line in io_text.splitlines():
            if line.startswith("read_bytes:"):
                m.read_bytes = int(line.split()[1])
            elif line.startswith("write_bytes:"):
                m.write_bytes = int(line.split()[1])
    except (OSError, PermissionError, ValueError, IndexError):
        pass

    # /proc/[pid]/fd count
    try:
        m.fd_count = len(list((proc / "fd").iterdir()))
    except (OSError, PermissionError):
        pass

    return m


def check_health() -> bool:
    """HTTP GET /health against gateway. Returns True on 200."""
    try:
        req = Request(HEALTH_URL, method="GET")
        resp = urlopen(req, timeout=HEALTH_TIMEOUT)
        return resp.status == 200
    except Exception:
        return False


def send_alert(level: str, message: str) -> None:
    """Send alert via alert.sh (Telegram + local log)."""
    try:
        subprocess.run(
            [ALERT_SCRIPT, level, message],
            timeout=30, capture_output=True,
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        logging.error("Alert dispatch failed: %s", e)


def write_proc_json(metrics: Optional[ProcessMetrics], state: State) -> None:
    """Atomically write latest proc snapshot for postmortem-collect.sh."""
    data = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "clawshell_state": state.value,
    }
    if metrics:
        data.update(metrics.to_dict())

    tmp = PROC_JSON_PATH + ".tmp"
    try:
        with open(tmp, "w") as f:
            json.dump(data, f, indent=2)
        os.replace(tmp, PROC_JSON_PATH)
    except OSError as e:
        logging.error("Failed to write proc JSON: %s", e)


# ── Config File Watcher (inotify via ctypes) ──────────────────

class ConfigWatcher:
    """Watch critical config files for modifications using Linux inotify.

    Uses ctypes to call inotify syscalls directly — zero external deps.
    Non-blocking: call process_events() from the main loop.
    """

    IN_MODIFY = 0x00000002
    IN_ATTRIB = 0x00000004
    IN_DELETE_SELF = 0x00000400
    IN_MOVE_SELF = 0x00000800
    IN_IGNORED = 0x00008000
    WATCH_MASK = IN_MODIFY | IN_ATTRIB | IN_DELETE_SELF | IN_MOVE_SELF
    EVENT_HEADER_SIZE = struct.calcsize("iIII")
    ALERT_COOLDOWN = 60  # seconds between alerts for the same file

    def __init__(self, paths: list):
        lib_name = ctypes.util.find_library("c")
        self._libc = ctypes.CDLL(lib_name, use_errno=True)
        self._fd = self._libc.inotify_init1(os.O_NONBLOCK)
        if self._fd < 0:
            raise OSError(f"inotify_init1 failed: errno={ctypes.get_errno()}")
        self._wd_to_path: dict = {}
        self._all_paths: list = list(paths)
        self._alert_cooldown: dict = {}
        for path in paths:
            self._add_watch(path)

    def _add_watch(self, path: str) -> None:
        if not os.path.exists(path):
            logging.warning("ConfigWatcher: %s does not exist, skipping", path)
            return
        wd = self._libc.inotify_add_watch(
            self._fd, path.encode(), self.WATCH_MASK)
        if wd < 0:
            logging.warning("ConfigWatcher: cannot watch %s (errno=%d)",
                            path, ctypes.get_errno())
            return
        self._wd_to_path[wd] = path
        logging.info("ConfigWatcher: watching %s", path)

    def process_events(self) -> None:
        """Non-blocking: drain inotify events and send alerts."""
        readable, _, _ = select.select([self._fd], [], [], 0)
        if not readable:
            return
        try:
            buf = os.read(self._fd, 4096)
        except OSError:
            return

        now = time.time()
        offset = 0
        while offset < len(buf):
            wd, mask, _, name_len = struct.unpack_from("iIII", buf, offset)
            offset += self.EVENT_HEADER_SIZE + name_len
            path = self._wd_to_path.get(wd, f"unknown(wd={wd})")

            # Cooldown: don't flood alerts for repeated edits
            last = self._alert_cooldown.get(path, 0)
            if now - last < self.ALERT_COOLDOWN:
                continue
            self._alert_cooldown[path] = now

            filename = os.path.basename(path)
            event_desc = self._describe_mask(mask)

            if filename in ("SOUL.md", "AGENTS.md", "USER.md"):
                level = "CRITICAL"
                msg = (f"IDENTITY FILE TAMPERED: {filename} was {event_desc}. "
                       "chattr +i may have been removed!")
            else:
                level = "WARNING"
                msg = f"Config file changed: {filename} ({event_desc})"

            logging.warning("ConfigWatcher: %s %s", path, event_desc)
            send_alert(level, msg)

            # Watch auto-removed on delete/move — will re-add in re_watch
            if mask & (self.IN_DELETE_SELF | self.IN_MOVE_SELF | self.IN_IGNORED):
                self._wd_to_path.pop(wd, None)

    def re_watch_missing(self) -> None:
        """Re-add watches for files that were deleted and re-created."""
        watched = set(self._wd_to_path.values())
        for path in self._all_paths:
            if path not in watched and os.path.exists(path):
                self._add_watch(path)

    @staticmethod
    def _describe_mask(mask: int) -> str:
        parts = []
        if mask & 0x00000002:
            parts.append("modified")
        if mask & 0x00000004:
            parts.append("attributes changed")
        if mask & 0x00000400:
            parts.append("deleted")
        if mask & 0x00000800:
            parts.append("moved")
        return ", ".join(parts) or f"event(0x{mask:x})"

    def close(self) -> None:
        if self._fd >= 0:
            os.close(self._fd)
            self._fd = -1


# ── Telegram Bot (Two-Way) ────────────────────────────────────

class TelegramBot:
    """Two-way Telegram bot using getUpdates long-polling (stdlib only).

    Runs in a daemon thread. Commands are queued for the main thread.
    Authorization: only messages from CLAWSHELL_TG_CHAT are accepted.
    """

    POLL_TIMEOUT = 15
    ERROR_BACKOFF = 30

    def __init__(self, token: str, chat_id: str,
                 command_queue: "queue.Queue",
                 get_status_fn):
        self._token = token
        self._chat_id = str(chat_id)
        self._base_url = f"https://api.telegram.org/bot{token}"
        self._offset = 0
        self._queue = command_queue
        self._get_status = get_status_fn
        self._running = True

    def start(self) -> None:
        t = threading.Thread(target=self._poll_loop,
                             name="telegram-bot", daemon=True)
        t.start()
        logging.info("TelegramBot: polling started (chat_id=%s)", self._chat_id)

    def stop(self) -> None:
        self._running = False

    def _api_call(self, method: str, params: dict = None,
                  timeout: int = None) -> Optional[dict]:
        url = f"{self._base_url}/{method}"
        data = json.dumps(params).encode() if params else None
        req = Request(url, data=data, method="POST" if data else "GET")
        if data:
            req.add_header("Content-Type", "application/json")
        try:
            resp = urlopen(req, timeout=timeout or 30)
            result = json.loads(resp.read().decode())
            if result.get("ok"):
                return result.get("result")
        except Exception as e:
            logging.debug("TelegramBot API error (%s): %s", method, e)
        return None

    def send_message(self, text: str) -> bool:
        result = self._api_call("sendMessage", {
            "chat_id": self._chat_id,
            "text": text,
            "parse_mode": "Markdown",
        })
        return result is not None

    def _poll_loop(self) -> None:
        while self._running:
            try:
                updates = self._api_call("getUpdates", {
                    "offset": self._offset,
                    "timeout": self.POLL_TIMEOUT,
                    "allowed_updates": ["message"],
                }, timeout=self.POLL_TIMEOUT + 10)

                if updates is None:
                    time.sleep(self.ERROR_BACKOFF)
                    continue

                for update in updates:
                    self._offset = update["update_id"] + 1
                    self._handle_update(update)
            except Exception:
                logging.exception("TelegramBot poll error")
                time.sleep(self.ERROR_BACKOFF)

    def _handle_update(self, update: dict) -> None:
        msg = update.get("message", {})
        text = msg.get("text", "").strip()
        chat_id = str(msg.get("chat", {}).get("id", ""))
        username = msg.get("from", {}).get("username", "unknown")

        if not text.startswith("/"):
            return

        # Only accept commands from the authorized chat
        if chat_id != self._chat_id:
            logging.warning("TelegramBot: rejected command from "
                            "unauthorized chat %s (user: %s)",
                            chat_id, username)
            return

        cmd = text.split()[0].lower().split("@")[0]  # strip @botname
        logging.info("TelegramBot: command '%s' from %s", cmd, username)

        if cmd == "/help":
            self.send_message(
                "*ClawShell Commands*\n"
                "`/status` — Gateway state & metrics\n"
                "`/restart` — Restart gateway service\n"
                "`/snapshots` — List recent snapshots\n"
                "`/help` — This help message")
        elif cmd == "/status":
            s = self._get_status()
            self.send_message(
                f"*ClawShell Status*\n"
                f"State: `{s['state']}`\n"
                f"PID: `{s['pid']}`\n"
                f"Uptime: `{s['uptime']}`\n"
                f"RSS: `{s['rss_mb']}MB`\n"
                f"Last check: `{s['last_check']}`")
        elif cmd == "/restart":
            self.send_message(
                f"Restarting gateway (requested by {username})...")
            self._queue.put({"action": "restart", "user": username})
        elif cmd == "/snapshots":
            self._cmd_snapshots()
        else:
            self.send_message(
                f"Unknown command: `{cmd}`\nUse /help for available commands.")

    def _cmd_snapshots(self) -> None:
        snapshot_dir = "/var/lib/occlawshell/snapshots"
        try:
            files = sorted(Path(snapshot_dir).glob("main-*.sqlite"),
                           key=lambda p: p.stat().st_mtime, reverse=True)[:5]
            if files:
                lines = [f"`{f.name}` ({f.stat().st_size // 1024}KB)"
                         for f in files]
                self.send_message("*Recent Snapshots*\n" + "\n".join(lines))
            else:
                self.send_message("No snapshots found.")
        except OSError as e:
            self.send_message(f"Error listing snapshots: {e}")


# ── ClawShell Main Class ──────────────────────────────────────

class ClawShell:
    """External watchdog for OpenClaw gateway."""

    def __init__(self):
        self.state = State.UNKNOWN
        self.prev_metrics: Optional[ProcessMetrics] = None
        self.stall_since: Optional[float] = None
        self.last_collect: float = 0.0
        self.running = True

        # Auto-heal state tracking
        self._restart_attempted_at: Optional[float] = None
        self._recovery_attempted_at: Optional[float] = None

        # Config file watcher
        self._config_watcher: Optional[ConfigWatcher] = None
        self._last_rewatch: float = 0.0
        try:
            self._config_watcher = ConfigWatcher(WATCHED_FILES)
        except OSError as e:
            logging.error("ConfigWatcher init failed (will run without): %s", e)

        # Telegram bot (two-way)
        self._tg_command_queue: queue.Queue = queue.Queue()
        self._tg_bot: Optional[TelegramBot] = None

        signal.signal(signal.SIGTERM, self._handle_signal)
        signal.signal(signal.SIGINT, self._handle_signal)

    def _handle_signal(self, signum, frame):
        logging.info("Received signal %d, shutting down", signum)
        self.running = False
        if self._tg_bot:
            self._tg_bot.stop()
        if self._config_watcher:
            self._config_watcher.close()

    def _classify(self, metrics: Optional[ProcessMetrics],
                  http_ok: bool) -> State:
        """Determine process state from collected data."""
        now = time.time()

        # No process found → DOWN
        if metrics is None:
            self.stall_since = None
            return State.DOWN

        # Zombie check
        if metrics.state == "Z":
            self.stall_since = None
            return State.ZOMBIE

        # HTTP responds → HEALTHY
        if http_ok:
            self.stall_since = None
            return State.HEALTHY

        # HTTP down — analyze I/O activity
        io_delta = 0
        ctx_delta = 0
        if self.prev_metrics and self.prev_metrics.pid == metrics.pid:
            io_delta = (
                (metrics.read_bytes - self.prev_metrics.read_bytes)
                + (metrics.write_bytes - self.prev_metrics.write_bytes)
            )
            ctx_delta = (
                (metrics.voluntary_ctxt_switches
                 - self.prev_metrics.voluntary_ctxt_switches)
                + (metrics.nonvoluntary_ctxt_switches
                   - self.prev_metrics.nonvoluntary_ctxt_switches)
            )

        # Active I/O → normal inference, reset stall timer
        if io_delta > IO_DELTA_THRESHOLD or ctx_delta > CTX_SWITCH_THRESHOLD:
            self.stall_since = None
            return State.HEAVY_INFERENCE

        # I/O stalled — start or continue tracking
        if self.stall_since is None:
            self.stall_since = now

        stall_duration = now - self.stall_since

        if stall_duration > HANG_CRIT_SECS:
            return State.CONFIRMED_HANG
        if stall_duration > HANG_WARN_SECS:
            return State.POSSIBLE_HANG

        # Brief stall, not yet concerning
        return State.HEAVY_INFERENCE

    def _alert_on_transition(self, old: State, new: State) -> None:
        """Send alerts when state changes."""
        if old == new:
            return

        logging.info("State transition: %s -> %s", old.value, new.value)

        # Recovery
        if new == State.HEALTHY and old in (
            State.DOWN, State.POSSIBLE_HANG, State.CONFIRMED_HANG,
            State.ZOMBIE, State.UNKNOWN,
        ):
            send_alert("INFO",
                        f"Gateway recovered: {old.value} -> HEALTHY")
            return

        # Degradation
        if new == State.POSSIBLE_HANG:
            send_alert("WARNING",
                        f"Gateway may be hung (I/O stalled >{HANG_WARN_SECS}s)")
        elif new == State.CONFIRMED_HANG:
            send_alert("CRITICAL",
                        f"Gateway CONFIRMED hung (I/O stalled >{HANG_CRIT_SECS}s)")
        elif new == State.ZOMBIE:
            send_alert("CRITICAL", "Gateway process is ZOMBIE (state=Z)")
        elif new == State.DOWN:
            send_alert("WARNING", "Gateway process DOWN (PID not found)")

    def _auto_heal(self) -> None:
        """Attempt automatic recovery on confirmed hang or repeated failures.

        Two-step escalation:
          1. CONFIRMED_HANG → restart gateway
          2. If still down 90s later → auto-recover from LKG
        One attempt per incident. 30-minute cooldown after recovery.
        """
        now = time.time()

        # On recovery to HEALTHY, reset everything
        if self.state == State.HEALTHY:
            if self._restart_attempted_at or self._recovery_attempted_at:
                logging.info("Gateway recovered, resetting auto-heal state")
            self._restart_attempted_at = None
            self._recovery_attempted_at = None
            return

        # Don't retry during cooldown after recovery attempt
        if (self._recovery_attempted_at is not None
                and now - self._recovery_attempted_at < RECOVERY_COOLDOWN):
            return

        # Step 1: On CONFIRMED_HANG, attempt restart (once per incident)
        if (self.state == State.CONFIRMED_HANG
                and self._restart_attempted_at is None):
            self._restart_attempted_at = now
            logging.warning("CONFIRMED_HANG: attempting gateway restart")
            send_alert("WARNING", "Auto-restarting gateway (CONFIRMED_HANG)")
            try:
                subprocess.run(
                    ["sudo", "systemctl", "restart", GATEWAY_SERVICE],
                    timeout=30, capture_output=True,
                )
            except (subprocess.TimeoutExpired, OSError) as e:
                logging.error("Gateway restart failed: %s", e)
            return

        # Step 2: If restart didn't help after settle period, try LKG recovery
        if (self.state in (State.DOWN, State.CONFIRMED_HANG)
                and self._restart_attempted_at is not None
                and now - self._restart_attempted_at > RESTART_SETTLE_WAIT
                and self._recovery_attempted_at is None):
            self._recovery_attempted_at = now
            logging.warning("Restart didn't resolve issue, "
                            "attempting LKG recovery")
            send_alert("WARNING",
                        "Auto-recovering from LKG (restart failed)")
            try:
                result = subprocess.run(
                    ["sudo", AUTO_RECOVER_SCRIPT],
                    timeout=120, capture_output=True, text=True,
                )
                if result.returncode == 0:
                    send_alert("WARNING",
                               "Auto-recovered from LKG. Data rolled back "
                               "to last known good state.")
                else:
                    stderr_tail = (result.stderr[-200:]
                                   if result.stderr else "unknown error")
                    send_alert("CRITICAL",
                               f"Auto-recovery FAILED: {stderr_tail}. "
                               "Manual intervention required.")
            except (subprocess.TimeoutExpired, OSError) as e:
                logging.error("Auto-recovery failed: %s", e)
                send_alert("CRITICAL",
                           f"Auto-recovery script failed: {e}. "
                           "Manual intervention required.")

    def _get_status(self) -> dict:
        """Build status dict for Telegram /status command."""
        uptime = "unknown"
        try:
            result = subprocess.run(
                ["systemctl", "show", GATEWAY_SERVICE,
                 "--property=ActiveEnterTimestamp", "--value"],
                capture_output=True, text=True, timeout=5,
            )
            ts = result.stdout.strip()
            if ts:
                uptime = ts
        except (subprocess.TimeoutExpired, OSError):
            pass
        return {
            "state": self.state.value,
            "pid": self.prev_metrics.pid if self.prev_metrics else "N/A",
            "uptime": uptime,
            "rss_mb": (self.prev_metrics.rss_bytes // (1024 * 1024)
                       if self.prev_metrics else "N/A"),
            "last_check": datetime.fromtimestamp(
                self.last_collect, tz=timezone.utc
            ).strftime("%H:%M:%S UTC") if self.last_collect else "never",
        }

    def _process_tg_commands(self) -> None:
        """Process commands queued by TelegramBot thread."""
        while not self._tg_command_queue.empty():
            try:
                cmd = self._tg_command_queue.get_nowait()
            except queue.Empty:
                break
            action = cmd.get("action")
            user = cmd.get("user", "unknown")

            if action == "restart":
                logging.info("Telegram: restart requested by %s", user)
                send_alert("WARNING",
                           f"Gateway restart via Telegram by {user}")
                try:
                    result = subprocess.run(
                        ["sudo", "systemctl", "restart", GATEWAY_SERVICE],
                        timeout=30, capture_output=True, text=True,
                    )
                    msg = ("Gateway restarted successfully."
                           if result.returncode == 0
                           else f"Restart failed: {result.stderr[:200]}")
                except (subprocess.TimeoutExpired, OSError) as e:
                    msg = f"Restart error: {e}"
                if self._tg_bot:
                    self._tg_bot.send_message(msg)

    def _tick(self) -> None:
        """Single collection + classification cycle (every 30s)."""
        pid = find_gateway_pid()
        metrics = collect_proc(pid) if pid else None
        http_ok = check_health()

        new_state = self._classify(metrics, http_ok)
        self._alert_on_transition(self.state, new_state)
        self.state = new_state
        self._auto_heal()

        self.prev_metrics = metrics
        write_proc_json(metrics, self.state)

        logging.debug(
            "tick: pid=%s state=%s http=%s io_r=%s io_w=%s",
            pid, self.state.value, http_ok,
            metrics.read_bytes if metrics else "-",
            metrics.write_bytes if metrics else "-",
        )

    def run(self) -> None:
        """Main loop: 1s tick for watchdog, 30s tick for collection."""
        logging.info("ClawShell started (gateway_port=%d, collect=%ds, "
                     "hang_warn=%ds, hang_crit=%ds)",
                     GATEWAY_PORT, COLLECT_INTERVAL,
                     HANG_WARN_SECS, HANG_CRIT_SECS)
        sd_notify("READY=1")

        # Start Telegram bot if credentials are configured
        if TG_TOKEN and TG_CHAT:
            try:
                self._tg_bot = TelegramBot(
                    TG_TOKEN, TG_CHAT,
                    self._tg_command_queue,
                    self._get_status,
                )
                self._tg_bot.start()
            except Exception:
                logging.exception("Failed to start Telegram bot")

        while self.running:
            sd_notify("WATCHDOG=1")

            now = time.time()
            if now - self.last_collect >= COLLECT_INTERVAL:
                try:
                    self._tick()
                except Exception:
                    logging.exception("Error in collection tick")
                self.last_collect = now

            # Config file watcher (non-blocking)
            if self._config_watcher:
                try:
                    self._config_watcher.process_events()
                    if now - self._last_rewatch >= CONFIG_REWATCH_INTERVAL:
                        self._config_watcher.re_watch_missing()
                        self._last_rewatch = now
                except Exception:
                    logging.exception("Error in config watcher")

            # Telegram command processing
            self._process_tg_commands()

            time.sleep(TICK_INTERVAL)

        sd_notify("STOPPING=1")
        if self._config_watcher:
            self._config_watcher.close()
        if self._tg_bot:
            self._tg_bot.stop()
        logging.info("ClawShell stopped")


# ── Entry Point ────────────────────────────────────────────────

def main():
    logging.basicConfig(
        level=(logging.DEBUG
               if os.environ.get("CLAWSHELL_DEBUG") else logging.INFO),
        format=LOG_FORMAT,
    )
    ClawShell().run()


if __name__ == "__main__":
    main()
