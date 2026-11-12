"""The command line passes its options to render() and reports failures without a traceback."""

import pytest

from schematizer import cli
from schematizer.engine.route import RoutingFailure


@pytest.fixture
def calls(monkeypatch):
    """Record what main() passes to render() instead of rendering."""
    seen = []

    def fake_render(netlist, **kwargs):
        seen.append(kwargs)
        return kwargs["filepath"]

    monkeypatch.setattr(cli, "render", fake_render)
    return seen


@pytest.mark.parametrize(
    "args, expected",
    [([], False), (["--auto-stub"], 3), (["--auto-stub", "6"], 6)],
)
def test_auto_stub_option(calls, args, expected):
    assert cli.main(["net.json", *args]) == 0
    assert calls[0]["auto_stub"] == expected


def test_routing_failure_is_reported_without_traceback(monkeypatch, capsys):
    def failing_render(netlist, **kwargs):
        raise RoutingFailure

    monkeypatch.setattr(cli, "render", failing_render)
    assert cli.main(["net.json"]) == 1
    err = capsys.readouterr().err
    assert "couldn't be routed" in err and "--auto-stub" in err
    assert "Traceback" not in err


def test_no_nudge_option(monkeypatch, tmp_path):
    seen = []

    def fake_render(netlist, **kwargs):
        seen.append(kwargs["nudge"])
        return kwargs["filepath"]

    monkeypatch.setattr(cli, "render", fake_render)
    assert cli.main(["netlist.json", "-o", str(tmp_path)]) == 0
    assert cli.main(["netlist.json", "-o", str(tmp_path), "--no-nudge"]) == 0
    assert seen == [True, False]
