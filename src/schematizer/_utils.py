# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""
Minimal, skidl-free utility surface used by the vendored schematic engine.

The engine originally pulled a few helpers from ``skidl.utilities``,
``skidl.logger`` and ``skidl.scriptinfo``. Only a small, stable subset is
actually used; it is reproduced here so the package does not depend on skidl.
"""

import logging
import os
import sys

__all__ = [
    "export_to_all",
    "rmv_attr",
    "sgn",
    "get_script_name",
    "active_logger",
]


def export_to_all(fn):
    """Append ``fn.__name__`` to its module's ``__all__`` (decorator, no-op else).

    Same behavior as ``skidl.utilities.export_to_all``: it lets a module opt a
    name into ``from module import *`` and otherwise returns the function
    unchanged.
    """
    mod = sys.modules[fn.__module__]
    if hasattr(mod, "__all__"):
        mod.__all__.append(fn.__name__)
    else:
        mod.__all__ = [fn.__name__]
    return fn


def _to_list(x):
    """Return ``x`` as a list; wrap scalars, pass sequences through."""
    if isinstance(x, (list, tuple, set)):
        return list(x)
    return [x]


def rmv_attr(objs, attrs):
    """Remove one or more attributes from one or more objects (ignore missing)."""
    for o in _to_list(objs):
        for a in _to_list(attrs):
            try:
                delattr(o, a)
            except AttributeError:
                pass


def sgn(x):
    """Return the sign of a number as -1, 0, or +1."""
    return (x > 0) - (x < 0)


def get_script_name():
    """Return the top-level script name without extension.

    Used only as the default ``top_name`` for schematic output; callers
    (``render``/CLI) normally pass an explicit name.
    """
    try:
        name = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else ""
    except Exception:
        name = ""
    return os.path.splitext(name)[0] or "schematic"


# A plain stdlib logger stands in for skidl's ``active_logger``. The engine only
# calls ``.info`` / ``.warning`` on it.
active_logger = logging.getLogger("schematizer")
if not active_logger.handlers:
    active_logger.addHandler(logging.NullHandler())
