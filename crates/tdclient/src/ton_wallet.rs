//! `td/telegram/TonWalletManager.{h,cpp}` — the TON wallet plane: the
//! wallet-state/gasless/TonConnect-session caches fed by the wire updates,
//! the secret-phrase backup/restore flows over the `tde2e` keychain, the
//! TonConnect session family, the on-ramp payment family and the TON
//! Center request/cache plane. Every `td_->create_handler<...>->send(...)`
//! query is a `[blocked: net]` seam on [`TonWalletManagerEnv`] — the send
//! logs the query name and the promise parks in the manager's per-family
//! vectors (or the shared [`TonWalletManager::pending`] lot) until the net
//! step lands the answer halves:
//! `GetWalletStateQuery`, `GetExistingWalletBalanceQuery`,
//! `GetUserWalletAddressesQuery` (the addresses/create/address forms),
//! `ExportWalletSecretPhraseQuery`,
//! `FetchWalletEncryptedSecretPhrasePartQuery` (per backup-holder DC),
//! `EnableWalletBackupQuery`, `DisableWalletBackupQuery` (both forms),
//! `GetWalletGaslessInfoQuery`, `GetWalletProofChallengeQuery`,
//! `ReplaceWalletQuery`, `GetTonWalletTransactionsQuery` (the
//! transactions/by-ids/by-msg-hash forms), `SendWalletTransferQuery`,
//! `GetCurrencyRatesQuery`, `GetTonNftsQuery`, the TonConnect query set,
//! the on-ramp query set, `PerformTonCenterApiRequestQuery`,
//! `GetTonCenterStreamingApiUrlQuery`, `GetWalletBackupHolderDcsQuery`.
//! The NFT/manifest web-document legs are `[blocked: files]` (the photo
//! and document objects answer `None`); the `timeout_expired` gasless
//! re-arm rides the actor plane (the periodic expiry is unobservable
//! offline — no wire update can arrive); `PasswordManager::
//! get_input_check_password_srp` is the `[blocked: PasswordManager]`
//! seam.
//!
//! The value plane seeded earlier stays: `check_ton_address`, shared by
//! the TON-address entity detector and the wallet request gates.

use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::{Promise, PromiseInterface};
use tdutils::status::{Result, Status};

/// `TonWalletManager::MAX_MNEMONIC_BACKUP_SIZE` (TonWalletManager.h:186).
pub const MAX_MNEMONIC_BACKUP_SIZE: usize = 215;

/// `TonWalletManager::check_ton_address` (TonWalletManager.cpp:1554-1576):
/// 48 base64(any-alphabet) characters decoding to 36 bytes with a valid
/// CRC-16/XMODEM checksum over the first 34 and the anycast/metadata bits
/// in the first byte.
pub fn check_ton_address(address: &str) -> Result<()> {
    let error = |code: i32, msg: &str| Status::error(code, msg);
    if address.len() != 48 {
        return Result::Err(error(400, "Invalid address length"));
    }
    let bytes = address.as_bytes();
    let decoded = match tdmtproto::utils::base64url_decode(bytes) {
        Result::Ok(decoded) => decoded,
        Result::Err(_) => match tdmtproto::utils::base64_decode(bytes) {
            Result::Ok(decoded) => decoded,
            Result::Err(_) => return Result::Err(error(400, "Invalid address format")),
        },
    };
    // The C++ CHECKs the size after a successful decode: both alphabets
    // yield exactly 36 bytes for a canonical 48-character address.
    assert_eq!(decoded.len(), 36);
    let buffer = &decoded[..];
    let actual_crc = tdutils::crypto::crc16(&buffer[..34]);
    let expected_crc = ((buffer[34] as u16) << 8) | buffer[35] as u16;
    if actual_crc != expected_crc {
        return Result::Err(error(400, "Invalid address checksum"));
    }
    if (buffer[0] & 0x3F) != 0x11 {
        return Result::Err(error(400, "Invalid address first byte"));
    }
    Result::Ok(())
}

/// `TonWalletManager::WalletState` — the cached wallet record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WalletState {
    pub is_being_created: bool,
    pub is_backup_enabled: bool,
    pub can_export_phrase: bool,
    pub can_enable_backup: bool,
    pub address: String,
    pub public_key: Vec<u8>,
    pub balance: i64,
}

impl WalletState {
    /// The wire ctor (TonWalletManager.cpp:1245-1265).
    pub fn from_wire(wallet_state: telegram_api::WalletState) -> WalletState {
        match wallet_state {
            telegram_api::WalletState::walletStateEmpty(state) => WalletState {
                is_being_created: state.creating,
                ..WalletState::default()
            },
            telegram_api::WalletState::walletState(state) => WalletState {
                is_backup_enabled: state.backup_enabled,
                can_export_phrase: state.can_export_phrase,
                can_enable_backup: state.can_enable_backup,
                address: state.address,
                public_key: state.public_key,
                balance: state.balance,
                ..WalletState::default()
            },
        }
    }

    /// `get_ton_wallet_state_object` (TonWalletManager.cpp:1267-1270).
    pub fn get_ton_wallet_state_object(&self) -> td_api::TonWalletState {
        td_api::TonWalletState {
            address: self.address.clone(),
            public_key: self.public_key.clone(),
            gram_amount: self.balance,
            is_being_created: self.is_being_created,
            is_backup_enabled: self.is_backup_enabled,
            can_enable_backup: self.can_enable_backup,
            can_export_phrase: self.can_export_phrase,
        }
    }
}

/// `TonWalletManager::WalletGaslessInfo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WalletGaslessInfo {
    pub is_available: bool,
    pub left: i32,
    pub reset_date: i32,
    pub min_amount: i64,
    pub relayer_address: String,
}

impl WalletGaslessInfo {
    /// The wire ctor (TonWalletManager.cpp:1292-1298).
    pub fn from_wire(wallet_info: &telegram_api::updateWalletGaslessInfo) -> WalletGaslessInfo {
        WalletGaslessInfo {
            is_available: wallet_info.available,
            left: wallet_info.left,
            reset_date: wallet_info.reset_at,
            min_amount: wallet_info.min_amount,
            relayer_address: wallet_info.relayer_address.clone(),
        }
    }

    /// `try_reset` (TonWalletManager.cpp:1300-1312) — the reset_date
    /// expiry over the daily-count option; returns the seconds to the
    /// next re-arm.
    pub fn try_reset(&mut self, now: i32, daily_count_max: i64) -> i32 {
        if self.reset_date == 0 {
            return 0;
        }
        if self.reset_date > now {
            return self.reset_date - now + 1;
        }
        self.reset_date = 0;
        self.left = daily_count_max.min(1_000_000) as i32;
        0
    }

    /// `get_ton_wallet_gasless_transfers_info_object`
    /// (TonWalletManager.cpp:1314-1317).
    pub fn get_ton_wallet_gasless_transfers_info_object(
        &self,
    ) -> td_api::TonWalletGaslessTransfersInfo {
        td_api::TonWalletGaslessTransfersInfo {
            left_count: if self.is_available { self.left } else { 0 },
            reset_date: self.reset_date,
            relayer_address: if self.is_available {
                self.relayer_address.clone()
            } else {
                String::new()
            },
        }
    }
}

/// `TonWalletManager::TonConnectSession` — the cached form is the wire
/// session; the object conversion derives the state umbrella and the
/// manifest mapping.
pub fn get_ton_connect_session_object(
    session: &telegram_api::TonConnectSession,
) -> td_api::TonConnectSession {
    let state = if session.pending {
        td_api::TonConnectSessionState::tonConnectSessionStatePending(
            td_api::tonConnectSessionStatePending,
        )
    } else if session.closing {
        td_api::TonConnectSessionState::tonConnectSessionStateClosing(
            td_api::tonConnectSessionStateClosing,
        )
    } else if session.closed {
        td_api::TonConnectSessionState::tonConnectSessionStateClosed(
            td_api::tonConnectSessionStateClosed,
        )
    } else {
        td_api::TonConnectSessionState::tonConnectSessionStateReady(
            td_api::tonConnectSessionStateReady,
        )
    };
    td_api::TonConnectSession {
        id: session.id,
        dapp_client_id: session.dapp_client_id.clone(),
        wallet_client_id: session.client_id.clone(),
        creation_date: session.date,
        manifest: Some(get_ton_connect_manifest_object(
            session.manifest.as_ref(),
            session.manifest_error,
        )),
        nonce: session.nonce.clone(),
        state: Some(state),
    }
}

/// `TonWalletManager::get_ton_connect_manifest_object`
/// (TonWalletManager.cpp:2372-2387) — the manifest states; the icon
/// document leg is the [blocked: files] seam.
pub fn get_ton_connect_manifest_object(
    manifest: Option<&telegram_api::TonConnectManifest>,
    manifest_error: i32,
) -> td_api::TonConnectManifest {
    if let Some(manifest) = manifest {
        if manifest_error != 0 {
            tdutils::log_error!("Have error {manifest_error} with a manifest");
        }
        return td_api::TonConnectManifest::tonConnectManifestInfo(
            td_api::tonConnectManifestInfo {
                url: manifest.url.clone(),
                name: manifest.name.clone(),
                // documents_manager_->get_document_object over the parsed icon
                // — [blocked: files] (the ported object leg answers None).
                icon: None,
            },
        );
    }
    match manifest_error {
        0 => {
            td_api::TonConnectManifest::tonConnectManifestPending(td_api::tonConnectManifestPending)
        }
        3 => {
            td_api::TonConnectManifest::tonConnectManifestInvalid(td_api::tonConnectManifestInvalid)
        }
        _ => {
            if manifest_error != 2 {
                tdutils::log_error!("Have manifest error {manifest_error}");
            }
            td_api::TonConnectManifest::tonConnectManifestFailed(td_api::tonConnectManifestFailed)
        }
    }
}

/// `TonWalletManager::TonConnectRequest::get_ton_connect_request_object`
/// (TonWalletManager.cpp:1494-1498).
#[cfg_attr(not(test), allow(dead_code))]
fn get_ton_connect_request_object(
    request: &telegram_api::TonConnectRequest,
) -> td_api::TonConnectRequest {
    td_api::TonConnectRequest {
        session_id: request.session_id,
        message_id: i64::from(request.msg_id),
        body: request.body.clone(),
        expiration_date: request.expires,
        topic: request.topic.clone(),
        trace_id: request.trace_id.clone(),
    }
}

/// `TonWalletManager::TonConnectRequests::get_ton_connect_requests_object`
/// (TonWalletManager.cpp:1513-1518).
#[cfg_attr(not(test), allow(dead_code))]
fn get_ton_connect_requests_object(
    requests: &telegram_api::wallet_TonConnectPending,
) -> td_api::TonConnectRequests {
    td_api::TonConnectRequests {
        session: requests
            .session
            .as_ref()
            .map(get_ton_connect_session_object),
        requests: requests
            .requests
            .iter()
            .map(get_ton_connect_request_object)
            .collect(),
    }
}

/// `TonWalletManager::get_ton_wallet_transaction_object`
/// (TonWalletManager.cpp:2311-2365) — the static conversion over the wire
/// transaction; the NFT web-document legs are [blocked: files] and the
/// peer user id rides the user id-object mapping.
pub fn get_ton_wallet_transaction_object(
    my_user_id: i64,
    transaction: telegram_api::WalletTransaction,
) -> td_api::TonWalletTransaction {
    let telegram_api::WalletTransaction {
        incoming,
        gasless,
        failed,
        key_change,
        comment_encrypted,
        id,
        amount,
        fee,
        date,
        peer,
        comment,
        tx_hash,
        nft,
        ..
    } = transaction;
    let mut peer_address = String::new();
    let mut peer_user_id = 0i64;
    let mut peer_domain = String::new();
    let mut peer_provider_name = String::new();
    match peer {
        Some(telegram_api::WalletTransactionPeer::walletTransactionPeerUser(peer)) => {
            peer_user_id = peer.user_id;
            if peer_user_id <= 0 {
                tdutils::log_error!("Receive invalid user {peer_user_id}");
                peer_user_id = 0;
            }
            peer_address = peer.address;
            peer_domain = peer.domain;
        }
        Some(telegram_api::WalletTransactionPeer::walletTransactionPeerAddress(peer)) => {
            peer_address = peer.address;
            peer_domain = peer.domain;
        }
        Some(telegram_api::WalletTransactionPeer::walletTransactionPeerOnramp(peer)) => {
            peer_address = peer.address;
            peer_domain = peer.domain;
            peer_provider_name = peer.provider_name;
        }
        Some(telegram_api::WalletTransactionPeer::walletTransactionPeerUnsupported(_)) | None => {}
    }
    let mut amount = amount;
    if !incoming && amount > 0 {
        amount = -amount;
    }
    let state = if failed {
        td_api::TonWalletTransactionState::tonWalletTransactionStateFailed(
            td_api::tonWalletTransactionStateFailed,
        )
    } else {
        td_api::TonWalletTransactionState::tonWalletTransactionStateSucceeded(
            td_api::tonWalletTransactionStateSucceeded {
                tx_hash: tx_hash.clone(),
            },
        )
    };
    let transaction_type = if let Some(nft) = nft {
        td_api::TonWalletTransactionType::tonWalletTransactionTypeNftTransfer(
            td_api::tonWalletTransactionTypeNftTransfer {
                nft: Some(get_ton_nft_object(&nft)),
                is_outgoing: !incoming,
                comment: comment.clone(),
                is_comment_encrypted: comment_encrypted,
            },
        )
    } else if !peer_provider_name.is_empty() {
        td_api::TonWalletTransactionType::tonWalletTransactionTypeOnRampDeposit(
            td_api::tonWalletTransactionTypeOnRampDeposit {
                amount,
                provider_name: peer_provider_name,
            },
        )
    } else if key_change {
        peer_user_id = my_user_id;
        td_api::TonWalletTransactionType::tonWalletTransactionTypeKeyChange(
            td_api::tonWalletTransactionTypeKeyChange,
        )
    } else {
        td_api::TonWalletTransactionType::tonWalletTransactionTypeTransfer(
            td_api::tonWalletTransactionTypeTransfer {
                amount,
                is_gasless: gasless,
                comment,
                is_comment_encrypted: comment_encrypted,
            },
        )
    };
    td_api::TonWalletTransaction {
        id,
        peer_address,
        peer_user_id,
        peer_domain,
        date,
        fee_amount: fee,
        state: Some(state),
        r#type: Some(transaction_type),
    }
}

/// `TonWalletManager::Nft::get_ton_nft_object`
/// (TonWalletManager.cpp:1425-1445) — the photo/document legs are the
/// [blocked: files] seams (`get_web_document_photo_size`,
/// `documents_manager_->on_get_document`); the extra JSON rides the
/// value-or-null parse.
fn get_ton_nft_object(item: &telegram_api::wallet_NftItem) -> td_api::TonNft {
    let extra = item
        .extra
        .as_ref()
        .map(|extra| extra.data.clone())
        .unwrap_or_default();
    td_api::TonNft {
        collection_address: item.collection_address.clone(),
        address: item.address.clone(),
        owner_address: item.owner_address.clone(),
        index: item.index.clone(),
        name: item.name.clone(),
        description: item.description.clone(),
        image: None,
        thumbnail: None,
        content: None,
        sticker: None,
        attributes: item
            .attributes
            .iter()
            .map(|attribute| td_api::TonNftAttribute {
                trait_type: attribute.trait_type.clone(),
                value: attribute.value.clone(),
            })
            .collect(),
        extra: match crate::synchronous_requests::get_json_value(&extra) {
            tdutils::status::Result::Ok(value) => Some(Box::new(value)),
            tdutils::status::Result::Err(_) => None,
        },
    }
}

/// `TonWalletManager::WalletOwnershipProof::get_wallet_ownership_proof`
/// (TonWalletManager.cpp:1526-1543) — the local proof validation.
pub fn get_wallet_ownership_proof(
    proof: Option<td_api::TonWalletOwnershipProof>,
) -> Result<(Vec<u8>, telegram_api::WalletOwnershipProof)> {
    let Some(proof) = proof else {
        return Result::Err(Status::error(400, "Proof must be non-empty"));
    };
    if proof.public_key.len() != tdutils::ed25519::PrivateKey::LENGTH {
        return Result::Err(Status::error(400, "Invalid public key specified"));
    }
    if proof.signature.len() != 64 {
        return Result::Err(Status::error(400, "Invalid signature specified"));
    }
    Result::Ok((
        proof.public_key,
        telegram_api::WalletOwnershipProof {
            timestamp: proof.timestamp,
            signature: proof.signature,
        },
    ))
}

/// `TonWalletManager::encrypt_secret_phrase_part`
/// (TonWalletManager.cpp:1742-1759) — the 5-byte domain tag + the
/// single-recipient encryption, output `public_key || encrypted`.
fn encrypt_secret_phrase_part(
    data: &[u8],
    dc_public_key_id: tde2e::keychain::PublicKeyId,
) -> Result<Vec<u8>> {
    assert_eq!(data.len(), MAX_MNEMONIC_BACKUP_SIZE);
    let private_key_id = match tde2e::keychain::key_generate_temporary_private_key() {
        Result::Ok(id) => id,
        Result::Err(_) => {
            return Result::Err(Status::error(400, "Failed to generate encryption key"));
        }
    };
    let public_key = match tde2e::keychain::key_to_public_key(private_key_id) {
        Result::Ok(key) => key,
        Result::Err(_) => return Result::Err(Status::error(400, "Failed to get public key")),
    };
    let key_id = match tde2e::keychain::key_from_ecdh(private_key_id, dc_public_key_id) {
        Result::Ok(id) => id,
        Result::Err(_) => return Result::Err(Status::error(400, "Failed to generate shared key")),
    };
    let mut message = Vec::with_capacity(5 + data.len());
    message.extend_from_slice(&[0x08, 0xdd, 0x90, 0x8b, 0xd7]);
    message.extend_from_slice(data);
    let encrypted_part = match tde2e::keychain::encrypt_message_for_one(key_id, &message) {
        Result::Ok(part) => part,
        Result::Err(_) => return Result::Err(Status::error(400, "Failed to encrypt phrase part")),
    };
    let mut result = public_key;
    result.extend_from_slice(&encrypted_part);
    Result::Ok(result)
}

/// `TonWalletManager::descrypt_secret_phrase_part`
/// (TonWalletManager.cpp:1761-1779) — the 288-byte form and the 5-byte
/// domain tag strip.
fn descrypt_secret_phrase_part(
    data: &[u8],
    private_key_id: tde2e::keychain::PrivateKeyId,
) -> Result<Vec<u8>> {
    if data.len() != 288 {
        return Result::Err(Status::error(
            400,
            "Receive secret phrase of invalid length",
        ));
    }
    let public_key_id = match tde2e::keychain::key_from_public_key(&data[..32]) {
        Result::Ok(id) => id,
        Result::Err(_) => {
            return Result::Err(Status::error(
                400,
                "Receive invalid secret phrase public key",
            ));
        }
    };
    let key_id = match tde2e::keychain::key_from_ecdh(private_key_id, public_key_id) {
        Result::Ok(id) => id,
        Result::Err(_) => return Result::Err(Status::error(400, "Failed to generate shared key")),
    };
    let decrypted = match tde2e::keychain::decrypt_message_for_one(key_id, &data[32..]) {
        Result::Ok(data) => data,
        Result::Err(_) => return Result::Err(Status::error(400, "Failed to decrypt phrase part")),
    };
    // The share is raw binary (the C++ string is byte-safe).
    Result::Ok(decrypted[5..].to_vec())
}

/// The update-intake surface of the manager — the slices the wire-update
/// paths (fed from the UpdatesManager arms through the Td sink) need.
pub trait TonWalletUpdateEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool {
        false
    }

    /// `td_->auth_manager_->is_authorized()`.
    fn auth_is_authorized(&self) -> bool {
        false
    }

    /// `td::Time::now()` — the wall clock of the reload/rates caches.
    fn now(&self) -> f64 {
        0.0
    }

    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32 {
        0
    }

    /// `OptionManager::get_option_integer` — the gasless daily-count and
    /// the transfer-amount-min names.
    fn get_option_integer(&self, _name: &str) -> i64 {
        0
    }

    /// `Td::send_update`.
    fn send_update(&mut self, _update: td_api::Update) {}
}

/// The `G()`/`td_` surface of the manager.
pub trait TonWalletManagerEnv: TonWalletUpdateEnv {
    /// `G()->close_status()`.
    fn close_status(&self) -> Status {
        Status::ok()
    }

    /// `UserManager::get_my_id` — the key-change transaction peer.
    fn get_my_id(&self) -> i64 {
        0
    }

    /// `UserManager::get_user_id_object`.
    fn get_user_id_object(&mut self, user_id: i64, _source: &str) -> i64 {
        user_id
    }

    /// `UserManager::get_input_user` — the request gates.
    fn get_input_user(&mut self, _user_id: i64) -> Result<telegram_api::InputUser> {
        Result::Err(Status::error(400, "User not found"))
    }

    /// `UserManager::get_me` — the flows re-enter through `do_*` once the
    /// own user is known; the seam parks the tail until then (offline the
    /// my_id restore covers the authorized client).
    fn get_me(&mut self) -> bool {
        true
    }

    /// `PasswordManager::get_input_check_password_srp` —
    /// [blocked: PasswordManager]; the SRP promise parks with the seam.
    fn get_input_check_password_srp(
        &mut self,
        password: String,
        _promise: Promise<telegram_api::InputCheckPasswordSRP>,
    ) {
        tdutils::log_info!(
            "PasswordManager::get_input_check_password_srp({password:?}) — [blocked: PasswordManager]"
        );
    }

    /// `MessagesManager::send_ton_wallet_transfer` — the local outgoing
    /// GramTransfer message; answers the `Random::secure_int64()` park of
    /// the C++ unreachable-dialog bands.
    fn send_ton_wallet_transfer_local_message(
        &mut self,
        _peer_user_id: i64,
        _peer_address: &str,
        _amount: i64,
        _comment: &str,
        _is_comment_encrypted: bool,
        _sending_id: i32,
    ) -> i64 {
        tdutils::random::secure_int64()
    }

    /// `GetWalletStateQuery::send` — `wallet_getState` — [blocked: net].
    fn send_wallet_get_state_query(&mut self) {
        tdutils::log_info!("Send wallet_getState — [blocked: net]");
    }

    /// `GetWalletGaslessInfoQuery::send` — [blocked: net].
    fn send_wallet_get_gasless_info_query(&mut self) {
        tdutils::log_info!("Send wallet_getGaslessInfo — [blocked: net]");
    }

    /// `GetExistingWalletBalanceQuery::send` — [blocked: net].
    fn send_wallet_get_existing_balance_query(&mut self) {
        tdutils::log_info!("Send wallet_getExistingWaltBalance — [blocked: net]");
    }

    /// `GetUserWalletAddressesQuery::send` (the three flag forms) —
    /// [blocked: net].
    fn send_wallet_get_user_addresses_query(
        &mut self,
        _create: bool,
        _input_users: Vec<telegram_api::InputUser>,
        _addresses: Vec<String>,
    ) {
        tdutils::log_info!("Send wallet_getUserAddresses — [blocked: net]");
    }

    /// `GetWalletProofChallengeQuery::send` — [blocked: net].
    fn send_wallet_get_proof_challenge_query(&mut self) {
        tdutils::log_info!("Send wallet_getProofChallenge — [blocked: net]");
    }

    /// `EnableWalletBackupQuery::send` — [blocked: net].
    fn send_wallet_enable_backup_query(
        &mut self,
        _parts: Vec<Vec<u8>>,
        _public_key: Vec<u8>,
        _proof: telegram_api::WalletOwnershipProof,
    ) {
        tdutils::log_info!("Send wallet_enableBackup — [blocked: net]");
    }

    /// `DisableWalletBackupQuery::send` (the password/proof forms) —
    /// [blocked: net].
    fn send_wallet_disable_backup_query(
        &mut self,
        _input_password: Option<telegram_api::InputCheckPasswordSRP>,
        _public_key: Vec<u8>,
        _proof: Option<telegram_api::WalletOwnershipProof>,
    ) {
        tdutils::log_info!("Send wallet_disableBackup — [blocked: net]");
    }

    /// `ReplaceWalletQuery::send` — [blocked: net].
    fn send_wallet_replace_wallet_query(
        &mut self,
        _new_wallet: telegram_api::InputWalletReplacement,
        _input_password: Option<telegram_api::InputCheckPasswordSRP>,
    ) {
        tdutils::log_info!("Send wallet_replaceWallet — [blocked: net]");
    }

    /// `ExportWalletSecretPhraseQuery::send` — [blocked: net].
    fn send_wallet_export_secret_phrase_query(
        &mut self,
        _input_password: Option<telegram_api::InputCheckPasswordSRP>,
    ) {
        tdutils::log_info!("Send wallet_exportSecretPhrase — [blocked: net]");
    }

    /// `FetchWalletEncryptedSecretPhrasePartQuery::send` (per DC) —
    /// [blocked: net].
    fn send_wallet_fetch_secret_phrase_part_query(
        &mut self,
        _token: &str,
        _public_key: Vec<u8>,
        _dc_id: i32,
    ) {
        tdutils::log_info!("Send wallet_fetchEncryptedSecretPhrasePart — [blocked: net]");
    }

    /// `GetTonWalletTransactionsQuery::send` (the three forms) —
    /// [blocked: net].
    fn send_wallet_get_transactions_query(
        &mut self,
        _inbound: bool,
        _outbound: bool,
        _offset: &str,
        _limit: i32,
    ) {
        tdutils::log_info!("Send wallet_getTransactions — [blocked: net]");
    }

    fn send_wallet_get_transactions_by_query(&mut self, _by_msg_hash: bool, _ids: Vec<String>) {
        tdutils::log_info!("Send wallet_getTransactionsByIDs/ByMsgHash — [blocked: net]");
    }

    /// `SendWalletTransferQuery::send` — [blocked: net].
    fn send_wallet_send_transfer_query(
        &mut self,
        _data_normal: &[u8],
        _data_gasless: &[u8],
        _user_id: i64,
        _random_id: i64,
    ) {
        tdutils::log_info!("Send wallet_sendTransfer — [blocked: net]");
    }

    /// `GetCurrencyRatesQuery::send` — [blocked: net].
    fn send_payments_get_currency_rates_query(&mut self) {
        tdutils::log_info!("Send payments_getCurrencyRates — [blocked: net]");
    }

    /// `GetTonNftsQuery::send` — [blocked: net].
    fn send_wallet_get_nfts_query(&mut self, _offset: &str, _limit: i32) {
        tdutils::log_info!("Send wallet_getNfts — [blocked: net]");
    }

    /// `GetTonConnectSessionsQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_get_sessions_query(&mut self) {
        tdutils::log_info!("Send wallet_tonConnectGetSessions — [blocked: net]");
    }

    /// `CreateTonConnectSessionQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_create_session_query(
        &mut self,
        _dapp_client_id: &str,
        _manifest_url: &str,
    ) {
        tdutils::log_info!("Send wallet_tonConnectCreateSession — [blocked: net]");
    }

    /// `RegisterTonConnectKeyQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_register_key_query(&mut self, _session_id: i64, _client_id: &str) {
        tdutils::log_info!("Send wallet_tonConnectRegisterKey — [blocked: net]");
    }

    /// `SubmitTonConnectResultQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_submit_connect_result_query(
        &mut self,
        _session_id: i64,
        _challenge_answer: &[u8],
        _is_error: bool,
        _body: &[u8],
        _trace_id: &str,
    ) {
        tdutils::log_info!("Send wallet_tonConnectSubmitConnectResult — [blocked: net]");
    }

    /// `GetTonConnectPendingQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_get_pending_query(
        &mut self,
        _by_dapp: bool,
        _session_id: i64,
        _dapp_client_id: &str,
    ) {
        tdutils::log_info!("Send wallet_tonConnectGetPending — [blocked: net]");
    }

    /// `ClaimTonConnectRequestQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_claim_request_query(
        &mut self,
        _session_id: i64,
        _message_id: i64,
        _dapp_request_id: &str,
        _is_rejected: bool,
    ) {
        tdutils::log_info!("Send wallet_tonConnectClaimRequest — [blocked: net]");
    }

    /// `SubmitTonConnectResponseQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_submit_response_query(
        &mut self,
        _session_id: i64,
        _message_id: i64,
        _trace_id: &str,
        _body: &[u8],
    ) {
        tdutils::log_info!("Send wallet_tonConnectSubmitResponse — [blocked: net]");
    }

    /// `GetTonConnectNextEventIdQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_next_event_id_query(&mut self, _session_id: i64) {
        tdutils::log_info!("Send wallet_tonConnectNextEventId — [blocked: net]");
    }

    /// `CloseTonConnectSessionQuery::send` — [blocked: net].
    fn send_wallet_ton_connect_close_session_query(&mut self, _session_id: i64, _body: &[u8]) {
        tdutils::log_info!("Send wallet_tonConnectCloseSession — [blocked: net]");
    }

    /// `GetOnRampProvidersQuery::send` — [blocked: net].
    fn send_payments_get_onramp_providers_query(&mut self, _cryptocurrency: &str) {
        tdutils::log_info!("Send payments_getOnrampProviders — [blocked: net]");
    }

    /// `GetOnRampBaseCurrenciesQuery::send` — [blocked: net].
    fn send_payments_get_onramp_base_currencies_query(
        &mut self,
        _provider: &str,
        _cryptocurrency: &str,
    ) {
        tdutils::log_info!("Send payments_getOnrampBaseCurrencies — [blocked: net]");
    }

    /// `GetOnRampAvailabilityQuery::send` — [blocked: net].
    fn send_payments_get_onramp_availability_query(
        &mut self,
        _provider: &str,
        _cryptocurrency: &str,
        _base_currency: &str,
    ) {
        tdutils::log_info!("Send payments_getOnrampAvailability — [blocked: net]");
    }

    /// `GetOnRampLimitsQuery::send` — [blocked: net].
    fn send_payments_get_onramp_limits_query(
        &mut self,
        _provider: &str,
        _cryptocurrency: &str,
        _base_currency: &str,
        _payment_method: &str,
    ) {
        tdutils::log_info!("Send payments_getOnrampLimits — [blocked: net]");
    }

    /// `GetOnRampQuoteQuery::send` — [blocked: net].
    #[allow(clippy::too_many_arguments)]
    fn send_payments_get_onramp_quote_query(
        &mut self,
        _provider: &str,
        _cryptocurrency: &str,
        _base_currency: &str,
        _cryptocurrency_amount: &str,
        _base_currency_amount: &str,
        _payment_method: &str,
    ) {
        tdutils::log_info!("Send payments_getOnrampQuote — [blocked: net]");
    }

    /// `CreateOnRampSessionQuery::send` — [blocked: net].
    #[allow(clippy::too_many_arguments)]
    fn send_payments_create_onramp_session_query(
        &mut self,
        _provider: &str,
        _cryptocurrency: &str,
        _address: &str,
        _payment_method: &str,
        _base_currency: &str,
        _cryptocurrency_amount: &str,
        _base_currency_amount: &str,
        _memo: &str,
        _theme: &str,
        _success_return_url: &str,
        _fail_return_url: &str,
    ) {
        tdutils::log_info!("Send payments_createOnrampSession — [blocked: net]");
    }

    /// `PerformTonCenterApiRequestQuery::send` — [blocked: net].
    fn send_toncenter_perform_api_request_query(
        &mut self,
        _endpoint: &str,
        _is_post: bool,
        _query: &str,
        _payload: &str,
    ) {
        tdutils::log_info!("Send toncenter_performApiRequest — [blocked: net]");
    }

    /// `GetTonCenterStreamingApiUrlQuery::send` — [blocked: net].
    fn send_toncenter_get_streaming_url_query(&mut self) {
        tdutils::log_info!("Send toncenter_getStreamingUrl — [blocked: net]");
    }

    /// `GetWalletBackupHolderDcsQuery::send` — [blocked: net].
    fn send_wallet_get_backup_holder_dcs_query(&mut self) {
        tdutils::log_info!("Send wallet_getBackupHolderDcs — [blocked: net]");
    }
}

/// `TonWalletManager::CurrencyRate`/`CurrencyRates` — the 60-second
/// exchange-rate cache.
#[derive(Clone, Debug, Default)]
struct CurrencyRates {
    rates: Vec<(String, f64)>,
    expires_at: f64,
}

/// `TonWalletManager::StreamingApiUrl` (2f63bf029) — the streaming URL
/// cache.
#[derive(Clone, Debug, Default)]
struct StreamingApiUrl {
    url: String,
    expiration_date: i32,
}

/// `TonWalletManager::BackupHolderDc` — the parsed wallet_holderDc row.
#[derive(Clone, Debug)]
struct BackupHolderDc {
    #[cfg_attr(not(test), allow(dead_code))]
    dc_id: i32,
    #[allow(dead_code)]
    public_key: Vec<u8>,
    public_key_id: tde2e::keychain::PublicKeyId,
}

// The net-tail-only members below (the with-parts fan-out, the
// TonConnect-request object converters) run from the query answer halves;
// the local planes and the tests reach them through the pub entries.

/// `TonWalletManager::GetSecretPhraseQuery` — the XOR-assembly state;
/// the per-DC private keys ride the entry (the C++ carries them in the
/// per-part promise closures).
struct GetSecretPhraseQuery {
    promise: Promise<String>,
    result: Vec<u8>,
    left_responses: usize,
    dc_private_keys: Vec<(i32, tde2e::keychain::PrivateKeyId)>,
}

/// A parked query promise — the net-tail parking lot (the families the
/// C++ parks inside its query handlers). The promises are held for the
/// net step's answer halves; their fields are read by that step, not the
/// local planes.
#[allow(dead_code, clippy::large_enum_variant)]
enum PendingPromise {
    Unit(Promise<()>),
    String(Promise<String>),
    WalletBotBalance(Promise<td_api::WalletBotBalance>),
    UserTonWalletAddresses(Promise<td_api::UserTonWalletAddresses>),
    UserTonWalletAddress(Promise<td_api::UserTonWalletAddress>),
    ProofChallenge(Promise<td_api::TonWalletOwnershipProofChallenge>),
    TonWalletTransaction(Promise<Option<td_api::TonWalletTransaction>>),
    TonWalletTransactions(Promise<td_api::TonWalletTransactions>),
    CurrencyExchangeRates(Promise<td_api::CurrencyExchangeRates>),
    TonNfts(Promise<td_api::TonNfts>),
    TonConnectSessions(Promise<td_api::TonConnectSessions>),
    TonConnectSession(Promise<td_api::TonConnectSession>),
    TonConnectChallenge(Promise<td_api::TonConnectChallenge>),
    TonConnectRequests(Promise<td_api::TonConnectRequests>),
    TonConnectSessionEventId(Promise<td_api::TonConnectSessionEventId>),
    OnRampProviders(Promise<td_api::OnRampProviders>),
    Currencies(Promise<td_api::Currencies>),
    OnRampAvailability(Promise<td_api::OnRampPaymentAvailability>),
    OnRampLimits(Promise<td_api::OnRampPaymentLimits>),
    OnRampQuote(Promise<td_api::OnRampPaymentQuote>),
    OnRampSession(Promise<td_api::OnRampPaymentSession>),
    TonCenterStreamingApiUrl(Promise<td_api::TonCenterStreamingApiUrl>),
}

/// The collapsed `TonWalletManager`.
#[derive(Default)]
pub struct TonWalletManager {
    is_wallet_state_inited: bool,
    next_wallet_state_reload_at: f64,
    wallet_state: WalletState,

    get_wallet_state_queries: Vec<Promise<()>>,

    is_wallet_gasless_info_inited: bool,
    wallet_gasless_info: WalletGaslessInfo,

    currency_rates: CurrencyRates,
    get_currency_rates_queries: Vec<Promise<td_api::CurrencyExchangeRates>>,

    streaming_api_url: StreamingApiUrl,
    get_streaming_api_url_queries: Vec<Promise<td_api::TonCenterStreamingApiUrl>>,

    backup_holder_dcs: Vec<BackupHolderDc>,
    get_backup_holder_dcs_queries: Vec<Promise<()>>,
    /// The secret-phrase flows parked at the backup-holder-DC load chain
    /// (the C++ `send_closure` re-entries — the park records them until
    /// the DCs land).
    pending_secret_phrase_flows: Vec<(String, Promise<String>)>,
    /// The enable-backup flows parked at the same chain.
    pending_enable_backup_flows:
        Vec<(String, Option<td_api::TonWalletOwnershipProof>, Promise<()>)>,

    #[cfg_attr(not(test), allow(dead_code))]
    current_get_secret_phrase_query_id: u64,
    get_secret_phrase_queries: std::collections::HashMap<u64, GetSecretPhraseQuery>,

    /// The net-tail parking lot.
    pending: Vec<PendingPromise>,
}

impl TonWalletManager {
    pub fn new() -> TonWalletManager {
        TonWalletManager::default()
    }

    /// `on_update_wallet_state` (TonWalletManager.cpp:1592-1606).
    pub fn on_update_wallet_state(
        &mut self,
        env: &mut impl TonWalletUpdateEnv,
        wallet_state: telegram_api::WalletState,
    ) {
        if env.auth_is_bot() {
            tdutils::log_error!("Receive WalletState");
            return;
        }
        let state = WalletState::from_wire(wallet_state);
        if self.is_wallet_state_inited && state == self.wallet_state {
            return;
        }
        self.is_wallet_state_inited = true;
        self.next_wallet_state_reload_at = env.now() + 3500.0;
        self.wallet_state = state;
        env.send_update(td_api::Update::updateTonWalletState(
            td_api::updateTonWalletState {
                state: Some(self.wallet_state.get_ton_wallet_state_object()),
            },
        ));
    }

    /// `get_wallet_state`/`reload_wallet_state`
    /// (TonWalletManager.cpp:1622-1642) — the loadTonWalletState flow.
    pub fn get_wallet_state(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        mut promise: Promise<()>,
    ) {
        if self.is_wallet_state_inited {
            promise.set_value(());
            if env.now() < self.next_wallet_state_reload_at {
                return;
            }
        }
        self.reload_wallet_state(env, promise);
    }

    /// `reload_wallet_state` — the dedup and the single in-flight query.
    pub fn reload_wallet_state(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        promise: Promise<()>,
    ) {
        self.get_wallet_state_queries.push(promise);
        if self.get_wallet_state_queries.len() == 1 {
            env.send_wallet_get_state_query();
        }
    }

    /// `on_get_wallet_state` (TonWalletManager.cpp:2001-2009).
    pub fn on_get_wallet_state(&mut self, result: Result<()>) {
        let promises = std::mem::take(&mut self.get_wallet_state_queries);
        assert!(!promises.is_empty());
        match result {
            Result::Ok(()) => {
                for mut promise in promises {
                    promise.set_value(());
                }
            }
            Result::Err(status) => {
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
            }
        }
    }

    /// `on_update_wallet_gasless_info` (TonWalletManager.cpp:1644-1660) —
    /// the f918e0021 cache; the try_reset re-arm parks at the actor
    /// plane.
    pub fn on_update_wallet_gasless_info(
        &mut self,
        env: &mut impl TonWalletUpdateEnv,
        wallet_info: telegram_api::updateWalletGaslessInfo,
    ) {
        if env.auth_is_bot() {
            tdutils::log_error!("Receive updateWalletGaslessInfo");
            return;
        }
        let info = WalletGaslessInfo::from_wire(&wallet_info);
        if self.is_wallet_gasless_info_inited && info == self.wallet_gasless_info {
            return;
        }
        self.is_wallet_gasless_info_inited = true;
        self.wallet_gasless_info = info;
        let _reset_in = self.wallet_gasless_info.try_reset(
            env.unix_time(),
            env.get_option_integer("ton_wallet_gasless_transfer_daily_count_max"),
        );
        env.send_update(td_api::Update::updateTonWalletGaslessTransfersInfo(
            td_api::updateTonWalletGaslessTransfersInfo {
                info: Some(
                    self.wallet_gasless_info
                        .get_ton_wallet_gasless_transfers_info_object(),
                ),
            },
        ));
    }

    /// `timeout_expired` (TonWalletManager.cpp:1546-1559) — the gasless
    /// reset re-arm; the periodic timer rides the actor plane, the reset
    /// runs on demand from the update paths.
    pub fn timeout_expired(&mut self, env: &mut impl TonWalletUpdateEnv) -> i32 {
        if self.is_wallet_gasless_info_inited {
            let reset_in = self.wallet_gasless_info.try_reset(
                env.unix_time(),
                env.get_option_integer("ton_wallet_gasless_transfer_daily_count_max"),
            );
            if reset_in > 0 {
                return reset_in;
            }
        }
        0
    }

    /// `on_update_wallet_ton_connect_session`
    /// (TonWalletManager.cpp:1674-1683, 533e7392c).
    pub fn on_update_wallet_ton_connect_session(
        &mut self,
        env: &mut impl TonWalletUpdateEnv,
        session: telegram_api::TonConnectSession,
    ) {
        if env.auth_is_bot() {
            tdutils::log_error!("Receive TonConnectSession");
            return;
        }
        env.send_update(td_api::Update::updateTonWalletTonConnectSession(
            td_api::updateTonWalletTonConnectSession {
                session: Some(get_ton_connect_session_object(&session)),
            },
        ));
    }

    /// `get_ton_wallet_gasless_info` (loadTonWalletGaslessTransfersInfo).
    pub fn get_ton_wallet_gasless_info(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        promise: Promise<()>,
    ) {
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_get_gasless_info_query();
    }

    /// `get_existing_wallet_balance` (checkWalletBotBalance).
    pub fn get_existing_wallet_balance(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        promise: Promise<td_api::WalletBotBalance>,
    ) {
        self.pending.push(PendingPromise::WalletBotBalance(promise));
        env.send_wallet_get_existing_balance_query();
    }

    /// `get_user_addresses` (getUserTonWalletAddresses) — the get_input_user
    /// gates then the query.
    pub fn get_user_addresses(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        user_ids: Vec<i64>,
        mut promise: Promise<td_api::UserTonWalletAddresses>,
    ) {
        for &user_id in &user_ids {
            if let Result::Err(status) = env.get_input_user(user_id) {
                return promise.set_error(status);
            }
        }
        self.pending
            .push(PendingPromise::UserTonWalletAddresses(promise));
        env.send_wallet_get_user_addresses_query(false, Vec::new(), Vec::new());
    }

    /// `create_user_ton_wallet` (createUserTonWallet).
    pub fn create_user_ton_wallet(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        user_id: i64,
        mut promise: Promise<td_api::UserTonWalletAddress>,
    ) {
        if let Result::Err(status) = env.get_input_user(user_id) {
            return promise.set_error(status);
        }
        self.pending
            .push(PendingPromise::UserTonWalletAddress(promise));
        env.send_wallet_get_user_addresses_query(true, Vec::new(), Vec::new());
    }

    /// `get_address_ton_wallet` (getAddressTonWallet).
    pub fn get_address_ton_wallet(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        address: &str,
        mut promise: Promise<td_api::UserTonWalletAddress>,
    ) {
        match check_ton_address(address) {
            Result::Ok(()) => {}
            Result::Err(status) => return promise.set_error(status),
        }
        self.pending
            .push(PendingPromise::UserTonWalletAddress(promise));
        env.send_wallet_get_user_addresses_query(false, Vec::new(), vec![address.to_owned()]);
    }

    /// `get_ton_wallet_secret_phrase` (TonWalletManager.cpp:1692-1722) —
    /// the close gate, the backup-holder-DC load chain, the empty-password
    /// fast path and the SRP seam.
    pub fn get_ton_wallet_secret_phrase(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        password: &str,
        mut promise: Promise<String>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        if self.backup_holder_dcs.is_empty() {
            // load_backup_holder_dcs + the flow re-entry — the park
            self.pending_secret_phrase_flows
                .push((password.to_owned(), promise));
            if self.get_backup_holder_dcs_queries.is_empty() {
                self.get_backup_holder_dcs_queries.push(Promise::default());
                env.send_wallet_get_backup_holder_dcs_query();
            }
            return;
        }
        if password.is_empty() {
            return self.do_get_ton_wallet_secret_phrase(env, None, promise);
        }
        env.get_input_check_password_srp(
            password.to_owned(),
            Promise::from_interface(SecretPhraseSrp {
                promise: Some(promise),
            }),
        );
    }

    /// `do_get_ton_wallet_secret_phrase` (TonWalletManager.cpp:1724-1737).
    fn do_get_ton_wallet_secret_phrase(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        input_password: Option<telegram_api::InputCheckPasswordSRP>,
        mut promise: Promise<String>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        self.pending.push(PendingPromise::String(promise));
        env.send_wallet_export_secret_phrase_query(input_password);
    }

    /// `do_get_ton_wallet_secret_phrase_with_parts`
    /// (TonWalletManager.cpp:1739-1760) — the per-DC key fan-out; the
    /// export-query answer half (and the round-trip test) reach it.
    #[cfg_attr(not(test), allow(dead_code))]
    fn do_get_ton_wallet_secret_phrase_with_parts(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        parts: telegram_api::wallet_SecretPhraseParts,
        mut promise: Promise<String>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        assert!(!self.backup_holder_dcs.is_empty());
        self.current_get_secret_phrase_query_id += 1;
        let query_id = self.current_get_secret_phrase_query_id;
        let mut dc_private_keys = Vec::with_capacity(self.backup_holder_dcs.len());
        for dc in self.backup_holder_dcs.clone() {
            let private_key_id = match tde2e::keychain::key_generate_temporary_private_key() {
                Result::Ok(id) => id,
                Result::Err(_) => {
                    return promise
                        .set_error(Status::error(400, "Failed to generate encryption key"));
                }
            };
            let public_key = match tde2e::keychain::key_to_public_key(private_key_id) {
                Result::Ok(key) => key,
                Result::Err(_) => {
                    return promise
                        .set_error(Status::error(400, "Failed to generate encryption key"));
                }
            };
            env.send_wallet_fetch_secret_phrase_part_query(&parts.token, public_key, dc.dc_id);
            // The per-part answer (query_id + private_key_id) re-enters
            // through on_get_ton_wallet_secret_phrase_part; the entry
            // below keeps the assembly state.
            dc_private_keys.push((dc.dc_id, private_key_id));
        }
        self.get_secret_phrase_queries.insert(
            query_id,
            GetSecretPhraseQuery {
                promise,
                result: Vec::new(),
                left_responses: dc_private_keys.len(),
                dc_private_keys,
            },
        );
    }

    /// `on_get_ton_wallet_secret_phrase_part`
    /// (TonWalletManager.cpp:1781-1838) — the decrypt, the XOR fold and
    /// the charset validation.
    pub fn on_get_ton_wallet_secret_phrase_part(
        &mut self,
        part: Result<Vec<u8>>,
        query_id: u64,
        private_key_id: tde2e::keychain::PrivateKeyId,
    ) {
        let Some(mut query) = self.get_secret_phrase_queries.remove(&query_id) else {
            return;
        };
        // The private key must belong to the query's DC fan-out (the C++
        // carries it in the per-part closure).
        if !query
            .dc_private_keys
            .iter()
            .any(|&(_, key)| key == private_key_id)
        {
            self.get_secret_phrase_queries.insert(query_id, query);
            return;
        }
        let data = match part {
            Result::Ok(data) => match descrypt_secret_phrase_part(&data, private_key_id) {
                Result::Ok(data) => data,
                Result::Err(status) => return query.promise.set_error(status),
            },
            Result::Err(status) => return query.promise.set_error(status),
        };
        if query.result.is_empty() {
            query.result.resize(MAX_MNEMONIC_BACKUP_SIZE, 0);
        }
        assert_eq!(data.len(), MAX_MNEMONIC_BACKUP_SIZE);
        for (byte, share) in query.result.iter_mut().zip(data) {
            *byte ^= share;
        }
        assert!(query.left_responses > 0);
        query.left_responses -= 1;
        if query.left_responses == 0 {
            let mut result = query.result;
            while result.last() == Some(&b' ') {
                result.pop();
            }
            for &c in &result {
                if !(c as char).is_ascii_alphabetic() && c != b' ' {
                    return query
                        .promise
                        .set_error(Status::error(400, "Receive invalid secret phrase"));
                }
            }
            // The charset gate above guarantees ASCII (alpha + space).
            query
                .promise
                .set_value(String::from_utf8(result).unwrap_or_default());
        } else {
            self.get_secret_phrase_queries.insert(query_id, query);
        }
    }

    /// `get_ton_wallet_proof_challenge`
    /// (getTonWalletOwnershipProofChallenge).
    pub fn get_ton_wallet_proof_challenge(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        promise: Promise<td_api::TonWalletOwnershipProofChallenge>,
    ) {
        self.pending.push(PendingPromise::ProofChallenge(promise));
        env.send_wallet_get_proof_challenge_query();
    }

    /// `enable_ton_wallet_backup` (TonWalletManager.cpp:1840-1890) — the
    /// proof gates, the charset gate and the 3-way XOR split over the
    /// holder-DC keys.
    pub fn enable_ton_wallet_backup(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        secret_phrase: &str,
        ownership_proof: Option<td_api::TonWalletOwnershipProof>,
        mut promise: Promise<()>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        if self.backup_holder_dcs.is_empty() {
            // load_backup_holder_dcs + the flow re-entry — the park
            self.pending_enable_backup_flows.push((
                secret_phrase.to_owned(),
                ownership_proof,
                promise,
            ));
            if self.get_backup_holder_dcs_queries.is_empty() {
                self.get_backup_holder_dcs_queries.push(Promise::default());
                env.send_wallet_get_backup_holder_dcs_query();
            }
            return;
        }
        let (public_key, proof) = match get_wallet_ownership_proof(ownership_proof) {
            Result::Ok(parsed) => parsed,
            Result::Err(status) => return promise.set_error(status),
        };
        if secret_phrase.len() > MAX_MNEMONIC_BACKUP_SIZE {
            return promise.set_error(Status::error(400, "Invalid secret phrase specified"));
        }
        for c in secret_phrase.chars() {
            if !c.is_ascii_alphabetic() && c != ' ' {
                return promise.set_error(Status::error(400, "Receive invalid secret phrase"));
            }
        }

        let mut a = vec![0u8; MAX_MNEMONIC_BACKUP_SIZE];
        let mut b = vec![0u8; MAX_MNEMONIC_BACKUP_SIZE];
        let mut c = vec![b' '; MAX_MNEMONIC_BACKUP_SIZE];
        let phrase = secret_phrase.as_bytes();
        c[..phrase.len()].copy_from_slice(phrase);
        tdutils::random::secure_bytes(&mut a);
        tdutils::random::secure_bytes(&mut b);
        for i in 0..MAX_MNEMONIC_BACKUP_SIZE {
            c[i] ^= a[i] ^ b[i];
        }
        assert_eq!(self.backup_holder_dcs.len(), 3);
        let part_a = match encrypt_secret_phrase_part(&a, self.backup_holder_dcs[0].public_key_id) {
            Result::Ok(part) => part,
            Result::Err(status) => return promise.set_error(status),
        };
        let part_b = match encrypt_secret_phrase_part(&b, self.backup_holder_dcs[1].public_key_id) {
            Result::Ok(part) => part,
            Result::Err(status) => return promise.set_error(status),
        };
        let part_c = match encrypt_secret_phrase_part(&c, self.backup_holder_dcs[2].public_key_id) {
            Result::Ok(part) => part,
            Result::Err(status) => return promise.set_error(status),
        };
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_enable_backup_query(vec![part_a, part_b, part_c], public_key, proof);
    }

    /// `disable_ton_wallet_backup` (TonWalletManager.cpp:1892-1910).
    pub fn disable_ton_wallet_backup(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        password: &str,
        promise: Promise<()>,
    ) {
        if password.is_empty() {
            return self.do_disable_ton_wallet_backup(env, None, promise);
        }
        env.get_input_check_password_srp(
            password.to_owned(),
            Promise::from_interface(DisableBackupSrp {
                promise: Some(promise),
            }),
        );
    }

    /// `do_disable_ton_wallet_backup`.
    fn do_disable_ton_wallet_backup(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        input_password: Option<telegram_api::InputCheckPasswordSRP>,
        mut promise: Promise<()>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_disable_backup_query(input_password, Vec::new(), None);
    }

    /// `disable_ton_wallet_backup_with_proof`.
    pub fn disable_ton_wallet_backup_with_proof(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        ownership_proof: Option<td_api::TonWalletOwnershipProof>,
        mut promise: Promise<()>,
    ) {
        let (public_key, proof) = match get_wallet_ownership_proof(ownership_proof) {
            Result::Ok(parsed) => parsed,
            Result::Err(status) => return promise.set_error(status),
        };
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_disable_backup_query(None, public_key, Some(proof));
    }

    /// `send_ton_wallet_transfer` (TonWalletManager.cpp:1942-1960) — the
    /// gates, then the get_me hop, then the local message + the query.
    #[allow(clippy::too_many_arguments)]
    pub fn send_ton_wallet_transfer(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        data_normal: Vec<u8>,
        data_gasless: Vec<u8>,
        peer_user_id: i64,
        peer_address: &str,
        amount: i64,
        comment: &str,
        is_comment_encrypted: bool,
        sending_id: i32,
        mut promise: Promise<td_api::TonWalletTransferResult>,
    ) {
        match check_ton_address(peer_address) {
            Result::Ok(()) => {}
            Result::Err(status) => return promise.set_error(status),
        }
        if amount <= 0 || amount < env.get_option_integer("ton_wallet_transfer_amount_min") {
            return promise.set_error(Status::error(400, "Invalid transfer amount specified"));
        }
        if peer_user_id != 0
            && let Result::Err(status) = env.get_input_user(peer_user_id)
        {
            return promise.set_error(status);
        }
        if !env.get_me() {
            // the get_me continuation park
            return;
        }
        self.do_send_ton_wallet_transfer(
            env,
            data_normal,
            data_gasless,
            peer_user_id,
            peer_address,
            amount,
            comment,
            is_comment_encrypted,
            sending_id,
            promise,
        );
    }

    /// `do_send_ton_wallet_transfer` (TonWalletManager.cpp:1962-1974).
    #[allow(clippy::too_many_arguments)]
    fn do_send_ton_wallet_transfer(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        data_normal: Vec<u8>,
        data_gasless: Vec<u8>,
        peer_user_id: i64,
        peer_address: &str,
        amount: i64,
        comment: &str,
        is_comment_encrypted: bool,
        sending_id: i32,
        mut promise: Promise<td_api::TonWalletTransferResult>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let random_id = env.send_ton_wallet_transfer_local_message(
            peer_user_id,
            peer_address,
            amount,
            comment,
            is_comment_encrypted,
            sending_id,
        );
        self.pending
            .push(PendingPromise::Unit(Promise::from_interface(
                TransferResultPark {
                    promise: Some(promise),
                },
            )));
        env.send_wallet_send_transfer_query(&data_normal, &data_gasless, peer_user_id, random_id);
    }

    /// `delete_ton_wallet` (TonWalletManager.cpp:1976-1994).
    pub fn delete_ton_wallet(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        password: &str,
        promise: Promise<()>,
    ) {
        if password.is_empty() {
            return self.do_delete_ton_wallet(env, None, promise);
        }
        env.get_input_check_password_srp(
            password.to_owned(),
            Promise::from_interface(DeleteWalletSrp {
                promise: Some(promise),
            }),
        );
    }

    /// `do_delete_ton_wallet` — the inputWalletNew replacement.
    fn do_delete_ton_wallet(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        input_password: Option<telegram_api::InputCheckPasswordSRP>,
        mut promise: Promise<()>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_replace_wallet_query(
            telegram_api::InputWalletReplacement::inputWalletNew(telegram_api::inputWalletNew {}),
            input_password,
        );
    }

    /// `replace_ton_wallet` (TonWalletManager.cpp:1996-2033).
    pub fn replace_ton_wallet(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        password: &str,
        anchor_public_key: &[u8],
        ownership_proof: Option<td_api::TonWalletOwnershipProof>,
        mut promise: Promise<()>,
    ) {
        let (public_key, proof) = match get_wallet_ownership_proof(ownership_proof) {
            Result::Ok(parsed) => parsed,
            Result::Err(status) => return promise.set_error(status),
        };
        if password.is_empty() {
            return self.do_replace_ton_wallet_with_proof(
                env,
                None,
                anchor_public_key,
                public_key,
                proof,
                promise,
            );
        }
        env.get_input_check_password_srp(
            password.to_owned(),
            Promise::from_interface(ReplaceWalletSrp {
                anchor_public_key: anchor_public_key.to_vec(),
                public_key,
                proof,
                promise: Some(promise),
            }),
        );
    }

    /// `do_replace_ton_wallet_with_proof` — the inputWalletImported flags.
    fn do_replace_ton_wallet_with_proof(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        input_password: Option<telegram_api::InputCheckPasswordSRP>,
        anchor_public_key: &[u8],
        public_key: Vec<u8>,
        proof: telegram_api::WalletOwnershipProof,
        mut promise: Promise<()>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        const ANCHOR_PUBLIC_KEY_MASK: i32 = 1;
        let flags = if anchor_public_key.is_empty() {
            0
        } else {
            ANCHOR_PUBLIC_KEY_MASK
        };
        let input_wallet = telegram_api::inputWalletImported {
            flags,
            public_key,
            anchor_public_key: anchor_public_key.to_vec(),
            proof: Some(proof),
        };
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_replace_wallet_query(
            telegram_api::InputWalletReplacement::inputWalletImported(input_wallet),
            input_password,
        );
    }

    /// `get_ton_wallet_transactions` (the get_me hop + the query).
    pub fn get_ton_wallet_transactions(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        offset: &str,
        limit: i32,
        direction: Option<td_api::TransactionDirection>,
        mut promise: Promise<td_api::TonWalletTransactions>,
    ) {
        if !env.get_me() {
            return;
        }
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let (inbound, outbound) = match &direction {
            Some(td_api::TransactionDirection::transactionDirectionIncoming(_)) => (true, false),
            Some(td_api::TransactionDirection::transactionDirectionOutgoing(_)) => (false, true),
            None => (false, false),
        };
        self.pending
            .push(PendingPromise::TonWalletTransactions(promise));
        env.send_wallet_get_transactions_query(inbound, outbound, offset, limit);
    }

    /// `get_ton_wallet_transaction` (by id).
    pub fn get_ton_wallet_transaction(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        transaction_id: &str,
        mut promise: Promise<Option<td_api::TonWalletTransaction>>,
    ) {
        if !env.get_me() {
            return;
        }
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        self.pending
            .push(PendingPromise::TonWalletTransaction(promise));
        env.send_wallet_get_transactions_by_query(false, vec![transaction_id.to_owned()]);
    }

    /// `get_ton_wallet_transaction_by_msg_hash`.
    pub fn get_ton_wallet_transaction_by_msg_hash(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        msg_hash: &str,
        mut promise: Promise<Option<td_api::TonWalletTransaction>>,
    ) {
        if !env.get_me() {
            return;
        }
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        self.pending
            .push(PendingPromise::TonWalletTransaction(promise));
        env.send_wallet_get_transactions_by_query(true, vec![msg_hash.to_owned()]);
    }

    /// `get_currency_rates` (TonWalletManager.cpp:2072-2090) — the
    /// 60-second cache and the promise aggregation.
    pub fn get_currency_rates(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        mut promise: Promise<td_api::CurrencyExchangeRates>,
    ) {
        if env.now() < self.currency_rates.expires_at {
            return promise.set_value(self.get_currency_exchange_rates_object());
        }
        self.get_currency_rates_queries.push(promise);
        if self.get_currency_rates_queries.len() == 1 {
            env.send_payments_get_currency_rates_query();
        }
    }

    /// `on_get_currency_rates` (TonWalletManager.cpp:2092-2114).
    pub fn on_get_currency_rates(
        &mut self,
        env: &impl TonWalletManagerEnv,
        rates: Result<telegram_api::payments_CurrencyRates>,
    ) {
        let promises = std::mem::take(&mut self.get_currency_rates_queries);
        assert!(!promises.is_empty());
        let rates = match rates {
            Result::Ok(rates) => rates,
            Result::Err(status) => {
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
                return;
            }
        };
        self.currency_rates.rates = rates
            .rates
            .into_iter()
            .map(|rate| (rate.currency, rate.rate))
            .collect();
        self.currency_rates.expires_at = env.now() + 60.0;
        let object = self.get_currency_exchange_rates_object();
        for mut promise in promises {
            promise.set_value(object.clone());
        }
    }

    /// `get_currency_exchange_rates_object` (TonWalletManager.cpp:2066-2070).
    fn get_currency_exchange_rates_object(&self) -> td_api::CurrencyExchangeRates {
        td_api::CurrencyExchangeRates {
            rates: self
                .currency_rates
                .rates
                .iter()
                .map(|(currency, rate)| td_api::CurrencyExchangeRate {
                    currency: currency.clone(),
                    rate: *rate,
                })
                .collect(),
        }
    }

    /// `get_nfts` (getTonWalletNfts).
    pub fn get_nfts(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::TonNfts>,
    ) {
        self.pending.push(PendingPromise::TonNfts(promise));
        env.send_wallet_get_nfts_query(offset, limit);
    }

    /// `get_ton_connect_sessions`.
    pub fn get_ton_connect_sessions(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        promise: Promise<td_api::TonConnectSessions>,
    ) {
        self.pending
            .push(PendingPromise::TonConnectSessions(promise));
        env.send_wallet_ton_connect_get_sessions_query();
    }

    /// `create_ton_connect_session`.
    pub fn create_ton_connect_session(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        dapp_client_id: &str,
        manifest_url: &str,
        promise: Promise<td_api::TonConnectSession>,
    ) {
        self.pending
            .push(PendingPromise::TonConnectSession(promise));
        env.send_wallet_ton_connect_create_session_query(dapp_client_id, manifest_url);
    }

    /// `register_ton_connect_key` (setTonConnectSessionWalletClientId).
    pub fn register_ton_connect_key(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        client_id: &str,
        promise: Promise<td_api::TonConnectChallenge>,
    ) {
        self.pending
            .push(PendingPromise::TonConnectChallenge(promise));
        env.send_wallet_ton_connect_register_key_query(session_id, client_id);
    }

    /// `submit_ton_connect_result`
    /// (sendTonConnectSessionConnectResult).
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn submit_ton_connect_result(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        challenge_answer: &[u8],
        is_error: bool,
        body: &[u8],
        trace_id: &str,
        promise: Promise<()>,
    ) {
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_ton_connect_submit_connect_result_query(
            session_id,
            challenge_answer,
            is_error,
            body,
            trace_id,
        );
    }

    /// `get_ton_connect_requests` (the session and dApp forms).
    pub fn get_ton_connect_requests(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        by_dapp: bool,
        session_id: i64,
        dapp_client_id: &str,
        promise: Promise<td_api::TonConnectRequests>,
    ) {
        self.pending
            .push(PendingPromise::TonConnectRequests(promise));
        env.send_wallet_ton_connect_get_pending_query(by_dapp, session_id, dapp_client_id);
    }

    /// `claim_ton_connect_request` (claimTonConnectRequest) — the
    /// server-message-id gate.
    pub fn claim_ton_connect_request(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        message_id: i64,
        dapp_request_id: &str,
        is_rejected: bool,
        mut promise: Promise<()>,
    ) {
        if !crate::messages_manager::MessageId::new(message_id).is_server() {
            return promise.set_error(Status::error(400, "Invalid message identifier specified"));
        }
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_ton_connect_claim_request_query(
            session_id,
            message_id,
            dapp_request_id,
            is_rejected,
        );
    }

    /// `submit_ton_connect_response` (answerTonConnectRequest).
    pub fn submit_ton_connect_response(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        message_id: i64,
        trace_id: &str,
        body: &[u8],
        mut promise: Promise<()>,
    ) {
        if !crate::messages_manager::MessageId::new(message_id).is_server() {
            return promise.set_error(Status::error(400, "Invalid message identifier specified"));
        }
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_ton_connect_submit_response_query(session_id, message_id, trace_id, body);
    }

    /// `get_ton_connect_next_event_id` (getTonConnectSessionNextEventId).
    pub fn get_ton_connect_next_event_id(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        promise: Promise<td_api::TonConnectSessionEventId>,
    ) {
        self.pending
            .push(PendingPromise::TonConnectSessionEventId(promise));
        env.send_wallet_ton_connect_next_event_id_query(session_id);
    }

    /// `close_ton_connect_session` (disconnectTonConnectSession).
    pub fn close_ton_connect_session(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        session_id: i64,
        body: &[u8],
        promise: Promise<()>,
    ) {
        self.pending.push(PendingPromise::Unit(promise));
        env.send_wallet_ton_connect_close_session_query(session_id, body);
    }

    /// `get_on_ramp_providers`.
    pub fn get_on_ramp_providers(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        cryptocurrency: &str,
        promise: Promise<td_api::OnRampProviders>,
    ) {
        self.pending.push(PendingPromise::OnRampProviders(promise));
        env.send_payments_get_onramp_providers_query(cryptocurrency);
    }

    /// `get_on_ramp_base_currencies`.
    pub fn get_on_ramp_base_currencies(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        provider: &str,
        cryptocurrency: &str,
        promise: Promise<td_api::Currencies>,
    ) {
        self.pending.push(PendingPromise::Currencies(promise));
        env.send_payments_get_onramp_base_currencies_query(provider, cryptocurrency);
    }

    /// `get_on_ramp_availability`.
    pub fn get_on_ramp_availability(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        provider: &str,
        cryptocurrency: &str,
        base_currency: &str,
        promise: Promise<td_api::OnRampPaymentAvailability>,
    ) {
        self.pending
            .push(PendingPromise::OnRampAvailability(promise));
        env.send_payments_get_onramp_availability_query(provider, cryptocurrency, base_currency);
    }

    /// `get_on_ramp_limits`.
    pub fn get_on_ramp_limits(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        provider: &str,
        cryptocurrency: &str,
        base_currency: &str,
        payment_method: &str,
        promise: Promise<td_api::OnRampPaymentLimits>,
    ) {
        self.pending.push(PendingPromise::OnRampLimits(promise));
        env.send_payments_get_onramp_limits_query(
            provider,
            cryptocurrency,
            base_currency,
            payment_method,
        );
    }

    /// `get_on_ramp_quote`.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn get_on_ramp_quote(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        provider: &str,
        cryptocurrency: &str,
        base_currency: &str,
        cryptocurrency_amount: &str,
        base_currency_amount: &str,
        payment_method: &str,
        promise: Promise<td_api::OnRampPaymentQuote>,
    ) {
        self.pending.push(PendingPromise::OnRampQuote(promise));
        env.send_payments_get_onramp_quote_query(
            provider,
            cryptocurrency,
            base_currency,
            cryptocurrency_amount,
            base_currency_amount,
            payment_method,
        );
    }

    /// `create_on_ramp_session` (9b5f0edf9 — the theme flag).
    #[allow(clippy::too_many_arguments)]
    pub fn create_on_ramp_session(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        provider: &str,
        cryptocurrency: &str,
        address: &str,
        payment_method: &str,
        base_currency: &str,
        cryptocurrency_amount: &str,
        base_currency_amount: &str,
        memo: &str,
        theme: &str,
        success_return_url: &str,
        fail_return_url: &str,
        promise: Promise<td_api::OnRampPaymentSession>,
    ) {
        self.pending.push(PendingPromise::OnRampSession(promise));
        env.send_payments_create_onramp_session_query(
            provider,
            cryptocurrency,
            address,
            payment_method,
            base_currency,
            cryptocurrency_amount,
            base_currency_amount,
            memo,
            theme,
            success_return_url,
            fail_return_url,
        );
    }

    /// `perform_ton_center_api_request` (sendTonCenterApiRequest) — the
    /// type dispatch with the UTF-8 gates (the post arm keeps the C++'s
    /// "Phone number" text).
    pub fn perform_ton_center_api_request(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        endpoint: &str,
        request_type: Option<td_api::TonCenterApiRequestType>,
        mut promise: Promise<String>,
    ) {
        let Some(request_type) = request_type else {
            return promise.set_error(Status::error(400, "Item type must be non-empty"));
        };
        match request_type {
            td_api::TonCenterApiRequestType::tonCenterApiRequestTypeGet(get) => {
                let Some(query) = crate::misc::clean_input_string(&get.query) else {
                    return promise.set_error(Status::error(400, "Query must be encoded in UTF-8"));
                };
                self.pending.push(PendingPromise::String(promise));
                env.send_toncenter_perform_api_request_query(endpoint, false, &query, "");
            }
            td_api::TonCenterApiRequestType::tonCenterApiRequestTypePost(post) => {
                let Some(payload) = crate::misc::clean_input_string(&post.payload) else {
                    return promise
                        .set_error(Status::error(400, "Phone number must be encoded in UTF-8"));
                };
                self.pending.push(PendingPromise::String(promise));
                env.send_toncenter_perform_api_request_query(endpoint, true, "", &payload);
            }
        }
    }

    /// `get_ton_center_streaming_api_url` (TonWalletManager.cpp:2253-2273,
    /// 2f63bf029) — the 60-second cache gate.
    pub fn get_ton_center_streaming_api_url(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        mut promise: Promise<td_api::TonCenterStreamingApiUrl>,
    ) {
        let cache_expires_in = self.streaming_api_url.expiration_date - env.unix_time();
        if cache_expires_in >= 60 {
            return promise.set_value(td_api::TonCenterStreamingApiUrl {
                url: self.streaming_api_url.url.clone(),
                expires_in: cache_expires_in,
            });
        }
        self.get_streaming_api_url_queries.push(promise);
        if self.get_streaming_api_url_queries.len() == 1 {
            env.send_toncenter_get_streaming_url_query();
        }
    }

    /// `on_get_ton_center_streaming_api_url`
    /// (TonWalletManager.cpp:2275-2300).
    pub fn on_get_ton_center_streaming_api_url(
        &mut self,
        env: &impl TonWalletManagerEnv,
        url: Result<telegram_api::toncenter_StreamingUrl>,
    ) {
        let promises = std::mem::take(&mut self.get_streaming_api_url_queries);
        assert!(!promises.is_empty());
        let url = match url {
            Result::Ok(url) => url,
            Result::Err(status) => {
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
                return;
            }
        };
        let expires_in = url.expires - env.unix_time();
        if expires_in <= 0 || url.url.is_empty() {
            tdutils::log_error!("Receive invalid streaming url");
            for mut promise in promises {
                promise.set_error(Status::error(500, "Receive invalid response"));
            }
            return;
        }
        self.streaming_api_url.url = url.url.clone();
        self.streaming_api_url.expiration_date = url.expires;
        for mut promise in promises {
            promise.set_value(td_api::TonCenterStreamingApiUrl {
                url: url.url.clone(),
                expires_in,
            });
        }
    }

    /// `load_backup_holder_dcs` (TonWalletManager.cpp:2302-2320).
    pub fn load_backup_holder_dcs(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        mut promise: Promise<()>,
    ) {
        if !self.backup_holder_dcs.is_empty() {
            return promise.set_value(());
        }
        self.get_backup_holder_dcs_queries.push(promise);
        if self.get_backup_holder_dcs_queries.len() == 1 {
            env.send_wallet_get_backup_holder_dcs_query();
        }
    }

    /// `on_get_backup_holder_dcs` (TonWalletManager.cpp:2322-2353) — the
    /// DC-id gate, the tde2e public-key parse, the 3-DC check and the
    /// parked-flow re-entries.
    pub fn on_get_backup_holder_dcs(
        &mut self,
        env: &mut impl TonWalletManagerEnv,
        dcs: Result<Vec<telegram_api::wallet_HolderDc>>,
    ) {
        let promises = std::mem::take(&mut self.get_backup_holder_dcs_queries);
        assert!(!promises.is_empty());
        let dcs = match dcs {
            Result::Ok(dcs) => dcs,
            Result::Err(status) => {
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
                return;
            }
        };
        let mut holder_dcs = Vec::with_capacity(dcs.len());
        for dc in dcs {
            // `DcId::is_valid` (DcId.h) — the internal DC band.
            let dc_valid = (1..=5).contains(&dc.dc);
            if !dc_valid {
                tdutils::log_error!("Receive invalid DC identifier");
                let status = Status::error(400, "Receive invalid DC identifier");
                for mut promise in promises {
                    promise.set_error(status.clone());
                }
                self.fail_pending_backup_flows(status.clone());
                return;
            }
            let public_key_id = match tde2e::keychain::key_from_public_key(&dc.public_key) {
                Result::Ok(id) => id,
                Result::Err(_) => {
                    let status = Status::error(400, "Failed to parse public key");
                    for mut promise in promises {
                        promise.set_error(status.clone());
                    }
                    self.fail_pending_backup_flows(status.clone());
                    return;
                }
            };
            holder_dcs.push(BackupHolderDc {
                dc_id: dc.dc,
                public_key: dc.public_key,
                public_key_id,
            });
        }
        if holder_dcs.len() != 3 {
            let status = Status::error(400, "Failed to get backup holder DCs");
            for mut promise in promises {
                promise.set_error(status.clone());
            }
            self.fail_pending_backup_flows(status.clone());
            return;
        }
        assert!(self.backup_holder_dcs.is_empty());
        self.backup_holder_dcs = holder_dcs;
        for mut promise in promises {
            promise.set_value(());
        }
        // The parked flow re-entries (the C++ send_closure chains).
        let secret_phrase_flows = std::mem::take(&mut self.pending_secret_phrase_flows);
        for (password, promise) in secret_phrase_flows {
            self.get_ton_wallet_secret_phrase(env, &password, promise);
        }
        let enable_flows = std::mem::take(&mut self.pending_enable_backup_flows);
        for (secret_phrase, ownership_proof, promise) in enable_flows {
            self.enable_ton_wallet_backup(env, &secret_phrase, ownership_proof, promise);
        }
    }

    /// The parked-flow failure spread of the DC-load error bands.
    fn fail_pending_backup_flows(&mut self, status: Status) {
        for (_, mut promise) in std::mem::take(&mut self.pending_secret_phrase_flows) {
            promise.set_error(status.clone());
        }
        for (_, _, mut promise) in std::mem::take(&mut self.pending_enable_backup_flows) {
            promise.set_error(status.clone());
        }
    }

    /// `get_current_state` (TonWalletManager.cpp:2389-2403) — the two
    /// cached updates over the authorized gate.
    pub fn get_current_state(
        &self,
        env: &impl TonWalletUpdateEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !env.auth_is_authorized() {
            return;
        }
        if self.is_wallet_state_inited {
            updates.push(td_api::Update::updateTonWalletState(
                td_api::updateTonWalletState {
                    state: Some(self.wallet_state.get_ton_wallet_state_object()),
                },
            ));
        }
        if self.is_wallet_gasless_info_inited {
            updates.push(td_api::Update::updateTonWalletGaslessTransfersInfo(
                td_api::updateTonWalletGaslessTransfersInfo {
                    info: Some(
                        self.wallet_gasless_info
                            .get_ton_wallet_gasless_transfers_info_object(),
                    ),
                },
            ));
        }
    }

    /// The transaction-object conversion over the env's user-id mapping —
    /// the static `get_ton_wallet_transaction_object` entry the query
    /// tails and tests use.
    pub fn get_ton_wallet_transaction_object_env(
        &self,
        env: &mut impl TonWalletManagerEnv,
        transaction: telegram_api::WalletTransaction,
    ) -> td_api::TonWalletTransaction {
        let mut object = get_ton_wallet_transaction_object(env.get_my_id(), transaction);
        // The peer user id rides the id-object mapping after the
        // key-change my-id substitution folded in the static.
        if object.peer_user_id != 0 {
            object.peer_user_id =
                env.get_user_id_object(object.peer_user_id, "tonWalletTransaction");
        }
        object
    }
}

// The SRP-chain continuations: the port's [blocked: PasswordManager]
// seam parks the closure state (the C++ send_closure lambdas); the
// error arm forwards to the carried promise, the value arm parks with
// the seam (the do_* re-entry rides the net step).

struct SecretPhraseSrp {
    promise: Option<Promise<String>>,
}

impl PromiseInterface<telegram_api::InputCheckPasswordSRP> for SecretPhraseSrp {
    fn set_value(&mut self, _value: telegram_api::InputCheckPasswordSRP) {
        tdutils::log_info!("SecretPhraseSrp — the SRP answer parks [blocked: PasswordManager]");
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

struct DisableBackupSrp {
    promise: Option<Promise<()>>,
}

impl PromiseInterface<telegram_api::InputCheckPasswordSRP> for DisableBackupSrp {
    fn set_value(&mut self, _value: telegram_api::InputCheckPasswordSRP) {
        tdutils::log_info!("DisableBackupSrp — the SRP answer parks [blocked: PasswordManager]");
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

struct DeleteWalletSrp {
    promise: Option<Promise<()>>,
}

impl PromiseInterface<telegram_api::InputCheckPasswordSRP> for DeleteWalletSrp {
    fn set_value(&mut self, _value: telegram_api::InputCheckPasswordSRP) {
        tdutils::log_info!("DeleteWalletSrp — the SRP answer parks [blocked: PasswordManager]");
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

struct ReplaceWalletSrp {
    /// The carried re-entry state (the C++ send_closure lambda captures);
    /// the [blocked: PasswordManager] seam parks until the SRP plane
    /// lands.
    #[allow(dead_code)]
    anchor_public_key: Vec<u8>,
    #[allow(dead_code)]
    public_key: Vec<u8>,
    #[allow(dead_code)]
    proof: telegram_api::WalletOwnershipProof,
    promise: Option<Promise<()>>,
}

impl PromiseInterface<telegram_api::InputCheckPasswordSRP> for ReplaceWalletSrp {
    fn set_value(&mut self, _value: telegram_api::InputCheckPasswordSRP) {
        tdutils::log_info!("ReplaceWalletSrp — the SRP answer parks [blocked: PasswordManager]");
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

/// The transfer-result park: the query-tail promise holder (the C++
/// carries it through the updates_manager promise chain).
struct TransferResultPark {
    promise: Option<Promise<td_api::TonWalletTransferResult>>,
}

impl PromiseInterface<()> for TransferResultPark {
    fn set_value(&mut self, _value: ()) {
        tdutils::log_info!("TransferResultPark — the transfer result parks [blocked: net]");
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_ton_address_vectors() {
        // A canonical mainnet-friendly shape: first byte encodes the
        // 0x11 anycast bits, then 33 workchain/account bytes, then the
        // crc16 over the first 34.
        let mut raw = [0u8; 36];
        raw[0] = 0x11; // (0x11 & 0x3F) == 0x11
        for (i, byte) in raw.iter_mut().enumerate().skip(1) {
            *byte = (i * 7 + 3) as u8;
        }
        let crc = tdutils::crypto::crc16(&raw[..34]);
        raw[34] = (crc >> 8) as u8;
        raw[35] = (crc & 0xFF) as u8;
        // base64url without padding: 36 bytes -> 48 characters.
        let address = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        };
        assert_eq!(address.len(), 48);
        assert!(check_ton_address(&address).is_ok());

        // A flipped checksum byte fails.
        let mut bad = raw;
        bad[35] ^= 1;
        let bad_address = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bad)
        };
        assert!(check_ton_address(&bad_address).is_error());

        // Wrong length.
        assert!(check_ton_address(&address[..47]).is_error());
    }

    /// The recording env: the update outbox + the seam sends.
    struct TestEnv {
        updates: Vec<td_api::Update>,
        fetch_calls: Vec<(String, Vec<u8>, i32)>,
        queries: Vec<&'static str>,
        close: bool,
        options: Vec<(&'static str, i64)>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                updates: Vec::new(),
                fetch_calls: Vec::new(),
                queries: Vec::new(),
                close: false,
                options: vec![("ton_wallet_gasless_transfer_daily_count_max", 100)],
            }
        }
    }

    impl TonWalletUpdateEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            false
        }

        fn auth_is_authorized(&self) -> bool {
            true
        }

        fn now(&self) -> f64 {
            1000.0
        }

        fn unix_time(&self) -> i32 {
            1_700_000_000
        }

        fn get_option_integer(&self, name: &str) -> i64 {
            self.options
                .iter()
                .find(|(option, _)| *option == name)
                .map_or(0, |(_, value)| *value)
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }
    }

    impl TonWalletManagerEnv for TestEnv {
        fn close_status(&self) -> Status {
            if self.close {
                Status::error(500, "Request aborted")
            } else {
                Status::ok()
            }
        }

        fn send_wallet_get_state_query(&mut self) {
            self.queries.push("getState");
        }

        fn send_wallet_get_gasless_info_query(&mut self) {
            self.queries.push("getGaslessInfo");
        }

        fn send_wallet_get_user_addresses_query(
            &mut self,
            _create: bool,
            _input_users: Vec<telegram_api::InputUser>,
            _addresses: Vec<String>,
        ) {
            self.queries.push("getUserAddresses");
        }

        fn send_wallet_fetch_secret_phrase_part_query(
            &mut self,
            token: &str,
            public_key: Vec<u8>,
            dc_id: i32,
        ) {
            self.fetch_calls.push((token.to_owned(), public_key, dc_id));
        }

        fn send_wallet_get_backup_holder_dcs_query(&mut self) {
            self.queries.push("getBackupHolderDcs");
        }

        fn send_payments_get_onramp_providers_query(&mut self, _cryptocurrency: &str) {
            self.queries.push("getOnrampProviders");
        }

        fn send_toncenter_get_streaming_url_query(&mut self) {
            self.queries.push("getStreamingUrl");
        }

        fn send_payments_get_currency_rates_query(&mut self) {
            self.queries.push("getCurrencyRates");
        }
    }

    fn wallet_state_wire() -> telegram_api::WalletState {
        telegram_api::WalletState::walletState(telegram_api::walletState {
            flags: 0,
            backup_enabled: true,
            can_export_phrase: false,
            can_enable_backup: true,
            address: "EQD123".to_owned(),
            public_key: vec![1u8; 32],
            balance: 77,
        })
    }

    type RecordedSlot<T> = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>;

    fn record_string() -> (Promise<String>, RecordedSlot<String>) {
        struct Rec(RecordedSlot<String>);
        impl tdutils::promise::PromiseInterface<String> for Rec {
            fn set_value(&mut self, value: String) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    fn record_unit() -> (Promise<()>, RecordedSlot<()>) {
        struct Rec(RecordedSlot<()>);
        impl tdutils::promise::PromiseInterface<()> for Rec {
            fn set_value(&mut self, _value: ()) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(()));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    #[test]
    fn wallet_state_cache_and_reload_band() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        // The first loadTonWalletState parks at the net seam.
        let (promise, recorded) = record_unit();
        manager.get_wallet_state(&mut env, promise);
        assert_eq!(env.queries, ["getState"]);
        assert!(recorded.lock().unwrap().is_none());

        // The wire state caches + emits; an identical state is a no-op.
        manager.on_update_wallet_state(&mut env, wallet_state_wire());
        assert_eq!(env.updates.len(), 1);
        manager.on_update_wallet_state(&mut env, wallet_state_wire());
        assert_eq!(env.updates.len(), 1);

        // The cached band answers ok without a new query (the reload
        // window is 3500s; the env clock is pinned).
        let (promise, recorded) = record_unit();
        manager.get_wallet_state(&mut env, promise);
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert_eq!(env.queries, ["getState"]);

        // get_current_state streams both cached planes after the gasless
        // update.
        manager.on_update_wallet_gasless_info(
            &mut env,
            telegram_api::updateWalletGaslessInfo {
                flags: 0,
                available: true,
                left: 3,
                reset_at: 0,
                min_amount: 5,
                relayer_address: "EQrelayer".to_owned(),
            },
        );
        assert_eq!(env.updates.len(), 2);
        let mut state = Vec::new();
        manager.get_current_state(&env, &mut state);
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn gasless_reset_bands() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        let now = env.unix_time();
        manager.on_update_wallet_gasless_info(
            &mut env,
            telegram_api::updateWalletGaslessInfo {
                flags: 0,
                available: true,
                left: 3,
                reset_at: now + 10,
                min_amount: 5,
                relayer_address: "EQrelayer".to_owned(),
            },
        );
        // 10s out — the re-arm answer.
        assert_eq!(manager.timeout_expired(&mut env), 11);
        // Past the date: the daily-count option (100) resets the counter.
        let env = TestEnv::default();
        let mut info = WalletGaslessInfo {
            is_available: true,
            left: 3,
            reset_date: env.unix_time() - 1,
            min_amount: 0,
            relayer_address: String::new(),
        };
        assert_eq!(
            info.try_reset(
                env.unix_time(),
                env.get_option_integer("ton_wallet_gasless_transfer_daily_count_max")
            ),
            0
        );
        assert_eq!(info.left, 100);
        assert_eq!(info.reset_date, 0);
    }

    #[test]
    fn ton_connect_session_object_states() {
        let session = telegram_api::TonConnectSession {
            flags: 0,
            pending: false,
            closing: true,
            closed: false,
            id: 9,
            dapp_client_id: "dapp".to_owned(),
            client_id: String::new(),
            nonce: vec![7u8; 16],
            manifest: None,
            manifest_error: 0,
            date: 123,
        };
        let object = get_ton_connect_session_object(&session);
        assert_eq!(object.id, 9);
        assert_eq!(object.creation_date, 123);
        assert!(matches!(
            object.state.as_ref(),
            Some(td_api::TonConnectSessionState::tonConnectSessionStateClosing(_))
        ));
        assert!(matches!(
            object.manifest.as_ref(),
            Some(td_api::TonConnectManifest::tonConnectManifestPending(_))
        ));
        // The manifest error bands: 3 → invalid, 2 → failed.
        assert!(matches!(
            get_ton_connect_manifest_object(None, 3),
            td_api::TonConnectManifest::tonConnectManifestInvalid(_)
        ));
        assert!(matches!(
            get_ton_connect_manifest_object(None, 2),
            td_api::TonConnectManifest::tonConnectManifestFailed(_)
        ));
        // The pending-request object conversion (the GetTonConnectPending
        // answer half).
        let pending = telegram_api::wallet_TonConnectPending {
            session: Some(session),
            requests: vec![telegram_api::TonConnectRequest {
                flags: 0,
                session_id: 9,
                msg_id: 55,
                body: b"{\"method\":\"sendTransaction\"}".to_vec(),
                expires: 99,
                topic: "t".to_owned(),
                trace_id: "tr".to_owned(),
            }],
        };
        let object = get_ton_connect_requests_object(&pending);
        assert_eq!(object.requests.len(), 1);
        assert_eq!(object.requests[0].message_id, 55);
        assert_eq!(object.requests[0].expiration_date, 99);
        assert_eq!(object.session.as_ref().unwrap().id, 9);
    }

    #[test]
    fn transaction_object_types() {
        let base = telegram_api::WalletTransaction {
            flags: 0,
            incoming: false,
            gasless: true,
            failed: false,
            key_change: false,
            comment_encrypted: false,
            id: "tx1".to_owned(),
            amount: 42,
            fee: 1,
            date: 10,
            peer: Some(
                telegram_api::WalletTransactionPeer::walletTransactionPeerAddress(
                    telegram_api::walletTransactionPeerAddress {
                        flags: 0,
                        address: "EQD".to_owned(),
                        domain: "ton.org".to_owned(),
                    },
                ),
            ),
            comment: "hi".to_owned(),
            tx_hash: "hash".to_owned(),
            nft: None,
        };
        // Outgoing transfer negates the amount.
        let object = get_ton_wallet_transaction_object(5, base.clone());
        match object.r#type.as_ref().unwrap() {
            td_api::TonWalletTransactionType::tonWalletTransactionTypeTransfer(transfer) => {
                assert_eq!(transfer.amount, -42);
                assert!(transfer.is_gasless);
            }
            other => panic!("unexpected type {other:?}"),
        }
        // The onramp peer arm.
        let mut onramp = base.clone();
        onramp.peer = Some(
            telegram_api::WalletTransactionPeer::walletTransactionPeerOnramp(
                telegram_api::walletTransactionPeerOnramp {
                    flags: 0,
                    address: "EQD".to_owned(),
                    domain: String::new(),
                    provider_name: "moonpay".to_owned(),
                },
            ),
        );
        match get_ton_wallet_transaction_object(5, onramp)
            .r#type
            .as_ref()
            .unwrap()
        {
            td_api::TonWalletTransactionType::tonWalletTransactionTypeOnRampDeposit(deposit) => {
                assert_eq!(deposit.provider_name, "moonpay");
                assert_eq!(deposit.amount, -42);
            }
            other => panic!("unexpected type {other:?}"),
        }
        // The key-change arm substitutes the my-id peer.
        let mut key_change = base.clone();
        key_change.key_change = true;
        let object = get_ton_wallet_transaction_object(5, key_change);
        assert_eq!(object.peer_user_id, 5);
        assert!(matches!(
            object.r#type.as_ref().unwrap(),
            td_api::TonWalletTransactionType::tonWalletTransactionTypeKeyChange(_)
        ));
        // The user peer arm + the invalid-id band.
        let mut user_peer = base;
        user_peer.peer = Some(
            telegram_api::WalletTransactionPeer::walletTransactionPeerUser(
                telegram_api::walletTransactionPeerUser {
                    flags: 0,
                    user_id: -3,
                    address: "EQD".to_owned(),
                    domain: String::new(),
                },
            ),
        );
        assert_eq!(
            get_ton_wallet_transaction_object(5, user_peer).peer_user_id,
            0
        );
    }

    #[test]
    fn proof_validation_gates() {
        let proof_of = |public_key: Vec<u8>, signature: Vec<u8>| {
            Some(td_api::TonWalletOwnershipProof {
                public_key,
                timestamp: 1,
                signature,
            })
        };
        assert_eq!(
            match get_wallet_ownership_proof(None) {
                Result::Err(status) => status.message().to_owned(),
                Result::Ok(_) => panic!("expected the null-proof error"),
            },
            "Proof must be non-empty"
        );
        assert_eq!(
            match get_wallet_ownership_proof(proof_of(vec![1u8; 31], vec![2u8; 64])) {
                Result::Err(status) => status.message().to_owned(),
                Result::Ok(_) => panic!("expected the public-key error"),
            },
            "Invalid public key specified"
        );
        assert_eq!(
            match get_wallet_ownership_proof(proof_of(vec![1u8; 32], vec![2u8; 63])) {
                Result::Err(status) => status.message().to_owned(),
                Result::Ok(_) => panic!("expected the signature error"),
            },
            "Invalid signature specified"
        );
        assert!(get_wallet_ownership_proof(proof_of(vec![1u8; 32], vec![2u8; 64])).is_ok());
    }

    fn three_holder_dcs() -> Vec<telegram_api::wallet_HolderDc> {
        three_holder_dcs_with_keys().0
    }

    /// The DC rows plus their keychain private ids (the test plays the
    /// DC side: it encrypts its share under the ECDH with the manager's
    /// per-DC temporary public key).
    fn three_holder_dcs_with_keys() -> (
        Vec<telegram_api::wallet_HolderDc>,
        Vec<tde2e::keychain::PrivateKeyId>,
    ) {
        let mut dcs = Vec::new();
        let mut keys = Vec::new();
        for dc in 1..=3 {
            let key = tde2e::keychain::key_generate_temporary_private_key().unwrap();
            dcs.push(telegram_api::wallet_HolderDc {
                dc,
                public_key: tde2e::keychain::key_to_public_key(key).unwrap(),
            });
            keys.push(key);
        }
        (dcs, keys)
    }

    #[test]
    fn backup_holder_dcs_validation() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        let (promise, recorded) = record_string();
        manager.get_ton_wallet_secret_phrase(&mut env, "", promise);
        assert!(!manager.get_backup_holder_dcs_queries.is_empty());
        assert_eq!(env.queries, ["getBackupHolderDcs"]);
        assert!(
            recorded.lock().unwrap().is_none(),
            "the flow parks at the DC load"
        );

        // The DC-id band.
        let mut bad = three_holder_dcs();
        bad[0].dc = 99;
        manager.on_get_backup_holder_dcs(&mut env, Result::Ok(bad));
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Receive invalid DC identifier"
        ));
        let _ = &recorded;

        // The 3-DC band (re-seed the load chain first — the C++ on_get
        // fires per in-flight send).
        let two: Vec<_> = three_holder_dcs().into_iter().take(2).collect();
        manager.load_backup_holder_dcs(&mut env, Promise::default());
        manager.on_get_backup_holder_dcs(&mut env, Result::Ok(two));
        assert!(manager.backup_holder_dcs.is_empty());

        // The good set caches and re-enters the parked phrase flow (which
        // parks at the SRP seam for a non-empty password; the empty
        // password parks at the export query).
        manager.load_backup_holder_dcs(&mut env, Promise::default());
        manager.on_get_backup_holder_dcs(&mut env, Result::Ok(three_holder_dcs()));
        assert_eq!(manager.backup_holder_dcs.len(), 3);
        assert!(!env.queries.contains(&"getState"));
    }

    #[test]
    fn enable_backup_xor_flow() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        manager.load_backup_holder_dcs(&mut env, Promise::default());
        manager.on_get_backup_holder_dcs(&mut env, Result::Ok(three_holder_dcs()));
        let proof = Some(td_api::TonWalletOwnershipProof {
            public_key: vec![1u8; 32],
            timestamp: 1,
            signature: vec![2u8; 64],
        });
        // The charset gate answers before the split.
        let (promise, recorded) = record_unit();
        manager.enable_ton_wallet_backup(&mut env, "not a phrase!", proof.clone(), promise);
        match recorded.lock().unwrap().as_ref() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!(status.message(), "Receive invalid secret phrase")
            }
            other => panic!("unexpected result {other:?}"),
        }
        // The oversize gate.
        let (promise, recorded) = record_unit();
        manager.enable_ton_wallet_backup(
            &mut env,
            &"a".repeat(MAX_MNEMONIC_BACKUP_SIZE + 1),
            proof.clone(),
            promise,
        );
        match recorded.lock().unwrap().as_ref() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!(status.message(), "Invalid secret phrase specified")
            }
            other => panic!("unexpected result {other:?}"),
        }
        // The valid phrase runs the 3-way split (the enable query parks).
        let (promise, recorded) = record_unit();
        manager.enable_ton_wallet_backup(&mut env, "ab cd ef", proof, promise);
        assert!(recorded.lock().unwrap().is_none());
    }

    #[test]
    fn secret_phrase_xor_assembly_round_trip() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        let dcs = three_holder_dcs();
        manager.load_backup_holder_dcs(&mut env, Promise::default());
        manager.on_get_backup_holder_dcs(&mut env, Result::Ok(dcs.clone()));
        let (promise, recorded) = record_string();
        manager.do_get_ton_wallet_secret_phrase_with_parts(
            &mut env,
            telegram_api::wallet_SecretPhraseParts {
                token: "tok".to_owned(),
                dcs: vec![1, 2, 3],
            },
            promise,
        );
        assert_eq!(env.fetch_calls.len(), 3);

        // The XOR shares: phrase padded to the mnemonic size, a and b
        // random; c = a ^ b ^ padded.
        let phrase = b"ab cd ef";
        let mut padded = [b' '; MAX_MNEMONIC_BACKUP_SIZE];
        padded[..phrase.len()].copy_from_slice(phrase);
        let mut a = [0u8; MAX_MNEMONIC_BACKUP_SIZE];
        let mut b = [0u8; MAX_MNEMONIC_BACKUP_SIZE];
        tdutils::random::secure_bytes(&mut a);
        tdutils::random::secure_bytes(&mut b);
        let mut c = [0u8; MAX_MNEMONIC_BACKUP_SIZE];
        for i in 0..MAX_MNEMONIC_BACKUP_SIZE {
            c[i] = a[i] ^ b[i] ^ padded[i];
        }
        let shares = [a, b, c];

        let (&query_id, entry) = manager.get_secret_phrase_queries.iter().next().unwrap();
        let dc_keys = entry.dc_private_keys.clone();
        assert_eq!(dc_keys.len(), 3);
        for (i, (_dc_id, _)) in dc_keys.iter().enumerate() {
            // The DC side: encrypt share i under the ECDH of the DC's own
            // key with the manager's per-DC temporary public key (the
            // fetch seam recorded it); the 288-byte form is
            // public_key(32) + encrypted(256) with the DC's fresh pair.
            let sender = tde2e::keychain::key_generate_temporary_private_key().unwrap();
            let sender_public = tde2e::keychain::key_to_public_key(sender).unwrap();
            let temp_public_id =
                tde2e::keychain::key_from_public_key(&env.fetch_calls[i].1).unwrap();
            let symmetric = tde2e::keychain::key_from_ecdh(sender, temp_public_id).unwrap();
            let mut message = vec![0x08, 0xdd, 0x90, 0x8b, 0xd7];
            message.extend_from_slice(&shares[i]);
            let encrypted = tde2e::keychain::encrypt_message_for_one(symmetric, &message).unwrap();
            let mut data = sender_public;
            data.extend_from_slice(&encrypted);
            assert_eq!(data.len(), 288);
            // The wrong private key is rejected (the membership gate).
            manager.on_get_ton_wallet_secret_phrase_part(
                Result::Ok(data.clone()),
                query_id,
                987654,
            );
            manager.on_get_ton_wallet_secret_phrase_part(Result::Ok(data), query_id, dc_keys[i].1);
        }
        match recorded.lock().unwrap().as_ref() {
            Some(tdutils::status::Result::Ok(phrase)) => assert_eq!(phrase, "ab cd ef"),
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    fn streaming_url_and_rates_caches() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        // The park (the query seam), then the fill.
        manager.get_ton_center_streaming_api_url(&mut env, Promise::default());
        assert_eq!(env.queries.last().copied(), Some("getStreamingUrl"));
        manager.on_get_ton_center_streaming_api_url(
            &env,
            Result::Ok(telegram_api::toncenter_StreamingUrl {
                url: "wss://ton".to_owned(),
                expires: env.unix_time() + 3600,
            }),
        );
        // The 60-second cache band answers.
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        struct Rec(std::sync::Arc<std::sync::Mutex<Option<td_api::TonCenterStreamingApiUrl>>>);
        impl tdutils::promise::PromiseInterface<td_api::TonCenterStreamingApiUrl> for Rec {
            fn set_value(&mut self, value: td_api::TonCenterStreamingApiUrl) {
                *self.0.lock().unwrap() = Some(value);
            }
            fn set_error(&mut self, _error: Status) {}
        }
        let promise = Promise::from_interface(Rec(recorded.clone()));
        manager.get_ton_center_streaming_api_url(&mut env, promise);
        let cached = recorded.lock().unwrap().take().unwrap();
        assert_eq!(cached.url, "wss://ton");
        assert!(cached.expires_in >= 60);

        // The invalid-url band fails the promises.
        let mut manager = TonWalletManager::new();
        manager.get_ton_center_streaming_api_url(&mut env, Promise::default());
        manager.on_get_ton_center_streaming_api_url(
            &env,
            Result::Ok(telegram_api::toncenter_StreamingUrl {
                url: String::new(),
                expires: 0,
            }),
        );
        assert!(manager.get_streaming_api_url_queries.is_empty());

        // The currency-rates cache fills and answers.
        manager.get_currency_rates(&mut env, Promise::default());
        assert_eq!(env.queries.last().copied(), Some("getCurrencyRates"));
        manager.on_get_currency_rates(
            &env,
            Result::Ok(telegram_api::payments_CurrencyRates {
                rates: vec![telegram_api::CurrencyRate {
                    currency: "TON".to_owned(),
                    rate: 5.5,
                }],
            }),
        );
        assert_eq!(manager.currency_rates.rates.len(), 1);
    }

    #[test]
    fn claim_and_answer_gates() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        let (promise, recorded) = record_unit();
        manager.claim_ton_connect_request(&mut env, 1, 12345, "req", false, promise);
        // 12345 is a local-band message id — the server gate rejects.
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Invalid message identifier specified"
        ));
        let (promise, recorded) = record_unit();
        manager.submit_ton_connect_response(&mut env, 1, 9999999999, "", b"", promise);
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Invalid message identifier specified"
        ));
    }

    #[test]
    fn transfer_gates() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        struct Rec(
            std::sync::Arc<
                std::sync::Mutex<Option<tdutils::status::Result<td_api::TonWalletTransferResult>>>,
            >,
        );
        impl tdutils::promise::PromiseInterface<td_api::TonWalletTransferResult> for Rec {
            fn set_value(&mut self, value: td_api::TonWalletTransferResult) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let record = || {
            let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
            (Promise::from_interface(Rec(slot.clone())), slot)
        };
        let address = {
            let mut raw = [0u8; 36];
            raw[0] = 0x11;
            for (i, byte) in raw.iter_mut().enumerate().skip(1) {
                *byte = (i * 7 + 3) as u8;
            }
            let crc = tdutils::crypto::crc16(&raw[..34]);
            raw[34] = (crc >> 8) as u8;
            raw[35] = (crc & 0xFF) as u8;
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        };
        // The amount gate (0 and below-min).
        let (promise, recorded) = record();
        manager.send_ton_wallet_transfer(
            &mut env,
            vec![],
            vec![],
            0,
            &address,
            0,
            "",
            false,
            0,
            promise,
        );
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Invalid transfer amount specified"
        ));
        // The address gate.
        let (promise, recorded) = record();
        manager.send_ton_wallet_transfer(
            &mut env,
            vec![],
            vec![],
            0,
            "bad",
            10,
            "",
            false,
            0,
            promise,
        );
        assert!(matches!(
            recorded.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Invalid address length"
        ));
        // The local-message + query tails park.
        let (promise, recorded) = record();
        manager.send_ton_wallet_transfer(
            &mut env,
            vec![1],
            vec![],
            0,
            &address,
            10,
            "c",
            false,
            3,
            promise,
        );
        assert!(recorded.lock().unwrap().is_none());
    }

    #[test]
    fn toncenter_type_dispatch_bands() {
        let mut env = TestEnv::default();
        let mut manager = TonWalletManager::new();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        struct Rec(std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<String>>>>);
        impl tdutils::promise::PromiseInterface<String> for Rec {
            fn set_value(&mut self, value: String) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let (promise, slot) = (Promise::from_interface(Rec(recorded.clone())), recorded);
        manager.perform_ton_center_api_request(&mut env, "/v2", None, promise);
        assert!(matches!(
            slot.lock().unwrap().as_ref(),
            Some(tdutils::status::Result::Err(status)) if status.message() == "Item type must be non-empty"
        ));
    }
}
