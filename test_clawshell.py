import unittest
import os
import queue
import struct
import sys
import json
import time
import signal
import logging
import tempfile
import subprocess
from unittest.mock import MagicMock, patch, Mock, call

# Add the directory to sys.path to import clawshell
sys.path.append(os.path.dirname(os.path.abspath(__file__)))
import clawshell
from clawshell import (
    State, ProcessMetrics, ClawShell, find_gateway_pid, collect_proc,
    check_health, send_alert, write_proc_json, sd_notify,
    ConfigWatcher, TelegramBot, list_sessions, kill_session, stop_gateway,
)

# ── ProcessMetrics and Enum Tests ──────────────────────────────

class TestBasicStructures(unittest.TestCase):
    def test_state_enum(self):
        self.assertEqual(State.UNKNOWN.value, "UNKNOWN")
        self.assertEqual(State.HEALTHY.value, "HEALTHY")
        self.assertEqual(State.DOWN.value, "DOWN")

    def test_metrics_defaults(self):
        m = ProcessMetrics()
        self.assertEqual(m.pid, 0)
        self.assertEqual(m.rss_bytes, 0)
        self.assertEqual(m.fd_count, 0)

    def test_metrics_to_dict(self):
        m = ProcessMetrics()
        m.pid = 123
        m.state = "S"
        d = m.to_dict()
        self.assertEqual(d["pid"], 123)
        self.assertEqual(d["proc_state"], "S")


# ── collect_proc Tests ────────────────────────────────────────

class TestCollectProc(unittest.TestCase):
    def _make_status(self, state="S", threads=4, rss_kb=50000,
                     vol_ctx=100, nonvol_ctx=20):
        return (
            f"Name:\tpython3\n"
            f"State:\t{state} (sleeping)\n"
            f"Threads:\t{threads}\n"
            f"VmRSS:\t{rss_kb} kB\n"
            f"voluntary_ctxt_switches:\t{vol_ctx}\n"
            f"nonvoluntary_ctxt_switches:\t{nonvol_ctx}\n"
        )

    def _make_io(self, read_bytes=1000, write_bytes=2000):
        return (
            f"rchar: 999\n"
            f"wchar: 999\n"
            f"read_bytes: {read_bytes}\n"
            f"write_bytes: {write_bytes}\n"
        )

    @patch("clawshell.Path")
    def test_returns_none_when_proc_absent(self, mock_path):
        mock_path.return_value.exists.return_value = False
        result = collect_proc(999)
        self.assertIsNone(result)

    @patch("clawshell.Path")
    def test_collects_all_fields(self, mock_path):
        proc_path = MagicMock()
        mock_path.return_value = proc_path
        proc_path.exists.return_value = True

        status_file = MagicMock()
        status_file.read_text.return_value = self._make_status()

        io_file = MagicMock()
        io_file.read_text.return_value = self._make_io(5000, 3000)

        fd_dir = MagicMock()
        fd_dir.iterdir.return_value = [Mock()] * 10

        def truediv(self_, name):
            if name == "status":
                return status_file
            elif name == "io":
                return io_file
            elif name == "fd":
                return fd_dir
            return MagicMock()

        proc_path.__truediv__ = truediv

        result = collect_proc(1234)
        self.assertIsNotNone(result)
        self.assertEqual(result.pid, 1234)
        self.assertEqual(result.state, "S")
        self.assertEqual(result.threads, 4)
        self.assertEqual(result.rss_bytes, 50000 * 1024)
        self.assertEqual(result.voluntary_ctxt_switches, 100)
        self.assertEqual(result.nonvoluntary_ctxt_switches, 20)
        self.assertEqual(result.read_bytes, 5000)
        self.assertEqual(result.write_bytes, 3000)
        self.assertEqual(result.fd_count, 10)
        self.assertGreater(result.timestamp, 0)

# ── check_health Tests ────────────────────────────────────────

class TestCheckHealth(unittest.TestCase):
    @patch("clawshell.urlopen")
    def test_returns_true_on_200(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_urlopen.return_value = mock_resp
        self.assertTrue(check_health())

    @patch("clawshell.urlopen")
    def test_returns_false_on_exception(self, mock_urlopen):
        mock_urlopen.side_effect = Exception("error")
        self.assertFalse(check_health())

# ── ClawShell._classify Tests ────────────────────────────────

class TestClawShellClassify(unittest.TestCase):
    def setUp(self):
        with patch("signal.signal"):
            self.g = ClawShell()

    def test_none_metrics_returns_down(self):
        result = self.g._classify(None, http_ok=False)
        self.assertEqual(result, State.DOWN)

    def test_http_ok_returns_healthy(self):
        m = ProcessMetrics()
        m.state = "S"
        result = self.g._classify(m, http_ok=True)
        self.assertEqual(result, State.HEALTHY)

    def test_zombie_state(self):
        m = ProcessMetrics()
        m.state = "Z"
        result = self.g._classify(m, http_ok=False)
        self.assertEqual(result, State.ZOMBIE)

# ── Integration Scenarios ─────────────────────────────────────

class TestIntegration(unittest.TestCase):
    def setUp(self):
        with patch("signal.signal"):
            self.g = ClawShell()

    def test_stall_lifecycle(self):
        prev = ProcessMetrics()
        prev.pid = 100
        self.g.prev_metrics = prev

        curr = ProcessMetrics()
        curr.pid = 100
        curr.state = "S"

        # First stall
        s = self.g._classify(curr, http_ok=False)
        self.assertEqual(s, State.HEAVY_INFERENCE)
        self.assertIsNotNone(self.g.stall_since)

        # Possible hang
        self.g.stall_since = time.time() - (clawshell.HANG_WARN_SECS + 1)
        s = self.g._classify(curr, http_ok=False)
        self.assertEqual(s, State.POSSIBLE_HANG)

        # Confirmed hang
        self.g.stall_since = time.time() - (clawshell.HANG_CRIT_SECS + 1)
        s = self.g._classify(curr, http_ok=False)
        self.assertEqual(s, State.CONFIRMED_HANG)

# ── ConfigWatcher Tests ───────────────────────────────────────

class TestConfigWatcher(unittest.TestCase):
    def test_describe_mask_modify(self):
        self.assertIn("modified", ConfigWatcher._describe_mask(0x00000002))

    def test_describe_mask_attrib(self):
        self.assertIn("attributes changed",
                      ConfigWatcher._describe_mask(0x00000004))

    def test_describe_mask_delete(self):
        self.assertIn("deleted", ConfigWatcher._describe_mask(0x00000400))

    def test_describe_mask_combined(self):
        desc = ConfigWatcher._describe_mask(0x00000002 | 0x00000004)
        self.assertIn("modified", desc)
        self.assertIn("attributes changed", desc)

    def test_describe_mask_unknown(self):
        desc = ConfigWatcher._describe_mask(0x01000000)
        self.assertIn("0x1000000", desc)

    def test_init_with_tempfile(self):
        """ConfigWatcher can watch a real file."""
        with tempfile.NamedTemporaryFile(suffix=".md", delete=False) as f:
            path = f.name
        try:
            cw = ConfigWatcher([path])
            self.assertIn(path, cw._wd_to_path.values())
            cw.close()
        finally:
            os.unlink(path)

    def test_init_skips_missing_files(self):
        """ConfigWatcher skips files that don't exist."""
        cw = ConfigWatcher(["/nonexistent/path/SOUL.md"])
        self.assertEqual(len(cw._wd_to_path), 0)
        cw.close()

    @patch("clawshell.send_alert")
    def test_process_events_with_modify(self, mock_alert):
        """ConfigWatcher detects file modification."""
        with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
            path = f.name
        try:
            cw = ConfigWatcher([path])
            # Trigger a modify event
            with open(path, "w") as f:
                f.write("changed")
            time.sleep(0.1)  # Let inotify process
            cw.process_events()
            if mock_alert.called:
                mock_alert.assert_called_once()
                args = mock_alert.call_args[0]
                self.assertEqual(args[0], "WARNING")
            cw.close()
        finally:
            os.unlink(path)

    @patch("clawshell.send_alert")
    def test_identity_file_gets_critical_alert(self, mock_alert):
        """Identity files (SOUL.md etc) trigger CRITICAL alerts."""
        with tempfile.NamedTemporaryFile(
                prefix="SOUL", suffix=".md", delete=False,
                dir="/tmp") as f:
            path = f.name
        # Rename to exactly SOUL.md
        soul_path = os.path.join("/tmp", "SOUL.md")
        os.rename(path, soul_path)
        try:
            cw = ConfigWatcher([soul_path])
            with open(soul_path, "w") as f:
                f.write("tampered")
            time.sleep(0.1)
            cw.process_events()
            if mock_alert.called:
                args = mock_alert.call_args[0]
                self.assertEqual(args[0], "CRITICAL")
                self.assertIn("TAMPERED", args[1])
            cw.close()
        finally:
            os.unlink(soul_path)


# ── TelegramBot Tests ────────────────────────────────────────

class TestTelegramBot(unittest.TestCase):
    def setUp(self):
        import queue
        self.cmd_queue = queue.Queue()
        self.status_fn = lambda: {
            "state": "HEALTHY", "pid": 123,
            "uptime": "2h", "rss_mb": 400, "last_check": "12:00:00 UTC",
        }
        self.bot = TelegramBot(
            "fake_token", "12345",
            self.cmd_queue, self.status_fn,
        )

    def test_rejects_unauthorized_chat(self):
        """Commands from wrong chat_id are rejected."""
        update = {
            "update_id": 1,
            "message": {
                "text": "/status",
                "chat": {"id": 99999},
                "from": {"username": "attacker"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            mock_send.assert_not_called()

    def test_help_command(self):
        update = {
            "update_id": 1,
            "message": {
                "text": "/help",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            mock_send.assert_called_once()
            self.assertIn("Commands", mock_send.call_args[0][0])

    def test_status_command(self):
        update = {
            "update_id": 2,
            "message": {
                "text": "/status",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            mock_send.assert_called_once()
            self.assertIn("HEALTHY", mock_send.call_args[0][0])

    def test_restart_queues_command(self):
        update = {
            "update_id": 3,
            "message": {
                "text": "/restart",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message"):
            self.bot._handle_update(update)
            self.assertFalse(self.cmd_queue.empty())
            cmd = self.cmd_queue.get_nowait()
            self.assertEqual(cmd["action"], "restart")
            self.assertEqual(cmd["user"], "admin")

    def test_unknown_command(self):
        update = {
            "update_id": 4,
            "message": {
                "text": "/foo",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            self.assertIn("Unknown", mock_send.call_args[0][0])

    def test_ignores_non_command_messages(self):
        update = {
            "update_id": 5,
            "message": {
                "text": "hello",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            mock_send.assert_not_called()

    def test_strips_bot_mention_from_command(self):
        update = {
            "update_id": 6,
            "message": {
                "text": "/status@MyClawBot",
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(update)
            mock_send.assert_called_once()
            self.assertIn("HEALTHY", mock_send.call_args[0][0])


class TestProcessManagement(unittest.TestCase):
    """Tests for /kill, /sessions, /kill_session commands."""

    def setUp(self):
        import queue
        self.cmd_queue = queue.Queue()
        self.status_fn = lambda: {
            "state": "HEALTHY", "pid": 123,
            "uptime": "2h", "rss_mb": 400, "last_check": "12:00:00 UTC",
        }
        self.bot = TelegramBot(
            "fake_token", "12345",
            self.cmd_queue, self.status_fn,
        )

    def _make_update(self, text, update_id=1):
        return {
            "update_id": update_id,
            "message": {
                "text": text,
                "chat": {"id": 12345},
                "from": {"username": "admin"},
            }
        }

    def test_kill_queues_command(self):
        with patch.object(self.bot, "send_message"):
            self.bot._handle_update(self._make_update("/kill"))
            self.assertFalse(self.cmd_queue.empty())
            cmd = self.cmd_queue.get_nowait()
            self.assertEqual(cmd["action"], "kill")
            self.assertEqual(cmd["user"], "admin")

    def test_sessions_command_success(self):
        sessions = [
            {"id": "sess-001", "status": "active", "created": "2026-03-21T10:00:00Z"},
            {"id": "sess-002", "status": "idle", "created": "2026-03-21T09:00:00Z"},
        ]
        with patch.object(self.bot, "send_message") as mock_send, \
             patch("clawshell.list_sessions", return_value=sessions):
            self.bot._handle_update(self._make_update("/sessions"))
            mock_send.assert_called_once()
            msg = mock_send.call_args[0][0]
            self.assertIn("sess-001", msg)
            self.assertIn("sess-002", msg)
            self.assertIn("Active Sessions", msg)

    def test_sessions_command_gateway_unreachable(self):
        with patch.object(self.bot, "send_message") as mock_send, \
             patch("clawshell.list_sessions", return_value=None):
            self.bot._handle_update(self._make_update("/sessions"))
            mock_send.assert_called_once()
            self.assertIn("unreachable", mock_send.call_args[0][0])

    def test_sessions_command_empty(self):
        with patch.object(self.bot, "send_message") as mock_send, \
             patch("clawshell.list_sessions", return_value=[]):
            self.bot._handle_update(self._make_update("/sessions"))
            mock_send.assert_called_once()
            self.assertIn("No active sessions", mock_send.call_args[0][0])

    def test_kill_session_queues_command(self):
        with patch.object(self.bot, "send_message"):
            self.bot._handle_update(self._make_update("/kill_session sess-001"))
            self.assertFalse(self.cmd_queue.empty())
            cmd = self.cmd_queue.get_nowait()
            self.assertEqual(cmd["action"], "kill_session")
            self.assertEqual(cmd["session_id"], "sess-001")

    def test_kill_session_no_id(self):
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(self._make_update("/kill_session"))
            mock_send.assert_called()
            self.assertIn("Usage", mock_send.call_args[0][0])

    def test_help_includes_new_commands(self):
        with patch.object(self.bot, "send_message") as mock_send:
            self.bot._handle_update(self._make_update("/help"))
            msg = mock_send.call_args[0][0]
            self.assertIn("/kill", msg)
            self.assertIn("/sessions", msg)
            self.assertIn("/kill_session", msg)

    @patch("clawshell.urlopen")
    def test_list_sessions_returns_list(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps([
            {"id": "s1", "status": "active"}
        ]).encode()
        mock_urlopen.return_value = mock_resp
        result = list_sessions()
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["id"], "s1")

    @patch("clawshell.urlopen")
    def test_list_sessions_handles_wrapped_response(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({
            "sessions": [{"id": "s1"}]
        }).encode()
        mock_urlopen.return_value = mock_resp
        result = list_sessions()
        self.assertEqual(len(result), 1)

    @patch("clawshell.urlopen")
    def test_list_sessions_returns_none_on_error(self, mock_urlopen):
        mock_urlopen.side_effect = Exception("Connection refused")
        result = list_sessions()
        self.assertIsNone(result)

    @patch("clawshell.urlopen")
    def test_kill_session_success(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_urlopen.return_value = mock_resp
        ok, msg = kill_session("sess-001")
        self.assertTrue(ok)
        self.assertIn("terminated", msg)

    @patch("clawshell.urlopen")
    def test_kill_session_failure(self, mock_urlopen):
        mock_urlopen.side_effect = Exception("404 Not Found")
        ok, msg = kill_session("sess-999")
        self.assertFalse(ok)
        self.assertIn("Failed", msg)


class TestStopGateway(unittest.TestCase):
    """Tests for stop_gateway() escalation logic."""

    @patch("clawshell.subprocess.run")
    def test_graceful_stop_success(self, mock_run):
        mock_run.return_value = MagicMock(returncode=0)
        msg = stop_gateway()
        self.assertIn("stopped successfully", msg)
        mock_run.assert_called_once()

    @patch("clawshell.subprocess.run")
    def test_graceful_stop_error(self, mock_run):
        mock_run.return_value = MagicMock(returncode=1, stderr="unit not found")
        msg = stop_gateway()
        self.assertIn("Stop failed", msg)

    @patch("clawshell.find_gateway_pid", return_value=None)
    @patch("clawshell.subprocess.run")
    def test_escalates_to_sigkill_on_timeout(self, mock_run, mock_find_pid):
        # First call (systemctl stop) times out, second (systemctl kill --signal=SIGKILL) succeeds
        mock_run.side_effect = [
            subprocess.TimeoutExpired("systemctl", 15),
            MagicMock(returncode=0),
        ]
        msg = stop_gateway()
        self.assertIn("force-killed", msg)
        self.assertEqual(mock_run.call_count, 2)
        # Verify SIGKILL was sent via systemctl kill
        kill_call = mock_run.call_args_list[1]
        self.assertIn("--signal=SIGKILL", kill_call[0][0])

    @patch("clawshell.subprocess.run")
    def test_escalation_sigkill_fails(self, mock_run):
        # Both systemctl stop and systemctl kill time out
        mock_run.side_effect = subprocess.TimeoutExpired("systemctl", 15)
        msg = stop_gateway()
        self.assertIn("Force-kill failed", msg)

    @patch("clawshell.find_gateway_pid", return_value=42)
    @patch("clawshell.subprocess.run")
    def test_escalation_sigkill_process_persists(self, mock_run, mock_find_pid):
        mock_run.side_effect = [
            subprocess.TimeoutExpired("systemctl", 15),
            MagicMock(returncode=0),
        ]
        msg = stop_gateway()
        self.assertIn("still present", msg)


class TestSessionIdValidation(unittest.TestCase):
    """Security tests for session_id input validation."""

    def test_valid_session_ids(self):
        """Normal session IDs should be accepted."""
        for sid in ["abc123", "session-1", "my_session", "A-Z_0-9"]:
            ok, msg = kill_session(sid)
            # Will fail with connection error but should NOT fail validation
            self.assertNotIn("Invalid session_id", msg, f"Rejected valid id: {sid}")

    def test_path_traversal_rejected(self):
        ok, msg = kill_session("../../admin/reset")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)

    def test_url_injection_rejected(self):
        ok, msg = kill_session("foo?bar=baz#fragment")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)

    def test_crlf_injection_rejected(self):
        ok, msg = kill_session("foo\r\nHost: evil.com")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)

    def test_empty_rejected(self):
        ok, msg = kill_session("")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)

    def test_slash_rejected(self):
        ok, msg = kill_session("foo/bar")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)

    def test_space_rejected(self):
        ok, msg = kill_session("foo bar")
        self.assertFalse(ok)
        self.assertIn("Invalid session_id", msg)


class TestTelegramRateLimiting(unittest.TestCase):
    """Test rate limiting on destructive Telegram commands."""

    def setUp(self):
        self.q = queue.Queue()
        self.bot = TelegramBot("123:fake", "12345", self.q, lambda: {})

    @patch.object(TelegramBot, 'send_message', return_value=True)
    def test_second_destructive_cmd_blocked(self, mock_send):
        """A second destructive command within 60s should be rate-limited."""
        # Simulate first /restart command
        self.bot._last_destructive_cmd = time.time()
        update = {"message": {"text": "/restart", "chat": {"id": "12345"},
                               "from": {"username": "admin"}}}
        self.bot._handle_update(update)
        # Should send rate limit message, NOT queue the command
        mock_send.assert_called()
        self.assertIn("Rate limited", mock_send.call_args[0][0])
        self.assertTrue(self.q.empty())

    @patch.object(TelegramBot, 'send_message', return_value=True)
    def test_destructive_cmd_allowed_after_cooldown(self, mock_send):
        """A destructive command should be allowed after cooldown expires."""
        self.bot._last_destructive_cmd = time.time() - 61  # 61s ago
        update = {"message": {"text": "/restart", "chat": {"id": "12345"},
                               "from": {"username": "admin"}}}
        self.bot._handle_update(update)
        # Should queue the restart, not rate-limit
        self.assertFalse(self.q.empty())
        cmd = self.q.get()
        self.assertEqual(cmd["action"], "restart")

    @patch.object(TelegramBot, 'send_message', return_value=True)
    def test_readonly_cmds_not_rate_limited(self, mock_send):
        """Non-destructive commands like /help should not be rate-limited."""
        self.bot._last_destructive_cmd = time.time()  # just now
        update = {"message": {"text": "/help", "chat": {"id": "12345"},
                               "from": {"username": "admin"}}}
        self.bot._handle_update(update)
        # Should get help response, not rate limit
        self.assertTrue(mock_send.called)
        self.assertNotIn("Rate limited", mock_send.call_args[0][0])


if __name__ == "__main__":
    unittest.main()
