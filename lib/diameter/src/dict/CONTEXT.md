# Diameter Dictionaries (`.dia`)

## Intent
Dictionary definitions in the Diameter dictionary DSL, one `.dia` per Diameter application shipped with OTP.
Each `.dia` is compiled by `diameterc` into the Erlang dictionary module `diameter_gen_<file>` plus an `.hrl` of message/Grouped-AVP records; the generated module is the encode/decode interface (and AVP/record schema) for one Diameter application.
These generated modules are what `diameter:start_service`/transport options `{dictionary, Mod}` and `{avp_dictionaries, [Mod]}` configure.
The DSL grammar/semantics are documented in `../doc/references/diameter_dict.md` and `../doc/references/diameterc_cmd.md` — do not re-derive them here.
There are no build files in this directory: `../src/Makefile` + `../src/modules.mk` (`DICTS`) drive the build, installation and app-file inclusion.

## API Surface
Shipped dictionaries (all in `DICTS`, `../src/modules.mk` — except `capup_rfc6737`, see Known Issues):

| File | Generated module | `@id` | `@prefix` | Role / RFC |
| --- | --- | --- | --- | --- |
| `base_rfc3588.dia` | `diameter_gen_base_rfc3588` | 0 | `diameter_base` | Diameter Common/Base messages (CER/CEA, DWR/DWA, DPR/DPA, RAR/RAA, STR/STA, ASR/ASA, ACR/ACA, `answer-message`), RFC 3588 (obsolete) |
| `base_rfc6733.dia` | `diameter_gen_base_rfc6733` | 0 | `diameter_base` | Same message set per RFC 6733 |
| `base_accounting.dia` | `diameter_gen_base_accounting` | 3 | `diameter_base_accounting` | Accounting (`ACR`/`ACA`), RFC 3588; `@inherits diameter_gen_base_rfc3588` |
| `acct_rfc6733.dia` | `diameter_gen_acct_rfc6733` | 3 | `diameter_base_accounting` | Accounting (`ACR`/`ACA`), RFC 6733; `@inherits diameter_gen_base_rfc6733` |
| `doic_rfc7683.dia` | `diameter_gen_doic_rfc7683` | none | `diameter_doic` | AVP-only (`OC-*`, RFC 7683 DOIC); for `{avp_dictionaries, [...]}`, not an application |
| `relay.dia` | `diameter_gen_relay` | `0xFFFFFFFF` | `diameter_relay` | Relay application; defines no `@messages`/`@avp_types` (unknown-everything codec) |
| `capup_rfc6737.dia` | — (not compiled) | 10 | `diameter_capup` | RFC 6737 Capability Update; reference only, not built or released |

Generated module API (created by `../src/compiler/diameter_codegen.erl`): `name/0`, `id/0`, `vendor_id/0`, `vendor_name/0`, `msg_name/2`, `msg_header/1`, `rec2msg/1`, `msg2rec/1`, `name2rec/1`, `avp_name/2`, `avp_arity/1`, `avp_arity/2`, `avp_header/1`, `avp/4`, `enumerated_avp/3`, `empty_value/2`, `dict/0`, plus `decode_avps/3`, `encode_avps/3`, `grouped_avp/4` from `../include/diameter_gen.hrl`.
`id/0` returns the `@id` and is the Application Id put into outgoing message headers (`msg_header/1`) and used to select the module for an incoming header; it raises `badarg` when `@id` is absent (`doic_rfc7683`).
`name/0` returns `@name` (the module's own name), not the file name.
Record names are `@prefix` ++ message/Grouped-AVP name *verbatim* (hyphens kept, thus quoted atoms): `#diameter_base_CER{}`, `'diameter_base_answer-message'`, `#diameter_base_accounting_ACR{}`, `#diameter_doic_OC_OLR{}`; field names are AVP names (e.g. `'Result-Code'`).
Records are exported through the `diameter_exprecs` parse transform, giving each record module `'#info-'/1`, `'#new-'/1`, `'#get-'/2`, `'#set-'/2` helpers used by `../src/base/diameter_capx.erl` et al.
`@enum`/`@define` sections (e.g. `Result-Code`, `Termination-Cause`) also produce constants/macros in the generated `.hrl`.

## Constraints
- Never edit the generated `../src/gen/diameter_gen_*.erl|.hrl` (build output, git-ignored); edit the `.dia` and rebuild via the `gen/diameter_gen_%.erl: dict/%.dia` rule (`escript ../bin/diameterc -o gen -i $(EBIN) $<`).
- A `.dia` in `DICTS` must compile using only the compiler modules plus the modules it `@inherits`; `@inherits` is resolved by loading an already-compiled dictionary beam from `-i $(EBIN)`.
  This is why `../src/Makefile` declares explicit orderings: `base_accounting` after `diameter_gen_base_rfc3588`, `acct_rfc6733` after `diameter_gen_base_rfc6733`.
- A dictionary that defines `@messages` must define `@id`; an AVP may be inherited from exactly one dictionary.
- Adding/removing a shipped dictionary requires editing `DICTS` in `../src/modules.mk` — the generated modules are what ends up in the `modules` list of `diameter.app`.
- The two base dictionaries share `@prefix diameter_base`, so their records/enums have identical names but are *not* interchangeable — a `.dia`/`.erl` must be compiled against exactly one of them.

## Design Decisions
- Base dictionaries carry the full AVP type/`@enum`/`@define`/`@grouped` definitions for the base protocol; application dictionaries are thin (mostly `@messages`) and pull the shared AVPs in via `@inherits`.
- The RFC 3588 and RFC 6733 base dictionaries are kept as separate, non-inheriting files because the RFCs are not strictly backward compatible (e.g. `answer-message` result codes, DiameterURI defaults); codegen differences show up in runtime behaviour of `diameter_traffic`/`diameter_capx`.
- Both accounting dictionaries deliberately share `@prefix diameter_base_accounting` (identical `ACR`/`ACA` layout) so names/APIs are stable across the RFC 3588 → 6733 variants.
- `relay.dia` defines nothing on purpose: the relay application proxied messages are unknown, so an id-only dictionary (every AVP decoded as `'AVP'`, every unknown command name `''`) is exactly what relay needs.
- RFC 7683 DOIC defines AVPs piggybacked on arbitrary messages rather than its own application, so `doic_rfc7683.dia` has no `@id` and is meant for the `avp_dictionaries` service/transport option.

## Known Issues / Notes for Agents
- `capup_rfc6737.dia` is NOT in `DICTS`: it is neither compiled, installed (`release_spec` installs only `$(DICTS:%=dict/%.dia)`) nor referenced anywhere else in the repository; commit 2a6751d20f added it as "neither built nor supported in code yet". Treat it as an unsupported reference dictionary — do not add it to `DICTS` without implementing capability-update support.
- `doic_rfc7683.dia` has no `@id` (it defines no `@messages`), so `diameter_gen_doic_rfc7683:id/0` raises `badarg`; use it only as an `avp_dictionaries` entry.
- `?DIAMETER_DICT_COMMON/ACCOUNTING/RELAY` in `../include/diameter.hrl` are deprecated but still used internally (e.g. `diameter_traffic` and `diameter_watchdog` use RFC 3588 as `BASE`), so `base_rfc3588.dia` cannot be removed.
- `../doc/references/diameter_dict.md` calls the RFC 3588 accounting module `diameter_gen_accounting`; the actual generated module is `diameter_gen_base_accounting`.
- `diameter_codegen:preprocess/2` (the `forms` codegen option) pulls `include_lib` forms out of an existing `diameter_gen_relay` beam, so `diameter_gen_relay` must already be compiled and on the code path for that codegen path to work.
- `base_rfc3588.dia` ends with a `@define Termination-Cause` block (backwards compatibility) alongside its `@enum Termination-Cause`; both are intentional.
- Test suites in `../test/` `-include` the generated `diameter_gen_*.hrl` (which only exist after a build) and hard-code record names such as `'diameter_base_answer-message'`, so renaming a dictionary or its prefix breaks tests.

## See Also
- `../src/Makefile` — dict→module rules, `DICT_*` variables, dependency lines, `.dia` install into `$(RELSYSDIR)/src/dict`.
- `../src/modules.mk` — `DICTS` (which dictionaries are built/shipped) and the runtime/compiler module lists.
- `../src/compiler/diameter_codegen.erl` + `diameter_dict_scanner.erl`/`diameter_dict_parser.yrl`/`diameter_dict_util.erl` — how a `.dia` becomes a module.
- `../bin/diameterc` — escript front-end used by the build; `../src/compiler/diameter_make.erl` — programmatic equivalent.
- `../include/diameter.hrl` (app-id macros, `#diameter_packet`/`#diameter_avp`) and `../include/diameter_gen.hrl` (codec callbacks: `decode_avps/3`, `encode_avps/3`, `grouped_avp/4`).
- `../doc/references/diameter_dict.md` (`@id`/`@name`/`@prefix`/`@vendor`/`@inherits`/`@avp_types`/`@messages`/`@grouped` semantics), `../doc/references/diameterc_cmd.md`.
- `../src/base/diameter.erl` — `dictionary` and `avp_dictionaries` service/transport options that consume these modules.
