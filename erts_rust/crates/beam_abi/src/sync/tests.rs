// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Unit tests for the shared threading / synchronisation primitive layer.
//!
//! Each test exercises the layer through its public free functions, i.e. exactly the way the two C
//! facades will call them. Thread bodies record their observations in atomics (or return them
//! through `join`) and the *test* thread makes the assertions: a panic inside a thread that
//! `pthread_create` started would abort the whole test process instead of failing one test.

use super::*;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::ffi::{CStr, CString};

/// A response value the workers return, so `join` has something observable to round-trip.
const MAGIC: usize = 0x0BAD_F00D;

/// Create a thread with the given entry point and context, asserting success.
fn spawn_worker(
    func: extern "C" fn(*mut c_void) -> *mut c_void,
    ctx: *mut c_void,
) -> Tid {
    let mut tid: Tid = ptr::null_mut();
    // SAFETY: a null name/opts means "unnamed, platform default stack"; `&mut tid` is a valid
    // out-slot and `func` is a plain `extern "C"` entry point.
    let res = unsafe { thread_create(ptr::null(), &mut tid, func, ctx, ptr::null_mut()) };
    assert_eq!(res, 0, "thread_create failed with {res}");
    assert!(!tid.is_null(), "thread_create left a null tid");
    tid
}

/// Join a thread created by [`spawn_worker`], asserting success, and return its response.
fn join_worker(tid: Tid) -> *mut c_void {
    let mut resp: *mut c_void = ptr::null_mut();
    // SAFETY: `tid` came from `spawn_worker` (so it is joinable and not yet joined) and `&mut resp`
    // is a valid out-slot.
    let res = unsafe { thread_join(tid, &mut resp) };
    assert_eq!(res, 0, "thread_join failed with {res}");
    resp
}

/// Read a `char *` returned by a `*_name` accessor as an owned `String` (null -> `None`).
fn read_name(name: *mut c_char) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // SAFETY: every `*_name` accessor returns either null or a NUL-terminated buffer owned by the
    // object / registry entry, which the caller keeps alive across this read.
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Spin (yielding) until `flag` becomes true, or give up after a bounded number of attempts so a
/// broken primitive fails the test instead of hanging the suite.
fn wait_for(flag: &AtomicBool) {
    for _ in 0..2_000_000 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("timed out waiting for the worker thread to make progress");
}

// ---------------------------------------------------------------------------------------------
// Mutex
// ---------------------------------------------------------------------------------------------

struct MutexState {
    mtx: *mut Mutex,
    counter: AtomicUsize,
    in_cs: AtomicUsize,
    overlap: AtomicBool,
}

extern "C" fn mutex_worker(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `MutexState` the test keeps alive on its stack until every worker has
    // been joined; all shared fields are atomics, so the shared references created here are sound.
    let state = unsafe { &*(ctx as *const MutexState) };
    for _ in 0..500 {
        // SAFETY: `state.mtx` is a live mutex handle created by this test.
        unsafe { mutex_lock(state.mtx) };
        if state.in_cs.fetch_add(1, Ordering::SeqCst) != 0 {
            state.overlap.store(true, Ordering::SeqCst);
        }
        std::thread::yield_now();
        let seen = state.counter.load(Ordering::SeqCst);
        std::thread::yield_now();
        state.counter.store(seen + 1, Ordering::SeqCst);
        state.in_cs.fetch_sub(1, Ordering::SeqCst);
        // SAFETY: this thread currently holds the mutex (a non-error-checking one, so releasing it
        // from any thread is well defined).
        unsafe { mutex_unlock(state.mtx) };
    }
    ptr::null_mut()
}

#[test]
fn mutex_serialises_threads() {
    // SAFETY: a null name is the "unnamed" case.
    let mtx = unsafe { mutex_create(ptr::null()) };
    assert!(!mtx.is_null(), "mutex_create returned null");
    let state = MutexState {
        mtx,
        counter: AtomicUsize::new(0),
        in_cs: AtomicUsize::new(0),
        overlap: AtomicBool::new(false),
    };
    let ctx = &state as *const MutexState as *mut c_void;

    let tids: Vec<Tid> = (0..4).map(|_| spawn_worker(mutex_worker, ctx)).collect();
    for tid in tids {
        join_worker(tid);
    }

    assert_eq!(
        state.counter.load(Ordering::SeqCst),
        4 * 500,
        "the non-atomic read/modify/write must have been fully serialised"
    );
    assert!(
        !state.overlap.load(Ordering::SeqCst),
        "two threads were inside the critical section at the same time"
    );
    // SAFETY: every worker has been joined and the handle is not used again.
    unsafe { mutex_destroy(mtx) };
}

#[test]
fn mutex_try_lock_reports_busy_until_unlocked() {
    // SAFETY: null name; the handle is live for the whole test.
    let mtx = unsafe { mutex_create(ptr::null()) };
    assert!(!mtx.is_null());
    // SAFETY: `mtx` is live.
    unsafe { mutex_lock(mtx) };
    // SAFETY: `mtx` is live and locked by this thread, so the try must report the busy code.
    assert_eq!(unsafe { mutex_try_lock(mtx) }, libc::EBUSY);
    // SAFETY: this thread holds the mutex.
    unsafe { mutex_unlock(mtx) };
    // SAFETY: `mtx` is live and unlocked.
    assert_eq!(unsafe { mutex_try_lock(mtx) }, 0);
    // SAFETY: the previous try acquired it.
    unsafe { mutex_unlock(mtx) };
    // SAFETY: the handle is not used again.
    unsafe { mutex_destroy(mtx) };
}

// ---------------------------------------------------------------------------------------------
// Condition variable
// ---------------------------------------------------------------------------------------------

struct CondState {
    mtx: *mut Mutex,
    cnd: *mut Cond,
    started: AtomicBool,
    ready: AtomicBool,
    woke: AtomicBool,
    held_on_wake: AtomicBool,
}

extern "C" fn cond_waiter(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `CondState` the test keeps alive until this thread has been joined.
    let state = unsafe { &*(ctx as *const CondState) };
    // SAFETY: both handles are live; the lock is taken before the predicate is examined.
    unsafe { mutex_lock(state.mtx) };
    // Signalled from under the lock, so the test thread cannot acquire the mutex before this point.
    state.started.store(true, Ordering::SeqCst);
    while !state.ready.load(Ordering::SeqCst) {
        // SAFETY: `state.mtx` is held by this thread, which is `cond_wait`'s precondition; the call
        // releases it and re-acquires it before returning.
        unsafe { cond_wait(state.cnd, state.mtx) };
    }
    state.woke.store(true, Ordering::SeqCst);
    // `cond_wait` must have re-acquired the mutex: a try from this thread reports the busy code
    // (a non-error-checking mutex, so "owned by the caller" is detected as EBUSY).
    // SAFETY: `state.mtx` is live.
    let held = unsafe { mutex_try_lock(state.mtx) } == libc::EBUSY;
    state.held_on_wake.store(held, Ordering::SeqCst);
    // SAFETY: this thread holds the mutex.
    unsafe { mutex_unlock(state.mtx) };
    ptr::null_mut()
}

#[test]
fn cond_wakes_a_waiter_and_reacquires_the_mutex() {
    // SAFETY: null names; both handles are live for the whole test.
    let mtx = unsafe { mutex_create(ptr::null()) };
    let cnd = unsafe { cond_create(ptr::null()) };
    assert!(!mtx.is_null() && !cnd.is_null());
    let state = CondState {
        mtx,
        cnd,
        started: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        woke: AtomicBool::new(false),
        held_on_wake: AtomicBool::new(false),
    };

    let tid = spawn_worker(cond_waiter, &state as *const CondState as *mut c_void);
    wait_for(&state.started);
    // Once this thread holds the mutex again, the waiter is provably inside `cond_wait` (it only
    // releases the mutex there), so setting the predicate and signalling cannot be lost.
    // SAFETY: `mtx` is live.
    unsafe { mutex_lock(mtx) };
    state.ready.store(true, Ordering::SeqCst);
    // SAFETY: `cnd` is live and a waiter is registered on it.
    unsafe { cond_signal(cnd) };
    // SAFETY: this thread holds the mutex.
    unsafe { mutex_unlock(mtx) };

    join_worker(tid);
    assert!(state.woke.load(Ordering::SeqCst), "the waiter was not woken");
    assert!(
        state.held_on_wake.load(Ordering::SeqCst),
        "cond_wait did not re-acquire the mutex before returning"
    );
    // SAFETY: the waiter has been joined and no handle is used again.
    unsafe { cond_destroy(cnd) };
    unsafe { mutex_destroy(mtx) };
}

#[test]
fn cond_broadcast_wakes_every_waiter() {
    static WOKEN: AtomicUsize = AtomicUsize::new(0);

    struct TwoWaiters {
        mtx: *mut Mutex,
        cnd: *mut Cond,
        registered: AtomicUsize,
    }

    extern "C" fn waiter(ctx: *mut c_void) -> *mut c_void {
        // SAFETY: `ctx` is the `TwoWaiters` state the test keeps alive until both threads are joined.
        let state = unsafe { &*(ctx as *const TwoWaiters) };
        // SAFETY: both handles are live.
        unsafe { mutex_lock(state.mtx) };
        state.registered.fetch_add(1, Ordering::SeqCst);
        // The parent broadcasts while it holds the mutex, so a non-broadcast signal would leave the
        // second waiter blocked here and the join below would hang (the test would fail loudly).
        // SAFETY: this thread holds the mutex.
        unsafe { cond_wait(state.cnd, state.mtx) };
        WOKEN.fetch_add(1, Ordering::SeqCst);
        // SAFETY: this thread holds the mutex again.
        unsafe { mutex_unlock(state.mtx) };
        ptr::null_mut()
    }

    WOKEN.store(0, Ordering::SeqCst);
    // SAFETY: null names; both handles are live for the whole test.
    let mtx = unsafe { mutex_create(ptr::null()) };
    let cnd = unsafe { cond_create(ptr::null()) };
    let state = TwoWaiters {
        mtx,
        cnd,
        registered: AtomicUsize::new(0),
    };
    let ctx = &state as *const TwoWaiters as *mut c_void;
    let tids = [spawn_worker(waiter, ctx), spawn_worker(waiter, ctx)];

    // Wait until both waiters hold (or have held) the mutex, then broadcast under it.
    for _ in 0..2_000_000 {
        if state.registered.load(Ordering::SeqCst) == 2 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(state.registered.load(Ordering::SeqCst), 2);
    // SAFETY: `mtx` is live.
    unsafe { mutex_lock(mtx) };
    // SAFETY: `cnd` is live and both waiters are registered on it.
    unsafe { cond_broadcast(cnd) };
    // SAFETY: this thread holds the mutex.
    unsafe { mutex_unlock(mtx) };

    for tid in tids {
        join_worker(tid);
    }
    assert_eq!(WOKEN.load(Ordering::SeqCst), 2, "broadcast must wake both waiters");
    // SAFETY: both waiters have been joined and no handle is used again.
    unsafe { cond_destroy(cnd) };
    unsafe { mutex_destroy(mtx) };
}

// ---------------------------------------------------------------------------------------------
// Reader/writer lock
// ---------------------------------------------------------------------------------------------

struct RwState {
    lck: *mut RwLock,
    /// Number of readers currently inside the lock.
    readers_inside: AtomicUsize,
    /// Set when two readers were inside the lock *simultaneously*.
    concurrent: AtomicBool,
    attempted: AtomicBool,
    writer_inside: AtomicBool,
}

extern "C" fn rw_reader(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `RwState` the test keeps alive until both readers have been joined.
    let state = unsafe { &*(ctx as *const RwState) };
    // SAFETY: `state.lck` is a live reader/writer lock.
    unsafe { rwlock_read_lock(state.lck) };
    if state.readers_inside.fetch_add(1, Ordering::SeqCst) == 1 {
        state.concurrent.store(true, Ordering::SeqCst);
    }
    // Hold the read lock until the peer arrives (bounded, so a lock that admits only one reader
    // fails the assertion below instead of hanging the suite).
    for _ in 0..2_000_000 {
        if state.readers_inside.load(Ordering::SeqCst) >= 2 {
            break;
        }
        std::thread::yield_now();
    }
    state.readers_inside.fetch_sub(1, Ordering::SeqCst);
    // SAFETY: this thread holds the read lock.
    unsafe { rwlock_read_unlock(state.lck) };
    ptr::null_mut()
}

extern "C" fn rw_writer(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `RwState` the test keeps alive until this thread has been joined.
    let state = unsafe { &*(ctx as *const RwState) };
    state.attempted.store(true, Ordering::SeqCst);
    // SAFETY: `state.lck` is a live reader/writer lock; this blocks while the reader holds it.
    unsafe { rwlock_write_lock(state.lck) };
    state.writer_inside.store(true, Ordering::SeqCst);
    // SAFETY: this thread holds the write lock.
    unsafe { rwlock_write_unlock(state.lck) };
    ptr::null_mut()
}

#[test]
fn rwlock_admits_concurrent_readers_and_excludes_a_writer() {
    // SAFETY: null name; the handle is live for the whole test.
    let lck = unsafe { rwlock_create(ptr::null()) };
    assert!(!lck.is_null());
    let state = RwState {
        lck,
        readers_inside: AtomicUsize::new(0),
        concurrent: AtomicBool::new(false),
        attempted: AtomicBool::new(false),
        writer_inside: AtomicBool::new(false),
    };
    let ctx = &state as *const RwState as *mut c_void;

    // Two readers must be able to hold the lock at the same time.
    let readers = [spawn_worker(rw_reader, ctx), spawn_worker(rw_reader, ctx)];
    for tid in readers {
        join_worker(tid);
    }
    assert!(
        state.concurrent.load(Ordering::SeqCst),
        "the two readers never held the lock simultaneously"
    );

    // A writer is excluded while a reader holds the lock.
    // SAFETY: `lck` is live.
    unsafe { rwlock_read_lock(lck) };
    // SAFETY: `lck` is live and held for reading by this thread -> the write try must fail.
    assert_ne!(unsafe { rwlock_try_write_lock(lck) }, 0);

    let writer = spawn_worker(rw_writer, ctx);
    wait_for(&state.attempted);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !state.writer_inside.load(Ordering::SeqCst),
        "the writer entered the lock while a reader held it"
    );
    // SAFETY: this thread holds the read lock.
    unsafe { rwlock_read_unlock(lck) };

    join_worker(writer);
    assert!(
        state.writer_inside.load(Ordering::SeqCst),
        "the writer never acquired the lock after the reader released it"
    );
    // The writer released it again.
    // SAFETY: `lck` is live and unlocked.
    assert_eq!(unsafe { rwlock_try_write_lock(lck) }, 0);
    // SAFETY: the try above acquired the write lock.
    unsafe { rwlock_write_unlock(lck) };
    // SAFETY: the handle is not used again.
    unsafe { rwlock_destroy(lck) };
}

// ---------------------------------------------------------------------------------------------
// Thread-specific data
// ---------------------------------------------------------------------------------------------

struct TsdState {
    key: TsdKey,
    /// The value seen by the new thread *before* it sets anything (0 = null).
    seen_first: AtomicUsize,
    value: *mut c_void,
}

extern "C" fn tsd_worker(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `TsdState` the test keeps alive until this thread has been joined.
    let state = unsafe { &*(ctx as *const TsdState) };
    state
        .seen_first
        .store(tsd_get(state.key) as usize, Ordering::SeqCst);
    tsd_set(state.key, state.value);
    tsd_get(state.key)
}

#[test]
fn tsd_values_are_per_thread() {
    let mut key: TsdKey = 0;
    // SAFETY: `&mut key` is a valid out-slot; the name is dropped (the C never exposes it either).
    let res = unsafe { tsd_key_create(ptr::null(), &mut key) };
    assert_eq!(res, 0, "tsd_key_create failed with {res}");

    let main_value = 0x1111 as *mut c_void;
    let thread_value = 0x2222 as *mut c_void;
    tsd_set(key, main_value);

    let state = TsdState {
        key,
        seen_first: AtomicUsize::new(usize::MAX),
        value: thread_value,
    };
    let tid = spawn_worker(tsd_worker, &state as *const TsdState as *mut c_void);

    assert_eq!(
        join_worker(tid),
        thread_value,
        "the new thread must read back what it set"
    );
    assert_eq!(
        state.seen_first.load(Ordering::SeqCst),
        0,
        "a fresh thread must start with no value for the key"
    );
    assert_eq!(
        tsd_get(key),
        main_value,
        "the new thread's writes must not be visible here"
    );
    // The key itself is global: a second thread created after the first one dies still gets a fresh
    // (null) slot.
    let state2 = TsdState {
        key,
        seen_first: AtomicUsize::new(usize::MAX),
        value: thread_value,
    };
    let tid2 = spawn_worker(tsd_worker, &state2 as *const TsdState as *mut c_void);
    assert_eq!(join_worker(tid2), thread_value);
    assert_eq!(state2.seen_first.load(Ordering::SeqCst), 0);

    tsd_key_destroy(key);
}

// ---------------------------------------------------------------------------------------------
// Threads, tids, thread type, options
// ---------------------------------------------------------------------------------------------

struct ProbeState {
    self_tid: AtomicUsize,
    seen_type: AtomicI32,
    seen_name: AtomicUsize,
}

extern "C" fn probe(ctx: *mut c_void) -> *mut c_void {
    // SAFETY: `ctx` is the `ProbeState` the test keeps alive until this thread has been joined.
    let state = unsafe { &*(ctx as *const ProbeState) };
    state
        .self_tid
        .store(thread_self_tid() as usize, Ordering::SeqCst);
    state.seen_type.store(thread_type(), Ordering::SeqCst);
    state
        .seen_name
        .store(thread_name(thread_self_tid()) as usize, Ordering::SeqCst);
    MAGIC as *mut c_void
}

fn empty_probe_state() -> ProbeState {
    ProbeState {
        self_tid: AtomicUsize::new(0),
        seen_type: AtomicI32::new(i32::MIN),
        seen_name: AtomicUsize::new(0),
    }
}

#[test]
fn thread_create_self_equal_and_join_round_trip() {
    let state = empty_probe_state();
    let tid = spawn_worker(probe, &state as *const ProbeState as *mut c_void);

    assert_eq!(join_worker(tid) as usize, MAGIC, "join must return the response");
    assert!(
        thread_equal_tids(state.self_tid.load(Ordering::SeqCst) as Tid, tid),
        "the tid the thread reported for itself must equal the created tid"
    );
    assert!(
        !thread_equal_tids(thread_self_tid(), tid),
        "distinct threads must not compare equal"
    );
    assert!(
        !thread_equal_tids(ptr::null_mut(), tid),
        "a null tid is never equal to a real one"
    );
}

#[test]
fn thread_type_is_other_only_on_created_threads() {
    // The test thread is not a thread created by this layer.
    assert_eq!(thread_type(), ERL_NIF_THR_UNDEFINED);

    let state = empty_probe_state();
    let tid = spawn_worker(probe, &state as *const ProbeState as *mut c_void);
    join_worker(tid);
    assert_eq!(
        state.seen_type.load(Ordering::SeqCst),
        ERL_NIF_THR_OTHER,
        "a thread created through this layer must report ERL_NIF_THR_OTHER"
    );
}

#[test]
fn thread_opts_create_and_destroy() {
    let opts = thread_opts_create(ptr::null());
    assert!(!opts.is_null(), "thread_opts_create returned null");
    // SAFETY: `opts` is a live record from `thread_opts_create`, and `suggested_stack_size` is the
    // record's public field (a driver may write it, as in C).
    assert_eq!(unsafe { (*opts).suggested_stack_size }, -1);
    // SAFETY: as above — 64 *kilowords* (= 512 KiB on LP64), the unit the C documents; the naive
    // byte reading would request a stack three orders of magnitude too small.
    unsafe { (*opts).suggested_stack_size = 64 };

    let state = empty_probe_state();
    let mut tid: Tid = ptr::null_mut();
    // SAFETY: `opts` is live, `&mut tid` is a valid out-slot and `probe` is a plain entry point.
    let res = unsafe {
        thread_create(
            ptr::null(),
            &mut tid,
            probe,
            &state as *const ProbeState as *mut c_void,
            opts,
        )
    };
    assert_eq!(res, 0, "thread_create with options failed with {res}");
    assert_eq!(join_worker(tid) as usize, MAGIC);
    // SAFETY: the thread no longer needs the record; the handle is not used again.
    unsafe { thread_opts_destroy(opts) };

    // A null record is a no-op, and `-1` (the default) means the platform stack.
    // SAFETY: null is explicitly allowed.
    unsafe { thread_opts_destroy(ptr::null_mut()) };
    let opts = thread_opts_create(ptr::null());
    // SAFETY: `opts` is live; `-1` is the documented "use the platform default" value.
    unsafe { (*opts).suggested_stack_size = -1 };
    let state = empty_probe_state();
    let mut tid: Tid = ptr::null_mut();
    // SAFETY: `opts` is live, `&mut tid` is a valid out-slot.
    let res = unsafe {
        thread_create(
            ptr::null(),
            &mut tid,
            probe,
            &state as *const ProbeState as *mut c_void,
            opts,
        )
    };
    assert_eq!(res, 0, "thread_create with the default stack failed with {res}");
    assert_eq!(join_worker(tid) as usize, MAGIC);
    // SAFETY: the handle is not used again.
    unsafe { thread_opts_destroy(opts) };
}

#[test]
fn suggested_stack_size_is_kilowords() {
    // The C's `suggested_stack_size` counts *kilowords* (`ETHR_KW2B` = `KW * sizeof(void*) * 1024`,
    // `erl_driver.md:637-638`), not bytes: 64 kW is 512 KiB on LP64, plus the extra page the C adds
    // "for possible system usage" (`pthread/ethread.c:360-379`).
    assert_eq!(stack_bytes(64), 64 * STACK_WORD_BYTES + PAGE_BYTES);
    // A request below the C's window is raised to its floor (`ethr_min_stack_size__`, 20 KiB here);
    // a pathological one is capped at `ethr_max_stack_size__` (64 MiB on 64-bit), never overflowed.
    assert_eq!(stack_bytes(0), MIN_STACK);
    assert_eq!(stack_bytes(1), MIN_STACK);
    assert!(stack_bytes(4) > MIN_STACK, "32 KiB must be usable as requested");
    assert_eq!(stack_bytes(c_int::MAX), MAX_STACK);
}

const EXIT_MAGIC: usize = 0x5A5A_1234;

extern "C" fn exiting_worker(_ctx: *mut c_void) -> *mut c_void {
    // Never returns: the response reaches the joiner through the thread's exit value.
    thread_exit(EXIT_MAGIC as *mut c_void)
}

#[test]
fn thread_exit_round_trips_through_join() {
    let tid = spawn_worker(exiting_worker, ptr::null_mut());
    assert_eq!(join_worker(tid) as usize, EXIT_MAGIC);
    assert!(
        thread_name(tid).is_null(),
        "the registry entry must be retired by the join"
    );
}

// ---------------------------------------------------------------------------------------------
// Debug names
// ---------------------------------------------------------------------------------------------

#[test]
fn name_accessors_return_the_created_name_or_null() {
    let mut named = Vec::from(&b"proto-lock\0"[..]);
    let name = named.as_ptr() as *const c_char;
    // SAFETY: `name` points at the NUL-terminated buffer above, which stays alive across the calls.
    let mtx = unsafe { mutex_create(name) };
    // SAFETY: as above.
    let cnd = unsafe { cond_create(name) };
    // SAFETY: as above.
    let lck = unsafe { rwlock_create(name) };
    assert!(!mtx.is_null() && !cnd.is_null() && !lck.is_null());
    // The name is *copied* at creation (as in the C), so mutating the caller's buffer afterwards
    // must not be visible.
    named[0] = b'X';
    // SAFETY: all three handles are live.
    assert_eq!(read_name(unsafe { mutex_name(mtx) }), Some("proto-lock".into()));
    // SAFETY: as above.
    assert_eq!(read_name(unsafe { cond_name(cnd) }), Some("proto-lock".into()));
    // SAFETY: as above.
    assert_eq!(read_name(unsafe { rwlock_name(lck) }), Some("proto-lock".into()));

    // Unnamed objects carry no *stored* name, which this layer reports as null. A frozen ABI whose C
    // stores the static `no_name` string in the handle instead (`erl_drv_thread.c:99,135,170`) renders
    // that state with `no_name()` — what the driver facade does.
    // SAFETY: a null name is the "unnamed" case; the handles are live.
    let unnamed_mtx = unsafe { mutex_create(ptr::null()) };
    // SAFETY: as above.
    let unnamed_cnd = unsafe { cond_create(ptr::null()) };
    // SAFETY: as above.
    let unnamed_lck = unsafe { rwlock_create(ptr::null()) };
    // SAFETY: all three handles are live.
    assert_eq!(read_name(unsafe { mutex_name(unnamed_mtx) }), None);
    // SAFETY: as above.
    assert_eq!(read_name(unsafe { cond_name(unnamed_cnd) }), None);
    // SAFETY: as above.
    assert_eq!(read_name(unsafe { rwlock_name(unnamed_lck) }), None);

    // SAFETY: none of the handles is used again.
    unsafe {
        mutex_destroy(mtx);
        cond_destroy(cnd);
        rwlock_destroy(lck);
        mutex_destroy(unnamed_mtx);
        cond_destroy(unnamed_cnd);
        rwlock_destroy(unnamed_lck);
    }
}

#[test]
fn the_shared_layer_exposes_the_c_no_name_string() {
    // The C's `no_name` (`erl_drv_thread.c:99`, assigned `"unknown"` at `:135`): what a frozen ABI
    // reports for a **live** handle created with a NULL name, because the C stores that pointer in the
    // handle at create time (`:170`, `:264`, `:341`, `:639`, `:678`).
    let name = no_name();
    assert_eq!(read_name(name), Some("unknown".into()));
    // The C hands out one static string, never a fresh buffer — so the pointer is stable.
    assert_eq!(no_name(), name, "`no_name` must be a single static string");
}

#[test]
fn created_thread_names_live_in_the_registry() {
    let name = CString::new("nif-worker-1").expect("test name has no interior NUL");
    let state = empty_probe_state();
    let mut tid: Tid = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated and alive across the call; `&mut tid` is a valid out-slot.
    let res = unsafe {
        thread_create(
            name.as_ptr(),
            &mut tid,
            probe,
            &state as *const ProbeState as *mut c_void,
            ptr::null_mut(),
        )
    };
    assert_eq!(res, 0);
    // The creator can look the name up as soon as `thread_create` returns.
    assert_eq!(read_name(thread_name(tid)), Some("nif-worker-1".into()));
    // A *named* created thread is never the "created unnamed" state.
    assert!(!thread_created_unnamed(tid));
    // The new thread sees the same name for itself (it registers it at its entry).
    while state.seen_name.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    assert_eq!(
        read_name(state.seen_name.load(Ordering::SeqCst) as *mut c_char),
        Some("nif-worker-1".into())
    );
    assert_eq!(join_worker(tid) as usize, MAGIC);
    // The entry was retired by the join, so the name is gone rather than dangling.
    assert!(thread_name(tid).is_null());
    assert!(!thread_created_unnamed(tid));

    // An unnamed created thread has no *stored* name: this layer reports null for it (never a
    // dangling name) and `thread_created_unnamed` is what lets a facade render the C's `no_name`
    // (`erl_drv_thread.c:638-639`) instead.
    let anonymous_state = empty_probe_state();
    let anonymous = spawn_worker(probe, &anonymous_state as *const ProbeState as *mut c_void);
    assert!(thread_name(anonymous).is_null());
    assert!(thread_created_unnamed(anonymous), "created with a null name");
    join_worker(anonymous);
    assert!(thread_name(anonymous).is_null());
    assert!(!thread_created_unnamed(anonymous), "the entry is retired by the join");
    // An unknown tid reports null and is not a created thread.
    assert!(thread_name(0x1234 as Tid).is_null());
    assert!(!thread_created_unnamed(0x1234 as Tid));
    assert!(!thread_created_unnamed(ptr::null_mut()));
}

// ---------------------------------------------------------------------------------------------
// Null-handle behaviour
// ---------------------------------------------------------------------------------------------

#[test]
fn null_handles_are_inert() {
    // SAFETY: every argument is null, which each function explicitly handles (the C aborts here;
    // see the module docs' deviation list).
    unsafe {
        mutex_destroy(ptr::null_mut());
        mutex_lock(ptr::null_mut());
        mutex_unlock(ptr::null_mut());
        assert_eq!(mutex_try_lock(ptr::null_mut()), libc::EINVAL);
        assert!(mutex_name(ptr::null_mut()).is_null());

        cond_destroy(ptr::null_mut());
        cond_signal(ptr::null_mut());
        cond_broadcast(ptr::null_mut());
        cond_wait(ptr::null_mut(), ptr::null_mut());
        assert!(cond_name(ptr::null_mut()).is_null());

        rwlock_destroy(ptr::null_mut());
        rwlock_read_lock(ptr::null_mut());
        rwlock_read_unlock(ptr::null_mut());
        rwlock_write_lock(ptr::null_mut());
        rwlock_write_unlock(ptr::null_mut());
        assert_eq!(rwlock_try_read_lock(ptr::null_mut()), libc::EINVAL);
        assert_eq!(rwlock_try_write_lock(ptr::null_mut()), libc::EINVAL);
        assert!(rwlock_name(ptr::null_mut()).is_null());

        assert_eq!(tsd_key_create(ptr::null(), ptr::null_mut()), libc::EINVAL);
        assert_eq!(thread_join(ptr::null_mut(), ptr::null_mut()), libc::EINVAL);
        assert_eq!(
            thread_create(ptr::null(), ptr::null_mut(), probe, ptr::null_mut(), ptr::null_mut()),
            libc::EINVAL
        );
        assert!(thread_name(ptr::null_mut()).is_null());
        assert!(!thread_equal_tids(ptr::null_mut(), thread_self_tid()));
        thread_opts_destroy(ptr::null_mut());
    }
}
