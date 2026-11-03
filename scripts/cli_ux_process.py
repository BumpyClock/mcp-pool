"""Bounded retirement for child processes owned by the CLI audit."""

import subprocess
from contextlib import suppress


class OwnedProcesses:
    def __init__(self):
        self.children = []

    def spawn(self, command, **options):
        child = subprocess.Popen(command, **options)
        self.children.append(child)
        return child

    def retire(self, child):
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
        for stream in (child.stdin, child.stdout, child.stderr):
            if stream is not None:
                # A dead child's stdin can reject its final buffered flush.
                with suppress(BrokenPipeError):
                    stream.close()
        self.children.remove(child)

    def close(self):
        failures = []
        for child in reversed(self.children.copy()):
            try:
                self.retire(child)
            except Exception as error:
                failures.append(error)
        if failures:
            raise ExceptionGroup("Owned child retirement was not verified", failures)
