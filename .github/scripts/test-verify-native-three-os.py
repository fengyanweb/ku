#!/usr/bin/env python3
"""Regression tests for the three-OS verifier's bounded child-process harness."""

from __future__ import annotations

from contextlib import redirect_stderr
import importlib.util
import io
import os
from pathlib import Path
import sys
import threading
import time
import unittest
from unittest import mock


sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("verify-native-three-os.py")
SPEC = importlib.util.spec_from_file_location("ku_native_verifier", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class BoundedProcessTests(unittest.TestCase):
    def test_main_emits_one_escaped_actions_annotation(self) -> None:
        stderr = io.StringIO()
        failure = SystemExit("bad % value\r\nnext line")
        with (
            mock.patch.object(VERIFIER, "run", side_effect=failure),
            mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}),
            redirect_stderr(stderr),
            self.assertRaisesRegex(SystemExit, "bad % value"),
        ):
            VERIFIER.main()
        self.assertEqual(
            stderr.getvalue(),
            "::error title=Native three-OS verification::"
            "bad %25 value%0D%0Anext line\n",
        )

    def test_live_pipe_reader_is_never_closed_synchronously(self) -> None:
        read_fd, write_fd = os.pipe()
        stream = os.fdopen(read_fd, "rb")
        started = threading.Event()

        def read_pipe() -> None:
            started.set()
            stream.read(8192)

        reader = threading.Thread(target=read_pipe, daemon=True)
        reader.start()
        self.assertTrue(started.wait(timeout=1))
        try:
            before = time.monotonic()
            VERIFIER.close_finished_output_streams((stream,), [reader])
            self.assertLess(time.monotonic() - before, 0.5)
            self.assertFalse(stream.closed)
        finally:
            os.close(write_fd)
            reader.join(timeout=2)
            self.assertFalse(reader.is_alive())
            VERIFIER.close_finished_output_streams((stream,), [reader])
        self.assertTrue(stream.closed)

    def test_normal_output_is_collected(self) -> None:
        result = VERIFIER.run_bounded(
            [sys.executable, "-B", "-c", "print('bounded-ok')"],
            SCRIPT.parent,
            "normal output self-test",
        )
        self.assertEqual(result.stdout.replace(b"\r\n", b"\n"), b"bounded-ok\n")

    def test_direct_exit_terminates_inherited_pipe_descendant(self) -> None:
        child = (
            "import subprocess,sys; "
            "subprocess.Popen([sys.executable,'-B','-c',"
            "\"import time; time.sleep(3); print('descendant-escaped',flush=True)\"]); "
            "print('parent-exited',flush=True)"
        )
        result = VERIFIER.run_bounded(
            [sys.executable, "-B", "-c", child],
            SCRIPT.parent,
            "inherited pipe self-test",
        )
        self.assertEqual(result.stdout.replace(b"\r\n", b"\n"), b"parent-exited\n")

    def test_output_overflow_is_a_bounded_failure(self) -> None:
        previous = VERIFIER.MAX_CAPTURE_BYTES
        VERIFIER.MAX_CAPTURE_BYTES = 1024
        try:
            with self.assertRaisesRegex(SystemExit, "more than 1024 bytes"):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", "print('x' * 4096)"],
                    SCRIPT.parent,
                    "output overflow self-test",
                )
        finally:
            VERIFIER.MAX_CAPTURE_BYTES = previous

    def test_process_timeout_is_a_bounded_failure(self) -> None:
        previous = VERIFIER.COMMAND_TIMEOUT_SECONDS
        VERIFIER.COMMAND_TIMEOUT_SECONDS = 0.05
        try:
            before = time.monotonic()
            with self.assertRaisesRegex(SystemExit, "exceeded 0.05 seconds"):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", "import time; time.sleep(30)"],
                    SCRIPT.parent,
                    "timeout self-test",
                )
            self.assertLess(time.monotonic() - before, 5)
        finally:
            VERIFIER.COMMAND_TIMEOUT_SECONDS = previous


if __name__ == "__main__":
    unittest.main(verbosity=2)
