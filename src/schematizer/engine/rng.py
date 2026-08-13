# -*- coding: utf-8 -*-

# The MIT License (MIT) - Copyright (c) Dave Vandenbout.

"""The engine's random number generator.

Placement and routing are randomized: parts start at random positions, symmetry
between exactly-overlapping parts is broken by a random nudge, and the router
picks randomly among equally-good growth directions. Seeding that stream makes a
run reproducible.

The generator is a dedicated :class:`random.Random` instance rather than the
``random`` module's global one. Seeding the global generator would be a side
effect on the whole process -- a caller who seeded ``random`` for their own
purposes would find schematizer had silently reset it, and schematizer's
reproducibility would in turn depend on whatever else in the process happened to
draw from the same stream. Keeping our own instance makes ``seed=`` mean exactly
one thing: this library's output.
"""

import random

# The engine's private stream. Unseeded, it is seeded from OS entropy on first
# use, so the default behavior stays random run-to-run.
rng = random.Random()


def seed(value=None):
    """Seed the engine's generator.

    Args:
        value: Any hashable seed (typically an int). ``None`` reseeds from OS
            entropy, restoring unpredictable placement.
    """
    rng.seed(value)
