//! The S52 "actor tests": the dispatcher's routing, retry, delay and
//! completion semantics exercised through test session/Td sinks and a
//! [`NetActor`] implementation, mirroring the C++ dispatcher's
//! `dispatch()` decision table.

use std::sync::Arc;

use parking_lot::Mutex;

use tdnet::net_query::{
    AuthFlag, CANCELED_ERROR_CODE, DcId, GzipFlag, NetActor, NetQuery, NetQueryCallback,
    NetQueryDispatcher, NetQueryType, RESEND_ERROR_CODE, ResendFn, SessionSink, TdSink,
};
use tdutils::status::Status;

/// One routed query: `(dest dc, query type, id, envelope dc)`.
#[derive(Debug, PartialEq)]
struct Sent {
    dest_dc: DcId,
    query_type: NetQueryType,
    id: u64,
    envelope_dc: DcId,
    dispatch_ttl: u32,
}

#[derive(Default)]
struct RecordingSession {
    sent: Mutex<Vec<Sent>>,
}

struct SessionHandle(Arc<RecordingSession>);

impl SessionSink for SessionHandle {
    fn send(&mut self, dc_id: DcId, query_type: NetQueryType, query: NetQuery) {
        self.0.sent.lock().push(Sent {
            dest_dc: dc_id,
            query_type,
            id: query.id(),
            envelope_dc: query.dc_id(),
            dispatch_ttl: query.dispatch_ttl(),
        });
    }
}

#[derive(Default)]
struct RecordingTd {
    results: Mutex<Vec<NetQuery>>,
}

impl TdSink for RecordingTd {
    fn on_result(&mut self, query: NetQuery) {
        self.results.lock().push(query);
    }
}

struct TdHandle(Arc<RecordingTd>);

impl TdSink for TdHandle {
    fn on_result(&mut self, query: NetQuery) {
        self.0.results.lock().push(query);
    }
}

fn make_query(dc_id: DcId) -> NetQuery {
    NetQuery::new(
        1,
        dc_id,
        NetQueryType::Common,
        AuthFlag::Off,
        GzipFlag::Off,
        -100_000, // a dummy tl constructor
        vec![1, 2, 3],
        10,
        60,
    )
}

fn make_dispatcher() -> (Arc<RecordingSession>, Arc<RecordingTd>, NetQueryDispatcher) {
    let session = Arc::new(RecordingSession::default());
    let td = Arc::new(RecordingTd::default());
    let dispatcher = NetQueryDispatcher::new(
        Box::new(SessionHandle(session.clone())),
        Box::new(TdHandle(td.clone())),
    );
    (session, td, dispatcher)
}

fn drain_td(td: &Arc<RecordingTd>) -> Vec<NetQuery> {
    std::mem::take(&mut *td.results.lock())
}

#[test]
fn dc_id_semantics() {
    assert!(DcId::is_valid_raw(1) && DcId::is_valid_raw(1000));
    assert!(!DcId::is_valid_raw(0) && !DcId::is_valid_raw(1001));
    assert!(DcId::main().is_main());
    assert!(DcId::main().is_valid());
    assert!(DcId::invalid().is_empty());
    assert!(DcId::empty().is_empty());
    assert!(DcId::internal(2).is_internal());
    assert!(DcId::external(2).is_external());
    assert_eq!(DcId::create(5), DcId::internal(5));
    assert_eq!(DcId::create(-7), DcId::invalid());
    assert_eq!(DcId::internal(2).get_raw_id(), 2);
}

#[test]
fn net_query_state_machine() {
    let mut query = make_query(DcId::main());
    assert!(!query.is_ready());
    query.set_ok(vec![9, 9]);
    assert!(query.is_ok() && query.is_ready());
    assert_eq!(query.ok(), &[9, 9]);
    assert_eq!(query.move_as_ok(), vec![9, 9]);
    assert!(!query.is_ok());

    query.set_error_resend();
    assert!(query.is_error());
    assert_eq!(query.error().code(), RESEND_ERROR_CODE);
    let error = query.move_as_error();
    assert_eq!(error.code(), RESEND_ERROR_CODE);
    assert!(!query.is_error());

    query.set_error_canceled();
    assert_eq!(query.error().code(), CANCELED_ERROR_CODE);
    query.set_error_resend_invoke_after();
    assert_eq!(query.error().code(), 204);
}

#[test]
fn routes_by_type_and_resolves_main_dc() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);

    for (query_type, tl) in [
        (NetQueryType::Common, 1),
        (NetQueryType::Upload, 2),
        (NetQueryType::Download, 3),
        (NetQueryType::DownloadSmall, 4),
    ] {
        let query = NetQuery::new(
            tl as u64,
            DcId::main(),
            query_type,
            AuthFlag::On,
            GzipFlag::Off,
            tl,
            Vec::new(),
            10,
            60,
        );
        dispatcher.dispatch(query);
    }
    assert!(drain_td(&td).is_empty());
    let sent = session.sent.lock();
    assert_eq!(sent.len(), 4);
    for (sent, expected_type) in sent.iter().zip([
        NetQueryType::Common,
        NetQueryType::Upload,
        NetQueryType::Download,
        NetQueryType::DownloadSmall,
    ]) {
        assert_eq!(sent.dest_dc, DcId::internal(1), "main dc resolution");
        assert_eq!(sent.query_type, expected_type);
    }
}

#[test]
fn explicit_dc_routed_without_main_resolution() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(4);
    dispatcher.dispatch(make_query(DcId::internal(4)));
    assert!(drain_td(&td).is_empty());
    assert_eq!(session.sent.lock()[0].dest_dc, DcId::internal(4));
}

#[test]
fn resend_error_redispatches() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error_resend();
    dispatcher.dispatch(query);
    assert!(drain_td(&td).is_empty());
    let sent = session.sent.lock();
    assert_eq!(sent.len(), 1);
    // `resend()` keeps the envelope DC (main); the router resolves it.
    assert_eq!(sent[0].envelope_dc, DcId::main());
    assert_eq!(sent[0].dest_dc, DcId::internal(1));
}

#[test]
fn frozen_method_becomes_406() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(420, "FROZEN_METHOD_INVALID"));
    dispatcher.dispatch(query);
    let results = drain_td(&td);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].error().code(), 406);
    assert!(session.sent.lock().is_empty());
}

#[test]
fn flood_wait_parks_until_deadline() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(420, "FLOOD_WAIT_5"));
    dispatcher.dispatch(query);
    assert_eq!(dispatcher.delayed_count(), 1);
    assert!(session.sent.lock().is_empty());

    // Not due yet.
    let now = tdutils::time::now();
    assert_eq!(dispatcher.retry_ready_delayed(now + 1.0), 0);
    assert!(session.sent.lock().is_empty());

    // Due: re-dispatched as a fresh query.
    assert_eq!(dispatcher.retry_ready_delayed(now + 6.0), 1);
    assert_eq!(dispatcher.delayed_count(), 0);
    assert_eq!(session.sent.lock().len(), 1);
    assert!(drain_td(&td).is_empty());
}

#[test]
fn worker_busy_delays_one_second() {
    let (session, _td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(500, "WORKER_BUSY_TOO_LONG_RETRY"));
    dispatcher.dispatch(query);
    assert_eq!(dispatcher.delayed_count(), 1);
    let now = tdutils::time::now();
    assert_eq!(dispatcher.retry_ready_delayed(now + 0.5), 0);
    assert_eq!(dispatcher.retry_ready_delayed(now + 1.5), 1);
    assert_eq!(session.sent.lock().len(), 1);
}

#[test]
fn unsupported_flood_parameter_becomes_400() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(420, "FLOOD_WAIT_10_20"));
    dispatcher.dispatch(query);
    assert_eq!(dispatcher.delayed_count(), 0);
    let results = drain_td(&td);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].error().code(), 400);
    assert!(session.sent.lock().is_empty());
}

#[test]
fn file_migrate_resends_to_new_dc() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(5);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(303, "FILE_MIGRATE_5"));
    dispatcher.dispatch(query);
    assert!(drain_td(&td).is_empty());
    let sent = session.sent.lock();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].dest_dc, DcId::internal(5));
}

#[test]
fn phone_migrate_moves_main_dc() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    dispatcher.mark_dc_inited(3);
    assert_eq!(dispatcher.get_main_dc_id(), DcId::internal(1));
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(303, "PHONE_MIGRATE_3"));
    dispatcher.dispatch(query);
    assert_eq!(dispatcher.get_main_dc_id(), DcId::internal(3));
    assert!(drain_td(&td).is_empty());
    let sent = session.sent.lock();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].dest_dc, DcId::internal(3));
}

#[test]
fn stop_completes_with_request_aborted() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    dispatcher.stop();
    dispatcher.dispatch(make_query(DcId::main()));
    assert!(session.sent.lock().is_empty());
    let results = drain_td(&td);
    assert_eq!(results.len(), 1);
    let error = results[0].error();
    assert_eq!(error.code(), 500);
    assert_eq!(error.message(), "Request aborted");
}

#[test]
fn callback_takes_priority_over_td_sink() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let collector = Arc::new(Mutex::new(Vec::new()));
    struct Forwarder {
        sink: Arc<Mutex<Vec<NetQuery>>>,
    }
    impl NetQueryCallback for Forwarder {
        fn on_result(&mut self, query: NetQuery) {
            self.sink.lock().push(query);
        }
    }
    // A dc-9 query completes immediately ("No such dc") — with the
    // callback set it must land in the callback, not the Td sink.
    dispatcher.dispatch_with_callback(
        make_query(DcId::internal(9)),
        Box::new(Forwarder {
            sink: collector.clone(),
        }),
    );
    assert!(drain_td(&td).is_empty());
    assert_eq!(collector.lock().len(), 1);
    assert!(session.sent.lock().is_empty());
}

#[test]
fn dispatch_ttl_exhaustion() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.mark_dc_inited(1);
    let mut query = make_query(DcId::main());
    query.set_error(Status::error(500, "boom"));
    // The delayer would normally park it; stop the dispatcher first so
    // the stop check fires after the delayer classification.
    dispatcher.stop();
    dispatcher.dispatch(query);
    let results = drain_td(&td);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].error().code(), 500);
    assert!(session.sent.lock().is_empty());

    // A zero-ttl query that is not ready errors with DispatchTtlError
    // (the C++ order: the ttl check precedes the DC-init gate).
    let (_session2, td2, dispatcher2) = make_dispatcher();
    let query = NetQuery::new(
        2,
        DcId::internal(9),
        NetQueryType::Common,
        AuthFlag::Off,
        GzipFlag::Off,
        -100_000,
        Vec::new(),
        0,
        60,
    );
    dispatcher2.dispatch(query);
    let results = drain_td(&td2);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].error().message(), "DispatchTtlError");
}

#[test]
fn unknown_dc_errors() {
    let (session, td, dispatcher) = make_dispatcher();
    dispatcher.dispatch(make_query(DcId::internal(9)));
    let results = drain_td(&td);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].error().message(), "No such dc DcId{9}");
    assert!(session.sent.lock().is_empty());

    // Marking it inited lets the query through.
    dispatcher.mark_dc_inited(9);
    dispatcher.dispatch(make_query(DcId::internal(9)));
    assert!(drain_td(&td).is_empty());
    assert_eq!(session.sent.lock().len(), 1);
}

/// A minimal `NetActor`: the C++ base routes results to
/// `on_result_packet`/`on_error` and calls `on_result_finish`.
#[derive(Default)]
struct EchoActor {
    packets: Mutex<Vec<Vec<u8>>>,
    errors: Mutex<Vec<Status>>,
    finished: Mutex<usize>,
}

impl NetActor for EchoActor {
    fn on_result_packet(&mut self, packet: Vec<u8>) {
        self.packets.lock().push(packet);
    }

    fn on_error(&mut self, error: Status) {
        self.errors.lock().push(error);
    }

    fn on_result_finish(&mut self) {
        *self.finished.lock() += 1;
    }
}

#[test]
fn net_actor_routes_results() {
    let mut actor = EchoActor::default();

    let mut ok = make_query(DcId::main());
    ok.set_ok(vec![7, 7, 7]);
    NetActor::on_result(&mut actor, ok);

    let mut error = make_query(DcId::main());
    error.set_error(Status::error(420, "FLOOD_WAIT_1"));
    NetActor::on_result(&mut actor, error);

    assert_eq!(actor.packets.lock().as_slice(), &[vec![7, 7, 7]]);
    assert_eq!(actor.errors.lock().len(), 1);
    assert_eq!(*actor.finished.lock(), 1);
}

#[test]
fn on_result_resendable_defaults_to_on_result() {
    #[derive(Default)]
    struct Counting {
        count: Mutex<usize>,
    }
    impl NetQueryCallback for Counting {
        fn on_result(&mut self, _query: NetQuery) {
            *self.count.lock() += 1;
        }
    }

    let mut callback = Counting::default();
    let mut query = make_query(DcId::main());
    query.set_ok(vec![1]);
    let resend: ResendFn = &mut |_query: NetQuery| panic!("default must not resend");
    callback.on_result_resendable(query, resend);
    assert_eq!(*callback.count.lock(), 1);
}
