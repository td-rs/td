# conformance

Differential-testing harness against the C++ oracle:

- `vectors/` — golden data extracted from the C++ `test/data.cpp` base64 blobs (S04).
- `recorder/` — wraps the C++ client, logs requests/results/updates of real sessions (S73).
- `replayer/` — feeds the corpus to `tdjson` and diffs outputs (S74).
