# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""Test settings.

render() uses the compiled (Rust) engine when it's installed. Tests that spy
on or patch the Python engine's internals are marked ``python_engine`` and run
on the Python engine, since the compiled one never calls those functions.
"""

import pytest


def pytest_configure(config):
    config.addinivalue_line(
        "markers", "python_engine: run render() on the Python engine (the test patches its internals)"
    )


@pytest.fixture(autouse=True)
def _engine_choice(request, monkeypatch):
    if request.node.get_closest_marker("python_engine"):
        monkeypatch.setenv("SCHEMATIZER_ENGINE", "python")
