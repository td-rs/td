//! Port of `td/mtproto/Handshake.{h,cpp}` (S60): the auth-key generation
//! state machine — `req_pq_multi` → `req_DH_params` → `set_client_DH_params`
//! with the encrypted-data blob construction, temp-key KDF and DH checks.

use crate::auth_key::AuthKey;
use crate::dh_callback::DhCallback;
use crate::dh_handshake::DhHandshake;
use crate::kdf::tmp_kdf;
use crate::mtproto_api::{self, Int128, Int256};
use crate::rsa::PublicRsaKeyInterface;
use tdutils::crypto::{aes_ige_encrypt, sha1, sha256};
use tdutils::random;
use tdutils::status::{Result, Status};
use tdutils::time;

/// `td::mtproto::AuthKeyHandshake::State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeState {
    Start,
    ResPQ,
    ServerDHParams,
    DHGenResponse,
    Finish,
}

/// `td::mtproto::AuthKeyHandshake::Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Main,
    Temp,
}

/// `td::mtproto::AuthKeyHandshake::Callback`.
pub trait AuthKeyHandshakeCallback {
    fn send_no_crypto(&mut self, data: &[u8]);
}

/// `td::mtproto::AuthKeyHandshakeContext`.
pub trait AuthKeyHandshakeContext {
    fn get_dh_callback(&self) -> &dyn DhCallback;
    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface;
}

// ---- Int128 / Int256 byte conversions (the generated model stores the
// fixed arrays as i32 words; the crypto layer works in bytes) ----

pub fn int128_to_bytes(v: &Int128) -> [u8; 16] {
    let mut res = [0u8; 16];
    for (i, word) in v.words.iter().enumerate() {
        res[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    res
}

pub fn int128_from_bytes(bytes: &[u8; 16]) -> Int128 {
    Int128 {
        words: std::array::from_fn(|i| {
            i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        }),
    }
}

pub fn int256_to_bytes(v: &Int256) -> [u8; 32] {
    let mut res = [0u8; 32];
    for (i, word) in v.words.iter().enumerate() {
        res[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    res
}

pub fn int256_from_bytes(bytes: &[u8; 32]) -> Int256 {
    Int256 {
        words: std::array::from_fn(|i| {
            i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        }),
    }
}

/// `td::mtproto::AuthKeyHandshake`.
pub struct AuthKeyHandshake {
    state: HandshakeState,
    mode: Mode,
    dc_id: i32,
    expires_in: i32,
    expires_at: f64,
    start_time: f64,
    timeout_in: f64,
    auth_key: AuthKey,
    server_time_diff: f64,
    server_salt: u64,
    nonce: [u8; 16],
    server_nonce: [u8; 16],
    new_nonce: [u8; 32],
    last_query: Vec<u8>,
}

impl AuthKeyHandshake {
    /// `AuthKeyHandshake(dc_id, expires_in)`.
    pub fn new(dc_id: i32, expires_in: i32) -> Self {
        Self {
            state: HandshakeState::Start,
            mode: if expires_in == 0 { Mode::Main } else { Mode::Temp },
            dc_id,
            expires_in,
            expires_at: 0.0,
            start_time: time::now(),
            timeout_in: 1e9,
            auth_key: AuthKey::default(),
            server_time_diff: 0.0,
            server_salt: 0,
            nonce: [0u8; 16],
            server_nonce: [0u8; 16],
            new_nonce: [0u8; 32],
            last_query: Vec::new(),
        }
    }

    pub fn set_timeout_in(&mut self, timeout_in: f64) {
        self.start_time = time::now();
        self.timeout_in = timeout_in;
    }

    pub fn is_ready_for_finish(&self) -> bool {
        self.state == HandshakeState::Finish
    }

    pub fn on_finish(&mut self) {
        self.clear();
    }

    pub fn clear(&mut self) {
        self.last_query.clear();
        self.state = HandshakeState::Start;
        self.start_time = time::now();
        self.timeout_in = 1e9;
    }

    pub fn get_auth_key(&self) -> &AuthKey {
        &self.auth_key
    }

    pub fn release_auth_key(&mut self) -> AuthKey {
        std::mem::take(&mut self.auth_key)
    }

    pub fn get_server_time_diff(&self) -> f64 {
        self.server_time_diff
    }

    pub fn get_server_salt(&self) -> u64 {
        self.server_salt
    }

    /// `AuthKeyHandshake::resume`.
    pub fn resume(&mut self, callback: &mut dyn AuthKeyHandshakeCallback) -> Status {
        if self.state == HandshakeState::Start {
            return self.on_start(callback);
        }
        if self.state == HandshakeState::Finish {
            tdutils::log_error!("State is Finish during resume. UNREACHABLE");
            self.clear();
            return Status::ok();
        }
        if self.last_query.is_empty() {
            tdutils::log_error!("Last query empty! UNREACHABLE {:?}", self.state);
            self.clear();
            return Status::ok();
        }
        tdutils::log_info!("Resume handshake");
        Self::do_send(callback, &self.last_query);
        Status::ok()
    }

    /// `AuthKeyHandshake::on_message`.
    pub fn on_message(
        &mut self,
        message: &[u8],
        callback: &mut dyn AuthKeyHandshakeCallback,
        context: &mut dyn AuthKeyHandshakeContext,
    ) -> Status {
        let status = match self.state {
            HandshakeState::ResPQ => self.on_res_pq(
                message,
                callback,
                context.get_public_rsa_key_interface(),
            ),
            HandshakeState::ServerDHParams => {
                self.on_server_dh_params(message, callback, context.get_dh_callback())
            }
            HandshakeState::DHGenResponse => self.on_dh_gen_response(message, callback),
            _ => unreachable!(),
        };
        if status.is_error() {
            tdutils::log_warning!(
                "Failed to process handshake response in state {:?}: {}",
                self.state,
                status.message()
            );
            self.clear();
        }
        status
    }

    fn send(&mut self, callback: &mut dyn AuthKeyHandshakeCallback, data: Vec<u8>) {
        self.last_query = data.clone();
        Self::do_send(callback, &data);
    }

    fn do_send(callback: &mut dyn AuthKeyHandshakeCallback, data: &[u8]) {
        callback.send_no_crypto(data);
    }

    /// `AuthKeyHandshake::on_start`.
    fn on_start(&mut self, callback: &mut dyn AuthKeyHandshakeCallback) -> Status {
        if self.state != HandshakeState::Start {
            self.clear();
            return Status::error_message(&format!(
                "on_start called after start [state = {:?}]",
                self.state
            ));
        }
        random::secure_bytes(&mut self.nonce);
        let mut data = Vec::new();
        mtproto_api::req_pq_multi {
            nonce: int128_from_bytes(&self.nonce),
        }
        .store_boxed(&mut data);
        self.send(callback, data);
        self.state = HandshakeState::ResPQ;
        Status::ok()
    }

    /// `AuthKeyHandshake::on_res_pq`.
    fn on_res_pq(
        &mut self,
        message: &[u8],
        callback: &mut dyn AuthKeyHandshakeCallback,
        public_rsa_key: &mut dyn PublicRsaKeyInterface,
    ) -> Status {
        if time::now() >= self.start_time + self.timeout_in * 0.6 {
            return Status::error_message("Handshake ResPQ timeout expired");
        }

        let res_pq = match mtproto_api::ResPQ::parse(message) {
            Ok(res) => res,
            Err(error) => return Status::error(500, error),
        };
        if int128_to_bytes(&res_pq.nonce) != self.nonce {
            return Status::error_message("Nonce mismatch");
        }

        self.server_nonce = int128_to_bytes(&res_pq.server_nonce);

        let rsa_key = match public_rsa_key.get_rsa_key(&res_pq.server_public_key_fingerprints) {
            Result::Ok(key) => key,
            Result::Err(status) => {
                public_rsa_key.drop_keys();
                return status;
            }
        };

        let (p, q) = match tdutils::crypto::pq_factorize(&res_pq.pq) {
            Some(factors) => factors,
            None => return Status::error_message("Failed to factorize"),
        };

        random::secure_bytes(&mut self.new_nonce);

        let mut data = Vec::new();
        match self.mode {
            Mode::Main => mtproto_api::p_q_inner_data_dc {
                pq: res_pq.pq.clone(),
                p: p.clone(),
                q: q.clone(),
                nonce: int128_from_bytes(&self.nonce),
                server_nonce: int128_from_bytes(&self.server_nonce),
                new_nonce: int256_from_bytes(&self.new_nonce),
                dc: self.dc_id,
            }
            .store_boxed(&mut data),
            Mode::Temp => {
                mtproto_api::p_q_inner_data_temp_dc {
                    pq: res_pq.pq.clone(),
                    p: p.clone(),
                    q: q.clone(),
                    nonce: int128_from_bytes(&self.nonce),
                    server_nonce: int128_from_bytes(&self.server_nonce),
                    new_nonce: int256_from_bytes(&self.new_nonce),
                    dc: self.dc_id,
                    expires_in: self.expires_in,
                }
                .store_boxed(&mut data);
                self.expires_at = time::now() + self.expires_in as f64;
            }
        }

        let mut encrypted_data = vec![0u8; 256];
        let data_size = data.len();
        if data_size > 144 {
            return Status::error_message("Too big data");
        }

        data.resize(192, 0);
        random::secure_bytes(&mut data[data_size..]);

        loop {
            let mut aes_key = [0u8; 32];
            random::secure_bytes(&mut aes_key);

            let mut data_with_hash = Vec::with_capacity(data.len() + 32);
            data_with_hash.extend_from_slice(&data);
            data_with_hash.extend_from_slice(&sha256(&[aes_key.as_slice(), data.as_slice()].concat()));

            // reverse the data part (the C++ std::reverse over [begin, begin + data.size())).
            data_with_hash[..data.len()].reverse();

            let mut decrypted_data = [0u8; 256];
            let aes_iv = [0u8; 32];
            let mut aes_iv = aes_iv;
            {
                let from = data_with_hash.as_slice();
                let to = &mut decrypted_data[32..];
                aes_ige_encrypt(&aes_key, &mut aes_iv, from, to);
            }

            let hash = sha256(&decrypted_data[32..]);
            for i in 0..32 {
                decrypted_data[i] = aes_key[i] ^ hash[i];
            }

            if rsa_key.rsa.encrypt(&decrypted_data, &mut encrypted_data) {
                break;
            }
        }

        let mut query = Vec::new();
        mtproto_api::req_DH_params {
            nonce: int128_from_bytes(&self.nonce),
            server_nonce: int128_from_bytes(&self.server_nonce),
            p,
            q,
            public_key_fingerprint: rsa_key.fingerprint,
            encrypted_data,
        }
        .store_boxed(&mut query);
        self.send(callback, query);
        self.state = HandshakeState::ServerDHParams;
        Status::ok()
    }

    /// `AuthKeyHandshake::on_server_dh_params`.
    fn on_server_dh_params(
        &mut self,
        message: &[u8],
        callback: &mut dyn AuthKeyHandshakeCallback,
        dh_callback: &dyn DhCallback,
    ) -> Status {
        if time::now() >= self.start_time + self.timeout_in * 0.8 {
            return Status::error_message("Handshake DH params timeout expired");
        }

        let dh_params = match mtproto_api::Server_DH_Params::parse(message) {
            Ok(res) => res,
            Err(error) => return Status::error(500, error),
        };

        // server_DH_params_ok#d0e8075c nonce:int128 server_nonce:int128 encrypted_answer:string = Server_DH_Params;
        if int128_to_bytes(&dh_params.nonce) != self.nonce {
            return Status::error_message("Nonce mismatch");
        }
        if int128_to_bytes(&dh_params.server_nonce) != self.server_nonce {
            return Status::error_message("Server nonce mismatch");
        }
        if dh_params.encrypted_answer.len() & 15 != 0 {
            return Status::error_message("Bad padding for encrypted part");
        }

        let (tmp_aes_key, mut tmp_aes_iv) = tmp_kdf(&self.server_nonce, &self.new_nonce);
        // encrypted_answer := AES256_ige_encrypt (answer_with_hash, tmp_aes_key, tmp_aes_iv);
        let mut answer = dh_params.encrypted_answer;
        tdutils::crypto::aes_ige_decrypt_in_place(&tmp_aes_key, &mut tmp_aes_iv, &mut answer);

        // answer_with_hash := SHA1(answer) + answer + (0-15 random bytes)
        if answer.len() < 24 {
            return Status::error_message("Failed to fetch server_DH_inner_data");
        }
        let answer_sha1 = answer[..20].to_vec();
        let id = i32::from_le_bytes(answer[20..24].try_into().unwrap());
        if id != mtproto_api::Server_DH_inner_data::ID {
            return Status::error_message("Failed to fetch server_DH_inner_data");
        }
        let dh_inner_data = {
            let mut r = crate::mtproto_api::tl::Reader::new(&answer[24..]);
            match mtproto_api::Server_DH_inner_data::read_bare(&mut r) {
                Ok(res) => res,
                Err(error) => return Status::error(500, error),
            }
        };

        let pad = answer.len() - 24 - server_dh_inner_data_consumed(&answer[24..]);
        if pad >= 16 {
            return Status::error_message("Too much pad");
        }

        let dh_inner_data_size = answer.len() - pad - 20;
        let answer_real_sha1 = sha1(&answer[20..20 + dh_inner_data_size]);
        if answer_sha1 != answer_real_sha1 {
            return Status::error_message("SHA1 mismatch");
        }

        if int128_to_bytes(&dh_inner_data.nonce) != self.nonce {
            return Status::error_message("Nonce mismatch");
        }
        if int128_to_bytes(&dh_inner_data.server_nonce) != self.server_nonce {
            return Status::error_message("Server nonce mismatch");
        }

        self.server_time_diff = dh_inner_data.server_time as f64 - time::now();

        let mut handshake = DhHandshake::new();
        handshake.set_config(dh_inner_data.g, &dh_inner_data.dh_prime);
        handshake.set_g_a(&dh_inner_data.g_a);
        let status = handshake.run_checks(false, dh_callback);
        if status.is_error() {
            return status;
        }
        let g_b = handshake.get_g_b();
        let (auth_key_id, auth_key) = handshake.gen_key();

        let mut data = Vec::new();
        mtproto_api::Client_DH_Inner_Data {
            nonce: int128_from_bytes(&self.nonce),
            server_nonce: int128_from_bytes(&self.server_nonce),
            retry_id: 0,
            g_b,
        }
        .store_boxed(&mut data);
        let encrypted_data_size = 20 + data.len();
        let encrypted_data_size_with_pad = (encrypted_data_size + 15) & !15;
        let mut encrypted_data = vec![0u8; encrypted_data_size_with_pad];
        encrypted_data[..20].copy_from_slice(&sha1(&data));
        encrypted_data[20..20 + data.len()].copy_from_slice(&data);
        random::secure_bytes(&mut encrypted_data[encrypted_data_size..]);
        let (tmp_aes_key, mut tmp_aes_iv) = tmp_kdf(&self.server_nonce, &self.new_nonce);
        tdutils::crypto::aes_ige_encrypt_in_place(&tmp_aes_key, &mut tmp_aes_iv, &mut encrypted_data);

        let mut query = Vec::new();
        mtproto_api::set_client_DH_params {
            nonce: int128_from_bytes(&self.nonce),
            server_nonce: int128_from_bytes(&self.server_nonce),
            encrypted_data,
        }
        .store_boxed(&mut query);
        self.send(callback, query);

        self.auth_key = AuthKey::new(auth_key_id as u64, auth_key);
        if self.mode == Mode::Temp {
            self.auth_key.set_expires_at(self.expires_at);
        }
        self.auth_key
            .set_created_at(dh_inner_data.server_time as f64);

        self.server_salt = i64::from_le_bytes(self.new_nonce[..8].try_into().unwrap()) as u64
            ^ i64::from_le_bytes(self.server_nonce[..8].try_into().unwrap()) as u64;

        self.state = HandshakeState::DHGenResponse;
        Status::ok()
    }

    /// `AuthKeyHandshake::on_dh_gen_response`.
    fn on_dh_gen_response(
        &mut self,
        message: &[u8],
        _callback: &mut dyn AuthKeyHandshakeCallback,
    ) -> Status {
        let answer = match mtproto_api::Set_client_DH_params_answer::parse(message) {
            Ok(res) => res,
            Err(error) => return Status::error(500, error),
        };
        match answer {
            mtproto_api::Set_client_DH_params_answer::dh_gen_ok(dh_gen_ok) => {
                if int128_to_bytes(&dh_gen_ok.nonce) != self.nonce {
                    return Status::error_message("Nonce mismatch");
                }
                if int128_to_bytes(&dh_gen_ok.server_nonce) != self.server_nonce {
                    return Status::error_message("Server nonce mismatch");
                }

                let auth_key_sha1 = sha1(self.auth_key.key());
                let mut new_nonce_hash_input = Vec::with_capacity(16 + 1 + 8);
                new_nonce_hash_input.extend_from_slice(&self.new_nonce);
                new_nonce_hash_input.push(1);
                new_nonce_hash_input.extend_from_slice(&auth_key_sha1[..8]);
                let new_nonce_hash = sha1(&new_nonce_hash_input);
                if new_nonce_hash[4..] != int128_to_bytes(&dh_gen_ok.new_nonce_hash1) {
                    return Status::error_message("New nonce hash mismatch");
                }
                self.state = HandshakeState::Finish;
                Status::ok()
            }
            mtproto_api::Set_client_DH_params_answer::dh_gen_fail(_) => {
                Status::error_message("DhGenFail")
            }
            mtproto_api::Set_client_DH_params_answer::dh_gen_retry(_) => {
                Status::error_message("DhGenRetry")
            }
        }
    }
}

/// The consumed byte length of a `Server_DH_inner_data` serialization:
/// nonce + server_nonce + g + dh_prime + g_a + server_time with TL string
/// framing — the C++ `answer_parser.get_left_len()` computation for the
/// padding check.
fn server_dh_inner_data_consumed(answer: &[u8]) -> usize {
    let mut pos = 0usize;
    pos += 32; // nonce + server_nonce
    pos += 4; // g
    pos += tl_string_wire_len(&answer[pos..]); // dh_prime
    pos += tl_string_wire_len(&answer[pos..]); // g_a
    pos += 4; // server_time
    pos
}

/// The wire length of a TL string starting at `data` (header + padded payload).
fn tl_string_wire_len(data: &[u8]) -> usize {
    let first = data[0] as usize;
    if first < 254 {
        let len = first;
        let counted = len + 1;
        1 + counted.div_ceil(4) * 4
    } else if first == 254 {
        let len = data[1] as usize | (data[2] as usize) << 8 | (data[3] as usize) << 16;
        4 + len.div_ceil(4) * 4
    } else {
        let len = data[1] as usize
            | (data[2] as usize) << 8
            | (data[3] as usize) << 16
            | (data[4] as usize) << 24;
        8 + len.div_ceil(4) * 4
    }
}
