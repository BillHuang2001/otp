//! Byte-exact `RECORD_EXT` (`67`) decode → re-encode image pin.
//!
//! A native record's **wire** form (`external.c:4006-4036`) emits the field keys in
//! **declaration** order, while the in-memory block sorts them by atom index
//! (`erl_record.c`). The decoder therefore has to recover the declaration order
//! (through the definition's `field_order`) and the encoder has to re-emit it — the
//! round-trip is only byte-exact if both do.
//!
//! The hand-written image below declares `[(b, 22), (a, 11)]` with atom `a` interned
//! **before** `b`, so the declaration order (`b`, `a`) is the reverse of the sorted
//! storage order (`a`, `b`). A decoder/encoder pair that emitted the *sorted* keys
//! would produce different bytes and fail this test. The whole thing is offline — it
//! never needs a system `erl`.

use beam_alloc::Heap;
use beam_term::prelude::{atom_val, make_atom, make_small, AtomTable};

use beam_codec::term::{
    is_record, record_declaration_keys, record_definition, record_definition_is_exported,
    record_definition_keys, record_definition_module, record_definition_name, record_field_count,
    record_values, AM_FALSE,
};
use beam_codec::{decode_term, encode_term, RECORD_EXT, VERSION_MAGIC};

/// A hand-written `RECORD_EXT` image: module `m`, name `r`, not exported, with the two
/// fields declared `[(b, 22), (a, 11)]` — i.e. the wire keys are `b`, `a`.
const DECLARATION_ORDER_IMAGE: &[u8] = &[
    VERSION_MAGIC, // 131
    RECORD_EXT,    // 67
    0,
    0,
    0,
    2, // num_fields = 2
    0, // flags: is_exported = false
    119,
    1,
    b'm', // module "m"
    119,
    1,
    b'r', // name "r"
    119,
    1,
    b'b', // key 0 (declared first) — `b` has the HIGHER atom index
    119,
    1,
    b'a', // key 1
    97,
    22, // value of the field declared `b`
    97,
    11, // value of the field declared `a`
];

#[test]
fn record_ext_declaration_order_round_trips_byte_exact() {
    // Seed the table as `atom.names` does (`false`/`true` first) and intern `a`
    // **before** `b`, so the sorted storage order (`a`, `b`) differs from the wire's
    // declaration order (`b`, `a`).
    let mut atoms = AtomTable::new();
    atoms.seed(["false", "true"]);
    atoms.seed(["m", "r", "a", "b"]);
    assert!(
        atoms.lookup("a").unwrap().raw() < atoms.lookup("b").unwrap().raw(),
        "atom `a` must precede `b` by index for this pin to be meaningful"
    );

    let mut heap = Heap::new(256).expect("a heap for a 2-field record");
    let term = decode_term(DECLARATION_ORDER_IMAGE, &mut heap, &mut atoms)
        .expect("the hand-written RECORD_EXT image decodes");

    // decode → encode is the identity on the wire, byte-for-byte — keys **and** values
    // in declaration order.
    let reencoded = encode_term(term, &atoms).expect("re-encode");
    assert_eq!(
        reencoded, DECLARATION_ORDER_IMAGE,
        "decode → encode must reproduce the RECORD_EXT image byte-for-byte, including \
         the declaration order of the keys"
    );

    // The decoded term is a native record carrying every field.
    assert!(is_record(term));
    assert_eq!(record_field_count(term).unwrap(), 2);

    let def = record_definition(term).unwrap();
    assert_eq!(
        atoms.name(atom_val(record_definition_module(def).unwrap())),
        Some("m")
    );
    assert_eq!(
        atoms.name(atom_val(record_definition_name(def).unwrap())),
        Some("r")
    );
    assert_eq!(
        record_definition_is_exported(def).unwrap(),
        make_atom(AM_FALSE)
    );
    assert_eq!(
        atoms.name(atom_val(record_definition_is_exported(def).unwrap())),
        Some("false")
    );

    // Storage is sorted by atom index: `a` (lower index) precedes `b`.
    let keys = record_definition_keys(def).unwrap();
    assert_eq!(atoms.name(atom_val(keys[0])), Some("a"));
    assert_eq!(atoms.name(atom_val(keys[1])), Some("b"));
    // The values are stored in that same sorted-key order (11 for `a`, 22 for `b`).
    assert_eq!(record_values(term).unwrap(), &[make_small(11), make_small(22)]);

    // The declaration order is recovered through `field_order`: declared 0 is `b`.
    let decl = record_declaration_keys(def).unwrap();
    assert_eq!(atoms.name(atom_val(decl[0])), Some("b"));
    assert_eq!(atoms.name(atom_val(decl[1])), Some("a"));
}
