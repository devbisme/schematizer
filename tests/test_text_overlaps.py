"""Reference, value and power-symbol text stays clear of other text, parts and wires."""

import json
import os

import pytest

from schematizer import render
from schematizer.quality import score_files

DATA = os.path.join(os.path.dirname(__file__), "data")


# The limits are a little above what's measured (1 each), since text sizes are estimates.
@pytest.mark.parametrize("case,seed,limit", [("esp32_audio_board", 1, 2), ("dual_adc_usb", 1, 4)])
def test_text_rarely_overlaps(tmp_path, case, seed, limit):
    """Before text was sized as KiCad draws it, these boards had 9 and 76 overlaps."""
    with open(os.path.join(DATA, f"{case}.json")) as f:
        doc = json.load(f)
    out = str(tmp_path / "out")
    render(doc, filepath=out, top_name="sch", seed=seed)
    assert score_files(out)["text_overlaps"] <= limit
