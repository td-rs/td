// S60 handshake oracle: the C++ td client — runs the full auth-key
// generation against the Rust mock DC, driven by td's own actor
// machinery (ConcurrentScheduler + HandshakeActor), exactly like
// td/test/mtproto.cpp drives its handshake test. The mock validates
// every message; this tool reports success (same output shape as the
// Rust client).
//
#include "td/mtproto/Handshake.h"
#include "td/mtproto/HandshakeActor.h"
#include "td/mtproto/RawConnection.h"
#include "td/mtproto/RSA.h"
#include "td/mtproto/TransportType.h"

#include "td/actor/ConcurrentScheduler.h"
#include "td/actor/PromiseFuture.h"

#include "td/utils/BufferedFd.h"
#include "td/utils/common.h"
#include "td/utils/port/IPAddress.h"
#include "td/utils/port/SocketFd.h"
#include "td/utils/Status.h"

#include <cstdio>
#include <cstdlib>
#include <string>

class RsaInterface final : public td::mtproto::PublicRsaKeyInterface {
 public:
  explicit RsaInterface(td::mtproto::RSA rsa) : rsa_(std::move(rsa)) {
  }
  td::Result<RsaKey> get_rsa_key(const td::vector<td::int64> &fingerprints) final {
    auto fingerprint = rsa_.get_fingerprint();
    for (auto &it : fingerprints) {
      if (it == fingerprint) {
        return RsaKey{rsa_.clone(), fingerprint};
      }
    }
    return td::Status::Error("No matching RSA key");
  }
  void drop_keys() final {
  }

 private:
  td::mtproto::RSA rsa_;
};

class Context final : public td::mtproto::AuthKeyHandshakeContext {
 public:
  explicit Context(td::mtproto::RSA rsa) : rsa_(std::move(rsa)) {
  }
  td::mtproto::DhCallback *get_dh_callback() final {
    return nullptr;
  }
  td::mtproto::PublicRsaKeyInterface *get_public_rsa_key_interface() final {
    return &rsa_;
  }

 private:
  RsaInterface rsa_;
};

static std::string read_file(const char *path) {
  FILE *f = std::fopen(path, "rb");
  CHECK(f != nullptr);
  std::fseek(f, 0, SEEK_END);
  long size = std::ftell(f);
  std::fseek(f, 0, SEEK_SET);
  std::string data(static_cast<size_t>(size), '\0');
  auto read = std::fread(data.data(), 1, data.size(), f);
  CHECK(read == data.size());
  std::fclose(f);
  return data;
}

struct ClientResult {
  bool done = false;
  bool ok = false;
  td::uint64 key_id = 0;
  td::uint64 server_salt = 0;
};

class ClientActor final : public td::Actor {
 public:
  ClientActor(int port, td::mtproto::RSA rsa, double timeout, ClientResult *result)
      : port_(port), rsa_(std::move(rsa)), timeout_(timeout), result_(result) {
  }

 private:
  int port_;
  td::mtproto::RSA rsa_;
  double timeout_;
  ClientResult *result_;

  bool wait_for_raw_connection_ = false;
  td::unique_ptr<td::mtproto::RawConnection> raw_connection_;
  bool wait_for_handshake_ = false;
  td::unique_ptr<td::mtproto::AuthKeyHandshake> handshake_;

  void finish(bool ok) {
    result_->done = true;
    result_->ok = ok;
  }

  void tear_down() final {
    if (raw_connection_) {
      raw_connection_->close();
    }
  }

  void loop() final {
    if (!wait_for_raw_connection_ && !raw_connection_) {
      td::IPAddress ip;
      auto status = ip.init_ipv4_port("127.0.0.1", port_);
      if (status.is_error()) {
        finish(false);
        return stop();
      }
      auto r_socket = td::SocketFd::open(ip);
      if (r_socket.is_error()) {
        finish(false);
        return stop();
      }
      raw_connection_ = td::mtproto::RawConnection::create(
          ip, td::BufferedFd<td::SocketFd>(r_socket.move_as_ok()),
          td::mtproto::TransportType{td::mtproto::TransportType::Tcp, 2, td::mtproto::ProxySecret()}, nullptr);
    }
    if (!wait_for_handshake_ && !handshake_) {
      handshake_ = td::make_unique<td::mtproto::AuthKeyHandshake>(2, 0);
    }
    if (raw_connection_ && handshake_) {
      td::create_actor<td::mtproto::HandshakeActor>(
          "HandshakeActor", std::move(handshake_), std::move(raw_connection_),
          td::make_unique<Context>(rsa_), timeout_,
          td::PromiseCreator::lambda(
              [actor_id = actor_id(this)](td::Result<td::unique_ptr<td::mtproto::RawConnection>> raw_connection) {
                td::send_closure(actor_id, &ClientActor::on_connection, std::move(raw_connection), 1);
              }),
          td::PromiseCreator::lambda(
              [actor_id = actor_id(this)](td::Result<td::unique_ptr<td::mtproto::AuthKeyHandshake>> handshake) {
                td::send_closure(actor_id, &ClientActor::on_handshake, std::move(handshake), 1);
              }))
          .release();
      wait_for_raw_connection_ = true;
      wait_for_handshake_ = true;
    }
  }

  void on_connection(td::Result<td::unique_ptr<td::mtproto::RawConnection>> r_raw_connection, bool /*dummy*/) {
    CHECK(wait_for_raw_connection_);
    wait_for_raw_connection_ = false;
    if (r_raw_connection.is_ok()) {
      raw_connection_ = r_raw_connection.move_as_ok();
    }
    check_status();
  }

  void on_handshake(td::Result<td::unique_ptr<td::mtproto::AuthKeyHandshake>> r_handshake, bool /*dummy*/) {
    CHECK(wait_for_handshake_);
    wait_for_handshake_ = false;
    if (r_handshake.is_ok()) {
      handshake_ = r_handshake.move_as_ok();
    }
    check_status();
  }

  void check_status() {
    if (wait_for_raw_connection_ || wait_for_handshake_) {
      return;
    }
    if (handshake_ && handshake_->is_ready_for_finish()) {
      result_->key_id = handshake_->get_auth_key().id();
      result_->server_salt = handshake_->get_server_salt();
      finish(true);
      return stop();
    }
    finish(false);
    return stop();
  }
};

int main(int argc, char **argv) {
  CHECK(argc == 3);
  int port = std::atoi(argv[1]);
  auto pem = read_file(argv[2]);

  auto r_rsa = td::mtproto::RSA::from_pem_public_key(pem);
  if (r_rsa.is_error()) {
    std::printf("rsa:error:%s\n", r_rsa.error().message().c_str());
    return 1;
  }

  ClientResult result;
  td::ConcurrentScheduler sched(0, 0);
  sched.start();
  {
    auto guard = sched.get_main_guard();
    td::create_actor<ClientActor>("ClientActor", port, r_rsa.move_as_ok(), 30.0, &result).release();
  }
  while (!result.done) {
    sched.run_main(0.5);
  }
  sched.finish();

  if (result.ok) {
    std::printf("handshake:ok:key_id=%llu\n", static_cast<unsigned long long>(result.key_id));
    std::printf("handshake:ok:salt=%llu\n", static_cast<unsigned long long>(result.server_salt));
    return 0;
  }
  std::printf("handshake:error:timeout\n");
  return 1;
}
