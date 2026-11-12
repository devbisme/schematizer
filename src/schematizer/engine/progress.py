# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Progress reporting while a schematic is generated.

The engine calls :func:`report` as it starts each step ("placing fpga",
"routing fpga", ...). Nothing happens unless a callback is installed with
:func:`reporting`, which ``render(progress=...)`` does.
"""

import contextlib
import shutil
import sys

__all__ = ["report", "reporting", "prefixed", "StatusLine"]

_callback = None
_prefixes = []


def report(message):
    """Tell the installed callback, if any, which step is starting."""
    if _callback:
        _callback(" ".join(_prefixes + [message]))


@contextlib.contextmanager
def reporting(progress):
    """Install a progress callback for the duration of a ``with`` block.

    Args:
        progress: ``True`` for a status line on stderr, a callable taking the
            message string, or ``None`` to leave whatever is installed alone
            (so a nested ``render()`` reports to its caller's callback).
            ``False`` turns reporting off.
    """
    global _callback
    if progress is None:
        yield
        return
    status = StatusLine() if progress is True else None
    previous, _callback = _callback, (status or progress or None)
    try:
        yield
    finally:
        _callback = previous
        if status:
            status.clear()


@contextlib.contextmanager
def prefixed(prefix):
    """Put ``prefix`` in front of the messages reported inside a ``with`` block."""
    _prefixes.append(prefix)
    try:
        yield
    finally:
        _prefixes.pop()


class StatusLine:
    """Show each message on one line of a terminal, replacing the last.

    On a stream that isn't a terminal, each message gets a line of its own.
    """

    def __init__(self, stream=None):
        self.stream = stream or sys.stderr
        self.tty = getattr(self.stream, "isatty", lambda: False)()
        self.shown = False

    def __call__(self, message):
        text = f"schematizer: {message}..."
        if self.tty:
            width = shutil.get_terminal_size().columns - 1
            self.stream.write("\r\033[K" + text[:width])
            self.shown = True
        else:
            self.stream.write(text + "\n")
        self.stream.flush()

    def clear(self):
        """Erase the status line so what's printed next starts on a clean line."""
        if self.tty and self.shown:
            self.stream.write("\r\033[K")
            self.stream.flush()
            self.shown = False
