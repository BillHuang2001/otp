# diameter_codec_SUITE_data — codec test inputs

## Intent
Input data for `../diameter_codec_SUITE.erl` (the `unknown` case): dictionaries and a hand-written test module that exercise encode/decode of unknown/mandatory AVPs.
These files are the only entries in `DATA` in `../modules.mk`, so they are installed into `$(RELEASE_PATH)/diameter_test/diameter_codec_SUITE_data` by `release_tests_spec` and found via ct's `data_dir` (in-tree runs resolve it relative to `code:lib_dir(diameter)`).

## API Surface
- `avps.dia` — AVP-only dictionary (`diameter_test_avps`, prefix-less AVPs `XXX`/`YYY`).
- `send.dia` — sending dictionary (`diameter_test_send`) used to encode test packets.
- `recv.dia` — receiving dictionary (`diameter_test_recv`, id 17) with different AVP handlings.
- `diameter_test_unknown.erl` — compiled by the suite (not part of `MODULES`) with `-I` pointing at the priv dir holding the generated `diameter_test_send/recv` hrls; asserts that an unknown AVP with the M flag is an error while one without M is returned as `AVP`.

## Constraints
- The dictionaries must stay consistent with each other and with `diameter_test_unknown.erl`'s expected AVP table; the module is compiled and run by the suite, so its API (`run/0`) is fixed by the suite.
- `diameter_test_unknown.erl` uses `export_all` and is deliberately *not* in `modules.mk` `MODULES` (it must not be installed as a suite module).

## Notes for Agents
- Generated output (`diameter_test_*.erl`/`.hrl`) is produced into the case's priv dir at run time — never commit it here.
