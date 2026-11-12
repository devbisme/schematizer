"""Progress is reported while a schematic is generated, and only when asked for."""

import io
import json
import os

import pytest

from schematizer import cli, render
from schematizer.engine import progress
from schematizer.engine.progress import StatusLine

DATA = os.path.join(os.path.dirname(__file__), "data")


def _load(case):
    with open(os.path.join(DATA, f"{case}.json")) as f:
        return json.load(f)


class _Tty(io.StringIO):
    def isatty(self):
        return True


def test_callback_gets_each_step(tmp_path):
    messages = []
    render(_load("hier"), filepath=str(tmp_path), seed=1, progress=messages.append)
    assert "placing stage1" in messages
    assert "routing stage1" in messages
    assert "placing top sheet" in messages
    assert messages[-1] == "writing files"
    # Each sheet is placed before any is routed.
    assert messages.index("placing top sheet") < messages.index("routing stage1")


def test_tries_number_the_layouts(tmp_path):
    messages = []
    render(_load("and_gate"), filepath=str(tmp_path), seed=1, tries=2, progress=messages.append)
    assert any(m.startswith("layout 1/2, placing") for m in messages)
    assert any(m.startswith("layout 2/2, routing") for m in messages)


def test_silent_by_default_and_callback_is_removed(tmp_path, capsys):
    messages = []
    render(_load("and_gate"), filepath=str(tmp_path / "a"), seed=1, progress=messages.append)
    count = len(messages)
    render(_load("and_gate"), filepath=str(tmp_path / "b"), seed=1)
    assert len(messages) == count
    captured = capsys.readouterr()
    assert captured.out == "" and captured.err == ""


def test_callback_is_removed_after_an_error(tmp_path):
    with pytest.raises(ValueError):
        render(_load("and_gate"), filepath=str(tmp_path), auto_stub=0, progress=lambda m: None)
    assert progress._callback is None


def test_progress_true_writes_to_stderr(tmp_path, capsys):
    render(_load("and_gate"), filepath=str(tmp_path), seed=1, progress=True)
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "schematizer: routing top sheet...\n" in captured.err


def test_bad_progress_value_is_rejected(tmp_path):
    with pytest.raises(ValueError, match="progress"):
        render(_load("and_gate"), filepath=str(tmp_path), progress="yes")


def test_status_line_overwrites_itself_on_a_terminal():
    stream = _Tty()
    status = StatusLine(stream)
    status("placing a")
    status("routing a")
    assert stream.getvalue() == "\r\033[Kschematizer: placing a...\r\033[Kschematizer: routing a..."
    status.clear()
    assert stream.getvalue().endswith("\r\033[K")


def test_status_line_writes_lines_when_not_a_terminal():
    stream = io.StringIO()
    status = StatusLine(stream)
    status("placing a")
    status.clear()
    assert stream.getvalue() == "schematizer: placing a...\n"


@pytest.mark.parametrize(
    "flags,tty,expected",
    [([], False, False), ([], True, True), (["--progress"], False, True), (["--no-progress"], True, False)],
)
def test_cli_shows_progress_on_a_terminal_unless_told(monkeypatch, tmp_path, flags, tty, expected):
    seen = {}

    def fake_render(netlist, **kwargs):
        seen.update(kwargs)
        return kwargs["filepath"]

    monkeypatch.setattr(cli, "render", fake_render)
    monkeypatch.setattr(cli.sys.stderr, "isatty", lambda: tty, raising=False)
    assert cli.main(["netlist.json", "-o", str(tmp_path)] + flags) == 0
    assert seen["progress"] is expected
