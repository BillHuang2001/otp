//! Access enforcement: the protection matrix and a public table.

mod common;

use beam_common::BeamError;
use beam_ets::{Access, OwnerId, Table, TableId, TableOptions};
use beam_term::prelude::*;
use common::*;

#[test]
fn access_enforcement_follows_the_protection_matrix() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    // (protection, other may write, other may read)
    let matrix = [
        (Access::Public, true, true),
        (Access::Protected, false, true),
        (Access::Private, false, false),
    ];

    for (access, other_write, other_read) in matrix {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut opts = TableOptions::new();
        opts.set_access(access);
        let mut t = Table::new(TableId::next(), opts).unwrap();
        t.set_owner(Some(owner));

        // The owner is always allowed; the non-owner follows the mode.
        t.check_read(Some(owner)).unwrap();
        t.check_write(Some(owner)).unwrap();
        assert_eq!(
            t.check_read(Some(other)).is_ok(),
            other_read,
            "{access:?} read by a non-owner"
        );
        assert_eq!(
            t.check_write(Some(other)).is_ok(),
            other_write,
            "{access:?} write by a non-owner"
        );
        // A requester that is not the owner is denied exactly as the mode says.
        assert_eq!(t.check_write(None).is_ok(), access == Access::Public);

        // The owner seeds the table: the owner may always write, whatever the mode.
        {
            let mut as_owner = t.as_requester(Some(owner));
            as_owner.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        }

        // The enforcing view rechecks every operation (a violation is `badarg`).
        {
            let mut view = t.as_requester(Some(other));
            assert_eq!(view.requester(), Some(other));

            // Reads follow the mode.
            if other_read {
                assert_eq!(view.lookup(make_small(1), &atoms).unwrap().len(), 1);
                assert!(view.member(make_small(1), &atoms).unwrap());
                assert_eq!(
                    view.lookup_element(make_small(1), 1, &atoms).unwrap(),
                    make_small(1)
                );
            } else {
                assert!(matches!(
                    view.lookup(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.member(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.lookup_element(make_small(1), 1, &atoms),
                    Err(BeamError::BadArg)
                ));
            }

            // Writes follow the mode too.
            if other_write {
                view.insert(object(&mut heap, 2, 20), &atoms).unwrap();
                assert!(view.insert_new(object(&mut heap, 3, 30), &atoms).unwrap());
                assert_eq!(view.take(make_small(2), &atoms).unwrap().len(), 1);
                view.delete(make_small(3), &atoms).unwrap();
            } else {
                assert!(matches!(
                    view.insert(object(&mut heap, 2, 20), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.insert_new(object(&mut heap, 2, 20), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.delete(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.take(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
            }
        }

        // A denied write changed nothing: the owner's object is still there and the
        // table holds exactly it.
        assert_eq!(t.lookup(make_small(1), &atoms).len(), 1);
        assert_eq!(t.size(), 1, "{access:?} final size");
    }
}

#[test]
fn a_public_table_is_open_to_every_requester() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut opts = TableOptions::new();
    opts.set_access(Access::Public);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(OwnerId::new(1)));

    // Even an ownerless requester may read and write a public table.
    let mut view = t.as_requester(None);
    view.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    assert_eq!(view.lookup(make_small(1), &atoms).unwrap().len(), 1);
    view.take(make_small(1), &atoms).unwrap();

    // A protected table still admits any reader, but only the owner may write.
    let mut opts = TableOptions::new();
    opts.set_access(Access::Protected);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(OwnerId::new(1)));
    {
        let mut owner_view = t.as_requester(Some(OwnerId::new(1)));
        owner_view.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    }
    let mut reader = t.as_requester(Some(OwnerId::new(2)));
    assert_eq!(reader.lookup(make_small(1), &atoms).unwrap().len(), 1);
    assert!(matches!(
        reader.insert(object(&mut heap, 2, 20), &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(!t.member(make_small(2), &atoms));
}
