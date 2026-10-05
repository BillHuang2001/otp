//! `DOP_*` control-message encode→decode round-trips (arity-checked) over a
//! `beam_alloc` heap + `AtomTable`; unknown-opcode and truncated-payload negatives.
//!
//! Every case is hermetic and exercises only the value layer: the control-message
//! body is `dop` byte + concatenated **body-form** `beam_codec` term images (no
//! `VERSION_MAGIC`), exactly as it appears after the distribution header on the
//! wire. Ground truth for the opcodes and their arities is
//! `erts/emulator/beam/dist.c` (the `{DOP_*, …}` tuple shapes) and `dist.h`.

use beam_alloc::Heap;
use beam_codec::{AtomCache, ATOM_CACHE_REF_EXT, INTEGER_EXT, SMALL_INTEGER_EXT, VERSION_MAGIC};
use beam_common::BeamError;
use beam_dist::control::{decode_control, encode_control};
use beam_dist::flags::{Dop, DOP_REG_SEND, DOP_SEND};
use beam_term::prelude::{make_atom, make_small, Eterm, NIL};
use beam_term::{Atom, AtomTable};

/// A decoding fixture: the production-shaped `beam_alloc` heap plus the node's
/// `AtomTable`. Test code may `unwrap` (a failure to allocate 1024 words is a
/// broken test environment, not wire data).
struct Fixture {
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            heap: Heap::new(1024).expect("1024-word test heap"),
            atoms: AtomTable::new(),
        }
    }

    /// Interns `name` into the fixture's table.
    fn intern(&mut self, name: &str) -> Atom {
        self.atoms.intern(name)
    }

    /// Interns `name` and returns the atom term for it.
    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.intern(name))
    }

    /// Encodes `terms` under `dop`, decodes the result and asserts the round-trip
    /// (opcode byte first, same opcode, same arguments). Returns the body so
    /// callers can additionally assert on the bytes.
    fn round_trip(&mut self, dop: Dop, terms: &[Eterm]) -> Vec<u8> {
        let body = encode_control(dop, terms, &self.atoms).expect("encode_control");
        assert_eq!(
            body.first().copied(),
            Some(dop.as_u8()),
            "{dop:?}: the opcode byte must come first"
        );
        let message = decode_control(&body, &mut self.heap, &mut self.atoms, None)
            .unwrap_or_else(|err| panic!("{dop:?}: decode_control failed: {err:?}"));
        assert_eq!(message.dop, dop, "{dop:?}: opcode round-trips");
        assert_eq!(message.terms, terms, "{dop:?}: arguments round-trip");
        body
    }
}

// ---------------------------------------------------------------------------
// Byte-exactness: the body form carries no version byte.
// ---------------------------------------------------------------------------

#[test]
fn send_encodes_to_the_exact_body_form_bytes() {
    // `{DOP_SEND, 1, 2}` -> `[DOP_SEND, SMALL_INTEGER_EXT 1, SMALL_INTEGER_EXT 2]`.
    // A `term_to_binary` image would start with `VERSION_MAGIC` (131); a control
    // body must not — the version byte is carried once by the enclosing header.
    let mut fixture = Fixture::new();
    let body = encode_control(Dop::Send, &[make_small(1), make_small(2)], &fixture.atoms)
        .expect("encode_control");
    assert_eq!(
        body,
        vec![DOP_SEND, SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 2]
    );
    assert_ne!(body[0], VERSION_MAGIC);
    assert_ne!(body[1], VERSION_MAGIC);

    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .expect("decode_control");
    assert_eq!(message.dop, Dop::Send);
    assert_eq!(message.terms, vec![make_small(1), make_small(2)]);
}

#[test]
fn every_opcode_byte_is_its_wire_value() {
    let fixture = Fixture::new();
    let body = encode_control(Dop::RegSend, &[make_small(7)], &fixture.atoms).unwrap();
    assert_eq!(body[0], DOP_REG_SEND);
    assert_eq!(DOP_REG_SEND, 6);
}

// ---------------------------------------------------------------------------
// Round-trips, one per `{DOP_*, …}` shape (`dist.c`).
// ---------------------------------------------------------------------------

#[test]
fn link_round_trips() {
    // `{DOP_LINK, FromPid, ToPid}` (`dist.c:2228`, `tuple_arity == 3`): 2 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    fixture.round_trip(Dop::Link, &[from, to]);
}

#[test]
fn reg_send_round_trips() {
    // `{DOP_REG_SEND, From, Cookie, ToName}` (`dist.c:2476`): 3 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let cookie = fixture.atom("cookie");
    let to_name = fixture.atom("to_name");
    fixture.round_trip(Dop::RegSend, &[from, cookie, to_name]);
}

#[test]
fn monitor_p_round_trips() {
    // `{DOP_MONITOR_P, RemotePid, LocalPidOrName, Ref}` (`dist.c:2348`): 3 terms.
    let mut fixture = Fixture::new();
    let watcher = fixture.atom("watcher");
    let watched = fixture.atom("watched");
    let reference = fixture.atom("reference");
    fixture.round_trip(Dop::MonitorP, &[watcher, watched, reference]);
}

#[test]
fn exit_tt_round_trips() {
    // `{DOP_EXIT_TT, FromPid, ToPid, Token, Reason}` (`dist.c:2774`): 4 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    let token = fixture.atom("token");
    let reason = fixture.atom("noproc");
    fixture.round_trip(Dop::ExitTt, &[from, to, token, reason]);
}

#[test]
fn send_tt_round_trips() {
    // `{DOP_SEND_TT, FromPid, ToPid, Token}` (`dist.c:2515`): 3 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    let token = fixture.atom("token");
    fixture.round_trip(Dop::SendTt, &[from, to, token]);
}

#[test]
fn spawn_request_round_trips() {
    // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts}` (`dist.c:2921`): 5 terms.
    let mut fixture = Fixture::new();
    let reference = fixture.atom("reference");
    let from = fixture.atom("from");
    let group_leader = fixture.atom("group_leader");
    let mfa = fixture.atom("mfa");
    let opts = fixture.atom("opts");
    fixture.round_trip(
        Dop::SpawnRequest,
        &[reference, from, group_leader, mfa, opts],
    );
}

#[test]
fn unlink_id_round_trips() {
    // `{DOP_UNLINK_ID, Id, FromPid, ToPid}` (`dist.c:2287`): 3 terms.
    let mut fixture = Fixture::new();
    let id = make_small(17);
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    fixture.round_trip(Dop::UnlinkId, &[id, from, to]);
}

#[test]
fn mixed_value_kinds_round_trip() {
    // Every immediate family at once — atoms, `NIL` and both integer wire forms
    // (`SMALL_INTEGER_EXT` for 7 and `INTEGER_EXT` for 100_000) — so the codec
    // seam is exercised beyond a single tag. `{DOP_MONITOR_P_EXIT,
    // RemotePidOrName, LocalPid, Ref, Reason}` (`dist.c:2716`): 4 terms.
    let mut fixture = Fixture::new();
    let watched = fixture.atom("watched");
    let watcher = fixture.atom("watcher");
    let body = fixture.round_trip(
        Dop::MonitorPExit,
        &[watched, watcher, NIL, make_small(100_000)],
    );
    assert!(body.len() > 1 + 4, "every argument is on the wire");

    // The integer wire form is chosen by value, never narrowed: 7 is one byte
    // after its tag, 100_000 (`0x0001_86A0`) is a 4-byte `INTEGER_EXT`.
    let small = encode_control(Dop::UnlinkId, &[make_small(7)], &fixture.atoms).unwrap();
    assert_eq!(small, vec![Dop::UnlinkId.as_u8(), SMALL_INTEGER_EXT, 7]);
    let big = encode_control(Dop::UnlinkId, &[make_small(100_000)], &fixture.atoms).unwrap();
    assert_eq!(
        big,
        vec![
            Dop::UnlinkId.as_u8(),
            INTEGER_EXT,
            0x00,
            0x01,
            0x86,
            0xA0
        ]
    );
}

// ---------------------------------------------------------------------------
// Arity checking.
// ---------------------------------------------------------------------------

#[test]
fn too_few_terms_for_the_opcode_is_rejected() {
    // `DOP_REG_SEND` needs 3 terms; an encoding with 2 must not decode.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let cookie = fixture.atom("cookie");
    let body = encode_control(Dop::RegSend, &[from, cookie], &fixture.atoms).unwrap();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn too_many_terms_for_the_opcode_is_rejected() {
    // `DOP_SEND` needs exactly 2 terms; a third one is a malformed message.
    let mut fixture = Fixture::new();
    let body = encode_control(
        Dop::Send,
        &[make_small(1), make_small(2), make_small(3)],
        &fixture.atoms,
    )
    .unwrap();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn zero_terms_for_a_non_nullary_opcode_is_rejected() {
    let mut fixture = Fixture::new();
    let body = encode_control(Dop::Link, &[], &fixture.atoms).unwrap();
    assert_eq!(body, vec![Dop::Link.as_u8()]);
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn altact_sig_send_is_variable_arity() {
    // `dist.c:2625` only requires `tuple_arity >= 4` (>= 3 terms here) because
    // "the tuple might be extended with more elements in the future"; `arity()`
    // is therefore the *minimum*, and the minimum plus further elements (the
    // token, and anything a later release appends) must decode too.
    let mut fixture = Fixture::new();
    let minimum = Dop::AltactSigSend.arity();
    assert!(minimum > 0, "a control opcode has at least one argument");

    let shortest: Vec<Eterm> = (0..minimum as i64).map(make_small).collect();
    fixture.round_trip(Dop::AltactSigSend, &shortest);

    let mut extended = shortest.clone();
    extended.push(make_small(42));
    extended.push(make_small(43));
    fixture.round_trip(Dop::AltactSigSend, &extended);

    // Below the minimum is still malformed.
    let short = encode_control(Dop::AltactSigSend, &shortest[..minimum - 1], &fixture.atoms)
        .expect("encode_control");
    let result = decode_control(&short, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// Malformed input.
// ---------------------------------------------------------------------------

#[test]
fn empty_body_is_rejected() {
    let mut fixture = Fixture::new();
    let result = decode_control(&[], &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn unknown_opcode_is_rejected() {
    let mut fixture = Fixture::new();
    // 0 and 5 are not `enum dop` values (4 = `DOP_UNLINK`, 6 = `DOP_REG_SEND`).
    for opcode in [0u8, 5u8] {
        let result = decode_control(&[opcode], &mut fixture.heap, &mut fixture.atoms, None);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "opcode {opcode} must be rejected"
        );
    }
}

#[test]
fn truncated_payload_is_rejected() {
    let mut fixture = Fixture::new();
    let body = encode_control(Dop::Send, &[make_small(1), make_small(2)], &fixture.atoms).unwrap();
    // Drop the last byte: the second argument is now incomplete.
    let truncated = &body[..body.len() - 1];
    let result = decode_control(truncated, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn payload_with_an_invalid_tag_is_rejected() {
    let mut fixture = Fixture::new();
    // `0x00` is not an external-term tag.
    let body = [DOP_SEND, 0x00];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn opcode_only_body_for_a_non_nullary_opcode_is_rejected() {
    let mut fixture = Fixture::new();
    let result = decode_control(&[DOP_SEND], &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The atom-cache seam (`ATOM_CACHE_REF`, `82`).
// ---------------------------------------------------------------------------

#[test]
fn atom_cache_refs_resolve_through_the_supplied_cache() {
    // `beam_codec` M1 never *emits* cache refs, so the wire image is hand-built:
    // `{DOP_SEND, <cache ref 7>, 3}`.
    let mut fixture = Fixture::new();
    let cached = fixture.intern("cached");
    let mut cache = AtomCache::new();
    cache.set(7, cached);

    let body = [DOP_SEND, ATOM_CACHE_REF_EXT, 7, SMALL_INTEGER_EXT, 3];
    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, Some(&cache))
        .expect("decode_control with an atom cache");
    assert_eq!(message.dop, Dop::Send);
    assert_eq!(message.terms, vec![make_atom(cached), make_small(3)]);
}

#[test]
fn atom_cache_ref_without_a_cache_is_rejected() {
    let mut fixture = Fixture::new();
    let body = [DOP_SEND, ATOM_CACHE_REF_EXT, 7, SMALL_INTEGER_EXT, 3];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn atom_cache_ref_to_an_unset_slot_is_rejected() {
    let mut fixture = Fixture::new();
    let cache = AtomCache::new();
    let body = [DOP_SEND, ATOM_CACHE_REF_EXT, 7, SMALL_INTEGER_EXT, 3];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, Some(&cache));
    assert!(matches!(result, Err(BeamError::BadArg)));
}
