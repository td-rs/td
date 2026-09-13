//! S32 tests: typed `ActorId<A>`/`ActorOwn<A>`/`ActorShared<A>` markers,
//! name access, and session-unique id identity.
//!
//! The plan replaces the C++ `ActorId<ActorType>` template with a
//! session-unique runtime id plus an inert type marker: allocation never
//! depends on `A`, erasure/retagging never allocates, and identity (equality,
//! liveness, routing) is marker-independent.

use std::sync::{Arc, Mutex};

use tdactor::{Actor, ActorId, ActorOwn, ConcurrentScheduler, Event, Raw, Scheduler, send_event};

struct A;
impl Actor for A {}

struct B;
impl Actor for B {}

/// Factories return typed `ActorOwn<A>`; ids are unique per actor, carry the
/// name, and the marker round-trips through erasure and `cast`.
#[test]
fn ids_are_session_unique_and_typed() {
    let mut sched = ConcurrentScheduler::new(0);
    let own_a: ActorOwn<A> = sched.create_actor_unsafe(0, "A", A);
    let own_b: ActorOwn<B> = sched.create_actor_unsafe(0, "B", B);
    let id_a: ActorId<A> = own_a.get();
    let id_b: ActorId<B> = own_b.get();

    // Session-unique: one slot per actor, even for the same actor type.
    let own_a2: ActorOwn<A> = sched.create_actor_unsafe(0, "A2", A);
    let id_a2 = own_a2.get();
    assert_ne!(id_a.clone().erase(), id_a2.clone().erase());
    assert_ne!(id_a2.clone().erase(), id_b.clone().erase());

    // Names come from registration and are readable from any thread.
    assert_eq!(id_a.get_name(), "A");
    assert_eq!(id_b.get_name(), "B");
    assert_eq!(id_a2.get_name(), "A2");

    // The marker survives erase + cast without touching the id.
    let retyped: ActorId<A> = id_a.clone().erase().cast();
    assert_eq!(id_a, retyped);
    assert_eq!(retyped.get_name(), "A");
    assert!(retyped.is_alive());

    sched.finish();
}

/// A destroyed actor's id reports dead (from any thread) but keeps its name;
/// sends to it become no-ops.
#[test]
fn dead_ids_report_and_keep_names() {
    struct Quitter;
    impl Actor for Quitter {
        fn start_up(&mut self) {
            self.stop();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let own = sched.create_actor_unsafe(0, "Quitter", Quitter);
    let id: ActorId<Quitter> = own.get();
    sched.start();
    let _ = sched.run_main(1.0);
    assert!(!id.is_alive());
    assert_eq!(id.get_name(), "Quitter");
    {
        let _guard = sched.get_main_guard();
        send_event(&id, Event::raw(Raw::from(0u32))); // must be a no-op
    }
    sched.finish();
}

/// `ActorOwn::erase` transfers ownership (no double hangup); dropping the
/// erased owner stops the actor exactly once.
#[test]
fn erased_ownership_transfers() {
    struct X;
    impl Actor for X {}

    let mut sched = ConcurrentScheduler::new(0);
    let own = sched.create_actor_unsafe(0, "X", X);
    let id = own.get();
    {
        let _guard = sched.get_main_guard();
        let erased: ActorOwn = own.erase();
        assert!(erased.is_alive());
    } // drop → hangup
    sched.start();
    let _ = sched.run_main(1.0);
    assert!(!id.is_alive());
    sched.finish();
}

/// `Actor::get_name()` inside dispatch, plus the typed `actor_id_of` marker
/// reaching the actor's own id.
#[test]
fn actor_self_name_and_typed_id() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    struct Namer {
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl Actor for Namer {
        fn start_up(&mut self) {
            let me: ActorId<Namer> = self.actor_id_of::<Namer>();
            assert!(me.is_alive());
            assert_eq!(me.get_name(), "Namer");
            self.seen.lock().unwrap().push(self.get_name());
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(0, "Namer", Namer { seen: seen.clone() });
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
    assert_eq!(seen.lock().unwrap().as_slice(), ["Namer"]);
}
