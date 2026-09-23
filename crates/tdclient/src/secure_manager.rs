//! Port of `td/telegram/SecureManager.{h,cpp}` (M.44) — the Telegram
//! Passport manager: the get/set/delete of the secure values, the
//! authorization-form flow and the preferred-country-language lookup.
//!
//! Seams (the [`SecureManagerEnv`] net/manager surface):
//! - the seven `Td::ResultHandler` queries (`GetSecureValue`,
//!   `GetAllSecureValues`, `SetSecureValue`, `DeleteSecureValue`,
//!   `SetSecureValueErrorsQuery`, `GetPassportAuthorizationForm`,
//!   `GetPassportConfig`) plus the container-dispatched
//!   `account.acceptAuthorization` are the `[blocked: net]` query seams —
//!   the sends log and the promises park (the M.31 convention); the answer
//!   handlers (`on_get_secure_value_result`,
//!   `on_get_all_secure_values_result`, `on_set_secure_value_result`,
//!   `on_delete_secure_value_result`, `on_set_secure_value_errors_result`,
//!   `on_get_passport_authorization_form`,
//!   `on_get_passport_authorization_form_secret`,
//!   `on_accept_authorization_result`, `on_get_passport_config_result`)
//!   are real and unit-tested.
//! - `PasswordManager::get_secure_secret`/`drop_cached_secret` are
//!   [blocked: PasswordManager] env seams (the manager is its own track);
//!   the collapsed secret answer is synchronous — a seam error fails the
//!   query promise exactly like the C++ `on_secret` → `on_error` hop.
//! - the `SetSecureValue` upload machine's
//!   `FileManager::resume_upload(UploadCallback)` contract has no secure
//!   variant on this base (`files.rs`'s [`crate::files::UploadCallback`]
//!   carries no `on_upload_secure_ok`), so the upload legs record the
//!   requests through [`SecureManagerEnv::file_resume_upload_secure`] and
//!   the `on_upload_ok`-advance never fires: file-carrying values park at
//!   the upload, exactly like the C++ before its uploads complete.
//!
//! Recorded divergences:
//! - the C++ actor machinery (`refcnt_`/`hangup_shared`, the per-query
//!   child actors `GetSecureValue`/`GetAllSecureValues`/`SetSecureValue`/
//!   `DeleteSecureValue`/`GetPassportConfig` and the
//!   `Container<Promise<NetQueryPtr>>`) collapses into manager state: the
//!   child queries are pending records, the net answers arrive through the
//!   `*_result` handlers keyed by type/FIFO (the C++ binds them per
//!   actor; the collapsed net step will route by request token).
//! - the C++ `get_secure_value`/`set_secure_value` answer lambdas check
//!   `file_manager == nullptr` (SecureManager.cpp:743-746) before the
//!   conversion; the collapsed env always carries the M.12 file manager,
//!   so the null leg is unobservable here.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use crate::files::{FileId, FileUploadId};
use crate::secure_storage::Secret;
use crate::secure_value::{
    self, DatedFile, SecureFileCredentials, SecureValueCredentials, SecureValueEnv,
    SecureValueType, SecureValueWithCredentials, SuitableSecureValue, decrypt_secure_value,
    get_encrypted_secure_value, get_input_secure_value_object, get_passport_element_object,
    get_passport_element_type_object, get_passport_required_elements_object,
    get_secure_credentials_encrypted_object, get_secure_value_data_field_name,
    get_secure_value_type, get_secure_value_type_td_api, get_suitable_secure_value,
};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// `SecureManager::TdApiSecureValue` (SecureManager.h:31) — the nullable
/// `PassportElement` promise payload (the C++ nullptr is `None`).
pub type TdApiSecureValue = Option<td_api::PassportElement>;
/// `SecureManager::TdApiSecureValues` (SecureManager.h:32).
pub type TdApiSecureValues = td_api::PassportElements;
/// `SecureManager::TdApiSecureValuesWithErrors` (SecureManager.h:33).
pub type TdApiSecureValuesWithErrors = td_api::PassportElementsWithErrors;
/// `SecureManager::TdApiAuthorizationForm` (SecureManager.h:34).
pub type TdApiAuthorizationForm = td_api::PassportAuthorizationForm;

/// The `G()`/`td_` surface of the manager on top of the value plane
/// ([`SecureValueEnv`]). The net queries are the `[blocked: net]` seams;
/// the secret surface is [blocked: PasswordManager].
pub trait SecureManagerEnv: SecureValueEnv {
    /// `td_->user_manager_->on_get_users(users, source)` (the
    /// authorization-form intake, SecureManager.cpp:998-999).
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->user_manager_->get_input_user(user_id)` (Requests.cpp:8815).
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser>;

    /// `G()->is_expected_error(error)` (Global.h:723-739) — the faithful
    /// code set over [`SecureValueEnv::close_flag`].
    fn is_expected_error(&self, error: &Status) -> bool {
        if error.code() == 401 {
            // authorization is lost
            return true;
        }
        if error.code() == 420 || error.code() == 429 {
            // flood wait
            return true;
        }
        if error.code() == 406 && error.message() == "FROZEN_METHOD_INVALID" {
            // the account is frozen
            return true;
        }
        self.close_flag()
    }

    /// `PasswordManager::get_secure_secret(password, promise)`
    /// (SecureManager.cpp:225-228 and friends) — [blocked:
    /// PasswordManager]; the default seam reports the missing manager.
    fn get_secure_secret(&mut self, password: &str) -> Result<Secret> {
        let _ = password;
        tdutils::log_info!("PasswordManager::get_secure_secret — [blocked: PasswordManager]");
        Result::Err(Status::error(500, "PasswordManager is not implemented yet"))
    }

    /// `PasswordManager::drop_cached_secret` (SecureManager.cpp:179 and
    /// friends) — [blocked: PasswordManager].
    fn drop_cached_secret(&mut self) {
        tdutils::log_info!("PasswordManager::drop_cached_secret — [blocked: PasswordManager]");
    }

    /// `GetSecureValue::start_up` (SecureManager.cpp:217-223) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_get_secure_value_result`].
    fn send_get_secure_value_query(&mut self, type_: SecureValueType) {
        tdutils::log_info!("Send account.getSecureValue — [blocked: net]");
        let _ = type_;
    }

    /// `GetAllSecureValues::start_up` (SecureManager.cpp:300-303) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_get_all_secure_values_result`].
    fn send_get_all_secure_values_query(&mut self) {
        tdutils::log_info!("Send account.getAllSecureValues — [blocked: net]");
    }

    /// `SetSecureValue::loop` (SecureManager.cpp:578-582) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_set_secure_value_result`].
    fn send_save_secure_value_query(
        &mut self,
        value: telegram_api::InputSecureValue,
        secure_secret_id: i64,
    ) {
        tdutils::log_info!(
            "Send account.saveSecureValue(secure_secret_id = {secure_secret_id}) — [blocked: net]"
        );
        let _ = value;
    }

    /// `DeleteSecureValue::start_up` (SecureManager.cpp:671-676) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_delete_secure_value_result`].
    fn send_delete_secure_value_query(&mut self, type_: SecureValueType) {
        tdutils::log_info!("Send account.deleteSecureValue — [blocked: net]");
        let _ = type_;
    }

    /// `SetSecureValueErrorsQuery::send` (SecureManager.cpp:146-150) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_set_secure_value_errors_result`].
    fn send_set_secure_value_errors_query(
        &mut self,
        input_user: telegram_api::InputUser,
        input_errors: Vec<telegram_api::SecureValueError>,
    ) {
        tdutils::log_info!("Send users.setSecureValueErrors — [blocked: net]");
        let _ = (input_user, input_errors);
    }

    /// `GetPassportAuthorizationForm::start_up` (SecureManager.cpp:716-721)
    /// — `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_get_passport_authorization_form`].
    fn send_get_authorization_form_query(
        &mut self,
        bot_user_id: UserId,
        scope: &str,
        public_key: &str,
    ) {
        tdutils::log_info!("Send account.getAuthorizationForm — [blocked: net]");
        let _ = (bot_user_id, scope, public_key);
    }

    /// `SecureManager::send_passport_authorization_form`'s
    /// `account.acceptAuthorization` (SecureManager.cpp:1284-1293) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_accept_authorization_result`].
    fn send_accept_authorization_query(
        &mut self,
        query: telegram_api::account_acceptAuthorization,
    ) {
        tdutils::log_info!("Send account.acceptAuthorization — [blocked: net]");
        let _ = query;
    }

    /// `GetPassportConfig::start_up` (SecureManager.cpp:772-775) —
    /// `[blocked: net]`; the answer arrives through
    /// [`SecureManager::on_get_passport_config_result`].
    fn send_get_passport_config_query(&mut self, hash: i32) {
        tdutils::log_info!("Send help.getPassportConfig(hash = {hash}) — [blocked: net]");
    }
}

// ---------------------------------------------------------------------------
// The collapsed child queries
// ---------------------------------------------------------------------------

/// The collapsed `GetSecureValue` query (SecureManager.cpp:35-54).
struct GetSecureValueQuery {
    promise: Promise<TdApiSecureValue>,
    encrypted_secure_value: Option<secure_value::EncryptedSecureValue>,
    secret: Option<Secret>,
}

impl GetSecureValueQuery {
    /// `GetSecureValue::loop` (SecureManager.cpp:200-215) — decrypts once
    /// both the answer and the secret are here; `Ok(false)` keeps the
    /// query parked.
    fn try_loop<Env: SecureManagerEnv>(
        &mut self,
        manager: &mut SecureManager,
        env: &mut Env,
    ) -> Result<bool> {
        let (Some(encrypted_secure_value), Some(secret)) =
            (&self.encrypted_secure_value, &self.secret)
        else {
            return Result::Ok(false);
        };

        let secure_value_with_credentials =
            match decrypt_secure_value(env, secret, encrypted_secure_value) {
                Result::Ok(value) => value,
                Result::Err(error) => return Result::Err(error),
            };

        manager.on_get_secure_value(secure_value_with_credentials.clone());

        // the request-side collapse of the `get_secure_value` lambda
        // (SecureManager.cpp:737-753): the conversion error answers the
        // C++ nullptr (the `Td::send_result` 404 rewrite)
        let element = match get_passport_element_object(env, &secure_value_with_credentials.value) {
            Result::Ok(element) => Some(element),
            Result::Err(error) => {
                tdutils::log_error!("Failed to get passport element object: {error}");
                None
            }
        };
        self.promise.set_value(element);
        Result::Ok(true)
    }

    /// `GetSecureValue::on_error` (SecureManager.cpp:177-187).
    fn on_error(&mut self, env: &mut impl SecureManagerEnv, error: Status) {
        if error.message() == "SECURE_SECRET_REQUIRED" {
            env.drop_cached_secret();
        }
        if error.code() > 0 {
            self.promise.set_error(error);
        } else {
            self.promise.set_error(Status::error(400, error.message()));
        }
    }
}

/// The collapsed `GetAllSecureValues` query (SecureManager.cpp:56-73).
struct GetAllSecureValuesQuery {
    promise: Promise<TdApiSecureValues>,
    encrypted_secure_values: Option<Vec<secure_value::EncryptedSecureValue>>,
    secret: Option<Secret>,
}

impl GetAllSecureValuesQuery {
    /// `GetAllSecureValues::loop` (SecureManager.cpp:279-298).
    fn try_loop<Env: SecureManagerEnv>(
        &mut self,
        manager: &mut SecureManager,
        env: &mut Env,
    ) -> Result<bool> {
        let (Some(encrypted_secure_values), Some(secret)) =
            (&self.encrypted_secure_values, &self.secret)
        else {
            return Result::Ok(false);
        };

        let secure_values =
            secure_value::decrypt_secure_values(env, secret, encrypted_secure_values);
        for secure_value in &secure_values {
            manager.on_get_secure_value(secure_value.clone());
        }

        let secure_values: Vec<secure_value::SecureValue> =
            secure_values.into_iter().map(|value| value.value).collect();
        self.promise
            .set_value(secure_value::get_passport_elements_object(
                env,
                &secure_values,
            ));
        Result::Ok(true)
    }

    /// `GetAllSecureValues::on_error` (SecureManager.cpp:256-266).
    fn on_error(&mut self, env: &mut impl SecureManagerEnv, error: Status) {
        if error.message() == "SECURE_SECRET_REQUIRED" {
            env.drop_cached_secret();
        }
        if error.code() > 0 {
            self.promise.set_error(error);
        } else {
            self.promise.set_error(Status::error(400, error.message()));
        }
    }
}

/// `SetSecureValue::State` (SecureManager.cpp:98).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetSecureValueState {
    WaitSecret,
    WaitSetValue,
}

/// The collapsed `SetSecureValue` query (SecureManager.cpp:75-137).
struct SetSecureValueQuery {
    password: String,
    secure_value: secure_value::SecureValue,
    promise: Promise<TdApiSecureValue>,
    secret: Option<Secret>,
    state: SetSecureValueState,
    files_left_to_upload: usize,
    upload_generation: u32,
    files_to_upload: Vec<secure_value::SecureInputFile>,
    translations_to_upload: Vec<secure_value::SecureInputFile>,
    front_side: Option<secure_value::SecureInputFile>,
    reverse_side: Option<secure_value::SecureInputFile>,
    selfie: Option<secure_value::SecureInputFile>,
}

impl SetSecureValueQuery {
    fn new(
        password: String,
        secure_value: secure_value::SecureValue,
        promise: Promise<TdApiSecureValue>,
    ) -> SetSecureValueQuery {
        SetSecureValueQuery {
            password,
            secure_value,
            promise,
            secret: None,
            state: SetSecureValueState::WaitSecret,
            files_left_to_upload: 0,
            upload_generation: 0,
            files_to_upload: Vec::new(),
            translations_to_upload: Vec::new(),
            front_side: None,
            reverse_side: None,
            selfie: None,
        }
    }

    fn view_main_file_id(env: &mut impl SecureManagerEnv, file_id: FileId) -> FileId {
        env.file_view(file_id)
            .map(|view| view.main_file_id)
            .unwrap_or_default()
    }

    /// `SetSecureValue::start_up` (SecureManager.cpp:414-489) — the
    /// duplicate-file detection.
    fn start_up(&mut self, env: &mut impl SecureManagerEnv) -> Result<()> {
        // Remove duplicate files
        let mut front_side_file_id = FileId::default();
        if self.secure_value.front_side.file_id.is_valid() {
            front_side_file_id = Self::view_main_file_id(env, self.secure_value.front_side.file_id);
            self.front_side = Some(secure_value::SecureInputFile::default());
        }
        let mut reverse_side_file_id = FileId::default();
        if self.secure_value.reverse_side.file_id.is_valid() {
            reverse_side_file_id =
                Self::view_main_file_id(env, self.secure_value.reverse_side.file_id);
            self.reverse_side = Some(secure_value::SecureInputFile::default());
            if front_side_file_id == reverse_side_file_id {
                return Result::Err(Status::error(
                    400,
                    "Front side and reverse side must be different",
                ));
            }
        }
        let mut selfie_file_id = FileId::default();
        if self.secure_value.selfie.file_id.is_valid() {
            selfie_file_id = Self::view_main_file_id(env, self.secure_value.selfie.file_id);
            self.selfie = Some(secure_value::SecureInputFile::default());
            if front_side_file_id == selfie_file_id {
                return Result::Err(Status::error(
                    400,
                    "Front side and selfie must be different",
                ));
            }
            if reverse_side_file_id == selfie_file_id {
                return Result::Err(Status::error(
                    400,
                    "Reverse side and selfie must be different",
                ));
            }
        }

        if !self.secure_value.files.is_empty() {
            assert!(!front_side_file_id.is_valid());
            assert!(!reverse_side_file_id.is_valid());
            assert!(!selfie_file_id.is_valid());
            // the C++ iterator-erase walk: drop files whose main file id
            // repeats an earlier one
            let files = std::mem::take(&mut self.secure_value.files);
            let mut kept: Vec<(DatedFile, FileId)> = Vec::with_capacity(files.len());
            for file in files {
                let main_file_id = Self::view_main_file_id(env, file.file_id);
                let is_duplicate = kept.iter().any(|(_, other)| *other == main_file_id);
                if !is_duplicate {
                    kept.push((file, main_file_id));
                }
            }
            self.secure_value.files = kept.into_iter().map(|(file, _)| file).collect();
        }
        if !self.secure_value.translations.is_empty() {
            let translations = std::mem::take(&mut self.secure_value.translations);
            let mut kept: Vec<(DatedFile, FileId)> = Vec::with_capacity(translations.len());
            for file in translations {
                let main_file_id = Self::view_main_file_id(env, file.file_id);
                let mut is_duplicate = main_file_id == front_side_file_id
                    || main_file_id == reverse_side_file_id
                    || main_file_id == selfie_file_id;
                if !is_duplicate {
                    is_duplicate = kept.iter().any(|(_, other)| *other == main_file_id);
                }
                if !is_duplicate {
                    is_duplicate = self.secure_value.files.iter().any(|dated_file| {
                        Self::view_main_file_id(env, dated_file.file_id) == main_file_id
                    });
                }
                if !is_duplicate {
                    kept.push((file, main_file_id));
                }
            }
            self.secure_value.translations = kept.into_iter().map(|(file, _)| file).collect();
        }

        Result::Ok(())
    }

    /// `SetSecureValue::load_secret` (SecureManager.cpp:491-497) — the
    /// collapsed synchronous secret answer.
    fn load_secret(&mut self, env: &mut impl SecureManagerEnv) -> Result<()> {
        self.secret = None;
        match env.get_secure_secret(&self.password) {
            Result::Ok(secret) => {
                self.secret = Some(secret);
                Result::Ok(())
            }
            Result::Err(error) => Result::Err(error),
        }
    }

    /// `SetSecureValue::cancel_upload` (SecureManager.cpp:499-521).
    fn cancel_upload(&mut self, env: &mut impl SecureManagerEnv) {
        self.upload_generation += 1;
        for file_info in &self.files_to_upload {
            env.file_cancel_upload(file_info.file_upload_id);
        }
        for file_info in &self.translations_to_upload {
            env.file_cancel_upload(file_info.file_upload_id);
        }
        if let Some(front_side) = &self.front_side {
            env.file_cancel_upload(front_side.file_upload_id);
        }
        if let Some(reverse_side) = &self.reverse_side {
            env.file_cancel_upload(reverse_side.file_upload_id);
        }
        if let Some(selfie) = &self.selfie {
            env.file_cancel_upload(selfie.file_upload_id);
        }
        self.files_left_to_upload = 0;
    }

    /// `SetSecureValue::start_upload` (SecureManager.cpp:550-564) — the
    /// file slot is allocated and the upload request recorded; the
    /// secure-upload callback contract is [blocked], so the slot stays
    /// pending (see the module notes).
    fn start_upload(
        &mut self,
        env: &mut impl SecureManagerEnv,
        file_id: FileId,
        info: &mut secure_value::SecureInputFile,
    ) {
        let mut force = false;
        let file_id = if !info.file_upload_id.file_id.is_valid() {
            let is_encrypted_secure = env
                .file_view(file_id)
                .is_some_and(|view| view.is_encrypted_secure);
            if !is_encrypted_secure {
                env.file_copy_file_id(file_id)
            } else {
                file_id
            }
        } else {
            force = true;
            info.file_upload_id.file_id
        };

        info.file_upload_id = FileUploadId::new(file_id, env.file_get_internal_upload_id());
        env.file_resume_upload_secure(info.file_upload_id, force);
        self.files_left_to_upload += 1;
    }

    /// `SetSecureValue::start_upload_all` (SecureManager.cpp:523-548).
    fn start_upload_all(&mut self, env: &mut impl SecureManagerEnv) {
        if self.files_left_to_upload != 0 {
            self.cancel_upload(env);
        }
        self.upload_generation += 1;

        self.files_to_upload =
            vec![secure_value::SecureInputFile::default(); self.secure_value.files.len()];
        for (i, dated_file) in self.secure_value.files.clone().iter().enumerate() {
            let file_id = dated_file.file_id;
            let mut slot = std::mem::take(&mut self.files_to_upload[i]);
            self.start_upload(env, file_id, &mut slot);
            self.files_to_upload[i] = slot;
        }
        self.translations_to_upload =
            vec![secure_value::SecureInputFile::default(); self.secure_value.translations.len()];
        for (i, dated_file) in self.secure_value.translations.clone().iter().enumerate() {
            let file_id = dated_file.file_id;
            let mut slot = std::mem::take(&mut self.translations_to_upload[i]);
            self.start_upload(env, file_id, &mut slot);
            self.translations_to_upload[i] = slot;
        }
        if self.front_side.is_some() {
            let mut input = self.front_side.take().unwrap();
            let file_id = self.secure_value.front_side.file_id;
            self.start_upload(env, file_id, &mut input);
            self.front_side = Some(input);
        }
        if self.reverse_side.is_some() {
            let mut input = self.reverse_side.take().unwrap();
            let file_id = self.secure_value.reverse_side.file_id;
            self.start_upload(env, file_id, &mut input);
            self.reverse_side = Some(input);
        }
        if self.selfie.is_some() {
            let mut input = self.selfie.take().unwrap();
            let file_id = self.secure_value.selfie.file_id;
            self.start_upload(env, file_id, &mut input);
            self.selfie = Some(input);
        }
    }

    /// `SetSecureValue::loop` (SecureManager.cpp:566-585) — builds and
    /// sends `account.saveSecureValue` once the secret is here and no
    /// upload is pending.
    fn try_loop(&mut self, env: &mut impl SecureManagerEnv) -> bool {
        if self.state != SetSecureValueState::WaitSecret {
            return false;
        }
        let Some(secret) = &self.secret else {
            return false;
        };
        if self.files_left_to_upload != 0 {
            return false;
        }

        let encrypted_secure_value =
            secure_value::encrypt_secure_value(env, secret, &self.secure_value);
        let input_secure_value = get_input_secure_value_object(
            env,
            &encrypted_secure_value,
            &mut self.files_to_upload,
            &mut self.front_side,
            &mut self.reverse_side,
            &mut self.selfie,
            &mut self.translations_to_upload,
        );
        env.send_save_secure_value_query(input_secure_value, secret.get_hash());
        self.state = SetSecureValueState::WaitSetValue;
        true
    }

    /// `SetSecureValue::on_error` (SecureManager.cpp:394-401).
    fn on_error(&mut self, error: Status) {
        if error.code() > 0 {
            self.promise.set_error(error);
        } else {
            self.promise.set_error(Status::error(400, error.message()));
        }
    }

    /// `SetSecureValue::hangup` (SecureManager.cpp:587-589) — the
    /// replacement query cancels the parked one.
    fn on_hangup(&mut self) {
        self.on_error(Status::error(406, "Request canceled"));
    }

    /// `SetSecureValue::merge` (SecureManager.cpp:649-658).
    fn merge(
        &self,
        env: &mut impl SecureManagerEnv,
        file_id: FileId,
        encrypted_file: &secure_value::EncryptedSecureFile,
    ) {
        let Some(file_view) = env.file_view(file_id) else {
            panic!("CHECK failed: !file_view.empty()");
        };
        assert!(file_view.encryption_key.has_value_hash());
        if file_view.encryption_key.value_hash() != encrypted_file.file_hash.as_slice() {
            tdutils::log_error!("Hash mismatch");
            return;
        }
        if let Result::Err(status) = env.file_merge(encrypted_file.file.file_id, file_id) {
            tdutils::log_info!("{status}");
        }
    }
}

/// `SecureManager::AuthorizationForm` (SecureManager.h:68-78) plus the
/// collapsed answer promise.
struct AuthorizationForm {
    bot_user_id: UserId,
    scope: String,
    public_key: String,
    nonce: String,
    is_received: bool,
    is_decrypted: bool,
    options: BTreeMap<SecureValueType, SuitableSecureValue>,
    values: Vec<telegram_api::SecureValue>,
    errors: Vec<telegram_api::SecureValueError>,
    /// the collapsed `on_get_passport_authorization_form` promise
    promise: Option<Promise<TdApiAuthorizationForm>>,
}

/// The port of `td::SecureManager`.
#[derive(Default)]
pub struct SecureManager {
    /// `set_secure_value_queries_`.
    set_secure_value_queries: BTreeMap<SecureValueType, SetSecureValueQuery>,
    /// `secure_value_cache_`.
    secure_value_cache: BTreeMap<SecureValueType, SecureValueWithCredentials>,
    /// `authorization_forms_` (the C++ `unique_ptr` values).
    authorization_forms: BTreeMap<i32, AuthorizationForm>,
    /// `max_authorization_form_id_`.
    max_authorization_form_id: i32,
    /// The collapsed `GetSecureValue` child queries (FIFO for the net
    /// answers).
    pending_get_secure_values: VecDeque<GetSecureValueQuery>,
    /// The collapsed `GetAllSecureValues` child query.
    pending_get_all_secure_values: Option<GetAllSecureValuesQuery>,
    /// The collapsed `DeleteSecureValue` child queries.
    pending_delete_secure_values: VecDeque<(SecureValueType, Promise<()>)>,
    /// The collapsed `SetSecureValueErrorsQuery` promises.
    pending_set_secure_value_errors: VecDeque<Promise<()>>,
    /// The collapsed `account.acceptAuthorization` promises.
    pending_accept_authorizations: VecDeque<Promise<()>>,
    /// The collapsed `GetPassportConfig` child queries.
    pending_passport_configs: VecDeque<(String, Promise<Option<td_api::Text>>)>,
}

impl SecureManager {
    /// `SecureManager::SecureManager` (SecureManager.cpp:733-734).
    pub fn new() -> SecureManager {
        SecureManager::default()
    }

    /// `SecureManager::on_get_secure_value` (SecureManager.cpp:814-817).
    pub fn on_get_secure_value(&mut self, value: SecureValueWithCredentials) {
        let type_ = value.value.type_;
        self.secure_value_cache.insert(type_, value);
    }

    // --- get_secure_value (SecureManager.cpp:736-758 + GetSecureValue) ---

    /// `SecureManager::get_secure_value`: the query parks, the net seam
    /// sends and the secret is requested (the collapsed synchronous answer
    /// either arms the decrypt loop or fails the promise like the C++
    /// `on_secret` → `on_error` hop).
    pub fn get_secure_value(
        &mut self,
        env: &mut impl SecureManagerEnv,
        password: &str,
        type_: SecureValueType,
        promise: Promise<TdApiSecureValue>,
    ) {
        let mut query = GetSecureValueQuery {
            promise,
            encrypted_secure_value: None,
            secret: None,
        };
        env.send_get_secure_value_query(type_);

        match env.get_secure_secret(password) {
            Result::Ok(secret) => {
                query.secret = Some(secret);
                self.pending_get_secure_values.push_back(query);
            }
            Result::Err(error) => query.on_error(env, error),
        }
    }

    /// `GetSecureValue::on_result` (SecureManager.cpp:231-249) — the net
    /// answer; resolves the oldest parked query.
    pub fn on_get_secure_value_result(
        &mut self,
        env: &mut impl SecureManagerEnv,
        r_result: Result<Vec<telegram_api::SecureValue>>,
    ) {
        let Some(mut query) = self.pending_get_secure_values.pop_front() else {
            return;
        };
        let result = match r_result {
            Result::Ok(result) => result,
            Result::Err(error) => return query.on_error(env, error),
        };
        if result.is_empty() {
            return query.on_error(env, Status::error(404, "Not Found"));
        }
        if result.len() != 1 {
            return query.on_error(
                env,
                Status::error(
                    0,
                    &format!(
                        "Expected result of size 1, but receive of size {}",
                        result.len()
                    ),
                ),
            );
        }
        let encrypted_secure_value =
            get_encrypted_secure_value(env, result.into_iter().next().unwrap());
        if encrypted_secure_value.type_ == SecureValueType::None {
            return query.on_error(env, Status::error(404, "Not Found"));
        }
        query.encrypted_secure_value = Some(encrypted_secure_value);

        match query.try_loop(self, env) {
            Result::Ok(_) => {} // resolved or still waiting for the secret
            Result::Err(error) => query.on_error(env, error),
        }
    }

    // --- get_all_secure_values (SecureManager.cpp:819-823 +
    //     GetAllSecureValues) ---

    /// `SecureManager::get_all_secure_values`.
    pub fn get_all_secure_values(
        &mut self,
        env: &mut impl SecureManagerEnv,
        password: &str,
        promise: Promise<TdApiSecureValues>,
    ) {
        let mut query = GetAllSecureValuesQuery {
            promise,
            encrypted_secure_values: None,
            secret: None,
        };
        env.send_get_all_secure_values_query();

        match env.get_secure_secret(password) {
            Result::Ok(secret) => {
                query.secret = Some(secret);
                self.pending_get_all_secure_values = Some(query);
            }
            Result::Err(error) => query.on_error(env, error),
        }
    }

    /// `GetAllSecureValues::on_result` (SecureManager.cpp:311-319).
    pub fn on_get_all_secure_values_result(
        &mut self,
        env: &mut impl SecureManagerEnv,
        r_result: Result<Vec<telegram_api::SecureValue>>,
    ) {
        let Some(mut query) = self.pending_get_all_secure_values.take() else {
            return;
        };
        let result = match r_result {
            Result::Ok(result) => result,
            Result::Err(error) => return query.on_error(env, error),
        };
        query.encrypted_secure_values =
            Some(secure_value::get_encrypted_secure_values(env, result));

        match query.try_loop(self, env) {
            Result::Ok(false) => {
                // still waiting for the secret: repark
                self.pending_get_all_secure_values = Some(query);
            }
            Result::Ok(true) => {}
            Result::Err(error) => query.on_error(env, error),
        }
    }

    // --- set_secure_value (SecureManager.cpp:825-843 + SetSecureValue) ---

    /// `SecureManager::set_secure_value`: the input parse already ran in
    /// Requests.cpp; the query runs `start_up`, loads the secret and
    /// starts the uploads. File-carrying values park at the upload seam;
    /// zero-file values reach the `account.saveSecureValue` seam.
    pub fn set_secure_value(
        &mut self,
        env: &mut impl SecureManagerEnv,
        password: &str,
        secure_value: secure_value::SecureValue,
        promise: Promise<TdApiSecureValue>,
    ) {
        let type_ = secure_value.type_;
        let mut query = SetSecureValueQuery::new(password.to_owned(), secure_value, promise);
        if let Result::Err(error) = query.start_up(env) {
            return query.on_error(error);
        }
        if let Result::Err(error) = query.load_secret(env) {
            return query.on_error(error);
        }
        query.start_upload_all(env);
        query.try_loop(env);

        // the C++ overwrites the map entry, hangup-ing the parked actor
        if let Some(mut old_query) = self.set_secure_value_queries.insert(type_, query) {
            old_query.on_hangup();
        }
    }

    /// `SetSecureValue::on_result` (SecureManager.cpp:595-647) — keyed by
    /// the element type (the map key the C++ actor lived under).
    pub fn on_set_secure_value_result(
        &mut self,
        env: &mut impl SecureManagerEnv,
        type_: SecureValueType,
        r_result: Result<telegram_api::SecureValue>,
    ) {
        let Some(mut query) = self.set_secure_value_queries.remove(&type_) else {
            return;
        };

        let result = match r_result {
            Result::Ok(result) => result,
            Result::Err(error) => {
                if error.message() == "SECURE_SECRET_REQUIRED" {
                    query.state = SetSecureValueState::WaitSecret;
                    env.drop_cached_secret();
                    if query.load_secret(env).is_ok() {
                        query.try_loop(env);
                    }
                    self.set_secure_value_queries.insert(type_, query);
                    return;
                }
                if error.message() == "SECURE_SECRET_INVALID" {
                    query.state = SetSecureValueState::WaitSecret;
                    query.start_upload_all(env);
                    query.try_loop(env);
                    self.set_secure_value_queries.insert(type_, query);
                    return;
                }
                return query.on_error(error);
            }
        };

        let encrypted_secure_value = secure_value::get_encrypted_secure_value(env, result);
        if encrypted_secure_value.type_ == SecureValueType::None {
            return query.on_error(Status::error(
                500,
                "Receive invalid Telegram Passport element",
            ));
        }
        if query.secure_value.files.len() != encrypted_secure_value.files.len() {
            return query.on_error(Status::error(500, "Different file count"));
        }
        if query.secure_value.translations.len() != encrypted_secure_value.translations.len() {
            return query.on_error(Status::error(500, "Different translation count"));
        }
        for (dated_file, encrypted_file) in query
            .secure_value
            .files
            .iter()
            .zip(encrypted_secure_value.files.iter())
        {
            query.merge(env, dated_file.file_id, encrypted_file);
        }
        if query.secure_value.front_side.file_id.is_valid()
            && encrypted_secure_value.front_side.file.file_id.is_valid()
        {
            query.merge(
                env,
                query.secure_value.front_side.file_id,
                &encrypted_secure_value.front_side,
            );
        }
        if query.secure_value.reverse_side.file_id.is_valid()
            && encrypted_secure_value.reverse_side.file.file_id.is_valid()
        {
            query.merge(
                env,
                query.secure_value.reverse_side.file_id,
                &encrypted_secure_value.reverse_side,
            );
        }
        if query.secure_value.selfie.file_id.is_valid()
            && encrypted_secure_value.selfie.file.file_id.is_valid()
        {
            query.merge(
                env,
                query.secure_value.selfie.file_id,
                &encrypted_secure_value.selfie,
            );
        }
        for (dated_file, encrypted_file) in query
            .secure_value
            .translations
            .iter()
            .zip(encrypted_secure_value.translations.iter())
        {
            query.merge(env, dated_file.file_id, encrypted_file);
        }

        let secret = match &query.secret {
            Some(secret) => *secret,
            None => return query.on_error(Status::error(400, "Secret is not loaded")),
        };
        let r_secure_value =
            secure_value::decrypt_secure_value(env, &secret, &encrypted_secure_value);
        let secure_value_with_credentials = match r_secure_value {
            Result::Ok(value) => value,
            Result::Err(error) => return query.on_error(error),
        };

        self.on_get_secure_value(secure_value_with_credentials.clone());

        // the `set_secure_value` lambda (SecureManager.cpp:828-840): the
        // conversion error is a 500, unlike the get-side nullptr
        let element =
            secure_value::get_passport_element_object(env, &secure_value_with_credentials.value);
        match element {
            Result::Ok(element) => query.promise.set_value(Some(element)),
            Result::Err(error) => {
                tdutils::log_error!("Failed to get passport element object: {error}");
                query
                    .promise
                    .set_error(Status::error(500, "Failed to get passport element object"));
            }
        }
    }

    // --- delete_secure_value (SecureManager.cpp:845-861 +
    //     DeleteSecureValue) ---

    /// `SecureManager::delete_secure_value`.
    pub fn delete_secure_value(
        &mut self,
        env: &mut impl SecureManagerEnv,
        type_: SecureValueType,
        promise: Promise<()>,
    ) {
        env.send_delete_secure_value_query(type_);
        self.pending_delete_secure_values
            .push_back((type_, promise));
    }

    /// `DeleteSecureValue::on_result` (SecureManager.cpp:678-687) +
    /// `SecureManager::on_delete_secure_value` (SecureManager.cpp:854-861).
    pub fn on_delete_secure_value_result(&mut self, type_: SecureValueType, r_result: Result<()>) {
        let position = self
            .pending_delete_secure_values
            .iter()
            .position(|(pending_type, _)| *pending_type == type_);
        let Some((_, mut promise)) =
            position.and_then(|position| self.pending_delete_secure_values.remove(position))
        else {
            return;
        };
        match r_result {
            Result::Err(error) => promise.set_error(error),
            Result::Ok(()) => {
                self.secure_value_cache.remove(&type_);
                promise.set_value(());
            }
        }
    }

    // --- set_secure_value_errors (SecureManager.cpp:863-957 +
    //     SetSecureValueErrorsQuery) ---

    /// `SecureManager::set_secure_value_errors` — the td_api → telegram
    /// error conversion runs here; the query send is the `[blocked: net]`
    /// seam and the promise parks.
    pub fn set_secure_value_errors(
        &mut self,
        env: &mut impl SecureManagerEnv,
        input_user: telegram_api::InputUser,
        errors: Vec<td_api::InputPassportElementError>,
        mut promise: Promise<()>,
    ) {
        match get_input_secure_value_errors(errors) {
            Result::Ok(input_errors) => {
                env.send_set_secure_value_errors_query(input_user, input_errors);
                self.pending_set_secure_value_errors.push_back(promise);
            }
            Result::Err(error) => promise.set_error(error),
        }
    }

    /// `SetSecureValueErrorsQuery::on_result`/`on_error`
    /// (SecureManager.cpp:152-169).
    pub fn on_set_secure_value_errors_result(&mut self, r_result: Result<()>) {
        let Some(mut promise) = self.pending_set_secure_value_errors.pop_front() else {
            return;
        };
        match r_result {
            Result::Ok(()) => promise.set_value(()),
            Result::Err(status) => {
                if status.code() != 0 {
                    promise.set_error(status);
                } else {
                    promise.set_error(Status::error(400, status.message()));
                }
            }
        }
    }

    // --- get_passport_authorization_form (SecureManager.cpp:959-1042) ---

    /// `SecureManager::get_passport_authorization_form`.
    pub fn get_passport_authorization_form(
        &mut self,
        env: &mut impl SecureManagerEnv,
        bot_user_id: UserId,
        scope: &str,
        public_key: &str,
        nonce: &str,
        promise: Promise<TdApiAuthorizationForm>,
    ) {
        assert!(self.max_authorization_form_id < i32::MAX);
        self.max_authorization_form_id += 1;
        let authorization_form_id = self.max_authorization_form_id;
        self.authorization_forms.insert(
            authorization_form_id,
            AuthorizationForm {
                bot_user_id,
                scope: scope.to_owned(),
                public_key: public_key.to_owned(),
                nonce: nonce.to_owned(),
                is_received: false,
                is_decrypted: false,
                options: BTreeMap::new(),
                values: Vec::new(),
                errors: Vec::new(),
                promise: Some(promise),
            },
        );
        env.send_get_authorization_form_query(bot_user_id, scope, public_key);
    }

    /// `SecureManager::on_get_passport_authorization_form`
    /// (SecureManager.cpp:984-1042).
    pub fn on_get_passport_authorization_form(
        &mut self,
        env: &mut impl SecureManagerEnv,
        authorization_form_id: i32,
        r_authorization_form: Result<telegram_api::account_AuthorizationForm>,
    ) {
        let Some(mut form) = self.authorization_forms.remove(&authorization_form_id) else {
            panic!("CHECK failed: the authorization form exists");
        };
        assert!(!form.is_received);
        let mut promise = form.promise.take().unwrap();

        let authorization_form = match r_authorization_form {
            Result::Ok(authorization_form) => authorization_form,
            Result::Err(error) => return promise.set_error(error),
        };
        tdutils::log_info!("Receive account.authorizationForm");
        env.on_get_users(
            authorization_form.users,
            "on_get_passport_authorization_form",
        );

        let mut required_types: Vec<Vec<SuitableSecureValue>> = Vec::new();
        let mut all_types: BTreeMap<SecureValueType, SuitableSecureValue> = BTreeMap::new();
        for type_ptr in &authorization_form.required_types {
            let mut required_type: Vec<SuitableSecureValue> = Vec::new();
            match &**type_ptr {
                telegram_api::SecureRequiredType::secureRequiredType(value) => {
                    let value = get_suitable_secure_value(value);
                    all_types.entry(value.type_).or_insert(value);
                    required_type.push(value);
                }
                telegram_api::SecureRequiredType::secureRequiredTypeOneOf(type_one_of) => {
                    for type_ in &type_one_of.types {
                        if let telegram_api::SecureRequiredType::secureRequiredType(value) =
                            &**type_
                        {
                            let value = get_suitable_secure_value(value);
                            all_types.entry(value.type_).or_insert(value);
                            required_type.push(value);
                        } else {
                            tdutils::log_error!("Receive unexpected required type");
                        }
                    }
                }
            }
            if !required_type.is_empty() {
                required_types.push(required_type);
            }
        }

        form.options = all_types;
        form.values = authorization_form.values;
        form.errors = authorization_form.errors;
        form.is_received = true;
        self.authorization_forms.insert(authorization_form_id, form);

        promise.set_value(td_api::PassportAuthorizationForm {
            id: authorization_form_id,
            required_elements: get_passport_required_elements_object(&required_types),
            privacy_policy_url: authorization_form.privacy_policy_url,
        });
    }

    // --- get_passport_authorization_form_available_elements
    //     (SecureManager.cpp:1044-1233) ---

    /// `SecureManager::get_passport_authorization_form_available_elements`.
    pub fn get_passport_authorization_form_available_elements(
        &mut self,
        env: &mut impl SecureManagerEnv,
        authorization_form_id: i32,
        password: &str,
        mut promise: Promise<TdApiSecureValuesWithErrors>,
    ) {
        let is_received = self
            .authorization_forms
            .get(&authorization_form_id)
            .map(|form| form.is_received);
        match is_received {
            None => {
                return promise.set_error(Status::error(400, "Unknown authorization_form_id"));
            }
            Some(false) => {
                return promise
                    .set_error(Status::error(400, "Authorization form isn't received yet"));
            }
            Some(true) => {}
        }

        match env.get_secure_secret(password) {
            Result::Ok(secret) => self.on_get_passport_authorization_form_secret(
                env,
                authorization_form_id,
                promise,
                Result::Ok(secret),
            ),
            Result::Err(error) => self.on_get_passport_authorization_form_secret(
                env,
                authorization_form_id,
                promise,
                Result::Err(error),
            ),
        }
    }

    /// `SecureManager::on_get_passport_authorization_form_secret`
    /// (SecureManager.cpp:1064-1233) — the promise is the request's own
    /// (the C++ lambda carries it here from
    /// `get_passport_authorization_form_available_elements`).
    pub fn on_get_passport_authorization_form_secret(
        &mut self,
        env: &mut impl SecureManagerEnv,
        authorization_form_id: i32,
        mut promise: Promise<TdApiSecureValuesWithErrors>,
        r_secret: Result<Secret>,
    ) {
        let (options, mut values, errors) = {
            let Some(form) = self.authorization_forms.get_mut(&authorization_form_id) else {
                return promise.set_error(Status::error(
                    400,
                    "Authorization form has already been sent",
                ));
            };
            assert!(form.is_received);
            if form.is_decrypted {
                return promise.set_error(Status::error(
                    400,
                    "Authorization form has already been decrypted",
                ));
            }
            form.is_decrypted = true;
            (
                std::mem::take(&mut form.options),
                std::mem::take(&mut form.values),
                std::mem::take(&mut form.errors),
            )
        };

        let secret = match r_secret {
            Result::Ok(secret) => secret,
            Result::Err(mut error) => {
                if !env.is_expected_error(&error) {
                    tdutils::log_error!("Receive error instead of secret: {error}");
                }
                if error.code() <= 0 {
                    error = Status::error(400, error.message());
                }
                return promise.set_error(error);
            }
        };

        let mut td_values: Vec<td_api::PassportElement> = Vec::new();
        let mut all_credentials: BTreeMap<SecureValueType, SecureValueCredentials> =
            BTreeMap::new();
        for &suitable_type in options.keys() {
            let position = values.iter().position(|value| {
                get_secure_value_type(value.r#type.as_ref().expect("secureValue carries its type"))
                    == suitable_type
            });
            let Some(position) = position else {
                continue;
            };
            let value = values.remove(position);
            let encrypted_value = get_encrypted_secure_value(env, value);
            let r_secure_value = secure_value::decrypt_secure_value(env, &secret, &encrypted_value);
            let secure_value_with_credentials = match r_secure_value {
                Result::Ok(value) => value,
                Result::Err(error) => {
                    tdutils::log_error!("Failed to decrypt secure value: {error}");
                    continue;
                }
            };

            self.on_get_secure_value(secure_value_with_credentials.clone());

            let element = secure_value::get_passport_element_object(
                env,
                &secure_value_with_credentials.value,
            );
            match element {
                Result::Ok(element) => td_values.push(element),
                Result::Err(error) => {
                    tdutils::log_error!("Failed to get passport element object: {error}");
                    continue;
                }
            }
            all_credentials
                .entry(suitable_type)
                .or_insert_with(|| secure_value_with_credentials.credentials.clone());
        }

        let get_file_index =
            |file_credentials: &[SecureFileCredentials], file_hash: &[u8]| -> i32 {
                for (i, file_credential) in file_credentials.iter().enumerate() {
                    if file_credential.hash == file_hash {
                        return i as i32;
                    }
                }
                -1
            };

        let mut td_errors: Vec<td_api::PassportElementError> = Vec::new();
        for error_ptr in &errors {
            let error_ptr = error_ptr.clone();
            let type_: SecureValueType;
            let message;
            let mut source: Option<td_api::PassportElementErrorSource> = None;
            match error_ptr {
                telegram_api::SecureValueError::secureValueError(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::passportElementErrorSourceUnspecified(
                            td_api::passportElementErrorSourceUnspecified,
                        ),
                    );
                }
                telegram_api::SecureValueError::secureValueErrorData(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    let field_name = get_secure_value_data_field_name(type_, &error.field);
                    if !field_name.is_empty() {
                        source = Some(
                            td_api::PassportElementErrorSource::passportElementErrorSourceDataField(
                                td_api::passportElementErrorSourceDataField { field_name },
                            ),
                        );
                    }
                }
                telegram_api::SecureValueError::secureValueErrorFile(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    let file_credentials = all_credentials
                        .get(&type_)
                        .map(|c| c.files.as_slice())
                        .unwrap_or(&[]);
                    let file_index = get_file_index(file_credentials, &error.file_hash);
                    if file_index == -1 {
                        tdutils::log_error!("Can't find file with error");
                    } else {
                        source = Some(
                            td_api::PassportElementErrorSource::passportElementErrorSourceFile(
                                td_api::passportElementErrorSourceFile { file_index },
                            ),
                        );
                    }
                }
                telegram_api::SecureValueError::secureValueErrorFiles(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::passportElementErrorSourceFiles(
                            td_api::passportElementErrorSourceFiles,
                        ),
                    );
                }
                telegram_api::SecureValueError::secureValueErrorFrontSide(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::passportElementErrorSourceFrontSide(
                            td_api::passportElementErrorSourceFrontSide,
                        ),
                    );
                }
                telegram_api::SecureValueError::secureValueErrorReverseSide(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::passportElementErrorSourceReverseSide(
                            td_api::passportElementErrorSourceReverseSide,
                        ),
                    );
                }
                telegram_api::SecureValueError::secureValueErrorSelfie(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::passportElementErrorSourceSelfie(
                            td_api::passportElementErrorSourceSelfie,
                        ),
                    );
                }
                telegram_api::SecureValueError::secureValueErrorTranslationFile(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    let translation_credentials = all_credentials
                        .get(&type_)
                        .map(|c| c.translations.as_slice())
                        .unwrap_or(&[]);
                    let file_index = get_file_index(translation_credentials, &error.file_hash);
                    if file_index == -1 {
                        tdutils::log_error!("Can't find translation file with error");
                    } else {
                        source = Some(
                            td_api::PassportElementErrorSource::
                                passportElementErrorSourceTranslationFile(
                                    td_api::passportElementErrorSourceTranslationFile {
                                        file_index,
                                    },
                                ),
                        );
                    }
                }
                telegram_api::SecureValueError::secureValueErrorTranslationFiles(error) => {
                    type_ = get_secure_value_type(error.r#type.as_ref().unwrap());
                    message = error.text;
                    source = Some(
                        td_api::PassportElementErrorSource::
                            passportElementErrorSourceTranslationFiles(
                                td_api::passportElementErrorSourceTranslationFiles,
                            ),
                    );
                }
            }
            let Some(source) = source else {
                continue;
            };

            td_errors.push(td_api::PassportElementError {
                r#type: Some(get_passport_element_type_object(type_)),
                message,
                source: Some(source),
            });
        }

        // restore the (now value-less) form state
        if let Some(form) = self.authorization_forms.get_mut(&authorization_form_id) {
            form.options = options;
            form.values = values;
            form.errors = errors;
        }

        promise.set_value(td_api::PassportElementsWithErrors {
            elements: td_values,
            errors: td_errors,
        });
    }

    // --- send_passport_authorization_form (SecureManager.cpp:1235-1294) ---

    /// `SecureManager::send_passport_authorization_form`.
    pub fn send_passport_authorization_form(
        &mut self,
        env: &mut impl SecureManagerEnv,
        authorization_form_id: i32,
        types: Vec<SecureValueType>,
        mut promise: Promise<()>,
    ) {
        if !self
            .authorization_forms
            .contains_key(&authorization_form_id)
        {
            return promise.set_error(Status::error(400, "Unknown authorization_form_id"));
        }
        let (bot_user_id, scope, public_key, nonce, is_received) = {
            let form = &self.authorization_forms[&authorization_form_id];
            (
                form.bot_user_id,
                form.scope.clone(),
                form.public_key.clone(),
                form.nonce.clone(),
                form.is_received,
            )
        };
        if !is_received {
            return promise.set_error(Status::error(400, "Authorization form isn't received yet"));
        }
        // there is no need to check for is_decrypted
        if types.is_empty() {
            return promise.set_error(Status::error(400, "Types must be non-empty"));
        }

        let mut credentials: Vec<SecureValueCredentials> = Vec::with_capacity(types.len());
        for type_ in &types {
            let Some(value) = self.secure_value_cache.get(type_) else {
                return promise.set_error(Status::error(
                    400,
                    "Passport Element with the specified type is not found",
                ));
            };
            credentials.push(value.credentials.clone());
        }

        let mut hashes: Vec<telegram_api::SecureValueHash> = Vec::new();
        let rename_payload_to_nonce = scope.starts_with('{') && scope.ends_with('}');
        for credential in &mut credentials {
            hashes.push(telegram_api::SecureValueHash {
                r#type: Some(secure_value::get_input_secure_value_type(credential.type_)),
                hash: credential.hash.clone(),
            });
            let Some(options) = self
                .authorization_forms
                .get(&authorization_form_id)
                .and_then(|form| form.options.get(&credential.type_))
            else {
                return promise.set_error(Status::error(
                    400,
                    "Passport Element with the specified type was not requested",
                ));
            };
            if !options.is_selfie_required {
                credential.selfie = None;
            }
            if !options.is_translation_required {
                credential.translations.clear();
            }
        }

        let r_encrypted_credentials = secure_value::get_encrypted_credentials(
            &credentials,
            nonce.as_bytes(),
            public_key.as_bytes(),
            rename_payload_to_nonce,
        );
        let encrypted_credentials = match r_encrypted_credentials {
            Result::Ok(encrypted_credentials) => encrypted_credentials,
            Result::Err(error) => return promise.set_error(error),
        };

        let query = telegram_api::account_acceptAuthorization {
            bot_id: bot_user_id.get(),
            scope,
            public_key,
            value_hashes: hashes,
            credentials: Some(get_secure_credentials_encrypted_object(
                &encrypted_credentials,
            )),
        };
        env.send_accept_authorization_query(query);
        self.pending_accept_authorizations.push_back(promise);
    }

    /// The `fetch_result<account_acceptAuthorization>` collapse
    /// (SecureManager.cpp:1288-1292).
    pub fn on_accept_authorization_result(&mut self, r_result: Result<()>) {
        let Some(mut promise) = self.pending_accept_authorizations.pop_front() else {
            return;
        };
        match r_result {
            Result::Ok(()) => promise.set_value(()),
            Result::Err(error) => promise.set_error(error),
        }
    }

    // --- get_preferred_country_language (SecureManager.cpp:760-812,
    //     1296-1304) ---

    /// `SecureManager::get_preferred_country_language`.
    pub fn get_preferred_country_language(
        &mut self,
        env: &mut impl SecureManagerEnv,
        country_code: &str,
        promise: Promise<Option<td_api::Text>>,
    ) {
        let country_code = country_code.to_ascii_uppercase();
        env.send_get_passport_config_query(0);
        self.pending_passport_configs
            .push_back((country_code, promise));
    }

    /// `GetPassportConfig::on_result` (SecureManager.cpp:777-811).
    pub fn on_get_passport_config_result(
        &mut self,
        r_result: Result<telegram_api::help_PassportConfig>,
    ) {
        let Some((country_code, mut promise)) = self.pending_passport_configs.pop_front() else {
            return;
        };
        let config = match r_result {
            Result::Ok(config) => config,
            Result::Err(error) => return promise.set_error(error),
        };
        match config {
            telegram_api::help_PassportConfig::help_passportConfigNotModified(_) => {
                promise.set_error(Status::error(500, "Wrong server response"));
            }
            telegram_api::help_PassportConfig::help_passportConfig(config) => {
                let Some(countries_langs) = config.countries_langs else {
                    return promise.set_value(None);
                };
                let data = countries_langs.data;
                let needle = format!("\"{country_code}\":\"");
                let Some(begin_pos) = data.find(&needle) else {
                    return promise.set_value(None);
                };
                let begin_pos = begin_pos + 4 + country_code.len();
                let Some(end_pos) = data[begin_pos..].find('"').map(|p| p + begin_pos) else {
                    return promise.set_error(Status::error(500, "Wrong server response"));
                };
                promise.set_value(Some(td_api::Text {
                    text: data[begin_pos..end_pos].to_owned(),
                }));
            }
        }
    }
}

/// `SecureManager::set_secure_value_errors`'s conversion loop — the first
/// invalid error fails the whole request, exactly like the C++
/// `return promise.set_error(...)` mid-loop. The consumed C++ nullptr
/// slots of the form values are removed instead (the position scan).
pub fn get_input_secure_value_errors(
    errors: Vec<td_api::InputPassportElementError>,
) -> Result<Vec<telegram_api::SecureValueError>> {
    let mut input_errors: Vec<telegram_api::SecureValueError> = Vec::new();
    for error in errors {
        let Some(error_type) = &error.r#type else {
            return Result::Err(Status::error(400, "Type must be non-empty"));
        };
        let Some(message) = crate::misc::clean_input_string(&error.message) else {
            return Result::Err(Status::error(400, "Error message must be encoded in UTF-8"));
        };
        let Some(source) = &error.source else {
            return Result::Err(Status::error(400, "Error source must be non-empty"));
        };

        let type_ =
            secure_value::get_input_secure_value_type(get_secure_value_type_td_api(error_type));
        input_errors.push(match source {
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceUnspecified(
                source,
            ) => telegram_api::SecureValueError::secureValueError(telegram_api::secureValueError {
                r#type: Some(type_),
                hash: source.element_hash.clone(),
                text: message,
            }),
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceDataField(
                source,
            ) => {
                let Some(field_name) = crate::misc::clean_input_string(&source.field_name) else {
                    return Result::Err(Status::error(
                        400,
                        "Field name must be encoded in UTF-8",
                    ));
                };
                telegram_api::SecureValueError::secureValueErrorData(
                    telegram_api::secureValueErrorData {
                        r#type: Some(type_),
                        data_hash: source.data_hash.clone(),
                        field: field_name,
                        text: message,
                    },
                )
            }
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceFrontSide(
                source,
            ) => telegram_api::SecureValueError::secureValueErrorFrontSide(
                telegram_api::secureValueErrorFrontSide {
                    r#type: Some(type_),
                    file_hash: source.file_hash.clone(),
                    text: message,
                },
            ),
            td_api::InputPassportElementErrorSource::
                inputPassportElementErrorSourceReverseSide(source) => {
                telegram_api::SecureValueError::secureValueErrorReverseSide(
                    telegram_api::secureValueErrorReverseSide {
                        r#type: Some(type_),
                        file_hash: source.file_hash.clone(),
                        text: message,
                    },
                )
            }
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceSelfie(
                source,
            ) => telegram_api::SecureValueError::secureValueErrorSelfie(
                telegram_api::secureValueErrorSelfie {
                    r#type: Some(type_),
                    file_hash: source.file_hash.clone(),
                    text: message,
                },
            ),
            td_api::InputPassportElementErrorSource::
                inputPassportElementErrorSourceTranslationFile(source) => {
                telegram_api::SecureValueError::secureValueErrorTranslationFile(
                    telegram_api::secureValueErrorTranslationFile {
                        r#type: Some(type_),
                        file_hash: source.file_hash.clone(),
                        text: message,
                    },
                )
            }
            td_api::InputPassportElementErrorSource::
                inputPassportElementErrorSourceTranslationFiles(source) => {
                if source.file_hashes.is_empty() {
                    return Result::Err(Status::error(400, "File hashes must be non-empty"));
                }
                telegram_api::SecureValueError::secureValueErrorTranslationFiles(
                    telegram_api::secureValueErrorTranslationFiles {
                        r#type: Some(type_),
                        file_hash: source.file_hashes.clone(),
                        text: message,
                    },
                )
            }
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceFile(
                source,
            ) => telegram_api::SecureValueError::secureValueErrorFile(
                telegram_api::secureValueErrorFile {
                    r#type: Some(type_),
                    file_hash: source.file_hash.clone(),
                    text: message,
                },
            ),
            td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceFiles(
                source,
            ) => {
                if source.file_hashes.is_empty() {
                    return Result::Err(Status::error(400, "File hashes must be non-empty"));
                }
                telegram_api::SecureValueError::secureValueErrorFiles(
                    telegram_api::secureValueErrorFiles {
                        r#type: Some(type_),
                        file_hash: source.file_hashes.clone(),
                        text: message,
                    },
                )
            }
        });
    }
    Result::Ok(input_errors)
}

// ---------------------------------------------------------------------------
// The tests: a recording env over the seams + the full manager flows
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileType;
    use crate::secure_value::{EncryptedSecureValue, SecureValue};
    use std::sync::Arc;
    use tdutils::promise::PromiseInterface;

    /// The M.44 test RSA public key (the `encrypted_secret` envelope).
    const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEArX7B/IqARVFnfrtT0TMs\n\
F52TAQm821m9Ow4jFRpuSpYKwYbWZWOJDRCuy0SOXvRXm/vTLhF5TpqneF0xcolu\n\
75jTQ1V59dRx4fKw4AYwPTKsguGYPaehiyJ+OM2TLMdV1XB3QmAQwfgMWiDkmD77\n\
6eZBdm+OFXwQO27wUiKtxNg/ir/AlktWK/YOEK8xUZkeEwZ8dmD8di3i5s9VfIuY\n\
3X24cblu+Mh96HSqjOzUuKohkqFtPCDReSmgLgjBeQL4beoC6+pRdBxlbrn7b7b5\n\
jMvDdweuSqa9uNt+Jo7u1r3EXyAWtXrLSOqz1Iy4B+JMuM8J849J5NShTbLGzJhw\n\
kQIDAQAB\n\
-----END PUBLIC KEY-----";

    /// The recording promise: stores both the set value and the error.
    struct Recorded<T: Send + 'static>(Arc<parking_lot::Mutex<Option<tdutils::status::Result<T>>>>);

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock() = Some(tdutils::status::Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock() = Some(tdutils::status::Result::Err(error));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(Arc::new(parking_lot::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(Arc::clone(&recorded.0)),
            }),
            recorded,
        )
    }

    impl<T: Send + Clone + 'static> Recorded<T> {
        fn error(&self) -> Status {
            match &*self.0.lock() {
                Some(tdutils::status::Result::Err(status)) => status.clone(),
                _ => panic!("expected an error"),
            }
        }

        fn is_settled(&self) -> bool {
            self.0.lock().is_some()
        }

        fn value(&self) -> Option<tdutils::status::Result<T>> {
            self.0.lock().clone()
        }
    }

    /// The recording env: the seam sends, the secret answer, the users
    /// intakes and the built telegram errors all land here.
    struct TestEnv {
        secret: Option<Secret>,
        queries: Vec<String>,
        users_intakes: Vec<String>,
        accept_authorizations: Vec<telegram_api::account_acceptAuthorization>,
        errors_sent: Vec<Vec<telegram_api::SecureValueError>>,
        input_users_sent: Vec<telegram_api::InputUser>,
        files_registered: Vec<FileType>,
        encryption_keys_set: Vec<FileId>,
        upload_resumes: Vec<FileUploadId>,
        next_file_id: i32,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                secret: Some(Secret::create_new()),
                queries: Vec::new(),
                users_intakes: Vec::new(),
                accept_authorizations: Vec::new(),
                errors_sent: Vec::new(),
                input_users_sent: Vec::new(),
                files_registered: Vec::new(),
                encryption_keys_set: Vec::new(),
                upload_resumes: Vec::new(),
                next_file_id: 100,
            }
        }
    }

    impl SecureValueEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn unix_time(&mut self) -> i32 {
            1700000000
        }

        fn file_register_secure_encrypted(
            &mut self,
            _id: i64,
            _access_hash: i64,
            _dc_id: crate::chat_manager::DcId,
            _size: i64,
            _remote_name: String,
        ) -> FileId {
            self.files_registered.push(FileType::SecureEncrypted);
            self.next_file_id += 1;
            FileId::new(self.next_file_id, self.next_file_id)
        }

        fn file_register_secure_decrypted(
            &mut self,
            _id: i64,
            _access_hash: i64,
            _dc_id: crate::chat_manager::DcId,
            _expected_size: i64,
            _suggested_path: String,
        ) -> FileId {
            self.files_registered.push(FileType::SecureDecrypted);
            self.next_file_id += 1;
            FileId::new(self.next_file_id, self.next_file_id)
        }

        fn file_view(&mut self, _file_id: FileId) -> Option<crate::secure_value::SecureFileView> {
            None
        }

        fn file_set_encryption_key(
            &mut self,
            file_id: FileId,
            _key: crate::files::FileEncryptionKey,
        ) {
            self.encryption_keys_set.push(file_id);
        }

        fn file_object(&mut self, _file_id: FileId) -> td_api::File {
            td_api::File {
                id: 1,
                size: 0,
                expected_size: 0,
                local: Some(td_api::LocalFile::default()),
                remote: Some(td_api::RemoteFile::default()),
            }
        }

        fn file_get_input_file_id(&mut self, _file: Option<&td_api::InputFile>) -> Result<FileId> {
            self.next_file_id += 1;
            Result::Ok(FileId::new(self.next_file_id, self.next_file_id))
        }

        fn file_merge(&mut self, _to_file_id: FileId, _from_file_id: FileId) -> Result<()> {
            Result::Ok(())
        }

        fn file_copy_file_id(&mut self, _file_id: FileId) -> FileId {
            self.next_file_id += 1;
            FileId::new(self.next_file_id, self.next_file_id)
        }

        fn file_cancel_upload(&mut self, file_upload_id: FileUploadId) {
            self.queries.push(format!(
                "cancelUpload:{}:{}",
                file_upload_id.file_id.get(),
                file_upload_id.internal_upload_id
            ));
        }

        fn file_resume_upload_secure(&mut self, file_upload_id: FileUploadId, force: bool) {
            self.upload_resumes.push(file_upload_id);
            self.queries.push(format!("resumeUploadSecure:{force}"));
        }

        fn file_get_internal_upload_id(&mut self) -> i64 {
            7
        }
    }

    impl SecureManagerEnv for TestEnv {
        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.users_intakes.push(format!("{source}:{}", users.len()));
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser> {
            Result::Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 0,
                },
            ))
        }

        fn get_secure_secret(&mut self, _password: &str) -> Result<Secret> {
            match self.secret {
                Some(secret) => Result::Ok(secret),
                None => Result::Err(Status::error(400, "PASSWORD_HASH_INVALID")),
            }
        }

        fn drop_cached_secret(&mut self) {
            self.queries.push("dropCachedSecret".to_owned());
        }

        fn send_get_secure_value_query(&mut self, type_: SecureValueType) {
            self.queries.push(format!("getSecureValue:{type_}"));
        }

        fn send_get_all_secure_values_query(&mut self) {
            self.queries.push("getAllSecureValues".to_owned());
        }

        fn send_save_secure_value_query(
            &mut self,
            _value: telegram_api::InputSecureValue,
            secure_secret_id: i64,
        ) {
            self.queries
                .push(format!("saveSecureValue:{secure_secret_id}"));
        }

        fn send_delete_secure_value_query(&mut self, type_: SecureValueType) {
            self.queries.push(format!("deleteSecureValue:{type_}"));
        }

        fn send_set_secure_value_errors_query(
            &mut self,
            input_user: telegram_api::InputUser,
            input_errors: Vec<telegram_api::SecureValueError>,
        ) {
            self.queries.push("setSecureValueErrors".to_owned());
            self.input_users_sent.push(input_user);
            self.errors_sent.push(input_errors);
        }

        fn send_get_authorization_form_query(
            &mut self,
            bot_user_id: UserId,
            scope: &str,
            _public_key: &str,
        ) {
            self.queries.push(format!(
                "getAuthorizationForm:{}:{scope}",
                bot_user_id.get()
            ));
        }

        fn send_accept_authorization_query(
            &mut self,
            query: telegram_api::account_acceptAuthorization,
        ) {
            self.queries.push("acceptAuthorization".to_owned());
            self.accept_authorizations.push(query);
        }

        fn send_get_passport_config_query(&mut self, hash: i32) {
            self.queries.push(format!("getPassportConfig:{hash}"));
        }
    }

    // --- the value fixtures ---

    fn email_secure_value() -> SecureValue {
        SecureValue {
            type_: SecureValueType::EmailAddress,
            data: "user@example.com".to_owned(),
            ..SecureValue::default()
        }
    }

    /// `encrypt_secure_value` over the test env (no file legs for the
    /// email element).
    fn encrypted_email(env: &mut TestEnv, master_secret: &Secret) -> EncryptedSecureValue {
        secure_value::encrypt_secure_value(env, master_secret, &email_secure_value())
    }

    /// The telegram `secureValue` carrying a plain-data email (what the
    /// server answers for the phone/email elements).
    fn telegram_email(value: &EncryptedSecureValue) -> telegram_api::SecureValue {
        telegram_api::SecureValue {
            flags: 1 << 5,
            r#type: Some(telegram_api::SecureValueType::secureValueTypeEmail(
                telegram_api::secureValueTypeEmail,
            )),
            data: None,
            front_side: None,
            reverse_side: None,
            selfie: None,
            translation: Vec::new(),
            files: Vec::new(),
            plain_data: Some(telegram_api::SecurePlainData::securePlainEmail(
                telegram_api::securePlainEmail {
                    email: String::from_utf8_lossy(&value.data.data).into_owned(),
                },
            )),
            hash: value.hash.clone(),
        }
    }

    /// The telegram `secureValue` carrying an encrypted data blob.
    fn telegram_personal_details(value: &EncryptedSecureValue) -> telegram_api::SecureValue {
        telegram_api::SecureValue {
            flags: 1,
            r#type: Some(
                telegram_api::SecureValueType::secureValueTypePersonalDetails(
                    telegram_api::secureValueTypePersonalDetails,
                ),
            ),
            data: Some(telegram_api::SecureData {
                data: value.data.data.clone(),
                data_hash: value.data.hash.clone(),
                secret: value.data.encrypted_secret.clone(),
            }),
            front_side: None,
            reverse_side: None,
            selfie: None,
            translation: Vec::new(),
            files: Vec::new(),
            plain_data: None,
            hash: value.hash.clone(),
        }
    }

    fn personal_details_secure_value() -> SecureValue {
        SecureValue {
            type_: SecureValueType::PersonalDetails,
            data: "{\"first_name\":\"Rust\",\"middle_name\":\"\",\"last_name\":\"Lang\",\"first_name_native\":\"\",\"middle_name_native\":\"\",\"last_name_native\":\"\",\"birth_date\":\"01.05.2015\",\"gender\":\"male\",\"country_code\":\"GB\",\"residence_country_code\":\"GB\"}".to_owned(),
            ..SecureValue::default()
        }
    }

    // --- the manager flows ---

    #[test]
    fn get_secure_value_parks_then_resolves_on_the_net_answer() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        let master_secret = env.secret.unwrap();
        let encrypted = encrypted_email(&mut env, &master_secret);

        manager.get_secure_value(&mut env, "pwd", SecureValueType::EmailAddress, promise);
        assert_eq!(env.queries, vec!["getSecureValue:EmailAddress"]);
        assert!(!recorded.is_settled(), "the query parks on the net seam");

        manager.on_get_secure_value_result(&mut env, Result::Ok(vec![telegram_email(&encrypted)]));
        let outcome = recorded.value();
        let element = match outcome {
            Some(Result::Ok(Some(element))) => element,
            _ => panic!("unexpected answer"),
        };
        match element {
            td_api::PassportElement::passportElementEmailAddress(v) => {
                assert_eq!(v.email_address, "user@example.com");
            }
            other => panic!("wrong element {other:?}"),
        }
        // the value landed in the cache
        assert!(
            manager
                .secure_value_cache
                .contains_key(&SecureValueType::EmailAddress)
        );
    }

    #[test]
    fn get_secure_value_personal_details_round_trip() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        let master_secret = env.secret.unwrap();
        let encrypted = {
            let value = personal_details_secure_value();
            secure_value::encrypt_secure_value(&mut env, &master_secret, &value)
        };

        manager.get_secure_value(&mut env, "pwd", SecureValueType::PersonalDetails, promise);
        manager.on_get_secure_value_result(
            &mut env,
            Result::Ok(vec![telegram_personal_details(&encrypted)]),
        );
        let outcome = recorded.value();
        let element = match outcome {
            Some(Result::Ok(Some(element))) => element,
            _ => panic!("unexpected answer"),
        };
        match element {
            td_api::PassportElement::passportElementPersonalDetails(v) => {
                let details = v.personal_details.as_ref().unwrap();
                assert_eq!(details.first_name, "Rust");
                assert_eq!(details.birthdate.as_ref().unwrap().year, 2015);
            }
            other => panic!("wrong element {other:?}"),
        }
    }

    #[test]
    fn get_secure_value_fails_when_the_secret_seam_errors() {
        let mut env = TestEnv {
            secret: None,
            ..Default::default()
        };
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        manager.get_secure_value(&mut env, "pwd", SecureValueType::EmailAddress, promise);
        // the collapsed synchronous `on_secret` → `on_error` hop: a
        // positive error code passes through unchanged
        let error = recorded.error();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "PASSWORD_HASH_INVALID");
        assert!(
            env.queries
                .contains(&"getSecureValue:EmailAddress".to_owned())
        );
        assert!(manager.pending_get_secure_values.is_empty());
    }

    #[test]
    fn get_secure_value_empty_answer_is_404() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();
        manager.get_secure_value(&mut env, "pwd", SecureValueType::EmailAddress, promise);
        manager.on_get_secure_value_result(&mut env, Result::Ok(Vec::new()));
        assert_eq!(recorded.error().code(), 404);
    }

    #[test]
    fn get_all_secure_values_resolves_the_elements() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        let master_secret = env.secret.unwrap();
        let encrypted = encrypted_email(&mut env, &master_secret);

        manager.get_all_secure_values(&mut env, "pwd", promise);
        assert_eq!(env.queries, vec!["getAllSecureValues"]);
        assert!(!recorded.is_settled());

        manager.on_get_all_secure_values_result(
            &mut env,
            Result::Ok(vec![telegram_email(&encrypted)]),
        );
        let elements = recorded.value().expect("settled");
        let elements = match elements {
            Result::Ok(elements) => elements,
            _ => panic!("unexpected answer"),
        };
        assert_eq!(elements.elements.len(), 1);
    }

    #[test]
    fn delete_secure_value_erases_the_cache() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        manager.on_get_secure_value(SecureValueWithCredentials {
            value: email_secure_value(),
            credentials: SecureValueCredentials {
                type_: SecureValueType::EmailAddress,
                hash: b"hash".to_vec(),
                ..SecureValueCredentials::default()
            },
        });

        let (promise, recorded) = recording_promise();
        manager.delete_secure_value(&mut env, SecureValueType::EmailAddress, promise);
        assert_eq!(env.queries, vec!["deleteSecureValue:EmailAddress"]);
        assert!(!recorded.is_settled());

        manager.on_delete_secure_value_result(SecureValueType::EmailAddress, Result::Ok(()));
        assert!(recorded.is_settled());
        assert!(
            !manager
                .secure_value_cache
                .contains_key(&SecureValueType::EmailAddress)
        );
    }

    fn form_park_and_receive(
        env: &mut TestEnv,
        manager: &mut SecureManager,
    ) -> (
        Promise<TdApiAuthorizationForm>,
        Recorded<TdApiAuthorizationForm>,
        i32,
    ) {
        let (promise, recorded) = recording_promise();
        manager.get_passport_authorization_form(
            env,
            UserId::new(777),
            "scope",
            TEST_PUBLIC_KEY,
            "the-nonce",
            promise,
        );
        assert_eq!(
            env.queries,
            vec!["getAuthorizationForm:777:scope".to_owned()]
        );
        let authorization_form_id = 1;
        let required_types = vec![Box::new(
            telegram_api::SecureRequiredType::secureRequiredType(
                telegram_api::secureRequiredType {
                    flags: 0,
                    native_names: false,
                    selfie_required: false,
                    translation_required: false,
                    r#type: Some(telegram_api::SecureValueType::secureValueTypeEmail(
                        telegram_api::secureValueTypeEmail,
                    )),
                },
            ),
        )];
        let master_secret = env.secret.unwrap();
        let encrypted = encrypted_email(env, &master_secret);
        let answer = telegram_api::account_AuthorizationForm {
            flags: 0,
            required_types,
            values: vec![telegram_email(&encrypted)],
            errors: Vec::new(),
            users: Vec::new(),
            privacy_policy_url: String::new(),
        };
        manager.on_get_passport_authorization_form(env, authorization_form_id, Result::Ok(answer));
        let outcome = recorded.value();
        let form = match outcome {
            Some(Result::Ok(form)) => form,
            _ => panic!("unexpected answer"),
        };
        assert_eq!(form.id, 1);
        assert_eq!(form.required_elements.len(), 1);
        assert_eq!(
            env.users_intakes,
            vec!["on_get_passport_authorization_form:0"]
        );
        (
            Promise::default(),
            Recorded(Arc::new(parking_lot::Mutex::new(None))),
            authorization_form_id,
        )
    }

    #[test]
    fn available_elements_decrypt_the_form_values() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();

        // pre-cache the email element so its credentials exist
        let master_secret = env.secret.unwrap();
        let encrypted = encrypted_email(&mut env, &master_secret);
        let decrypted =
            secure_value::decrypt_secure_value(&mut env, &master_secret, &encrypted).unwrap();
        manager.on_get_secure_value(decrypted);

        let (_, _, authorization_form_id) = form_park_and_receive(&mut env, &mut manager);

        let (promise, recorded) = recording_promise();
        manager.get_passport_authorization_form_available_elements(
            &mut env,
            authorization_form_id,
            "pwd",
            promise,
        );
        let elements = recorded.value().expect("settled");
        let elements = match elements {
            Result::Ok(elements) => elements,
            _ => panic!("unexpected answer"),
        };
        assert_eq!(elements.elements.len(), 1);
        match &elements.elements[0] {
            td_api::PassportElement::passportElementEmailAddress(v) => {
                assert_eq!(v.email_address, "user@example.com");
            }
            other => panic!("wrong element {other:?}"),
        }
        assert!(elements.errors.is_empty());

        // the form is one-shot on the decrypt side
        let (promise, recorded) = recording_promise();
        manager.get_passport_authorization_form_available_elements(
            &mut env,
            authorization_form_id,
            "pwd",
            promise,
        );
        assert_eq!(recorded.error().code(), 400);
    }

    #[test]
    fn send_passport_authorization_form_validates_and_sends() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();

        // unknown form id
        let (promise, recorded) = recording_promise();
        manager.send_passport_authorization_form(
            &mut env,
            42,
            vec![SecureValueType::EmailAddress],
            promise,
        );
        assert_eq!(recorded.error().message(), "Unknown authorization_form_id");

        let (_, _, authorization_form_id) = form_park_and_receive(&mut env, &mut manager);

        // the requested-type cache check fires before the RSA envelope
        let (promise, recorded) = recording_promise();
        manager.send_passport_authorization_form(
            &mut env,
            authorization_form_id,
            vec![SecureValueType::Passport],
            promise,
        );
        assert_eq!(
            recorded.error().message(),
            "Passport Element with the specified type is not found"
        );

        // cache the element, then send for real
        let master_secret = env.secret.unwrap();
        let encrypted = encrypted_email(&mut env, &master_secret);
        let expected_hash = encrypted.hash.clone();
        let decrypted =
            secure_value::decrypt_secure_value(&mut env, &master_secret, &encrypted).unwrap();
        manager.on_get_secure_value(decrypted);

        let (promise, recorded) = recording_promise();
        manager.send_passport_authorization_form(
            &mut env,
            authorization_form_id,
            vec![SecureValueType::EmailAddress],
            promise,
        );
        assert!(!recorded.is_settled(), "the ack parks on the net seam");
        assert_eq!(env.accept_authorizations.len(), 1);
        let accept = &env.accept_authorizations[0];
        assert_eq!(accept.bot_id, 777);
        assert_eq!(accept.scope, "scope");
        assert_eq!(accept.value_hashes.len(), 1);
        assert_eq!(
            accept.value_hashes[0].hash, expected_hash,
            "the value hash rides the credentials"
        );
        // the RSA envelope: one modulus-sized `encrypted_secret`
        let credentials = accept.credentials.as_ref().unwrap();
        assert_eq!(credentials.secret.len(), 256);
        assert!(!credentials.data.is_empty());

        manager.on_accept_authorization_result(Result::Ok(()));
        assert!(recorded.is_settled());
    }

    #[test]
    fn set_secure_value_email_reaches_the_save_seam() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        let secure_value = email_secure_value();
        let master_secret = env.secret.unwrap();
        manager.set_secure_value(&mut env, "pwd", secure_value, promise);
        assert!(
            env.queries
                .contains(&format!("saveSecureValue:{}", master_secret.get_hash())),
            "the save query fires for the zero-file element: {:?}",
            env.queries
        );
        assert!(!recorded.is_settled(), "the answer parks on the net seam");
        assert!(
            manager
                .set_secure_value_queries
                .contains_key(&SecureValueType::EmailAddress)
        );

        // the server ack decrypts and resolves
        let encrypted = encrypted_email(&mut env, &master_secret);
        manager.on_set_secure_value_result(
            &mut env,
            SecureValueType::EmailAddress,
            Result::Ok(telegram_email(&encrypted)),
        );
        let outcome = recorded.value();
        match outcome {
            Some(Result::Ok(Some(element))) => match element {
                td_api::PassportElement::passportElementEmailAddress(v) => {
                    assert_eq!(v.email_address, "user@example.com");
                }
                other => panic!("wrong element {other:?}"),
            },
            other => panic!("unexpected answer {other:?}"),
        }
        assert!(
            manager
                .secure_value_cache
                .contains_key(&SecureValueType::EmailAddress)
        );
    }

    #[test]
    fn get_preferred_country_language_parses_the_config() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        manager.get_preferred_country_language(&mut env, "us", promise);
        assert_eq!(env.queries, vec!["getPassportConfig:0"]);

        let config = telegram_api::help_PassportConfig::help_passportConfig(
            telegram_api::help_passportConfig {
                hash: 0,
                countries_langs: Some(telegram_api::DataJSON {
                    data: "{\"US\":\"en\",\"DE\":\"de\"}".to_owned(),
                }),
            },
        );
        manager.on_get_passport_config_result(Result::Ok(config));
        let outcome = recorded.value();
        match outcome {
            Some(Result::Ok(Some(text))) => assert_eq!(text.text, "en"),
            _ => panic!("unexpected answer"),
        }

        // a missing country answers the C++ nullptr (the 404 rewrite)
        let (promise, recorded) = recording_promise();
        manager.get_preferred_country_language(&mut env, "XX", promise);
        let config = telegram_api::help_PassportConfig::help_passportConfig(
            telegram_api::help_passportConfig {
                hash: 0,
                countries_langs: Some(telegram_api::DataJSON {
                    data: "{\"US\":\"en\"}".to_owned(),
                }),
            },
        );
        manager.on_get_passport_config_result(Result::Ok(config));
        let outcome = recorded.value();
        match outcome {
            Some(Result::Ok(None)) => {}
            _ => panic!("unexpected answer"),
        }
    }

    // --- the td_api error conversion ---

    #[test]
    fn set_secure_value_errors_converts_and_sends() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();
        let (promise, recorded) = recording_promise();

        let errors = vec![td_api::InputPassportElementError {
            r#type: Some(td_api::PassportElementType::passportElementTypeAddress(
                td_api::passportElementTypeAddress,
            )),
            message: "Bad address".to_owned(),
            source: Some(
                td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceDataField(
                    td_api::inputPassportElementErrorSourceDataField {
                        field_name: "city".to_owned(),
                        data_hash: vec![1, 2, 3],
                    },
                ),
            ),
        }];
        let input_user = telegram_api::InputUser::inputUser(telegram_api::inputUser {
            user_id: 777,
            access_hash: 0,
        });
        manager.set_secure_value_errors(&mut env, input_user, errors, promise);
        assert_eq!(env.queries, vec!["setSecureValueErrors"]);
        assert_eq!(env.errors_sent.len(), 1);
        match &env.errors_sent[0][0] {
            telegram_api::SecureValueError::secureValueErrorData(error) => {
                assert_eq!(error.field, "city");
                assert_eq!(error.text, "Bad address");
                assert_eq!(error.data_hash, vec![1, 2, 3]);
            }
            other => panic!("wrong error {other:?}"),
        }
        assert!(!recorded.is_settled());

        manager.on_set_secure_value_errors_result(Result::Ok(()));
        assert!(recorded.is_settled());
    }

    #[test]
    fn set_secure_value_errors_validation() {
        let mut env = TestEnv::default();
        let mut manager = SecureManager::new();

        // a missing source fails the whole request
        let (promise, recorded) = recording_promise();
        manager.set_secure_value_errors(
            &mut env,
            telegram_api::InputUser::inputUserEmpty(telegram_api::inputUserEmpty),
            vec![td_api::InputPassportElementError {
                r#type: Some(td_api::PassportElementType::passportElementTypeAddress(
                    td_api::passportElementTypeAddress,
                )),
                message: "m".to_owned(),
                source: None,
            }],
            promise,
        );
        assert_eq!(recorded.error().code(), 400);
        assert_eq!(recorded.error().message(), "Error source must be non-empty");

        // empty file hashes fail too
        let (promise, recorded) = recording_promise();
        manager.set_secure_value_errors(
            &mut env,
            telegram_api::InputUser::inputUserEmpty(telegram_api::inputUserEmpty),
            vec![td_api::InputPassportElementError {
                r#type: Some(td_api::PassportElementType::passportElementTypeUtilityBill(
                    td_api::passportElementTypeUtilityBill,
                )),
                message: "m".to_owned(),
                source: Some(td_api::InputPassportElementErrorSource::inputPassportElementErrorSourceTranslationFiles(
                    td_api::inputPassportElementErrorSourceTranslationFiles {
                        file_hashes: Vec::new(),
                    },
                )),
            }],
            promise,
        );
        assert_eq!(recorded.error().message(), "File hashes must be non-empty");
    }
}
