//! Path-included tests for [`super`] (`nif::enif`) — the `enif_*` host functions.
//!
//! Kept in a sibling file so `enif/mod.rs` stays under the workspace's ~1000-line convention;
//! included via `#[path]` as a **child** of `enif`, so `use super::*;` still reaches the module's
//! items (the shared helpers and every re-exported `enif_*` function).
use libc::c_int;

use super::*;
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_small};

use crate::nif::host::NifHost;
struct TestHost {
    heap: Heap,
    atoms: AtomTable,
}

impl NifHost for TestHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(prelude::make_atom(self.atoms.intern(name)))
    }
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }
}

#[test]
fn int_round_trip_through_the_c_abi() {
    let mut host = TestHost {
        heap: Heap::new(256).expect("heap"),
        atoms: AtomTable::new(),
    };
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live for the whole test.
    let term = unsafe { enif_make_int(env.as_ptr(), 5) };
    assert_eq!(t(term), make_small(5));
    let mut out: c_int = 0;
    // SAFETY: `out` is a live `c_int` and `env` is live.
    let ok = unsafe { enif_get_int(env.as_ptr(), term, &mut out) };
    assert_eq!((ok, out), (1, 5));
}

#[test]
fn table_exposes_representative_slots_and_leaves_variadics_none() {
    let table = enif_api_table();
    assert!(table.enif_alloc.is_some());
    assert!(table.enif_make_int.is_some());
    assert!(table.enif_get_tuple.is_some());
    assert!(table.enif_make_badarg.is_some());
    // Variadic: stable Rust cannot define these.
    assert!(table.enif_make_tuple.is_none());
    assert!(table.enif_make_list.is_none());
    assert!(table.enif_set_option.is_none());
    assert!(table.enif_fprintf.is_none());
}
