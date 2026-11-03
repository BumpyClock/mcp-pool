#!/usr/bin/env python3
"""Verify bounded escalation only targets registered child handles."""

import subprocess
import unittest
from unittest.mock import Mock, call, patch

from cli_ux_process import OwnedProcesses


class OwnedProcessTests(unittest.TestCase):
    def test_terminate_timeout_escalates_to_kill_and_reaps_before_closing_pipes(self):
        child = Mock()
        child.poll.return_value = None
        child.wait.side_effect = [subprocess.TimeoutExpired("owned", 5), 0]
        processes = OwnedProcesses()
        with patch("cli_ux_process.subprocess.Popen", return_value=child):
            processes.spawn(["owned-fixture"])
        processes.close()
        child.terminate.assert_called_once()
        child.kill.assert_called_once()
        self.assertEqual(child.wait.call_args_list, [call(timeout=5), call(timeout=5)])
        child.stdout.close.assert_called_once()
        processes.close()
        child.terminate.assert_called_once()

    def test_failed_reap_does_not_skip_other_owned_children_and_can_be_retried(self):
        first = Mock(stdin=None, stdout=None, stderr=None)
        first.poll.return_value = None
        last = Mock(stdin=None, stdout=None, stderr=None)
        last.poll.return_value = None
        last.wait.side_effect = subprocess.TimeoutExpired("owned", 5)
        processes = OwnedProcesses()
        with patch("cli_ux_process.subprocess.Popen", side_effect=[first, last]):
            processes.spawn(["first-fixture"])
            processes.spawn(["last-fixture"])
        with self.assertRaises(ExceptionGroup):
            processes.close()
        first.terminate.assert_called_once()
        first.wait.assert_called_once_with(timeout=5)
        last.kill.assert_called_once()
        last.wait.side_effect = None
        last.wait.return_value = 0
        processes.close()
        first.terminate.assert_called_once()


if __name__ == "__main__":
    unittest.main()
