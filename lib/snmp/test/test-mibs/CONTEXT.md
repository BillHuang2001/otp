# snmp Test MIB Corpus (`test-mibs/`)

## Intent
Corpus of standard and locally-authored MIB files used to exercise the SNMP MIB compiler (`snmpc:compile/1`) manually.
Two roles: real MIBs that must compile cleanly (good-path coverage, including cross-MIB IMPORTS and required compilation order), and deliberately broken `*-error.mib` files that MUST be rejected.

## API Surface
- Usage: hand-run `snmpc:compile("<Name>")`; `README` states the rules and one recommended compile order (RMON-MIB → RFC1271-MIB → TOKEN-RING-RMON-MIB → SNMPv2-TC → RMON2-MIB → ENTITY-MIB → SNMPv2-MIB → SNMPv2-USEC-MIB → STANDARD-MIB → INTERNAL-MIB).
- Standard/legacy MIBs (must compile, largely unchanged IETF RFC MIBs): `RMON-MIB.mib`, `RFC1271-MIB.mib`, `TOKEN-RING-RMON-MIB.mib`, `RMON2-MIB.mib`, `SNMPv2-TC.mib`, `ENTITY-MIB.mib`, `SNMPv2-MIB.mib`, `SNMPv2-USEC-MIB.mib`, `STANDARD-MIB.mib`, `INTERNAL-MIB.mib`, `RFC1213-MIB.mib`, `ALARM-MIB.mib`.
- Local (non-standard) MIBs: `Klas1.mib` (Apache-2.0 header, locally authored) and `SNMPv2-test.mib` (locally modified variant of `SNMPv2-MIB.mib`).
- Negative test cases (must FAIL compilation): `Bitsindex-error.mib`, `Oid1-error.mib`, `Type-error.mib`, `Table1-error.mib`.
- `README` — normative rules for this directory; read it before adding or editing a MIB here.

## Constraints
- Any `*-error.mib` file MUST produce a compilation error; every other file in this directory MUST compile without error.
- Standard MIBs must stay verbatim originals — do not edit them.
- If a standard MIB ever must be modified, tag the change with the comment keyword `erlsnmpmodification` and reference the ticket as an `OTP-<no>` comment (per `README`).
- New negative cases must follow the name convention `<Name>-error.mib`; the good/negative split is name-based, there is no manifest.

## Known Issues
- Not wired into anything: `rg -n "test-mibs"` over the whole repository (including hidden files) finds no Makefile, `.mk`, `.spec`, `.erl` or CI reference — these files are exercised manually only, so a broken MIB here never fails CI.
- The Common Test suites use their own MIB copies under `../snmp_test_data/` (`Klas1.mib`, `STANDARD-MIB.mib`, `RFC1213-MIB.mib`, `SNMPv2-MIB.mib` exist in both places); only the `snmp_test_data` copies are compiled, driven by `../test/modules.mk` `MIB_FILES`.
- Declared MIB module names do not match conventional file names (`SNMPv2-test`, `Oid1-error`, `Type-error`, `Table1-error`) — do not assume name ↔ filename equality.
- `SNMPv2-test.mib` is a locally modified `SNMPv2-MIB.mib` yet carries no `erlsnmpmodification` tag as the README prescribes; only `RMON2-MIB.mib` currently contains an `OTP-<no>` comment (OTP-1427).
- Standard IETF MIBs carry SPDX `LicenseRef-IETF-MIB` headers (see `../../LICENSES/LicenseRef-IETF-MIB.txt`); locally authored ones use Apache-2.0.

## See Also
- `../snmp_test_data/` → MIBs actually compiled by the Common Test suites (sibling — read-only, escalate writes to the parent).
- `../../src/compile/` → the SNMP MIB compiler (`snmpc`) these files target.
- `../../CONTEXT.md` → snmp application overview.
