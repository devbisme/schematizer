# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""schematizer: generic JSON netlist -> KiCad schematic."""

# Defined before importing render so the vendored engine (which does
# ``from .. import __version__`` at import time) can see it.
__version__ = "0.2.0"

from .render import render  # noqa: E402

# Part of render()'s contract: the exceptions the engine raises when placement
# or routing can't complete. Exported so callers (e.g. SKiDL) can catch them.
from .engine.place import PlacementFailure  # noqa: E402
from .engine.route import RoutingFailure  # noqa: E402

__all__ = ["render", "PlacementFailure", "RoutingFailure", "__version__"]
