# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""schematizer: generic JSON netlist -> KiCad schematic."""

# Defined before importing render so the vendored engine (which does
# ``from .. import __version__`` at import time) can see it.
__version__ = "0.2.0"

from .render import render  # noqa: E402

__all__ = ["render", "__version__"]
