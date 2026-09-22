//! Port of `td/telegram/SecureValue.{h,cpp,hpp}` (M.44) — the Telegram
//! Passport value plane: the `SecureValueType` set and its td_api/
//! telegram_api conversions, the plain/encrypted value and file records,
//! the td_api ↔ model conversions, and the value encrypt/decrypt machine
//! over [`crate::secure_storage`].
//!
//! Helper surfaces pulled in with the slice:
//! - the `Address` JSON/check helpers live in `td/telegram/OrderInfo.cpp`
//!   in the C++ (SecureValue.cpp:9-10 pulls them through `misc.h` /
//!   `OrderInfo.h`); the OrderInfo full file is its own track, so the
//!   `Address` subset SecureValue needs lands here (check_country_code,
//!   get_address, get_address_object, address_to_json, address_from_json)
//!   and moves when that track arrives;
//! - `clean_input_string` rides [`crate::misc`].
//!
//! Seams ([`SecureValueEnv`]):
//! - every `FileManager` call the C++ makes through the raw
//!   `FileManager *file_manager` parameter rides the env (register_remote
//!   for the secure file types, the file views, the encryption keys, the
//!   input-file-id resolution, `get_file_object`); the Td-side env wires
//!   them to the real M.12 [`crate::files::FileManager`];
//! - `FullRemoteFileLocation::as_input_secure_file` (FileLocation.h:491-495)
//!   is rebuilt inline from the view snapshot (the files.rs module keeps
//!   the `as_input_*` builders M.12.4);
//! - the SecureValue.hpp store/parse templates have no consumer on this
//!   base (MessageContent.cpp:86 includes the header but never serializes
//!   these records), so the binlog serialization is left to the track that
//!   first persists them.

use crate::chat_manager::DcId;
use crate::files::{FileEncryptionKey, FileId, FileType, FileUploadId};
use crate::misc::clean_input_string;
use crate::secure_storage::{
    EncryptedSecret, EnryptionAlgorithm, Secret, ValueHash, calc_value_hash, decrypt_value,
    encrypt_value,
};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::json::{JsonValueType, json_array_items, json_decode, json_encode, json_object_fn};
use tdutils::status::{Result, Status};
use tdutils::{try_result, try_status};

// ---------------------------------------------------------------------------
// The type set (SecureValue.h:27-79)
// ---------------------------------------------------------------------------

/// `td::SecureValueType` (SecureValue.h:27-42) — append-only; `None` is the
/// C++ default member initializer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecureValueType {
    #[default]
    None,
    PersonalDetails,
    Passport,
    DriverLicense,
    IdentityCard,
    InternalPassport,
    Address,
    UtilityBill,
    BankStatement,
    RentalAgreement,
    PassportRegistration,
    TemporaryRegistration,
    PhoneNumber,
    EmailAddress,
}

impl std::fmt::Display for SecureValueType {
    /// `operator<<(StringBuilder &, const SecureValueType &)` (SecureValue.cpp:34-68).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            SecureValueType::PersonalDetails => "PersonalDetails",
            SecureValueType::Passport => "Passport",
            SecureValueType::DriverLicense => "DriverLicense",
            SecureValueType::IdentityCard => "IdentityCard",
            SecureValueType::InternalPassport => "InternalPassport",
            SecureValueType::Address => "Address",
            SecureValueType::UtilityBill => "UtilityBill",
            SecureValueType::BankStatement => "BankStatement",
            SecureValueType::RentalAgreement => "RentalAgreement",
            SecureValueType::PassportRegistration => "PassportRegistration",
            SecureValueType::TemporaryRegistration => "TemporaryRegistration",
            SecureValueType::PhoneNumber => "PhoneNumber",
            SecureValueType::EmailAddress => "EmailAddress",
            SecureValueType::None => "None",
        };
        f.write_str(name)
    }
}

/// `unique_secure_value_types` (SecureValue.cpp:140-153): stable de-
/// duplication with the C++ ERROR log on each duplicate.
pub fn unique_secure_value_types(types: Vec<SecureValueType>) -> Vec<SecureValueType> {
    let mut types = types;
    let mut size = types.len();
    let mut i = 0;
    while i < size {
        let mut is_duplicate = false;
        for j in 0..i {
            if types[i] == types[j] {
                tdutils::log_error!(
                    "Have duplicate passport element type {} at positions {i} and {j}",
                    types[i]
                );
                // the C++ swap-with-back + re-scan of position i
                types.swap(i, size - 1);
                size -= 1;
                is_duplicate = true;
                break;
            }
        }
        if is_duplicate {
            i -= 1;
        }
        i += 1;
    }
    types.truncate(size);
    types
}

/// `get_secure_value_type` (SecureValue.cpp:70-103).
pub fn get_secure_value_type(secure_value_type: &telegram_api::SecureValueType) -> SecureValueType {
    match secure_value_type {
        telegram_api::SecureValueType::secureValueTypePersonalDetails(_) => {
            SecureValueType::PersonalDetails
        }
        telegram_api::SecureValueType::secureValueTypePassport(_) => SecureValueType::Passport,
        telegram_api::SecureValueType::secureValueTypeDriverLicense(_) => {
            SecureValueType::DriverLicense
        }
        telegram_api::SecureValueType::secureValueTypeIdentityCard(_) => {
            SecureValueType::IdentityCard
        }
        telegram_api::SecureValueType::secureValueTypeInternalPassport(_) => {
            SecureValueType::InternalPassport
        }
        telegram_api::SecureValueType::secureValueTypeAddress(_) => SecureValueType::Address,
        telegram_api::SecureValueType::secureValueTypeUtilityBill(_) => {
            SecureValueType::UtilityBill
        }
        telegram_api::SecureValueType::secureValueTypeBankStatement(_) => {
            SecureValueType::BankStatement
        }
        telegram_api::SecureValueType::secureValueTypeRentalAgreement(_) => {
            SecureValueType::RentalAgreement
        }
        telegram_api::SecureValueType::secureValueTypePassportRegistration(_) => {
            SecureValueType::PassportRegistration
        }
        telegram_api::SecureValueType::secureValueTypeTemporaryRegistration(_) => {
            SecureValueType::TemporaryRegistration
        }
        telegram_api::SecureValueType::secureValueTypePhone(_) => SecureValueType::PhoneNumber,
        telegram_api::SecureValueType::secureValueTypeEmail(_) => SecureValueType::EmailAddress,
    }
}

/// `get_secure_value_type_td_api` (SecureValue.cpp:105-138).
pub fn get_secure_value_type_td_api(
    passport_element_type: &td_api::PassportElementType,
) -> SecureValueType {
    match passport_element_type {
        td_api::PassportElementType::passportElementTypePersonalDetails(_) => {
            SecureValueType::PersonalDetails
        }
        td_api::PassportElementType::passportElementTypePassport(_) => SecureValueType::Passport,
        td_api::PassportElementType::passportElementTypeDriverLicense(_) => {
            SecureValueType::DriverLicense
        }
        td_api::PassportElementType::passportElementTypeIdentityCard(_) => {
            SecureValueType::IdentityCard
        }
        td_api::PassportElementType::passportElementTypeInternalPassport(_) => {
            SecureValueType::InternalPassport
        }
        td_api::PassportElementType::passportElementTypeAddress(_) => SecureValueType::Address,
        td_api::PassportElementType::passportElementTypeUtilityBill(_) => {
            SecureValueType::UtilityBill
        }
        td_api::PassportElementType::passportElementTypeBankStatement(_) => {
            SecureValueType::BankStatement
        }
        td_api::PassportElementType::passportElementTypeRentalAgreement(_) => {
            SecureValueType::RentalAgreement
        }
        td_api::PassportElementType::passportElementTypePassportRegistration(_) => {
            SecureValueType::PassportRegistration
        }
        td_api::PassportElementType::passportElementTypeTemporaryRegistration(_) => {
            SecureValueType::TemporaryRegistration
        }
        td_api::PassportElementType::passportElementTypePhoneNumber(_) => {
            SecureValueType::PhoneNumber
        }
        td_api::PassportElementType::passportElementTypeEmailAddress(_) => {
            SecureValueType::EmailAddress
        }
    }
}

/// `get_secure_value_types` (SecureValue.cpp:155-158).
pub fn get_secure_value_types(
    secure_value_types: &[telegram_api::SecureValueType],
) -> Vec<SecureValueType> {
    unique_secure_value_types(
        secure_value_types
            .iter()
            .map(get_secure_value_type)
            .collect(),
    )
}

/// `get_secure_value_types_td_api` (SecureValue.cpp:160-163).
pub fn get_secure_value_types_td_api(
    secure_value_types: &[td_api::PassportElementType],
) -> Vec<SecureValueType> {
    unique_secure_value_types(
        secure_value_types
            .iter()
            .map(get_secure_value_type_td_api)
            .collect(),
    )
}

/// `get_passport_element_type_object` (SecureValue.cpp:165-198).
pub fn get_passport_element_type_object(type_: SecureValueType) -> td_api::PassportElementType {
    use td_api::PassportElementType as T;
    match type_ {
        SecureValueType::PersonalDetails => {
            T::passportElementTypePersonalDetails(td_api::passportElementTypePersonalDetails)
        }
        SecureValueType::Passport => {
            T::passportElementTypePassport(td_api::passportElementTypePassport)
        }
        SecureValueType::DriverLicense => {
            T::passportElementTypeDriverLicense(td_api::passportElementTypeDriverLicense)
        }
        SecureValueType::IdentityCard => {
            T::passportElementTypeIdentityCard(td_api::passportElementTypeIdentityCard)
        }
        SecureValueType::InternalPassport => {
            T::passportElementTypeInternalPassport(td_api::passportElementTypeInternalPassport)
        }
        SecureValueType::Address => {
            T::passportElementTypeAddress(td_api::passportElementTypeAddress)
        }
        SecureValueType::UtilityBill => {
            T::passportElementTypeUtilityBill(td_api::passportElementTypeUtilityBill)
        }
        SecureValueType::BankStatement => {
            T::passportElementTypeBankStatement(td_api::passportElementTypeBankStatement)
        }
        SecureValueType::RentalAgreement => {
            T::passportElementTypeRentalAgreement(td_api::passportElementTypeRentalAgreement)
        }
        SecureValueType::PassportRegistration => T::passportElementTypePassportRegistration(
            td_api::passportElementTypePassportRegistration,
        ),
        SecureValueType::TemporaryRegistration => T::passportElementTypeTemporaryRegistration(
            td_api::passportElementTypeTemporaryRegistration,
        ),
        SecureValueType::PhoneNumber => {
            T::passportElementTypePhoneNumber(td_api::passportElementTypePhoneNumber)
        }
        SecureValueType::EmailAddress => {
            T::passportElementTypeEmailAddress(td_api::passportElementTypeEmailAddress)
        }
        SecureValueType::None => unreachable!("get_passport_element_type_object(None)"),
    }
}

/// `get_input_secure_value_type` (SecureValue.cpp:200-233).
pub fn get_input_secure_value_type(type_: SecureValueType) -> telegram_api::SecureValueType {
    use telegram_api::SecureValueType as T;
    match type_ {
        SecureValueType::PersonalDetails => {
            T::secureValueTypePersonalDetails(telegram_api::secureValueTypePersonalDetails)
        }
        SecureValueType::Passport => {
            T::secureValueTypePassport(telegram_api::secureValueTypePassport)
        }
        SecureValueType::DriverLicense => {
            T::secureValueTypeDriverLicense(telegram_api::secureValueTypeDriverLicense)
        }
        SecureValueType::IdentityCard => {
            T::secureValueTypeIdentityCard(telegram_api::secureValueTypeIdentityCard)
        }
        SecureValueType::InternalPassport => {
            T::secureValueTypeInternalPassport(telegram_api::secureValueTypeInternalPassport)
        }
        SecureValueType::Address => T::secureValueTypeAddress(telegram_api::secureValueTypeAddress),
        SecureValueType::UtilityBill => {
            T::secureValueTypeUtilityBill(telegram_api::secureValueTypeUtilityBill)
        }
        SecureValueType::BankStatement => {
            T::secureValueTypeBankStatement(telegram_api::secureValueTypeBankStatement)
        }
        SecureValueType::RentalAgreement => {
            T::secureValueTypeRentalAgreement(telegram_api::secureValueTypeRentalAgreement)
        }
        SecureValueType::PassportRegistration => T::secureValueTypePassportRegistration(
            telegram_api::secureValueTypePassportRegistration,
        ),
        SecureValueType::TemporaryRegistration => T::secureValueTypeTemporaryRegistration(
            telegram_api::secureValueTypeTemporaryRegistration,
        ),
        SecureValueType::PhoneNumber => T::secureValueTypePhone(telegram_api::secureValueTypePhone),
        SecureValueType::EmailAddress => {
            T::secureValueTypeEmail(telegram_api::secureValueTypeEmail)
        }
        SecureValueType::None => unreachable!("get_input_secure_value_type(None)"),
    }
}

/// `get_passport_element_types_object` (SecureValue.cpp:235-238).
pub fn get_passport_element_types_object(
    types: &[SecureValueType],
) -> Vec<td_api::PassportElementType> {
    types
        .iter()
        .map(|&t| get_passport_element_type_object(t))
        .collect()
}

/// `td::SuitableSecureValue` (SecureValue.h:62-67).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SuitableSecureValue {
    pub type_: SecureValueType,
    pub is_selfie_required: bool,
    pub is_translation_required: bool,
    pub is_native_name_required: bool,
}

/// `get_suitable_secure_value` (SecureValue.cpp:240-248).
pub fn get_suitable_secure_value(
    secure_required_type: &telegram_api::secureRequiredType,
) -> SuitableSecureValue {
    SuitableSecureValue {
        type_: get_secure_value_type(
            secure_required_type
                .r#type
                .as_ref()
                .expect("secureRequiredType carries its type"),
        ),
        is_selfie_required: secure_required_type.selfie_required,
        is_translation_required: secure_required_type.translation_required,
        is_native_name_required: secure_required_type.native_names,
    }
}

/// `get_passport_suitable_element_object` (SecureValue.cpp:250-255).
pub fn get_passport_suitable_element_object(
    required_element: &SuitableSecureValue,
) -> td_api::PassportSuitableElement {
    td_api::PassportSuitableElement {
        r#type: Some(get_passport_element_type_object(required_element.type_)),
        is_selfie_required: required_element.is_selfie_required,
        is_translation_required: required_element.is_translation_required,
        is_native_name_required: required_element.is_native_name_required,
    }
}

/// `get_passport_required_element_object` (SecureValue.cpp:257-261).
pub fn get_passport_required_element_object(
    required_element: &[SuitableSecureValue],
) -> td_api::PassportRequiredElement {
    td_api::PassportRequiredElement {
        suitable_elements: required_element
            .iter()
            .map(get_passport_suitable_element_object)
            .collect(),
    }
}

/// `get_passport_required_elements_object` (SecureValue.cpp:263-266).
pub fn get_passport_required_elements_object(
    required_elements: &[Vec<SuitableSecureValue>],
) -> Vec<td_api::PassportRequiredElement> {
    required_elements
        .iter()
        .map(|element| get_passport_required_element_object(element))
        .collect()
}

/// `get_secure_value_data_field_name` (SecureValue.cpp:268-323) — maps a
/// server error field name onto the td-side JSON field; an empty string
/// models the C++ unknown-field error return.
pub fn get_secure_value_data_field_name(type_: SecureValueType, field_name: &str) -> String {
    let known = match type_ {
        SecureValueType::PersonalDetails => match field_name {
            "first_name"
            | "middle_name"
            | "last_name"
            | "gender"
            | "country_code"
            | "residence_country_code" => Some(field_name.to_owned()),
            "first_name_native" => Some("native_first_name".to_owned()),
            "middle_name_native" => Some("native_middle_name".to_owned()),
            "last_name_native" => Some("native_last_name".to_owned()),
            "birth_date" => Some("birthdate".to_owned()),
            _ => None,
        },
        SecureValueType::Passport
        | SecureValueType::DriverLicense
        | SecureValueType::IdentityCard
        | SecureValueType::InternalPassport => match field_name {
            "expiry_date" => Some(field_name.to_owned()),
            "document_no" => Some("number".to_owned()),
            _ => None,
        },
        SecureValueType::Address => match field_name {
            "state" | "city" | "street_line1" | "street_line2" | "country_code" => {
                Some(field_name.to_owned())
            }
            "post_code" => Some("postal_code".to_owned()),
            _ => None,
        },
        SecureValueType::UtilityBill
        | SecureValueType::BankStatement
        | SecureValueType::RentalAgreement
        | SecureValueType::PassportRegistration
        | SecureValueType::TemporaryRegistration
        | SecureValueType::PhoneNumber
        | SecureValueType::EmailAddress => None,
        SecureValueType::None => {
            unreachable!("get_secure_value_data_field_name on None (the C++ UNREACHABLE)")
        }
    };
    match known {
        Some(name) => name,
        None => {
            tdutils::log_error!(
                "Receive error about unknown field \"{field_name}\" in type {type_}"
            );
            String::new()
        }
    }
}

// ---------------------------------------------------------------------------
// The records (SecureValue.h:83-215)
// ---------------------------------------------------------------------------

/// `td::DatedFile` (SecureValue.h:83-86).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DatedFile {
    pub file_id: FileId,
    pub date: i32,
}

/// `td::EncryptedSecureFile` (SecureValue.h:91-95) — the byte-string fields
/// are `Vec<u8>` (the C++ binary `string`s).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureFile {
    pub file: DatedFile,
    pub file_hash: Vec<u8>,
    pub encrypted_secret: Vec<u8>,
}

/// `td::SecureInputFile` (SecureValue.h:106-109) — the in-flight upload
/// state of one encrypted file.
#[derive(Clone, Debug, Default)]
pub struct SecureInputFile {
    pub file_upload_id: FileUploadId,
    pub input_file: Option<telegram_api::InputSecureFile>,
}

/// `td::EncryptedSecureData` (SecureValue.h:117-121).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureData {
    pub data: Vec<u8>,
    pub hash: Vec<u8>,
    pub encrypted_secret: Vec<u8>,
}

/// `td::EncryptedSecureValue` (SecureValue.h:130-139).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureValue {
    pub type_: SecureValueType,
    pub data: EncryptedSecureData,
    pub files: Vec<EncryptedSecureFile>,
    pub front_side: EncryptedSecureFile,
    pub reverse_side: EncryptedSecureFile,
    pub selfie: EncryptedSecureFile,
    pub translations: Vec<EncryptedSecureFile>,
    pub hash: Vec<u8>,
}

/// `td::EncryptedSecureCredentials` (SecureValue.h:160-164).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedSecureCredentials {
    pub data: Vec<u8>,
    pub hash: Vec<u8>,
    pub encrypted_secret: Vec<u8>,
}

/// `td::SecureDataCredentials` (SecureValue.h:177-180).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecureDataCredentials {
    pub secret: Vec<u8>,
    pub hash: Vec<u8>,
}

/// `td::SecureFileCredentials` (SecureValue.h:181-184).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecureFileCredentials {
    pub secret: Vec<u8>,
    pub hash: Vec<u8>,
}

/// `td::SecureValueCredentials` (SecureValue.h:186-195) — the C++
/// `optional` members are `Option`s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecureValueCredentials {
    pub type_: SecureValueType,
    pub hash: Vec<u8>,
    pub data: Option<SecureDataCredentials>,
    pub files: Vec<SecureFileCredentials>,
    pub front_side: Option<SecureFileCredentials>,
    pub reverse_side: Option<SecureFileCredentials>,
    pub selfie: Option<SecureFileCredentials>,
    pub translations: Vec<SecureFileCredentials>,
}

/// `td::SecureValue` (SecureValue.h:201-210).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecureValue {
    pub type_: SecureValueType,
    pub data: String,
    pub files: Vec<DatedFile>,
    pub front_side: DatedFile,
    pub reverse_side: DatedFile,
    pub selfie: DatedFile,
    pub translations: Vec<DatedFile>,
}

/// `td::SecureValueWithCredentials` (SecureValue.h:212-215).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecureValueWithCredentials {
    pub value: SecureValue,
    pub credentials: SecureValueCredentials,
}

// ---------------------------------------------------------------------------
// The FileManager seam (the C++ `FileManager *file_manager` parameter)
// ---------------------------------------------------------------------------

/// The `FullRemoteFileLocation` snapshot the value plane reads off a file
/// view (id/access-hash/dc-id plus the web flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecureFullRemote {
    pub id: i64,
    pub access_hash: i64,
    pub dc_id: DcId,
}

/// The owned `FileView` snapshot the env returns (the C++ reads these
/// through the borrowed `file_manager->get_file_view(...)`); fields carry
/// the exact accessors SecureValue.cpp reads. The C++ `FileView::empty()`
/// is the `None` env answer (the Rust `get_file_view` is `Option`).
#[derive(Clone, Debug)]
pub struct SecureFileView {
    /// `FileView::get_type()`.
    pub file_type: FileType,
    /// `FileView::get_main_file_id()`.
    pub main_file_id: FileId,
    /// `FileView::is_encrypted_secure()`.
    pub is_encrypted_secure: bool,
    /// `FileView::get_full_remote_location()` — `None` when absent.
    pub full_remote: Option<SecureFullRemote>,
    /// `full_remote->is_web()`.
    pub full_remote_is_web: bool,
    /// `FileView::encryption_key()` (owned clone).
    pub encryption_key: FileEncryptionKey,
    /// `FileView::expected_size(false)`.
    pub expected_size: i64,
    /// `FileView::suggested_path()`.
    pub suggested_path: String,
    /// `FileView::size()`.
    pub size: i64,
}

/// The `G()`/`FileManager` surface of the value plane. The default
/// implementations answer the not-wired seams; the Td-side env forwards to
/// the real M.12 [`crate::files::FileManager`].
pub trait SecureValueEnv {
    /// `G()->close_flag()` (the `is_expected_error` tail).
    fn close_flag(&self) -> bool;

    /// `G()->unix_time()` (the `DatedFile::date` source, SecureValue.cpp:854).
    fn unix_time(&mut self) -> i32;

    /// `file_manager->register_remote(FullRemoteFileLocation(FileType::
    /// SecureEncrypted, id, access_hash, dc_id, ""), FileLocationSource::
    /// FromServer, DialogId(), size, 0, remote_name)` (SecureValue.cpp:355-358).
    fn file_register_secure_encrypted(
        &mut self,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        size: i64,
        remote_name: String,
    ) -> FileId;

    /// `file_manager->register_remote(FullRemoteFileLocation(FileType::
    /// SecureDecrypted, ...), FileLocationSource::FromServer, DialogId(),
    /// 0, expected_size, suggested_path)` (SecureValue.cpp:436-439).
    fn file_register_secure_decrypted(
        &mut self,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        expected_size: i64,
        suggested_path: String,
    ) -> FileId;

    /// `file_manager->get_file_view(file_id)` — the owned snapshot; `None`
    /// for the C++ empty view.
    fn file_view(&mut self, file_id: FileId) -> Option<SecureFileView>;

    /// `file_manager->set_encryption_key(file_id, key)` (SecureValue.cpp:1161).
    fn file_set_encryption_key(&mut self, file_id: FileId, key: FileEncryptionKey);

    /// `file_manager->get_file_object(file_id)` (SecureValue.cpp:412).
    fn file_object(&mut self, file_id: FileId) -> td_api::File;

    /// `file_manager->get_input_file_id(FileType::SecureEncrypted, file,
    /// DialogId(), false, false, false, true)` (SecureValue.cpp:849-851).
    fn file_get_input_file_id(&mut self, file: Option<&td_api::InputFile>) -> Result<FileId>;

    /// `file_manager->merge(to_file_id, from_file_id)` (SecureValue.cpp:657).
    fn file_merge(&mut self, to_file_id: FileId, from_file_id: FileId) -> Result<()>;

    /// `file_manager->copy_file_id(file_id, FileType::SecureEncrypted,
    /// DialogId(), "SetSecureValue")` (SecureManager.cpp:555).
    fn file_copy_file_id(&mut self, file_id: FileId) -> FileId;

    /// `file_manager->cancel_upload(file_upload_id)` (SecureManager.cpp:506).
    fn file_cancel_upload(&mut self, file_upload_id: FileUploadId);

    /// `file_manager->resume_upload(file_upload_id, {}, upload_callback_,
    /// 1, 0, force)` (SecureManager.cpp:562) — the secure-upload callback
    /// contract (`on_upload_secure_ok`) is [blocked: the secure upload
    /// slice]; the seam records the request without a completion channel.
    fn file_resume_upload_secure(&mut self, file_upload_id: FileUploadId, force: bool) {
        let _ = (file_upload_id, force);
        tdutils::log_info!(
            "FileManager::resume_upload(secure) — [blocked: the secure upload contract]"
        );
    }

    /// `FileManager::get_internal_upload_id()` (SecureManager.cpp:558).
    fn file_get_internal_upload_id(&mut self) -> i64;
}

// ---------------------------------------------------------------------------
// The telegram_api → model conversions (SecureValue.cpp:325-648)
// ---------------------------------------------------------------------------

/// `get_encrypted_secure_file` (SecureValue.cpp:341-372).
pub fn get_encrypted_secure_file(
    env: &mut impl SecureValueEnv,
    secure_file_ptr: telegram_api::SecureFile,
) -> EncryptedSecureFile {
    let mut result = EncryptedSecureFile::default();
    match secure_file_ptr {
        telegram_api::SecureFile::secureFileEmpty(_) => {}
        telegram_api::SecureFile::secureFile(secure_file) => {
            let dc_id = secure_file.dc_id;
            if !DcId::is_valid_raw(dc_id) {
                tdutils::log_error!("Wrong dc_id = {dc_id}");
            } else {
                result.file.file_id = env.file_register_secure_encrypted(
                    secure_file.id,
                    secure_file.access_hash,
                    DcId::internal(dc_id),
                    secure_file.size,
                    format!("{}.jpg", secure_file.id),
                );
                result.file.date = secure_file.date;
                if result.file.date < 0 {
                    tdutils::log_error!("Receive wrong date {}", result.file.date);
                    result.file.date = 0;
                }
                result.encrypted_secret = secure_file.secret;
                result.file_hash = secure_file.file_hash;
            }
        }
    }
    result
}

/// `get_encrypted_secure_files` (SecureValue.cpp:374-385) — skips the
/// files without a valid registered id.
pub fn get_encrypted_secure_files(
    env: &mut impl SecureValueEnv,
    secure_files: Vec<telegram_api::SecureFile>,
) -> Vec<EncryptedSecureFile> {
    let mut results = Vec::with_capacity(secure_files.len());
    for secure_file in secure_files {
        let result = get_encrypted_secure_file(env, secure_file);
        if result.file.file_id.is_valid() {
            results.push(result);
        }
    }
    results
}

/// `FullRemoteFileLocation::as_input_secure_file` (FileLocation.h:491-495)
/// rebuilt over the env snapshot (see the module seams note).
fn as_input_secure_file(full_remote: SecureFullRemote) -> telegram_api::InputSecureFile {
    telegram_api::InputSecureFile::inputSecureFile(telegram_api::inputSecureFile {
        id: full_remote.id,
        access_hash: full_remote.access_hash,
    })
}

/// `get_input_secure_file_object` (SecureValue.cpp:387-409) — attaches the
/// file hashes to the uploaded secure file, or falls back to the remote
/// location. `None` models the C++ nullptr return on an invalid file.
pub fn get_input_secure_file_object(
    env: &mut impl SecureValueEnv,
    file: &EncryptedSecureFile,
    input_file: &mut SecureInputFile,
) -> Option<telegram_api::InputSecureFile> {
    if !file.file.file_id.is_valid() {
        tdutils::log_error!("Receive invalid EncryptedSecureFile");
        return None;
    }
    assert!(input_file.file_upload_id.file_id.is_valid());
    {
        let uploaded_view = env.file_view(input_file.file_upload_id.file_id)?;
        let file_view = env.file_view(file.file.file_id)?;
        assert_eq!(
            file_view.main_file_id, uploaded_view.main_file_id,
            "the encrypted file and its upload must be the same node"
        );
    }
    if let Some(res) = std::mem::take(&mut input_file.input_file) {
        match res {
            telegram_api::InputSecureFile::inputSecureFileUploaded(mut uploaded) => {
                uploaded.secret = file.encrypted_secret.clone();
                uploaded.file_hash = file.file_hash.clone();
                return Some(telegram_api::InputSecureFile::inputSecureFileUploaded(
                    uploaded,
                ));
            }
            other => {
                unreachable!("the upload input must be inputSecureFileUploaded: {other:?}");
            }
        }
    }
    let file_view = env.file_view(file.file.file_id)?;
    let full_remote = file_view.full_remote?;
    Some(as_input_secure_file(full_remote))
}

/// `get_input_secure_files_object` (SecureValue.cpp:449-461).
pub fn get_input_secure_files_object(
    env: &mut impl SecureValueEnv,
    files: &[EncryptedSecureFile],
    input_files: &mut [SecureInputFile],
) -> Vec<telegram_api::InputSecureFile> {
    assert_eq!(files.len(), input_files.len());
    let mut results = Vec::with_capacity(files.len());
    for (file, input_file) in files.iter().zip(input_files.iter_mut()) {
        if let Some(result) = get_input_secure_file_object(env, file, input_file) {
            results.push(result);
        }
    }
    results
}

/// `get_encrypted_secure_data` (SecureValue.cpp:471-478).
pub fn get_encrypted_secure_data(secure_data: telegram_api::SecureData) -> EncryptedSecureData {
    EncryptedSecureData {
        data: secure_data.data,
        hash: secure_data.data_hash,
        encrypted_secret: secure_data.secret,
    }
}

/// `get_secure_data_object` (SecureValue.cpp:480-483).
pub fn get_secure_data_object(data: &EncryptedSecureData) -> telegram_api::SecureData {
    telegram_api::SecureData {
        data: data.data.clone(),
        data_hash: data.hash.clone(),
        secret: data.encrypted_secret.clone(),
    }
}

/// `check_encrypted_secure_value` (SecureValue.cpp:494-528).
fn check_encrypted_secure_value(value: &EncryptedSecureValue) -> bool {
    let has_encrypted_data = !value.data.hash.is_empty();
    let has_plain_data = !has_encrypted_data && !value.data.data.is_empty();
    let has_files = !value.files.is_empty();
    let has_front_side = value.front_side.file.file_id.is_valid();
    let has_reverse_side = value.reverse_side.file.file_id.is_valid();
    let has_selfie = value.selfie.file.file_id.is_valid();
    let has_translations = !value.translations.is_empty();
    match value.type_ {
        SecureValueType::PersonalDetails | SecureValueType::Address => {
            has_encrypted_data
                && !has_files
                && !has_front_side
                && !has_reverse_side
                && !has_selfie
                && !has_translations
        }
        SecureValueType::Passport | SecureValueType::InternalPassport => {
            has_encrypted_data && !has_files && has_front_side && !has_reverse_side
        }
        SecureValueType::DriverLicense | SecureValueType::IdentityCard => {
            has_encrypted_data && !has_files && has_front_side && has_reverse_side
        }
        SecureValueType::UtilityBill
        | SecureValueType::BankStatement
        | SecureValueType::RentalAgreement
        | SecureValueType::PassportRegistration
        | SecureValueType::TemporaryRegistration => {
            !has_encrypted_data
                && !has_plain_data
                && has_files
                && !has_front_side
                && !has_reverse_side
                && !has_selfie
        }
        SecureValueType::PhoneNumber | SecureValueType::EmailAddress => {
            has_plain_data
                && !has_files
                && !has_front_side
                && !has_reverse_side
                && !has_selfie
                && !has_translations
        }
        SecureValueType::None => false,
    }
}

/// `get_encrypted_secure_value` (SecureValue.cpp:530-569) — the invalid
/// values collapse to the default (None-typed) record, exactly like the
/// C++ early return.
pub fn get_encrypted_secure_value(
    env: &mut impl SecureValueEnv,
    secure_value: telegram_api::SecureValue,
) -> EncryptedSecureValue {
    let mut result = EncryptedSecureValue {
        type_: get_secure_value_type(
            secure_value
                .r#type
                .as_ref()
                .expect("secureValue carries its type"),
        ),
        ..EncryptedSecureValue::default()
    };
    if let Some(plain_data) = secure_value.plain_data {
        match plain_data {
            telegram_api::SecurePlainData::securePlainPhone(phone) => {
                result.data.data = phone.phone.into_bytes();
            }
            telegram_api::SecurePlainData::securePlainEmail(email) => {
                result.data.data = email.email.into_bytes();
            }
        }
    }
    if let Some(data) = secure_value.data {
        result.data = get_encrypted_secure_data(data);
    }
    result.files = get_encrypted_secure_files(env, secure_value.files);
    if let Some(front_side) = secure_value.front_side {
        result.front_side = get_encrypted_secure_file(env, front_side);
    }
    if let Some(reverse_side) = secure_value.reverse_side {
        result.reverse_side = get_encrypted_secure_file(env, reverse_side);
    }
    if let Some(selfie) = secure_value.selfie {
        result.selfie = get_encrypted_secure_file(env, selfie);
    }
    result.translations = get_encrypted_secure_files(env, secure_value.translation);
    result.hash = secure_value.hash;
    if !check_encrypted_secure_value(&result) {
        tdutils::log_error!(
            "Receive invalid encrypted secure value of type {}",
            result.type_
        );
        return EncryptedSecureValue::default();
    }
    result
}

/// `get_encrypted_secure_values` (SecureValue.cpp:571-582) — skips the
/// invalid (None-typed) records.
pub fn get_encrypted_secure_values(
    env: &mut impl SecureValueEnv,
    secure_values: Vec<telegram_api::SecureValue>,
) -> Vec<EncryptedSecureValue> {
    let mut results = Vec::with_capacity(secure_values.len());
    for secure_value in secure_values {
        let result = get_encrypted_secure_value(env, secure_value);
        if result.type_ != SecureValueType::None {
            results.push(result);
        }
    }
    results
}

/// `get_encrypted_passport_element_object` (SecureValue.cpp:584-594).
pub fn get_encrypted_passport_element_object(
    env: &mut impl SecureValueEnv,
    value: &EncryptedSecureValue,
) -> td_api::EncryptedPassportElement {
    let is_plain = value.data.hash.is_empty();
    td_api::EncryptedPassportElement {
        r#type: Some(get_passport_element_type_object(value.type_)),
        data: if is_plain {
            Vec::new()
        } else {
            value.data.data.clone()
        },
        front_side: value
            .front_side
            .file
            .file_id
            .is_valid()
            .then(|| get_dated_file_object_for_encrypted_file(env, &value.front_side)),
        reverse_side: value
            .reverse_side
            .file
            .file_id
            .is_valid()
            .then(|| get_dated_file_object_for_encrypted_file(env, &value.reverse_side)),
        selfie: value
            .selfie
            .file
            .file_id
            .is_valid()
            .then(|| get_dated_file_object_for_encrypted_file(env, &value.selfie)),
        translation: get_dated_files_object_for_encrypted_files(env, &value.translations),
        files: get_dated_files_object_for_encrypted_files(env, &value.files),
        value: if is_plain {
            String::from_utf8_lossy(&value.data.data).into_owned()
        } else {
            String::new()
        },
        hash: String::from_utf8_lossy(&value.hash).into_owned(),
    }
}

/// `get_secure_credentials_encrypted_object` (SecureValue.cpp:658-662).
pub fn get_secure_credentials_encrypted_object(
    credentials: &EncryptedSecureCredentials,
) -> telegram_api::SecureCredentialsEncrypted {
    telegram_api::SecureCredentialsEncrypted {
        data: credentials.data.clone(),
        hash: credentials.hash.clone(),
        secret: credentials.encrypted_secret.clone(),
    }
}

/// `get_encrypted_secure_credentials` (SecureValue.cpp:664-672).
pub fn get_encrypted_secure_credentials(
    credentials: telegram_api::SecureCredentialsEncrypted,
) -> EncryptedSecureCredentials {
    EncryptedSecureCredentials {
        data: credentials.data,
        hash: credentials.hash,
        encrypted_secret: credentials.secret,
    }
}

/// `get_encrypted_credentials_object` (SecureValue.cpp:674-678).
pub fn get_encrypted_credentials_object(
    credentials: &EncryptedSecureCredentials,
) -> td_api::EncryptedCredentials {
    td_api::EncryptedCredentials {
        data: credentials.data.clone(),
        hash: credentials.hash.clone(),
        secret: credentials.encrypted_secret.clone(),
    }
}

// ---------------------------------------------------------------------------
// The input validation + td_api → model conversions (SecureValue.cpp:680-1148)
// ---------------------------------------------------------------------------

/// `check_date` (SecureValue.cpp:681-699).
fn check_date(day: i32, month: i32, year: i32) -> Status {
    if !(1..=31).contains(&day) {
        return Status::error(400, "Wrong day number specified");
    }
    if !(1..=12).contains(&month) {
        return Status::error(400, "Wrong month number specified");
    }
    if !(1..=9999).contains(&year) {
        return Status::error(400, "Wrong year number specified");
    }

    let is_leap = month == 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0));
    let days_in_month = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day > days_in_month[month as usize - 1] + i32::from(is_leap) {
        return Status::error(400, "Wrong day in month number specified");
    }

    Status::ok()
}

/// `get_date` (SecureValue.cpp:701-709) — an absent date stays empty.
fn get_date(date: Option<td_api::Date>) -> Result<String> {
    let Some(date) = date else {
        return Result::Ok(String::new());
    };
    try_status!(check_date(date.day, date.month, date.year));

    Result::Ok(format!(
        "{:02}.{:02}.{:04}",
        date.day, date.month, date.year
    ))
}

/// `to_int32` (SecureValue.cpp:711-721).
fn to_int32(str: &[u8]) -> Result<i32> {
    assert!(str.len() <= 9, "the C++ CHECK on digits10");
    let mut integer_value: i32 = 0;
    for &c in str {
        if !c.is_ascii_digit() {
            return Result::Err(Status::error(
                400,
                &format!("Can't parse \"{}\" as number", String::from_utf8_lossy(str)),
            ));
        }
        integer_value = integer_value * 10 + i32::from(c - b'0');
    }
    Result::Ok(integer_value)
}

/// `get_date_object` (SecureValue.cpp:723-741) — an empty date is the C++
/// nullptr.
fn get_date_object(date: &str) -> Result<Option<td_api::Date>> {
    if date.is_empty() {
        return Result::Ok(None);
    }
    if date.len() > 10 || date.len() < 8 {
        return Result::Err(Status::error(
            400,
            &format!("Date \"{date}\" has wrong length"),
        ));
    }
    let parts: Vec<&str> = date.split('.').collect();
    if parts.len() != 3
        || parts[0].len() > 2
        || parts[1].len() > 2
        || parts[2].len() != 4
        || parts[0].is_empty()
        || parts[1].is_empty()
    {
        return Result::Err(Status::error(
            400,
            &format!("Date \"{date}\" has wrong parts"),
        ));
    }
    try_result!(day, to_int32(parts[0].as_bytes()));
    try_result!(month, to_int32(parts[1].as_bytes()));
    try_result!(year, to_int32(parts[2].as_bytes()));
    try_status!(check_date(day, month, year));

    Result::Ok(Some(td_api::Date { day, month, year }))
}

/// `check_name` (SecureValue.cpp:743-751).
fn check_name(name: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(name) else {
        return Status::error(400, "Name must be encoded in UTF-8");
    };
    *name = cleaned;
    if tdutils::utf8::utf8_length(name) > 255 {
        return Status::error(400, "Name is too long");
    }
    Status::ok()
}

/// `check_gender` (SecureValue.cpp:753-758).
fn check_gender(gender: &str) -> Status {
    if gender != "male" && gender != "female" {
        return Status::error(400, "Unsupported gender specified");
    }
    Status::ok()
}

/// `get_personal_details` (SecureValue.cpp:760-790) — the JSON payload the
/// value data holds.
fn get_personal_details(personal_details: Option<td_api::PersonalDetails>) -> Result<String> {
    let Some(mut personal_details) = personal_details else {
        return Result::Err(Status::error(400, "Personal details must be non-empty"));
    };
    try_status!(check_name(&mut personal_details.first_name));
    try_status!(check_name(&mut personal_details.middle_name));
    try_status!(check_name(&mut personal_details.last_name));
    try_status!(check_name(&mut personal_details.native_first_name));
    try_status!(check_name(&mut personal_details.native_middle_name));
    try_status!(check_name(&mut personal_details.native_last_name));
    try_result!(birthdate, get_date(personal_details.birthdate));
    if birthdate.is_empty() {
        return Result::Err(Status::error(400, "Birthdate must be non-empty"));
    }
    try_status!(check_gender(&personal_details.gender));
    try_status!(check_country_code(&mut personal_details.country_code));
    try_status!(check_country_code(
        &mut personal_details.residence_country_code
    ));

    Result::Ok(json_encode(
        &json_object_fn(|o| {
            o.field("first_name", personal_details.first_name.as_str());
            o.field("middle_name", personal_details.middle_name.as_str());
            o.field("last_name", personal_details.last_name.as_str());
            o.field(
                "first_name_native",
                personal_details.native_first_name.as_str(),
            );
            o.field(
                "middle_name_native",
                personal_details.native_middle_name.as_str(),
            );
            o.field(
                "last_name_native",
                personal_details.native_last_name.as_str(),
            );
            o.field("birth_date", birthdate.as_str());
            o.field("gender", personal_details.gender.as_str());
            o.field("country_code", personal_details.country_code.as_str());
            o.field(
                "residence_country_code",
                personal_details.residence_country_code.as_str(),
            );
        }),
        false,
    ))
}

/// `get_personal_details_object` (SecureValue.cpp:792-834).
fn get_personal_details_object(personal_details: &str) -> Result<td_api::PersonalDetails> {
    let value = match json_decode(personal_details) {
        Result::Ok(value) => value,
        Result::Err(_) => {
            return Result::Err(Status::error(
                400,
                "Can't parse personal details JSON object",
            ));
        }
    };
    if value.ty() != JsonValueType::Object {
        return Result::Err(Status::error(400, "Personal details must be an Object"));
    }

    let object = value.get_object();
    try_result!(
        first_name,
        object.get_optional_string_field("first_name", "")
    );
    try_result!(
        middle_name,
        object.get_optional_string_field("middle_name", "")
    );
    try_result!(last_name, object.get_optional_string_field("last_name", ""));
    try_result!(
        native_first_name,
        object.get_optional_string_field("first_name_native", "")
    );
    try_result!(
        native_middle_name,
        object.get_optional_string_field("middle_name_native", "")
    );
    try_result!(
        native_last_name,
        object.get_optional_string_field("last_name_native", "")
    );
    try_result!(
        birthdate,
        object.get_optional_string_field("birth_date", "")
    );
    if birthdate.is_empty() {
        return Result::Err(Status::error(400, "Birthdate must be non-empty"));
    }
    try_result!(gender, object.get_optional_string_field("gender", ""));
    try_result!(
        country_code,
        object.get_optional_string_field("country_code", "")
    );
    try_result!(
        residence_country_code,
        object.get_optional_string_field("residence_country_code", "")
    );

    let mut first_name = first_name;
    let mut middle_name = middle_name;
    let mut last_name = last_name;
    let mut native_first_name = native_first_name;
    let mut native_middle_name = native_middle_name;
    let mut native_last_name = native_last_name;
    try_status!(check_name(&mut first_name));
    try_status!(check_name(&mut middle_name));
    try_status!(check_name(&mut last_name));
    try_status!(check_name(&mut native_first_name));
    try_status!(check_name(&mut native_middle_name));
    try_status!(check_name(&mut native_last_name));
    try_result!(date, get_date_object(&birthdate));
    try_status!(check_gender(&gender));
    let mut country_code = country_code;
    let mut residence_country_code = residence_country_code;
    try_status!(check_country_code(&mut country_code));
    try_status!(check_country_code(&mut residence_country_code));

    Result::Ok(td_api::PersonalDetails {
        first_name,
        middle_name,
        last_name,
        native_first_name,
        native_middle_name,
        native_last_name,
        birthdate: date,
        gender,
        country_code,
        residence_country_code,
    })
}

/// `check_document_number` (SecureValue.cpp:836-847).
fn check_document_number(number: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(number) else {
        return Status::error(400, "Document number must be encoded in UTF-8");
    };
    *number = cleaned;
    if number.is_empty() {
        return Status::error(400, "Document number must be non-empty");
    }
    if tdutils::utf8::utf8_length(number) > 24 {
        return Status::error(400, "Document number is too long");
    }
    Status::ok()
}

/// `get_secure_file` (SecureValue.cpp:849-856).
fn get_secure_file(
    env: &mut impl SecureValueEnv,
    file: Option<td_api::InputFile>,
) -> Result<DatedFile> {
    try_result!(file_id, env.file_get_input_file_id(file.as_ref()));
    Result::Ok(DatedFile {
        file_id,
        date: env.unix_time(),
    })
}

/// `get_secure_files` (SecureValue.cpp:858-866).
fn get_secure_files(
    env: &mut impl SecureValueEnv,
    files: Vec<td_api::InputFile>,
) -> Result<Vec<DatedFile>> {
    let mut result = Vec::with_capacity(files.len());
    for file in files {
        try_result!(dated_file, get_secure_file(env, Some(file)));
        result.push(dated_file);
    }
    Result::Ok(result)
}

/// `get_identity_document` (SecureValue.cpp:868-908).
fn get_identity_document(
    type_: SecureValueType,
    env: &mut impl SecureValueEnv,
    identity_document: Option<td_api::InputIdentityDocument>,
    need_reverse_side: bool,
) -> Result<SecureValue> {
    let Some(mut identity_document) = identity_document else {
        return Result::Err(Status::error(400, "Identity document must be non-empty"));
    };
    try_status!(check_document_number(&mut identity_document.number));
    try_result!(date, get_date(identity_document.expiration_date));

    let mut res = SecureValue {
        type_,
        data: json_encode(
            &json_object_fn(|o| {
                o.field("document_no", identity_document.number.as_str());
                o.field("expiry_date", date.as_str());
            }),
            false,
        ),
        ..SecureValue::default()
    };

    let Some(front_side) = identity_document.front_side else {
        return Result::Err(Status::error(400, "Document's front side is required"));
    };
    if identity_document.reverse_side.is_none() {
        if need_reverse_side {
            return Result::Err(Status::error(400, "Document's reverse side is required"));
        }
    } else if !need_reverse_side {
        return Result::Err(Status::error(400, "Document can't have a reverse side"));
    }

    try_result!(front_side_file, get_secure_file(env, Some(front_side)));
    res.front_side = front_side_file;
    if let Some(reverse_side) = identity_document.reverse_side {
        try_result!(reverse_side_file, get_secure_file(env, Some(reverse_side)));
        res.reverse_side = reverse_side_file;
    }
    if let Some(selfie) = identity_document.selfie {
        try_result!(selfie_file, get_secure_file(env, Some(selfie)));
        res.selfie = selfie_file;
    }
    if !identity_document.translation.is_empty() {
        try_result!(
            translation_files,
            get_secure_files(env, identity_document.translation)
        );
        res.translations = translation_files;
    }
    Result::Ok(res)
}

/// `get_identity_document_object` (SecureValue.cpp:910-949).
fn get_identity_document_object(
    env: &mut impl SecureValueEnv,
    value: &SecureValue,
) -> Result<td_api::IdentityDocument> {
    assert!(value.files.is_empty());

    let mut front_side = None;
    let mut reverse_side = None;
    let mut selfie = None;
    if value.front_side.file_id.is_valid() {
        front_side = Some(get_dated_file_object(env, value.front_side));
    }
    if value.reverse_side.file_id.is_valid() {
        reverse_side = Some(get_dated_file_object(env, value.reverse_side));
    }
    if value.selfie.file_id.is_valid() {
        selfie = Some(get_dated_file_object(env, value.selfie));
    }

    let json_value = match json_decode(&value.data) {
        Result::Ok(json_value) => json_value,
        Result::Err(_) => {
            return Result::Err(Status::error(
                400,
                "Can't parse identity document JSON object",
            ));
        }
    };
    if json_value.ty() != JsonValueType::Object {
        return Result::Err(Status::error(400, "Identity document must be an Object"));
    }

    let object = json_value.get_object();
    let mut number = match object.get_optional_string_field("document_no", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    try_result!(
        expiry_date,
        object.get_optional_string_field("expiry_date", "")
    );

    try_status!(check_document_number(&mut number));
    try_result!(date, get_date_object(&expiry_date));

    let translations = get_dated_files_object(env, &value.translations);
    Result::Ok(td_api::IdentityDocument {
        number,
        expiration_date: date,
        front_side,
        reverse_side,
        selfie,
        translation: translations,
    })
}

/// `get_personal_document` (SecureValue.cpp:951-968).
fn get_personal_document(
    type_: SecureValueType,
    env: &mut impl SecureValueEnv,
    personal_document: Option<td_api::InputPersonalDocument>,
) -> Result<SecureValue> {
    let Some(personal_document) = personal_document else {
        return Result::Err(Status::error(400, "Personal document must be non-empty"));
    };

    let mut res = SecureValue {
        type_,
        ..SecureValue::default()
    };
    if personal_document.files.is_empty() {
        return Result::Err(Status::error(400, "Document's files are required"));
    }
    try_result!(
        document_files,
        get_secure_files(env, personal_document.files)
    );
    res.files = document_files;
    if !personal_document.translation.is_empty() {
        try_result!(
            document_translations,
            get_secure_files(env, personal_document.translation)
        );
        res.translations = document_translations;
    }
    Result::Ok(res)
}

/// `get_personal_document_object` (SecureValue.cpp:970-974).
fn get_personal_document_object(
    env: &mut impl SecureValueEnv,
    value: &SecureValue,
) -> td_api::PersonalDocument {
    td_api::PersonalDocument {
        files: get_dated_files_object(env, &value.files),
        translation: get_dated_files_object(env, &value.translations),
    }
}

/// `check_phone_number` (SecureValue.cpp:976-981).
fn check_phone_number(phone_number: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(phone_number) else {
        return Status::error(400, "Phone number must be encoded in UTF-8");
    };
    *phone_number = cleaned;
    Status::ok()
}

/// `check_email_address` (SecureValue.cpp:983-988).
fn check_email_address(email_address: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(email_address) else {
        return Status::error(400, "Email address must be encoded in UTF-8");
    };
    *email_address = cleaned;
    Status::ok()
}

/// `get_secure_value` (SecureValue.cpp:990-1069).
pub fn get_secure_value(
    env: &mut impl SecureValueEnv,
    input_passport_element: Option<td_api::InputPassportElement>,
) -> Result<SecureValue> {
    let Some(input_passport_element) = input_passport_element else {
        return Result::Err(Status::error(400, "InputPassportElement must be non-empty"));
    };

    let mut res = SecureValue::default();
    match input_passport_element {
        td_api::InputPassportElement::inputPassportElementPersonalDetails(input) => {
            res.type_ = SecureValueType::PersonalDetails;
            try_result!(details_data, get_personal_details(input.personal_details));
            res.data = details_data;
            Result::Ok(res)
        }
        td_api::InputPassportElement::inputPassportElementPassport(input) => {
            get_identity_document(SecureValueType::Passport, env, input.passport, false)
        }
        td_api::InputPassportElement::inputPassportElementDriverLicense(input) => {
            get_identity_document(
                SecureValueType::DriverLicense,
                env,
                input.driver_license,
                true,
            )
        }
        td_api::InputPassportElement::inputPassportElementIdentityCard(input) => {
            get_identity_document(
                SecureValueType::IdentityCard,
                env,
                input.identity_card,
                true,
            )
        }
        td_api::InputPassportElement::inputPassportElementInternalPassport(input) => {
            get_identity_document(
                SecureValueType::InternalPassport,
                env,
                input.internal_passport,
                false,
            )
        }
        td_api::InputPassportElement::inputPassportElementAddress(input) => {
            res.type_ = SecureValueType::Address;
            try_result!(address, get_address(input.address));
            res.data = address_to_json(&address);
            Result::Ok(res)
        }
        td_api::InputPassportElement::inputPassportElementUtilityBill(input) => {
            get_personal_document(SecureValueType::UtilityBill, env, input.utility_bill)
        }
        td_api::InputPassportElement::inputPassportElementBankStatement(input) => {
            get_personal_document(SecureValueType::BankStatement, env, input.bank_statement)
        }
        td_api::InputPassportElement::inputPassportElementRentalAgreement(input) => {
            get_personal_document(
                SecureValueType::RentalAgreement,
                env,
                input.rental_agreement,
            )
        }
        td_api::InputPassportElement::inputPassportElementPassportRegistration(input) => {
            get_personal_document(
                SecureValueType::PassportRegistration,
                env,
                input.passport_registration,
            )
        }
        td_api::InputPassportElement::inputPassportElementTemporaryRegistration(input) => {
            get_personal_document(
                SecureValueType::TemporaryRegistration,
                env,
                input.temporary_registration,
            )
        }
        td_api::InputPassportElement::inputPassportElementPhoneNumber(mut input) => {
            res.type_ = SecureValueType::PhoneNumber;
            try_status!(check_phone_number(&mut input.phone_number));
            res.data = input.phone_number;
            Result::Ok(res)
        }
        td_api::InputPassportElement::inputPassportElementEmailAddress(mut input) => {
            res.type_ = SecureValueType::EmailAddress;
            try_status!(check_email_address(&mut input.email_address));
            res.data = input.email_address;
            Result::Ok(res)
        }
    }
}

/// `get_passport_element_object` (SecureValue.cpp:1071-1131).
pub fn get_passport_element_object(
    env: &mut impl SecureValueEnv,
    value: &SecureValue,
) -> Result<td_api::PassportElement> {
    use td_api::PassportElement as E;
    match value.type_ {
        SecureValueType::PersonalDetails => {
            try_result!(personal_details, get_personal_details_object(&value.data));
            Result::Ok(E::passportElementPersonalDetails(
                td_api::passportElementPersonalDetails {
                    personal_details: Some(personal_details),
                },
            ))
        }
        SecureValueType::Passport => {
            try_result!(passport, get_identity_document_object(env, value));
            Result::Ok(E::passportElementPassport(
                td_api::passportElementPassport {
                    passport: Some(passport),
                },
            ))
        }
        SecureValueType::DriverLicense => {
            try_result!(driver_license, get_identity_document_object(env, value));
            Result::Ok(E::passportElementDriverLicense(
                td_api::passportElementDriverLicense {
                    driver_license: Some(driver_license),
                },
            ))
        }
        SecureValueType::IdentityCard => {
            try_result!(identity_card, get_identity_document_object(env, value));
            Result::Ok(E::passportElementIdentityCard(
                td_api::passportElementIdentityCard {
                    identity_card: Some(identity_card),
                },
            ))
        }
        SecureValueType::InternalPassport => {
            try_result!(internal_passport, get_identity_document_object(env, value));
            Result::Ok(E::passportElementInternalPassport(
                td_api::passportElementInternalPassport {
                    internal_passport: Some(internal_passport),
                },
            ))
        }
        SecureValueType::Address => {
            try_result!(address, address_from_json(&value.data));
            Result::Ok(E::passportElementAddress(td_api::passportElementAddress {
                address: Some(get_address_object(&address)),
            }))
        }
        SecureValueType::UtilityBill
        | SecureValueType::BankStatement
        | SecureValueType::RentalAgreement
        | SecureValueType::PassportRegistration
        | SecureValueType::TemporaryRegistration => {
            let document = get_personal_document_object(env, value);
            Result::Ok(match value.type_ {
                SecureValueType::UtilityBill => {
                    E::passportElementUtilityBill(td_api::passportElementUtilityBill {
                        utility_bill: Some(document),
                    })
                }
                SecureValueType::BankStatement => {
                    E::passportElementBankStatement(td_api::passportElementBankStatement {
                        bank_statement: Some(document),
                    })
                }
                SecureValueType::RentalAgreement => {
                    E::passportElementRentalAgreement(td_api::passportElementRentalAgreement {
                        rental_agreement: Some(document),
                    })
                }
                SecureValueType::PassportRegistration => E::passportElementPassportRegistration(
                    td_api::passportElementPassportRegistration {
                        passport_registration: Some(document),
                    },
                ),
                SecureValueType::TemporaryRegistration => E::passportElementTemporaryRegistration(
                    td_api::passportElementTemporaryRegistration {
                        temporary_registration: Some(document),
                    },
                ),
                _ => unreachable!("the outer match pins the type"),
            })
        }
        SecureValueType::PhoneNumber => Result::Ok(E::passportElementPhoneNumber(
            td_api::passportElementPhoneNumber {
                phone_number: value.data.clone(),
            },
        )),
        SecureValueType::EmailAddress => Result::Ok(E::passportElementEmailAddress(
            td_api::passportElementEmailAddress {
                email_address: value.data.clone(),
            },
        )),
        SecureValueType::None => {
            unreachable!("get_passport_element_object on None (the C++ UNREACHABLE)")
        }
    }
}

/// `get_passport_elements_object` (SecureValue.cpp:1133-1148) — the failing
/// conversions are skipped with the C++ ERROR log.
pub fn get_passport_elements_object(
    env: &mut impl SecureValueEnv,
    values: &[SecureValue],
) -> td_api::PassportElements {
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let r_obj = get_passport_element_object(env, value);
        match r_obj {
            Result::Ok(obj) => result.push(obj),
            Result::Err(error) => {
                tdutils::log_error!("Can't get passport element object: {error}");
            }
        }
    }

    td_api::PassportElements { elements: result }
}

// ---------------------------------------------------------------------------
// The dated-file object helpers (SecureValue.cpp:411-447)
// ---------------------------------------------------------------------------

/// `get_dated_file_object(FileManager *, DatedFile)` (SecureValue.cpp:411-413).
pub fn get_dated_file_object(env: &mut impl SecureValueEnv, file: DatedFile) -> td_api::DatedFile {
    td_api::DatedFile {
        file: Some(env.file_object(file.file_id)),
        date: file.date,
    }
}

/// `get_dated_files_object(FileManager *, const vector<DatedFile> &)`
/// (SecureValue.cpp:415-418).
pub fn get_dated_files_object(
    env: &mut impl SecureValueEnv,
    files: &[DatedFile],
) -> Vec<td_api::DatedFile> {
    files
        .iter()
        .map(|&file| get_dated_file_object(env, file))
        .collect()
}

/// The C++ nullptr dated file (the td_api field stays absent); the
/// generated struct keeps the shape.
fn empty_dated_file() -> td_api::DatedFile {
    td_api::DatedFile {
        file: None,
        date: 0,
    }
}

/// `get_dated_file_object(FileManager *, const EncryptedSecureFile &)`
/// (SecureValue.cpp:420-441) — resolves the decrypted twin of the encrypted
/// file; the C++ nullptr models as the empty dated file.
pub fn get_dated_file_object_for_encrypted_file(
    env: &mut impl SecureValueEnv,
    file: &EncryptedSecureFile,
) -> td_api::DatedFile {
    let dated_file = file.file;
    let file_id = dated_file.file_id;
    assert!(file_id.is_valid());
    let Some(file_view) = env.file_view(file_id) else {
        tdutils::log_error!("Have wrong file in get_dated_file_object");
        return empty_dated_file();
    };
    let Some(full_remote) = file_view.full_remote else {
        tdutils::log_error!("Have wrong file in get_dated_file_object");
        return empty_dated_file();
    };
    if file_view.full_remote_is_web {
        tdutils::log_error!("Have wrong file in get_dated_file_object");
        return empty_dated_file();
    }
    if file_view.file_type != FileType::SecureEncrypted {
        tdutils::log_error!("Have file of a wrong type in get_dated_file_object");
    } else if file_view.encryption_key.empty() {
        return get_dated_file_object(env, dated_file);
    }
    let decrypted_file_id = env.file_register_secure_decrypted(
        full_remote.id,
        full_remote.access_hash,
        full_remote.dc_id,
        file_view.expected_size,
        file_view.suggested_path,
    );
    get_dated_file_object(
        env,
        DatedFile {
            file_id: decrypted_file_id,
            date: dated_file.date,
        },
    )
}

/// `get_dated_files_object(FileManager *, const vector<EncryptedSecureFile>
/// &)` (SecureValue.cpp:443-447).
pub fn get_dated_files_object_for_encrypted_files(
    env: &mut impl SecureValueEnv,
    files: &[EncryptedSecureFile],
) -> Vec<td_api::DatedFile> {
    files
        .iter()
        .map(|file| get_dated_file_object_for_encrypted_file(env, file))
        .collect()
}

// ---------------------------------------------------------------------------
// Decrypt/encrypt (SecureValue.cpp:1150-1363)
// ---------------------------------------------------------------------------

/// `decrypt_secure_file` (SecureValue.cpp:1150-1163).
fn decrypt_secure_file(
    env: &mut impl SecureValueEnv,
    master_secret: &Secret,
    secure_file: &EncryptedSecureFile,
) -> Result<(DatedFile, SecureFileCredentials)> {
    if !secure_file.file.file_id.is_valid() {
        return Result::Ok((DatedFile::default(), SecureFileCredentials::default()));
    }
    try_result!(hash, ValueHash::create(&secure_file.file_hash));
    try_result!(
        encrypted_secret,
        EncryptedSecret::create(&secure_file.encrypted_secret)
    );
    let mut key_bytes = Vec::new();
    key_bytes.extend_from_slice(master_secret.as_slice());
    key_bytes.extend_from_slice(hash.as_slice());
    try_result!(
        secret,
        encrypted_secret.decrypt(&key_bytes, b"", EnryptionAlgorithm::Sha512)
    );

    let mut key = FileEncryptionKey::from_secret(secret.as_slice());
    key.set_value_hash(hash.as_slice());
    env.file_set_encryption_key(secure_file.file.file_id, key);
    Result::Ok((
        secure_file.file,
        SecureFileCredentials {
            secret: secret.as_slice().to_vec(),
            hash: hash.as_slice().to_vec(),
        },
    ))
}

/// `decrypt_secure_files` (SecureValue.cpp:1165-1178).
fn decrypt_secure_files(
    env: &mut impl SecureValueEnv,
    secret: &Secret,
    secure_files: &[EncryptedSecureFile],
) -> Result<(Vec<DatedFile>, Vec<SecureFileCredentials>)> {
    let mut result = Vec::with_capacity(secure_files.len());
    let mut credentials = Vec::with_capacity(secure_files.len());
    for file in secure_files {
        try_result!(decrypted_file, decrypt_secure_file(env, secret, file));
        result.push(decrypted_file.0);
        credentials.push(decrypted_file.1);
    }

    Result::Ok((result, credentials))
}

/// `decrypt_secure_data` (SecureValue.cpp:1180-1188).
fn decrypt_secure_data(
    master_secret: &Secret,
    secure_data: &EncryptedSecureData,
) -> Result<(String, SecureDataCredentials)> {
    try_result!(hash, ValueHash::create(&secure_data.hash));
    try_result!(
        encrypted_secret,
        EncryptedSecret::create(&secure_data.encrypted_secret)
    );
    let mut key_bytes = Vec::new();
    key_bytes.extend_from_slice(master_secret.as_slice());
    key_bytes.extend_from_slice(hash.as_slice());
    try_result!(
        secret,
        encrypted_secret.decrypt(&key_bytes, b"", EnryptionAlgorithm::Sha512)
    );
    try_result!(value, decrypt_value(&secret, &hash, &secure_data.data));
    Result::Ok((
        String::from_utf8_lossy(&value).into_owned(),
        SecureDataCredentials {
            secret: secret.as_slice().to_vec(),
            hash: hash.as_slice().to_vec(),
        },
    ))
}

/// `decrypt_secure_value` (SecureValue.cpp:1190-1246).
pub fn decrypt_secure_value(
    env: &mut impl SecureValueEnv,
    secret: &Secret,
    encrypted_secure_value: &EncryptedSecureValue,
) -> Result<SecureValueWithCredentials> {
    let mut res = SecureValue {
        type_: encrypted_secure_value.type_,
        ..SecureValue::default()
    };
    let mut res_credentials = SecureValueCredentials {
        type_: res.type_,
        hash: encrypted_secure_value.hash.clone(),
        ..SecureValueCredentials::default()
    };
    match encrypted_secure_value.type_ {
        SecureValueType::None => Result::Err(Status::error(
            400,
            "Receive invalid Telegram Passport element",
        )),
        SecureValueType::EmailAddress | SecureValueType::PhoneNumber => {
            res.data = String::from_utf8_lossy(&encrypted_secure_value.data.data).into_owned();
            Result::Ok(SecureValueWithCredentials {
                value: res,
                credentials: res_credentials,
            })
        }
        SecureValueType::UtilityBill
        | SecureValueType::BankStatement
        | SecureValueType::RentalAgreement
        | SecureValueType::PassportRegistration
        | SecureValueType::TemporaryRegistration => {
            try_result!(
                files,
                decrypt_secure_files(env, secret, &encrypted_secure_value.files)
            );
            res.files = files.0;
            res_credentials.files = files.1;
            try_result!(
                translations,
                decrypt_secure_files(env, secret, &encrypted_secure_value.translations)
            );
            res.translations = translations.0;
            res_credentials.translations = translations.1;
            Result::Ok(SecureValueWithCredentials {
                value: res,
                credentials: res_credentials,
            })
        }
        _ => {
            try_result!(
                data,
                decrypt_secure_data(secret, &encrypted_secure_value.data)
            );
            res.data = data.0;
            if !res.data.is_empty() {
                res_credentials.data = Some(data.1);
            }
            assert!(encrypted_secure_value.files.is_empty());
            try_result!(
                front_side,
                decrypt_secure_file(env, secret, &encrypted_secure_value.front_side)
            );
            res.front_side = front_side.0;
            if res.front_side.file_id.is_valid() {
                res_credentials.front_side = Some(front_side.1);
            }
            try_result!(
                reverse_side,
                decrypt_secure_file(env, secret, &encrypted_secure_value.reverse_side)
            );
            res.reverse_side = reverse_side.0;
            if res.reverse_side.file_id.is_valid() {
                res_credentials.reverse_side = Some(reverse_side.1);
            }
            try_result!(
                selfie,
                decrypt_secure_file(env, secret, &encrypted_secure_value.selfie)
            );
            res.selfie = selfie.0;
            if res.selfie.file_id.is_valid() {
                res_credentials.selfie = Some(selfie.1);
            }
            try_result!(
                translations,
                decrypt_secure_files(env, secret, &encrypted_secure_value.translations)
            );
            res.translations = translations.0;
            res_credentials.translations = translations.1;
            Result::Ok(SecureValueWithCredentials {
                value: res,
                credentials: res_credentials,
            })
        }
    }
}

/// `decrypt_secure_values` (SecureValue.cpp:1248-1262) — the failing values
/// are skipped with the C++ ERROR log.
pub fn decrypt_secure_values(
    env: &mut impl SecureValueEnv,
    secret: &Secret,
    encrypted_secure_values: &[EncryptedSecureValue],
) -> Vec<SecureValueWithCredentials> {
    let mut result = Vec::with_capacity(encrypted_secure_values.len());
    for encrypted_secure_value in encrypted_secure_values {
        match decrypt_secure_value(env, secret, encrypted_secure_value) {
            Result::Ok(secure_value_with_credentials) => {
                result.push(secure_value_with_credentials);
            }
            Result::Err(error) => {
                tdutils::log_error!("Cannot decrypt secure value: {error}");
            }
        }
    }
    result
}

/// `encrypt_secure_file` (SecureValue.cpp:1264-1292) — appends the hashed
/// material to `to_hash`.
fn encrypt_secure_file(
    env: &mut impl SecureValueEnv,
    master_secret: &Secret,
    file: DatedFile,
    to_hash: &mut String,
) -> EncryptedSecureFile {
    let Some(file_view) = env.file_view(file.file_id) else {
        return EncryptedSecureFile::default();
    };
    if !file_view.encryption_key.is_secure() {
        tdutils::log_error!("File {} has no encryption key", file.file_id.get());
        return EncryptedSecureFile::default();
    }
    if !file_view.encryption_key.has_value_hash() {
        tdutils::log_error!("File {} has no hash", file.file_id.get());
        return EncryptedSecureFile::default();
    }
    let value_hash = file_view.encryption_key.value_hash().to_vec();
    let secret = file_view.encryption_key.secret().to_vec();
    let mut key_bytes = Vec::new();
    key_bytes.extend_from_slice(master_secret.as_slice());
    key_bytes.extend_from_slice(&value_hash);
    let encrypted_secret =
        Secret::create(&secret)
            .unwrap()
            .encrypt(&key_bytes, b"", EnryptionAlgorithm::Sha512);

    to_hash.push_str(&String::from_utf8_lossy(&value_hash));
    to_hash.push_str(&String::from_utf8_lossy(secret.as_slice()));

    EncryptedSecureFile {
        file,
        file_hash: value_hash,
        encrypted_secret: encrypted_secret.as_slice().to_vec(),
    }
}

/// `encrypt_secure_files` (SecureValue.cpp:1294-1310).
fn encrypt_secure_files(
    env: &mut impl SecureValueEnv,
    master_secret: &Secret,
    files: &[DatedFile],
    to_hash: &mut String,
) -> Vec<EncryptedSecureFile> {
    files
        .iter()
        .map(|&file| encrypt_secure_file(env, master_secret, file, to_hash))
        .collect()
}

/// `encrypt_secure_data` (SecureValue.cpp:1312-1327).
fn encrypt_secure_data(
    master_secret: &Secret,
    data: &[u8],
    to_hash: &mut String,
) -> EncryptedSecureData {
    let secret = Secret::create_new();
    let encrypted = encrypt_value(&secret, data)
        .unwrap_or_else(|_| unreachable!("encrypt_value only fails on bad sizes"));
    let mut key_bytes = Vec::new();
    key_bytes.extend_from_slice(master_secret.as_slice());
    key_bytes.extend_from_slice(encrypted.hash.as_slice());
    let encrypted_secret = secret.encrypt(&key_bytes, b"", EnryptionAlgorithm::Sha512);

    let res = EncryptedSecureData {
        encrypted_secret: encrypted_secret.as_slice().to_vec(),
        data: encrypted.data,
        hash: encrypted.hash.as_slice().to_vec(),
    };
    to_hash.push_str(&String::from_utf8_lossy(&res.hash));
    to_hash.push_str(&String::from_utf8_lossy(secret.as_slice()));
    res
}

/// `encrypt_secure_value` (SecureValue.cpp:1329-1363).
pub fn encrypt_secure_value(
    env: &mut impl SecureValueEnv,
    master_secret: &Secret,
    secure_value: &SecureValue,
) -> EncryptedSecureValue {
    let mut res = EncryptedSecureValue {
        type_: secure_value.type_,
        ..EncryptedSecureValue::default()
    };
    match res.type_ {
        SecureValueType::EmailAddress | SecureValueType::PhoneNumber => {
            res.data = EncryptedSecureData {
                data: secure_value.data.as_bytes().to_vec(),
                hash: Vec::new(),
                encrypted_secret: Vec::new(),
            };
            res.hash = calc_value_hash(secure_value.data.as_bytes())
                .as_slice()
                .to_vec();
        }
        SecureValueType::UtilityBill
        | SecureValueType::BankStatement
        | SecureValueType::RentalAgreement
        | SecureValueType::PassportRegistration
        | SecureValueType::TemporaryRegistration => {
            let mut to_hash = String::new();
            res.files = encrypt_secure_files(env, master_secret, &secure_value.files, &mut to_hash);
            res.translations =
                encrypt_secure_files(env, master_secret, &secure_value.translations, &mut to_hash);
            res.hash = calc_value_hash(to_hash.as_bytes()).as_slice().to_vec();
        }
        _ => {
            let mut to_hash = String::new();
            res.data =
                encrypt_secure_data(master_secret, secure_value.data.as_bytes(), &mut to_hash);
            assert!(secure_value.files.is_empty());
            res.front_side =
                encrypt_secure_file(env, master_secret, secure_value.front_side, &mut to_hash);
            res.reverse_side =
                encrypt_secure_file(env, master_secret, secure_value.reverse_side, &mut to_hash);
            res.selfie = encrypt_secure_file(env, master_secret, secure_value.selfie, &mut to_hash);
            res.translations =
                encrypt_secure_files(env, master_secret, &secure_value.translations, &mut to_hash);
            res.hash = calc_value_hash(to_hash.as_bytes()).as_slice().to_vec();
        }
    }
    res
}

// ---------------------------------------------------------------------------
// The credentials JSON + RSA envelope (SecureValue.cpp:1365-1468)
// ---------------------------------------------------------------------------

/// `as_jsonable_data` (SecureValue.cpp:1365-1370).
fn as_jsonable_data(credentials: &SecureDataCredentials) -> impl tdutils::json::Jsonable + '_ {
    json_object_fn(|o| {
        o.field("data_hash", base64_encode(&credentials.hash).as_str());
        o.field("secret", base64_encode(&credentials.secret).as_str());
    })
}

/// `as_jsonable_file` (SecureValue.cpp:1372-1377).
fn as_jsonable_file(credentials: &SecureFileCredentials) -> impl tdutils::json::Jsonable + '_ {
    json_object_fn(|o| {
        o.field("file_hash", base64_encode(&credentials.hash).as_str());
        o.field("secret", base64_encode(&credentials.secret).as_str());
    })
}

/// `as_jsonable_files` (SecureValue.cpp:1379-1381).
fn as_jsonable_files(files: &[SecureFileCredentials]) -> impl tdutils::json::Jsonable + '_ {
    json_array_items(files.iter(), |c| as_jsonable_file(c))
}

/// `secure_value_type_as_slice` (SecureValue.cpp:1383-1416).
fn secure_value_type_as_slice(type_: SecureValueType) -> &'static str {
    match type_ {
        SecureValueType::PersonalDetails => "personal_details",
        SecureValueType::Passport => "passport",
        SecureValueType::DriverLicense => "driver_license",
        SecureValueType::IdentityCard => "identity_card",
        SecureValueType::InternalPassport => "internal_passport",
        SecureValueType::Address => "address",
        SecureValueType::UtilityBill => "utility_bill",
        SecureValueType::BankStatement => "bank_statement",
        SecureValueType::RentalAgreement => "rental_agreement",
        SecureValueType::PassportRegistration => "passport_registration",
        SecureValueType::TemporaryRegistration => "temporary_registration",
        SecureValueType::PhoneNumber => "phone_number",
        SecureValueType::EmailAddress => "email",
        SecureValueType::None => {
            unreachable!("secure_value_type_as_slice on None (the C++ UNREACHABLE)")
        }
    }
}

/// `credentials_as_jsonable` (SecureValue.cpp:1418-1451).
fn credentials_as_jsonable<'a>(
    credentials: &'a [SecureValueCredentials],
    nonce: &'a [u8],
    rename_payload_to_nonce: bool,
) -> impl tdutils::json::Jsonable + 'a {
    json_object_fn(move |o| {
        o.field(
            "secure_data",
            &json_object_fn(|o| {
                for cred in credentials {
                    if cred.type_ == SecureValueType::PhoneNumber
                        || cred.type_ == SecureValueType::EmailAddress
                    {
                        continue;
                    }

                    o.field(
                        secure_value_type_as_slice(cred.type_),
                        &json_object_fn(|o| {
                            if let Some(data) = &cred.data {
                                o.field("data", &as_jsonable_data(data));
                            }
                            if !cred.files.is_empty() {
                                o.field("files", &as_jsonable_files(&cred.files));
                            }
                            if let Some(front_side) = &cred.front_side {
                                o.field("front_side", &as_jsonable_file(front_side));
                            }
                            if let Some(reverse_side) = &cred.reverse_side {
                                o.field("reverse_side", &as_jsonable_file(reverse_side));
                            }
                            if let Some(selfie) = &cred.selfie {
                                o.field("selfie", &as_jsonable_file(selfie));
                            }
                            if !cred.translations.is_empty() {
                                o.field("translation", &as_jsonable_files(&cred.translations));
                            }
                        }),
                    );
                }
            }),
        );
        // the nonce bytes ride the raw escaping (the C++ `o(name,
        // Slice)` overload); the manager-sourced nonces are UTF-8
        let nonce = String::from_utf8_lossy(nonce);
        o.field(
            if rename_payload_to_nonce {
                "nonce"
            } else {
                "payload"
            },
            &*nonce,
        );
    })
}

/// `get_encrypted_credentials` (SecureValue.cpp:1453-1468).
pub fn get_encrypted_credentials(
    credentials: &[SecureValueCredentials],
    nonce: &[u8],
    public_key: &[u8],
    rename_payload_to_nonce: bool,
) -> Result<EncryptedSecureCredentials> {
    let encoded_credentials = json_encode(
        &credentials_as_jsonable(credentials, nonce, rename_payload_to_nonce),
        false,
    );
    tdutils::log_info!("Created credentials {encoded_credentials}");

    let secret = Secret::create_new();
    let encrypted_value = encrypt_value(&secret, encoded_credentials.as_bytes())
        .unwrap_or_else(|_| unreachable!("encrypt_value only fails on bad sizes"));
    try_result!(
        encrypted_secret,
        tdutils::rsa::rsa_encrypt_pkcs1_oaep(
            &String::from_utf8_lossy(public_key),
            secret.as_slice(),
        )
    );
    Result::Ok(EncryptedSecureCredentials {
        data: encrypted_value.data,
        hash: encrypted_value.hash.as_slice().to_vec(),
        encrypted_secret,
    })
}

/// `td::base64_encode` — rides the tdutils base64 engine.
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ---------------------------------------------------------------------------
// The Address helpers (OrderInfo.cpp:32-147, the pulled-in helper header)
// ---------------------------------------------------------------------------

/// `td::Address` (OrderInfo.h:19-37).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Address {
    pub country_code: String,
    pub state: String,
    pub city: String,
    pub street_line1: String,
    pub street_line2: String,
    pub postal_code: String,
}

/// `is_capital_alpha` (OrderInfo.cpp:43-45).
fn is_capital_alpha(c: u8) -> bool {
    c.is_ascii_uppercase()
}

/// `check_country_code` (OrderInfo.cpp:45-54).
pub fn check_country_code(country_code: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(country_code) else {
        return Status::error(400, "Country code must be encoded in UTF-8");
    };
    *country_code = cleaned;
    if country_code.len() != 2
        || !is_capital_alpha(country_code.as_bytes()[0])
        || !is_capital_alpha(country_code.as_bytes()[1])
    {
        return Status::error(400, "Wrong country code specified");
    }
    Status::ok()
}

/// `check_state` (OrderInfo.cpp:56-61).
fn check_state(state: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(state) else {
        return Status::error(400, "State must be encoded in UTF-8");
    };
    *state = cleaned;
    Status::ok()
}

/// `check_city` (OrderInfo.cpp:63-68).
fn check_city(city: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(city) else {
        return Status::error(400, "City must be encoded in UTF-8");
    };
    *city = cleaned;
    Status::ok()
}

/// `check_street_line` (OrderInfo.cpp:70-75).
fn check_street_line(street_line: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(street_line) else {
        return Status::error(400, "Street line must be encoded in UTF-8");
    };
    *street_line = cleaned;
    Status::ok()
}

/// `check_postal_code` (OrderInfo.cpp:77-82).
fn check_postal_code(postal_code: &mut String) -> Status {
    let Some(cleaned) = clean_input_string(postal_code) else {
        return Status::error(400, "Postal code must be encoded in UTF-8");
    };
    *postal_code = cleaned;
    Status::ok()
}

/// `get_address(td_api::object_ptr<td_api::address>)` (OrderInfo.cpp:83-97).
pub fn get_address(address: Option<td_api::Address>) -> Result<Address> {
    let Some(mut address) = address else {
        return Result::Err(Status::error(400, "Address must be non-empty"));
    };
    try_status!(check_country_code(&mut address.country_code));
    try_status!(check_state(&mut address.state));
    try_status!(check_city(&mut address.city));
    try_status!(check_street_line(&mut address.street_line1));
    try_status!(check_street_line(&mut address.street_line2));
    try_status!(check_postal_code(&mut address.postal_code));

    Result::Ok(Address {
        country_code: address.country_code,
        state: address.state,
        city: address.city,
        street_line1: address.street_line1,
        street_line2: address.street_line2,
        postal_code: address.postal_code,
    })
}

/// `get_address_object` (OrderInfo.cpp:106-109).
pub fn get_address_object(address: &Address) -> td_api::Address {
    td_api::Address {
        country_code: address.country_code.clone(),
        state: address.state.clone(),
        city: address.city.clone(),
        street_line1: address.street_line1.clone(),
        street_line2: address.street_line2.clone(),
        postal_code: address.postal_code.clone(),
    }
}

/// `address_to_json` (OrderInfo.cpp:111-120).
pub fn address_to_json(address: &Address) -> String {
    json_encode(
        &json_object_fn(|o| {
            o.field("country_code", address.country_code.as_str());
            o.field("state", address.state.as_str());
            o.field("city", address.city.as_str());
            o.field("street_line1", address.street_line1.as_str());
            o.field("street_line2", address.street_line2.as_str());
            o.field("post_code", address.postal_code.as_str());
        }),
        false,
    )
}

/// `address_from_json` (OrderInfo.cpp:122-147).
pub fn address_from_json(json: &str) -> Result<Address> {
    let value = match json_decode(json) {
        Result::Ok(value) => value,
        Result::Err(_) => {
            return Result::Err(Status::error(400, "Can't parse address JSON object"));
        }
    };
    if value.ty() != JsonValueType::Object {
        return Result::Err(Status::error(400, "Address must be an Object"));
    }

    let object = value.get_object();
    let mut country_code = match object.get_optional_string_field("country_code", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    let mut state = match object.get_optional_string_field("state", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    let mut city = match object.get_optional_string_field("city", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    let mut street_line1 = match object.get_optional_string_field("street_line1", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    let mut street_line2 = match object.get_optional_string_field("street_line2", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };
    let mut postal_code = match object.get_optional_string_field("post_code", "") {
        Result::Ok(value) => value,
        Result::Err(status) => return Result::Err(status),
    };

    try_status!(check_country_code(&mut country_code));
    try_status!(check_state(&mut state));
    try_status!(check_city(&mut city));
    try_status!(check_street_line(&mut street_line1));
    try_status!(check_street_line(&mut street_line2));
    try_status!(check_postal_code(&mut postal_code));

    Result::Ok(Address {
        country_code,
        state,
        city,
        street_line1,
        street_line2,
        postal_code,
    })
}

// ---------------------------------------------------------------------------
// The input secure value builder (SecureValue.cpp:596-641, the SetSecureValue
// send leg)
// ---------------------------------------------------------------------------

/// `get_input_secure_value_object` (SecureValue.cpp:596-641) — the
/// `account.saveSecureValue` payload builder; the CHECKs ride the upload
/// state (blocked with the secure upload contract, see the module notes).
#[allow(clippy::too_many_arguments)]
pub fn get_input_secure_value_object(
    env: &mut impl SecureValueEnv,
    value: &EncryptedSecureValue,
    files: &mut [SecureInputFile],
    front_side: &mut Option<SecureInputFile>,
    reverse_side: &mut Option<SecureInputFile>,
    selfie: &mut Option<SecureInputFile>,
    translations: &mut [SecureInputFile],
) -> telegram_api::InputSecureValue {
    let is_plain =
        value.type_ == SecureValueType::PhoneNumber || value.type_ == SecureValueType::EmailAddress;
    let has_front_side = value.front_side.file.file_id.is_valid();
    let has_reverse_side = value.reverse_side.file.file_id.is_valid();
    let has_selfie = value.selfie.file.file_id.is_valid();
    let mut flags = 0;
    let mut plain_data = None;
    if is_plain {
        if value.type_ == SecureValueType::PhoneNumber {
            plain_data = Some(telegram_api::SecurePlainData::securePlainPhone(
                telegram_api::securePlainPhone {
                    phone: String::from_utf8_lossy(&value.data.data).into_owned(),
                },
            ));
        } else {
            plain_data = Some(telegram_api::SecurePlainData::securePlainEmail(
                telegram_api::securePlainEmail {
                    email: String::from_utf8_lossy(&value.data.data).into_owned(),
                },
            ));
        }
        flags |= 1 << 5; // PLAIN_DATA_MASK
    } else {
        flags |= 1; // DATA_MASK
    }
    if !value.files.is_empty() {
        flags |= 1 << 4; // FILES_MASK
    }
    if has_front_side {
        flags |= 1 << 1; // FRONT_SIDE_MASK
        assert!(front_side.is_some());
    }
    if has_reverse_side {
        flags |= 1 << 2; // REVERSE_SIDE_MASK
        assert!(reverse_side.is_some());
    }
    if has_selfie {
        flags |= 1 << 3; // SELFIE_MASK
        assert!(selfie.is_some());
    }
    if !value.translations.is_empty() {
        flags |= 1 << 6; // TRANSLATION_MASK
    }
    telegram_api::InputSecureValue {
        flags,
        r#type: Some(get_input_secure_value_type(value.type_)),
        data: if is_plain {
            None
        } else {
            Some(get_secure_data_object(&value.data))
        },
        front_side: if has_front_side {
            front_side
                .as_mut()
                .and_then(|input| get_input_secure_file_object(env, &value.front_side, input))
        } else {
            None
        },
        reverse_side: if has_reverse_side {
            reverse_side
                .as_mut()
                .and_then(|input| get_input_secure_file_object(env, &value.reverse_side, input))
        } else {
            None
        },
        selfie: if has_selfie {
            selfie
                .as_mut()
                .and_then(|input| get_input_secure_file_object(env, &value.selfie, input))
        } else {
            None
        },
        translation: get_input_secure_files_object(env, &value.translations, translations),
        files: get_input_secure_files_object(env, &value.files, files),
        plain_data,
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `Result::Err` for the tests (the tdutils `Result` has no
    /// `unwrap_err`).
    fn err_message<T>(result: Result<T>) -> String {
        match result {
            Result::Err(status) => status.message().to_owned(),
            Result::Ok(_) => panic!("expected an error"),
        }
    }

    /// The null env: the pure-value conversions never touch it.
    struct NullEnv;

    impl SecureValueEnv for NullEnv {
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
            _dc_id: DcId,
            _size: i64,
            _remote_name: String,
        ) -> FileId {
            FileId::new(1, 1)
        }

        fn file_register_secure_decrypted(
            &mut self,
            _id: i64,
            _access_hash: i64,
            _dc_id: DcId,
            _expected_size: i64,
            _suggested_path: String,
        ) -> FileId {
            FileId::new(2, 2)
        }

        fn file_view(&mut self, _file_id: FileId) -> Option<SecureFileView> {
            None
        }

        fn file_set_encryption_key(&mut self, _file_id: FileId, _key: FileEncryptionKey) {}

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
            Result::Ok(FileId::new(3, 3))
        }

        fn file_merge(&mut self, _to_file_id: FileId, _from_file_id: FileId) -> Result<()> {
            Result::Ok(())
        }

        fn file_copy_file_id(&mut self, _file_id: FileId) -> FileId {
            FileId::new(4, 4)
        }

        fn file_cancel_upload(&mut self, _file_upload_id: FileUploadId) {}

        fn file_get_internal_upload_id(&mut self) -> i64 {
            7
        }
    }

    #[test]
    fn secure_value_type_conversions_cover_the_whole_set() {
        use SecureValueType as T;
        let all = [
            T::PersonalDetails,
            T::Passport,
            T::DriverLicense,
            T::IdentityCard,
            T::InternalPassport,
            T::Address,
            T::UtilityBill,
            T::BankStatement,
            T::RentalAgreement,
            T::PassportRegistration,
            T::TemporaryRegistration,
            T::PhoneNumber,
            T::EmailAddress,
        ];
        for type_ in all {
            // td_api → model → td_api round-trip
            let td_type = get_passport_element_type_object(type_);
            assert_eq!(get_secure_value_type_td_api(&td_type), type_);
            // model → telegram → model round-trip
            let input_type = get_input_secure_value_type(type_);
            assert_eq!(get_secure_value_type(&input_type), type_);
        }
        // the Display names match the C++ StringBuilder output
        assert_eq!(
            format!("{}", SecureValueType::PersonalDetails),
            "PersonalDetails"
        );
        assert_eq!(format!("{}", SecureValueType::EmailAddress), "EmailAddress");
        assert_eq!(format!("{}", SecureValueType::None), "None");
    }

    #[test]
    fn unique_secure_value_types_dedups_stably() {
        let types = vec![
            SecureValueType::Passport,
            SecureValueType::EmailAddress,
            SecureValueType::Passport,
            SecureValueType::Address,
            SecureValueType::EmailAddress,
        ];
        assert_eq!(
            unique_secure_value_types(types),
            vec![
                SecureValueType::Passport,
                SecureValueType::EmailAddress,
                SecureValueType::Address,
            ]
        );
        assert!(unique_secure_value_types(Vec::new()).is_empty());
    }

    #[test]
    fn data_field_names_map_like_the_cpp() {
        // identity documents
        assert_eq!(
            get_secure_value_data_field_name(SecureValueType::DriverLicense, "document_no"),
            "number"
        );
        assert_eq!(
            get_secure_value_data_field_name(SecureValueType::Passport, "expiry_date"),
            "expiry_date"
        );
        // personal details renames
        assert_eq!(
            get_secure_value_data_field_name(SecureValueType::PersonalDetails, "first_name_native"),
            "native_first_name"
        );
        assert_eq!(
            get_secure_value_data_field_name(SecureValueType::PersonalDetails, "birth_date"),
            "birthdate"
        );
        assert_eq!(
            get_secure_value_data_field_name(
                SecureValueType::PersonalDetails,
                "residence_country_code"
            ),
            "residence_country_code"
        );
        // addresses
        assert_eq!(
            get_secure_value_data_field_name(SecureValueType::Address, "post_code"),
            "postal_code"
        );
        // the file-less types and unknown fields name nothing
        assert!(
            get_secure_value_data_field_name(SecureValueType::UtilityBill, "anything").is_empty()
        );
    }

    #[test]
    fn input_passport_element_parses_the_td_api_shapes() {
        let mut env = NullEnv;

        // phone: the raw string data
        let value = get_secure_value(
            &mut env,
            Some(
                td_api::InputPassportElement::inputPassportElementPhoneNumber(
                    td_api::inputPassportElementPhoneNumber {
                        phone_number: "+15551234567".to_owned(),
                    },
                ),
            ),
        )
        .unwrap();
        assert_eq!(value.type_, SecureValueType::PhoneNumber);
        assert_eq!(value.data, "+15551234567");

        // address: the JSON payload
        let value = get_secure_value(
            &mut env,
            Some(td_api::InputPassportElement::inputPassportElementAddress(
                td_api::inputPassportElementAddress {
                    address: Some(td_api::Address {
                        country_code: "GB".to_owned(),
                        state: "".to_owned(),
                        city: "London".to_owned(),
                        street_line1: "1 Road".to_owned(),
                        street_line2: "".to_owned(),
                        postal_code: "SW1A 1AA".to_owned(),
                    }),
                },
            )),
        )
        .unwrap();
        assert_eq!(value.type_, SecureValueType::Address);
        assert!(value.data.contains("\"country_code\":\"GB\""));
        assert!(value.data.contains("\"post_code\":\"SW1A 1AA\""));

        // identity documents need the front side
        let message = err_message(get_secure_value(
            &mut env,
            Some(td_api::InputPassportElement::inputPassportElementPassport(
                td_api::inputPassportElementPassport {
                    passport: Some(td_api::InputIdentityDocument {
                        number: "123456789".to_owned(),
                        expiration_date: Some(td_api::Date {
                            day: 1,
                            month: 1,
                            year: 2030,
                        }),
                        front_side: None,
                        reverse_side: None,
                        selfie: None,
                        translation: Vec::new(),
                    }),
                },
            )),
        ));
        assert_eq!(message, "Document's front side is required");

        // ... and the driver license requires the reverse side
        let message = err_message(get_secure_value(
            &mut env,
            Some(
                td_api::InputPassportElement::inputPassportElementDriverLicense(
                    td_api::inputPassportElementDriverLicense {
                        driver_license: Some(td_api::InputIdentityDocument {
                            number: "123456789".to_owned(),
                            expiration_date: None,
                            front_side: Some(td_api::InputFile::inputFileLocal(
                                td_api::inputFileLocal {
                                    path: "front.jpg".to_owned(),
                                },
                            )),
                            reverse_side: None,
                            selfie: None,
                            translation: Vec::new(),
                        }),
                    },
                ),
            ),
        ));
        assert_eq!(message, "Document's reverse side is required");
    }

    #[test]
    fn passport_element_objects_round_trip_the_data() {
        let mut env = NullEnv;

        // phone
        let value = SecureValue {
            type_: SecureValueType::PhoneNumber,
            data: "+15551234567".to_owned(),
            ..SecureValue::default()
        };
        let object = get_passport_element_object(&mut env, &value).unwrap();
        match object {
            td_api::PassportElement::passportElementPhoneNumber(v) => {
                assert_eq!(v.phone_number, "+15551234567");
            }
            other => panic!("wrong element {other:?}"),
        }

        // personal details: the JSON decodes into the td_api struct
        let value = SecureValue {
            type_: SecureValueType::PersonalDetails,
            data: "{\"first_name\":\"Rust\",\"middle_name\":\"\",\"last_name\":\"Lang\",\"first_name_native\":\"\",\"middle_name_native\":\"\",\"last_name_native\":\"\",\"birth_date\":\"01.05.2015\",\"gender\":\"male\",\"country_code\":\"GB\",\"residence_country_code\":\"GB\"}".to_owned(),
            ..SecureValue::default()
        };
        let object = get_passport_element_object(&mut env, &value).unwrap();
        match object {
            td_api::PassportElement::passportElementPersonalDetails(v) => {
                let details = v.personal_details.as_ref().unwrap();
                assert_eq!(details.first_name, "Rust");
                assert_eq!(details.gender, "male");
                let date = details.birthdate.as_ref().unwrap();
                assert_eq!((date.day, date.month, date.year), (1, 5, 2015));
            }
            other => panic!("wrong element {other:?}"),
        }

        // a broken JSON or a bad date fails the conversion
        let value = SecureValue {
            type_: SecureValueType::PersonalDetails,
            data: "not-json".to_owned(),
            ..SecureValue::default()
        };
        assert!(get_passport_element_object(&mut env, &value).is_error());

        // the elements list skips the failures
        let values = vec![
            SecureValue {
                type_: SecureValueType::PhoneNumber,
                data: "+15551234567".to_owned(),
                ..SecureValue::default()
            },
            SecureValue {
                type_: SecureValueType::PersonalDetails,
                data: "not-json".to_owned(),
                ..SecureValue::default()
            },
        ];
        let elements = get_passport_elements_object(&mut env, &values);
        assert_eq!(elements.elements.len(), 1);
    }

    #[test]
    fn encrypted_secure_value_round_trips_through_the_telegram_shape() {
        use crate::secure_storage::Secret;

        let mut env = NullEnv;
        let master_secret = Secret::create_new();
        let plain = SecureValue {
            type_: SecureValueType::EmailAddress,
            data: "user@example.com".to_owned(),
            ..SecureValue::default()
        };
        let encrypted = encrypt_secure_value(&mut env, &master_secret, &plain);

        // the email element stores the plain data + the value hash
        assert_eq!(encrypted.data.data, plain.data.as_bytes());
        assert!(encrypted.data.hash.is_empty());
        assert_eq!(
            encrypted.hash.as_slice(),
            crate::secure_storage::calc_value_hash(plain.data.as_bytes()).as_slice()
        );

        // wrap into the telegram `secureValue` and back
        let telegram_value = telegram_api::SecureValue {
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
                    email: "user@example.com".to_owned(),
                },
            )),
            hash: encrypted.hash.clone(),
        };
        let parsed = get_encrypted_secure_value(&mut env, telegram_value);
        assert_eq!(parsed, encrypted);

        // decrypt recovers the plain value
        let decrypted = decrypt_secure_value(&mut env, &master_secret, &parsed).unwrap();
        assert_eq!(decrypted.value, plain);
        assert_eq!(decrypted.credentials.type_, SecureValueType::EmailAddress);
        assert_eq!(decrypted.credentials.hash, encrypted.hash);

        // the encrypted-data types ride the full envelope
        let plain = personal_details_value();
        let encrypted = encrypt_secure_value(&mut env, &master_secret, &plain);
        let telegram_value = telegram_api::SecureValue {
            flags: 1,
            r#type: Some(
                telegram_api::SecureValueType::secureValueTypePersonalDetails(
                    telegram_api::secureValueTypePersonalDetails,
                ),
            ),
            data: Some(telegram_api::SecureData {
                data: encrypted.data.data.clone(),
                data_hash: encrypted.data.hash.clone(),
                secret: encrypted.data.encrypted_secret.clone(),
            }),
            front_side: None,
            reverse_side: None,
            selfie: None,
            translation: Vec::new(),
            files: Vec::new(),
            plain_data: None,
            hash: encrypted.hash.clone(),
        };
        let parsed = get_encrypted_secure_value(&mut env, telegram_value);
        assert_eq!(parsed, encrypted);
        let decrypted = decrypt_secure_value(&mut env, &master_secret, &parsed).unwrap();
        assert_eq!(decrypted.value.data, plain.data);
        assert!(decrypted.credentials.data.is_some());
    }

    fn personal_details_value() -> SecureValue {
        SecureValue {
            type_: SecureValueType::PersonalDetails,
            data: "{\"first_name\":\"Rust\",\"middle_name\":\"\",\"last_name\":\"Lang\",\"first_name_native\":\"\",\"middle_name_native\":\"\",\"last_name_native\":\"\",\"birth_date\":\"01.05.2015\",\"gender\":\"male\",\"country_code\":\"GB\",\"residence_country_code\":\"GB\"}".to_owned(),
            ..SecureValue::default()
        }
    }

    #[test]
    fn encrypted_secure_value_check_rejects_wrong_shapes() {
        let mut env = NullEnv;

        // an email with an encrypted data blob fails the shape check and
        // collapses to the None-typed record
        let telegram_value = telegram_api::SecureValue {
            flags: 1,
            r#type: Some(telegram_api::SecureValueType::secureValueTypeEmail(
                telegram_api::secureValueTypeEmail,
            )),
            data: Some(telegram_api::SecureData {
                data: vec![1, 2, 3],
                data_hash: vec![4; 32],
                secret: vec![5; 32],
            }),
            front_side: None,
            reverse_side: None,
            selfie: None,
            translation: Vec::new(),
            files: Vec::new(),
            plain_data: None,
            hash: b"hash".to_vec(),
        };
        let parsed = get_encrypted_secure_value(&mut env, telegram_value);
        assert_eq!(parsed.type_, SecureValueType::None);

        // and get_encrypted_secure_values skips it
        assert!(get_encrypted_secure_values(&mut env, Vec::new()).is_empty());
    }

    #[test]
    fn get_encrypted_credentials_builds_the_rsa_envelope() {
        let credentials = vec![SecureValueCredentials {
            type_: SecureValueType::EmailAddress,
            hash: vec![9; 32],
            data: None,
            files: Vec::new(),
            front_side: None,
            reverse_side: None,
            selfie: None,
            translations: Vec::new(),
        }];
        let encrypted = get_encrypted_credentials(
            &credentials,
            b"the-nonce",
            TEST_PUBLIC_KEY.as_bytes(),
            false,
        )
        .unwrap();
        // the RSA envelope is one modulus-sized block
        assert_eq!(encrypted.encrypted_secret.len(), 256);
        // the value hash + the JSON payload ride `data`
        assert!(!encrypted.data.is_empty());
        assert_eq!(encrypted.hash.len(), 32);
    }

    #[test]
    fn credentials_json_shapes_the_hash_fields() {
        // the as_jsonable_* helpers produce the base64 form the bot-side
        // payload expects
        let credentials = SecureDataCredentials {
            secret: b"secret".to_vec(),
            hash: b"hash!".to_vec(),
        };
        use tdutils::json::{json_decode, json_encode};
        let encoded = json_encode(&as_jsonable_data(&credentials), false);
        let decoded = json_decode(&encoded).unwrap();
        let object = decoded.get_object();
        let data_hash = object.get_optional_string_field("data_hash", "").unwrap();
        let secret = object.get_optional_string_field("secret", "").unwrap();
        assert_eq!(data_hash, "aGFzaCE=", "base64 of b\"hash!\"");
        assert_eq!(secret, "c2VjcmV0", "base64 of b\"secret\"");
    }

    #[test]
    fn address_helpers_round_trip() {
        let address = Address {
            country_code: "GB".to_owned(),
            state: String::new(),
            city: "London".to_owned(),
            street_line1: "1 Road".to_owned(),
            street_line2: String::new(),
            postal_code: "SW1A 1AA".to_owned(),
        };

        // the JSON uses the post_code key
        let json = address_to_json(&address);
        assert!(json.contains("\"post_code\":\"SW1A 1AA\""));

        // ... and the parser accepts it back
        let parsed = address_from_json(&json).unwrap();
        assert_eq!(parsed, address);

        // the country code check is strict
        let mut wrong_country = "gbr".to_owned();
        assert_eq!(
            check_country_code(&mut wrong_country).message(),
            "Wrong country code specified"
        );

        let mut lower = "gb".to_owned();
        assert_eq!(
            check_country_code(&mut lower).message(),
            "Wrong country code specified"
        );
        let mut good = "GB".to_owned();
        assert!(check_country_code(&mut good).is_ok());

        // the td_api parse validates and drops into the model
        let message = err_message(get_address(None));
        assert_eq!(message, "Address must be non-empty");
    }

    #[test]
    fn date_checks_match_the_cpp() {
        // the good date formats zero-padded
        assert_eq!(
            get_date(Some(td_api::Date {
                day: 1,
                month: 5,
                year: 2015
            }))
            .unwrap(),
            "01.05.2015"
        );
        // out-of-range fields
        assert_eq!(
            err_message(get_date(Some(td_api::Date {
                day: 32,
                month: 1,
                year: 2015
            }))),
            "Wrong day number specified"
        );
        assert_eq!(
            err_message(get_date(Some(td_api::Date {
                day: 29,
                month: 2,
                year: 2015
            }))),
            "Wrong day in month number specified"
        );
        // a leap year accepts Feb 29
        assert!(
            get_date(Some(td_api::Date {
                day: 29,
                month: 2,
                year: 2016
            }))
            .is_ok()
        );
        // the parser accepts the zero-padded and dotted forms
        assert!(get_date_object("1.5.2015").unwrap().is_some());
        assert!(get_date_object("01.05.15").is_error());
        assert!(get_date_object("").unwrap().is_none());
    }
}
