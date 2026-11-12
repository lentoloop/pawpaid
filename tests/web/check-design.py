"""Check that shipped fixtures, CSS and Plex assets still match approved r3 sources."""
import json
from pathlib import Path
from bs4 import BeautifulSoup

ROOT = Path(__file__).resolve().parents[2]
DESIGN = Path('/workspace/design/paypal')
LEDGER = DESIGN / 'r3-ledger/mockups/ledger'
expected = {}
for source in sorted(LEDGER.glob('0*.html')):
    soup = BeautifulSoup(source.read_text(), 'html.parser')
    expected[source.stem] = [str(section) for section in soup.select('.state-section')]
assert json.loads((ROOT / 'web/src/templates.json').read_text()) == expected
assert len(expected) == 7
assert sum(map(len, expected.values())) == 51
for name in ['app.css', 'tokens.css']:
    assert (ROOT / 'web/src' / name).read_bytes() == (LEDGER / name).read_bytes()
for family in ['ibmplexsans', 'ibmplexmono']:
    for source in (DESIGN / 'fonts' / family).iterdir():
        shipped = ROOT / 'web/public/fonts' / family / source.name
        if source.name == 'OFL.txt':
            assert [line.rstrip() for line in shipped.read_text().splitlines()] == [line.rstrip() for line in source.read_text().splitlines()]
        else:
            assert shipped.read_bytes() == source.read_bytes()
print('PASS: seven pages, 51 original sections, approved CSS/tokens and licensed Plex assets match')
