//! Integration tests for the `BufferedUdp` port (`td/utils/BufferedUdp.h`).

use std::net::SocketAddr;
use std::time::Duration;

use tdnet::udp::{BufferedUdp, UdpMessage, is_critical_read_error};
use tdutils::buffer::BufferSlice;
use tdutils::status::Status;

fn datagram(address: SocketAddr, payload: &[u8]) -> UdpMessage {
    UdpMessage::new(address, BufferSlice::from_slice_bytes(payload))
}

async fn bind_local() -> BufferedUdp {
    BufferedUdp::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
}

#[test]
fn critical_read_errors_are_only_oom() {
    // ENOMEM and ENOBUFS are critical; everything else (including plain
    // status errors) is reported as a message instead.
    assert!(is_critical_read_error(&Status::posix_error(
        12,
        "out of memory"
    )));
    assert!(is_critical_read_error(&Status::posix_error(
        105,
        "no buffer space"
    )));
    assert!(!is_critical_read_error(&Status::posix_error(
        111,
        "connection refused"
    )));
    assert!(!is_critical_read_error(&Status::error_message("anything")));
}

#[tokio::test]
async fn round_trip_through_both_queues() {
    let mut a = bind_local().await;
    let mut b = bind_local().await;
    let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());

    a.send(datagram(b_addr, b"PING"));
    a.flush_send().await.unwrap();

    let received = b.receive().await.unwrap();
    assert_eq!(received.address, Some(a_addr));
    assert_eq!(received.data.as_slice(), b"PING");
    assert!(received.error.is_none());

    b.send(datagram(a_addr, b"PONG"));
    b.flush_send().await.unwrap();
    let reply = a.receive().await.unwrap();
    assert_eq!(reply.data.as_slice(), b"PONG");
}

#[tokio::test]
async fn queued_datagrams_are_delivered_in_order() {
    let mut a = bind_local().await;
    let mut b = bind_local().await;
    let b_addr = b.local_addr().unwrap();

    for i in 0..8u8 {
        a.send(datagram(b_addr, &[i; 16]));
    }
    a.flush_send().await.unwrap();

    let mut received = Vec::new();
    for i in 0..8u8 {
        let message = b.receive().await.unwrap();
        assert_eq!(message.data.as_slice()[0], i, "datagrams stay in order");
        received.push(message.data.as_slice()[0]);
    }
    assert_eq!(received, Vec::from_iter(0..8u8));
}

#[tokio::test]
async fn receive_awaits_a_message_sent_later() {
    let mut a = bind_local().await;
    let mut b = bind_local().await;
    let b_addr = b.local_addr().unwrap();

    let receiver = tokio::spawn(async move {
        let received = b.receive().await.unwrap();
        (received.data.as_slice().to_vec(), received.address)
    });
    // The receiver is parked in `receive()` when this send happens.
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.send(datagram(b_addr, b"later"));
    a.flush_send().await.unwrap();

    let (data, address) = receiver.await.unwrap();
    assert_eq!(data, b"later");
    assert_eq!(address, Some(a.local_addr().unwrap()));
}

#[tokio::test]
async fn oversized_datagrams_are_truncated_to_max_packet_size() {
    let mut a = bind_local().await;
    let mut b = bind_local().await;

    a.send(datagram(b.local_addr().unwrap(), &[7u8; 4096]));
    a.flush_send().await.unwrap();

    let received = b.receive().await.unwrap();
    assert_eq!(received.data.size(), 2048);
    assert!(received.data.as_slice().iter().all(|byte| *byte == 7));
}

#[tokio::test]
async fn flush_send_fails_on_addressless_message() {
    let mut a = bind_local().await;
    a.send(UdpMessage::error(Status::error_message("transport hiccup")));
    assert!(a.flush_send().await.is_error());
    // The offending message stays queued, like an unsent datagram in C++.
    assert_eq!(a.output().len(), 1);
}

#[tokio::test]
async fn icmp_unreachable_arrives_as_error_message() {
    // Linux reports transport errors only to a socket *connected* to the
    // dead peer; unconnected sockets never see them without IP_RECVERR
    // (which the C++ code never sets either).
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let closed_port = {
        let probe = bind_local().await;
        probe.local_addr().unwrap()
    };
    socket.connect(closed_port).await.unwrap();
    let mut sender = BufferedUdp::new(socket);

    sender.send(datagram(closed_port, b"nobody is home"));
    sender.flush_send().await.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(2), sender.receive())
        .await
        .expect("ICMP error within 2s")
        .unwrap();
    assert!(received.address.is_none());
    assert!(received.data.empty());
    let error = received.error.expect("error message carries a Status");
    assert_eq!(error.code(), 111); // ECONNREFUSED on Linux
}
