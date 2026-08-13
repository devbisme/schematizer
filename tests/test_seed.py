# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Seeding the placer/router makes a run reproducible.

Placement starts parts at random positions and the router breaks ties randomly,
so the same netlist normally draws differently every run. ``seed=`` pins that
down. These tests assert the property that makes the option worth having:
byte-identical output, not merely "a seed was accepted".
"""

import glob
import hashlib
import json
import os
import subprocess
import sys

import pytest

from schematizer import render

DATA = os.path.join(os.path.dirname(__file__), "data")


def _doc(name):
    with open(os.path.join(DATA, name)) as f:
        return json.load(f)


def _digest(path):
    """Hash every generated file, names included, into one value."""
    h = hashlib.sha256()
    for p in sorted(glob.glob(os.path.join(str(path), "*"))):
        h.update(os.path.basename(p).encode())
        with open(p, "rb") as f:
            h.update(f.read())
    return h.hexdigest()


def _run(doc, out, **kw):
    kw.setdefault("tool", "kicad9")
    kw.setdefault("top_name", "sch")
    render(_doc(doc), filepath=str(out), **kw)
    return _digest(out)


@pytest.mark.parametrize("doc", ["flat.json", "hier.json"])
@pytest.mark.parametrize("format", ["kicad", "svg"])
def test_same_seed_reproduces_the_drawing(doc, format, tmp_path):
    """The whole point of the option, across both fixtures and both writers."""
    digests = {
        _run(doc, tmp_path / f"run{i}", seed=1234, format=format) for i in range(3)
    }
    assert len(digests) == 1, "same seed produced different output"


def test_different_seeds_give_different_drawings(tmp_path):
    """A seed has to actually select a layout, not just switch determinism on."""
    a = _run("hier.json", tmp_path / "a", seed=1)
    b = _run("hier.json", tmp_path / "b", seed=2)
    assert a != b


def test_unseeded_runs_still_vary(tmp_path):
    """The default must stay random -- seeding is opt-in.

    Guards against "fixing" reproducibility by accidentally making every run
    identical, which would hide the placer's randomness rather than control it.
    """
    digests = {_run("hier.json", tmp_path / f"u{i}") for i in range(4)}
    assert len(digests) > 1, "unseeded output was identical; randomness was lost"


def test_seed_survives_repeated_renders_in_one_process(tmp_path):
    """Reproducibility must not depend on being the first render of a process.

    An earlier render leaves module-level state behind, and objects allocated
    later land at different addresses -- which used to reorder sets keyed by
    object identity. Interleave two seeds so each is preceded by different work.
    """
    for i in range(3):
        _run("hier.json", tmp_path / f"warm{i}", seed=7)
        _run("flat.json", tmp_path / f"other{i}", seed=8)

    digests = {_run("hier.json", tmp_path / f"late{i}", seed=7) for i in range(2)}
    first = _run("hier.json", tmp_path / "again", seed=7)
    assert digests == {first}


def test_seed_does_not_disturb_the_callers_random_stream(tmp_path):
    """Seeding schematizer must not reach into the process-wide generator."""
    import random

    random.seed(0)
    expected = [random.random() for _ in range(3)]

    random.seed(0)
    before = random.random()
    _run("flat.json", tmp_path / "iso", seed=999)
    after = [before] + [random.random() for _ in range(2)]

    assert after == expected


def test_seed_is_reproducible_across_processes(tmp_path):
    """Separate processes must agree.

    The strongest form of the property: a fresh interpreter has a different
    memory layout and a different string-hash seed, so this is what catches
    ordering that depends on either.
    """
    script = (
        "import json,sys;"
        "from schematizer import render;"
        "render(json.load(open(sys.argv[1])), tool='kicad9',"
        " filepath=sys.argv[2], top_name='sch', seed=4242)"
    )
    doc = os.path.join(DATA, "hier.json")
    digests = set()
    for i in range(2):
        out = tmp_path / f"proc{i}"
        subprocess.run(
            [sys.executable, "-c", script, doc, str(out)],
            check=True,
        )
        digests.add(_digest(out))
    assert len(digests) == 1


def test_cli_accepts_seed(tmp_path):
    """The flag is wired to the same option the API uses."""
    doc = os.path.join(DATA, "flat.json")
    digests = set()
    for i in range(2):
        out = tmp_path / f"cli{i}"
        subprocess.run(
            [
                sys.executable,
                "-m",
                "schematizer.cli",
                doc,
                "--tool",
                "kicad9",
                "--seed",
                "77",
                "-o",
                str(out),
                "--top-name",
                "sch",
            ],
            check=True,
            capture_output=True,
        )
        digests.add(_digest(out))
    assert len(digests) == 1
