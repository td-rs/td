//! The bridge-actor machinery — the Rust port of the C++ externally-owned
//! manager actor (`Td.cpp:1320`
//! `register_actor("UpdatesManager", updates_manager_.get())`): a normal
//! actor (own mailbox, own context tag, own dispatch slices) whose
//! handlers reach the owner actor's state, where C++ reaches it through
//! the member `unique_ptr`.
//!
//! What is verified here (no C++ counterpart — in C++ the mechanism is
//! transparent because the two objects alias freely):
//!
//! - a send from the owner's dispatch frame into its idle bridge takes
//!   the mailbox instead of running inline: two live `&mut` into one
//!   object would be aliasing UB, so the guard turns the send into a
//!   later slice;
//! - the mirror direction — a send from a bridge handler to its owner —
//!   queues too;
//! - [`Scheduler::run_nested`] runs bridge work nested in the owner's
//!   frame under the bridge's log context (the C++ inline `EventGuard`),
//!   restoring the owner's tags after;
//! - [`Scheduler::send_typed_later`] delivers a typed message into a
//!   bridge slice, and [`Scheduler::can_run_inline`] tracks the
//!   nested-vs-mailbox choice;
//! - the bridge is a fully normal actor otherwise: it receives its Start
//!   event and runs `start_up` like any `create_actor` child.

use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorId, ActorOwn, ConcurrentScheduler, Scheduler, actor_messages, create_actor,
    send_closure,
};
use tdutils::logging::current_log_tag2;

/// The shared trace: one character per handler run, asserted in order.
type Trace = Arc<Mutex<String>>;

fn mark(trace: &Trace, c: char) {
    trace.lock().unwrap().push(c);
}

/// The owner — stands in for `Td`: the bridge's registered owner, whose
/// state the bridge would reach through a raw pointer in production.
struct Owner {
    trace: Trace,
}

impl Actor for Owner {}

impl Owner {
    fn probe_bridge(&mut self, bridge_id: ActorId) {
        mark(&self.trace, 'a');
        send_closure(&bridge_id, BridgeMessage::Mark { c: 'b' });
        mark(&self.trace, 'z');
    }

    fn owner_mark(&mut self) {
        mark(&self.trace, 'o');
    }

    fn run_nested_probe(&mut self, bridge_id: ActorId) {
        mark(&self.trace, 'a');
        Scheduler::instance().run_nested(&bridge_id, || {
            // The bridge's identity is installed for the nested frame (the
            // C++ inline EventGuard's swap_context).
            assert_eq!(current_log_tag2(), "Bridge");
            mark(&self.trace, 'n');
        });
        assert_eq!(current_log_tag2(), "Owner");
        mark(&self.trace, 'z');
    }
}

actor_messages! {
    enum OwnerMessage for Owner {
        /// The owner→bridge probe: mark, send to the (idle) bridge, mark
        /// again — the guard must turn the send into a mailbox hop, so the
        /// trace reads `az` before the pump and `azb` after.
        ProbeBridge { bridge_id: ActorId, } => probe_bridge(bridge_id),
        /// The bridge→owner probe target (marks `o`).
        FromBridge => owner_mark(),
    }
}

actor_messages! {
    enum OwnerNestedMessage for Owner {
        Run { bridge_id: ActorId, } => run_nested_probe(bridge_id),
    }
}

/// The bridge — reaches the owner only through the captured `ActorId`
/// (sends) and the shared trace (marks); the production `UmBridge` derefs
/// a raw owner pointer in exactly these slices.
struct Bridge {
    trace: Trace,
    owner_id: ActorId,
}

impl Actor for Bridge {}

impl Bridge {
    fn mark_bridge(&mut self, c: char) {
        mark(&self.trace, c);
    }
}

actor_messages! {
    enum BridgeMessage for Bridge {
        Mark { c: char, } => mark_bridge(c),
    }
}

/// One scheduler with an owner + its registered bridge, both started.
struct Handles {
    sched: ConcurrentScheduler,
    _owner: ActorOwn<Owner>,
    _bridge: ActorOwn<Bridge>,
    owner_id: ActorId,
    bridge_id: ActorId<Bridge>,
    trace: Trace,
}

fn setup() -> Handles {
    let trace: Trace = Arc::default();
    let mut sched = ConcurrentScheduler::new(0);
    let (owner, bridge, owner_id, bridge_id) = {
        let _guard = sched.get_main_guard();
        let owner = create_actor(
            "Owner",
            Owner {
                trace: trace.clone(),
            },
        );
        let owner_id = owner.get();
        let bridge = Scheduler::instance().create_bridge_actor(
            "Bridge",
            Bridge {
                trace: trace.clone(),
                owner_id: owner_id.clone().erase(),
            },
            &owner_id.clone().erase(),
        );
        let bridge_id = bridge.get();
        (owner, bridge, owner_id.erase(), bridge_id)
    };
    sched.start();
    {
        let _guard = sched.get_main_guard();
        let _ = sched.run_main_no_guard(0.01); // the Start events (start_up)
    }
    Handles {
        sched,
        _owner: owner,
        _bridge: bridge,
        owner_id,
        bridge_id,
        trace,
    }
}

#[test]
fn owner_to_idle_bridge_send_takes_the_mailbox() {
    let mut h = setup();
    {
        let _guard = h.sched.get_main_guard();
        send_closure(
            &h.owner_id,
            OwnerMessage::ProbeBridge {
                bridge_id: h.bridge_id.clone().erase(),
            },
        );
        // The bridge was idle, but the send came from the owner's dispatch
        // frame: the guard queued it (no inline 'b' between 'a' and 'z').
        assert_eq!(h.trace.lock().unwrap().as_str(), "az");
        let _ = h.sched.run_main_no_guard(0.01);
        assert_eq!(h.trace.lock().unwrap().as_str(), "azb");
    }
    h.sched.finish();
}

#[test]
fn bridge_to_owner_send_takes_the_mailbox() {
    let mut h = setup();
    {
        let _guard = h.sched.get_main_guard();
        // The owner is idle while the bridge slice runs — without the
        // guard the sends would inline between the marks (`oboB`); with
        // it, both queue and the owner's slice runs after the bridge's.
        Scheduler::instance().send_typed_later(&h.bridge_id, |bridge: &mut Bridge| {
            send_closure(&bridge.owner_id, OwnerMessage::FromBridge);
            mark(&bridge.trace, 'b');
            send_closure(&bridge.owner_id, OwnerMessage::FromBridge);
            mark(&bridge.trace, 'B');
        });
        assert_eq!(h.trace.lock().unwrap().as_str(), "");
        let _ = h.sched.run_main_no_guard(0.01);
        assert_eq!(h.trace.lock().unwrap().as_str(), "bBoo");
    }
    h.sched.finish();
}

#[test]
fn run_nested_swaps_the_context_and_runs_inline() {
    let mut h = setup();
    {
        let _guard = h.sched.get_main_guard();
        send_closure(
            &h.owner_id,
            OwnerNestedMessage::Run {
                bridge_id: h.bridge_id.clone().erase(),
            },
        );
        // Nested = inline: 'n' landed between 'a' and 'z', same stack; the
        // context asserts ran inside the closure.
        assert_eq!(h.trace.lock().unwrap().as_str(), "anz");
    }
    h.sched.finish();
}

#[test]
fn send_typed_later_delivers_in_a_bridge_slice() {
    let mut h = setup();
    {
        let _guard = h.sched.get_main_guard();
        assert!(Scheduler::instance().can_run_inline(&h.bridge_id));
        Scheduler::instance().send_typed_later(&h.bridge_id, |bridge: &mut Bridge| {
            mark(&bridge.trace, 't');
        });
        // Mailboxed: nothing ran inline, and the idle predicate flipped.
        assert_eq!(h.trace.lock().unwrap().as_str(), "");
        assert!(!Scheduler::instance().can_run_inline(&h.bridge_id));
        let _ = h.sched.run_main_no_guard(0.01);
        assert_eq!(h.trace.lock().unwrap().as_str(), "t");
    }
    h.sched.finish();
}

#[test]
fn bridge_receives_its_start_event() {
    let trace: Trace = Arc::default();
    let mut sched = ConcurrentScheduler::new(0);
    {
        let _guard = sched.get_main_guard();
        struct StartOwner;
        impl Actor for StartOwner {}
        let owner = create_actor("StartOwner", StartOwner);
        struct StartBridge {
            trace: Trace,
        }
        impl Actor for StartBridge {
            fn start_up(&mut self) {
                mark(&self.trace, 's');
            }
        }
        let _bridge = Scheduler::instance().create_bridge_actor(
            "StartBridge",
            StartBridge {
                trace: trace.clone(),
            },
            &owner.get().erase(),
        );
    }
    sched.start();
    {
        let _guard = sched.get_main_guard();
        let _ = sched.run_main_no_guard(0.01);
    }
    assert_eq!(trace.lock().unwrap().as_str(), "s");
    sched.finish();
}
