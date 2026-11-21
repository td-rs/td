# Mid-version upstream pins

The sync pin may point at any upstream commit, not only "Update version to
X" commits: catching up now beats waiting for an arbitrary version boundary,
and nothing in the verification runbook requires a version-commit pin. The
tdjson surface keeps answering the upstream version/layer strings of the
tree it was built from — those move only when a version commit falls inside
a sync range. Both prior pins (c15d3f5a, 23383aef) happened to be version
commits; 03220f6 (still 1.8.70, layer 233, 49 commits ahead) is the first
deliberate mid-version pin.

Considered options: wait for the next version commit (rejected: couples
catch-up cadence to upstream release cadence with no verification benefit);
pin only version commits but port interim commits (rejected: the pin must
name the exact tree the oracles diff against).
