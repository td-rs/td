# td-rs

The from-scratch Rust rewrite of TDLib as a drop-in `libtdjson` replacement.
This glossary defines the vocabulary the sync campaigns and parity records
are written in.

## Language

**Pin**:
The single upstream C++ commit the whole tree is verified against — scheme
bytes, oracle answers, and conformance CI all reference it.
_Avoid_: base, target commit, upstream ref

**Sync**:
A campaign that moves the pin from one upstream commit to another: every
upstream commit in the range is triaged and the tree is re-verified against
the new pin.
_Avoid_: upgrade, update, port (a port is what happens to an individual commit)

**Ported**:
The triage verdict for an upstream commit whose observable behavior has an
equivalent in this tree and lands there.

**Absorbed**:
The triage verdict for an upstream commit whose change is already satisfied —
by the scheme base, by the Rust shape, or by a hazard that cannot occur here.

**Site-less**:
The triage verdict for an upstream commit with no port site: the touched C++
plane is an unported seam, a test-only surface, or build machinery. Always
carries a recorded reason, and behavior-bearing ones record a seam-landing
requirement.
_Avoid_: skipped, ignored, wontfix

**Seam**:
A boundary of the port that has not been crossed yet; work behind it is
blocked until the seam lands.

**Seam-landing requirement**:
The recorded behavior an unported seam must implement when it lands, so
upstream fixes are not silently lost at the seam.

**Census**:
The per-function ledger of the request surface: every scheme function appears
once as `sync`, `dispatched`, or `interim`.
_Avoid_: coverage list, arm list

**Arm**:
One census entry — a single request function and its state.

**Interim arm**:
A census arm whose function parses and routes but answers the
not-implemented-yet 500 placeholder; reserved for unported arms, never for
recorded divergences on ported ones.

**Dispatched arm**:
A census arm with a real routing handler behind it.

**Sync arm**:
A census arm answered natively by the synchronous request surface.

**Layer**:
The upstream scheme version counter; moves only with upstream version
commits, never with the pin.

**Family ticket**:
The issue carving one sync's ported commits into a wave that lands as a
single gate-green commit.

**Oracle**:
A differential gate that grades this tree against the unmodified C++
reference build — never against itself.
_Avoid_: test (tests grade the tree alone; oracles diff against C++)

**REMAINING doc**:
The untracked, canonical triage record of a sync; the PR body carries its
digest.
