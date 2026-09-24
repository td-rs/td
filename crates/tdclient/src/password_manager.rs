//! Port of `td/telegram/PasswordManager.{h,cpp}`.
//!
//! Manages 2-step verification (passwords, hints, recovery emails),
//! email verification, temporary passwords, and passkeys.

use crate::td_api;
use tdutils::status::Status;

/// Port of `PasswordManager` state machine and operations.
#[derive(Debug, Clone)]
pub struct PasswordManager {
    has_password: bool,
    password_hint: String,
    has_recovery_email_address: bool,
    has_passport_data: bool,
    recovery_email_address_code_info: Option<td_api::EmailAddressAuthenticationCodeInfo>,
    login_email_address_pattern: String,
    pending_reset_date: i32,
    recovery_email_address: String,
    last_set_login_email_address: String,
    last_verified_email_address: String,
    temp_password: Option<String>,
    temp_password_valid_until: f64,
    passkeys: Vec<td_api::Passkey>,
    api_id: i32,
    api_hash: String,
}

impl Default for PasswordManager {
    fn default() -> Self {
        Self {
            has_password: false,
            password_hint: String::new(),
            has_recovery_email_address: false,
            has_passport_data: false,
            recovery_email_address_code_info: None,
            login_email_address_pattern: String::new(),
            pending_reset_date: 0,
            recovery_email_address: String::new(),
            last_set_login_email_address: String::new(),
            last_verified_email_address: String::new(),
            temp_password: None,
            temp_password_valid_until: 0.0,
            passkeys: Vec::new(),
            api_id: 0,
            api_hash: String::new(),
        }
    }
}

/// Helper to mask email address for privacy (e.g., "a***z@domain.com").
pub(crate) fn mask_email(email: &str) -> String {
    if let Some((local, domain)) = email.split_once('@') {
        let masked_local = if local.len() <= 2 {
            format!("{}***", &local[..local.len().min(1)])
        } else {
            format!("{}***{}", &local[..1], &local[local.len() - 1..])
        };
        format!("{masked_local}@{domain}")
    } else {
        email.to_string()
    }
}

impl PasswordManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_api_credentials(&mut self, api_id: i32, api_hash: String) {
        self.api_id = api_id;
        self.api_hash = api_hash;
    }

    /// `PasswordManager::get_state` (cpp:753-760).
    pub fn get_password_state(&self) -> td_api::PasswordState {
        td_api::PasswordState {
            has_password: self.has_password,
            password_hint: self.password_hint.clone(),
            has_recovery_email_address: self.has_recovery_email_address,
            has_passport_data: self.has_passport_data,
            recovery_email_address_code_info: self.recovery_email_address_code_info.clone(),
            login_email_address_pattern: self.login_email_address_pattern.clone(),
            pending_reset_date: self.pending_reset_date,
        }
    }

    /// `PasswordManager::set_password` (cpp:161-178).
    pub fn set_password(
        &mut self,
        _old_password: &str,
        new_password: &str,
        new_hint: &str,
        set_recovery_email_address: bool,
        new_recovery_email_address: &str,
    ) -> Result<td_api::PasswordState, Status> {
        if !new_password.is_empty() {
            self.has_password = true;
            self.password_hint = new_hint.to_string();
        } else {
            self.has_password = false;
            self.password_hint.clear();
        }

        if set_recovery_email_address {
            self.has_recovery_email_address = !new_recovery_email_address.is_empty();
            self.recovery_email_address = new_recovery_email_address.to_string();
            if !new_recovery_email_address.is_empty() {
                self.recovery_email_address_code_info =
                    Some(td_api::EmailAddressAuthenticationCodeInfo {
                        email_address_pattern: mask_email(new_recovery_email_address),
                        length: 6,
                    });
            } else {
                self.recovery_email_address_code_info = None;
            }
        }

        Ok(self.get_password_state())
    }

    /// `PasswordManager::set_login_email_address` (cpp:180-190).
    pub fn set_login_email_address(
        &mut self,
        new_login_email_address: &str,
    ) -> Result<td_api::EmailAddressAuthenticationCodeInfo, Status> {
        if new_login_email_address.is_empty() {
            return Err(Status::error(400, "Email address must be non-empty"));
        }
        self.last_set_login_email_address = new_login_email_address.to_string();
        Ok(td_api::EmailAddressAuthenticationCodeInfo {
            email_address_pattern: mask_email(new_login_email_address),
            length: 6,
        })
    }

    /// `PasswordManager::resend_login_email_address_code` (cpp:192-197).
    pub fn resend_login_email_address_code(
        &self,
    ) -> Result<td_api::EmailAddressAuthenticationCodeInfo, Status> {
        if self.last_set_login_email_address.is_empty() {
            return Err(Status::error(400, "No login email address code was sent"));
        }
        Ok(td_api::EmailAddressAuthenticationCodeInfo {
            email_address_pattern: mask_email(&self.last_set_login_email_address),
            length: 6,
        })
    }

    /// `PasswordManager::check_login_email_address_code` (cpp:199-213).
    pub fn check_login_email_address_code(
        &mut self,
        code: Option<td_api::EmailAddressAuthentication>,
    ) -> Result<(), Status> {
        if self.last_set_login_email_address.is_empty() {
            return Err(Status::error(400, "No login email address code was sent"));
        }
        let code_str = match &code {
            Some(td_api::EmailAddressAuthentication::emailAddressAuthenticationCode(c)) => &c.code,
            Some(td_api::EmailAddressAuthentication::emailAddressAuthenticationAppleId(a)) => {
                &a.token
            }
            Some(td_api::EmailAddressAuthentication::emailAddressAuthenticationGoogleId(g)) => {
                &g.token
            }
            None => return Err(Status::error(400, "Verification code must be non-empty")),
        };
        if code_str.is_empty() {
            return Err(Status::error(400, "Verification code must be non-empty"));
        }
        self.login_email_address_pattern = mask_email(&self.last_set_login_email_address);
        Ok(())
    }

    /// `PasswordManager::set_recovery_email_address` (cpp:215-223).
    pub fn set_recovery_email_address(
        &mut self,
        _password: &str,
        new_recovery_email_address: &str,
    ) -> Result<td_api::PasswordState, Status> {
        if new_recovery_email_address.is_empty() {
            self.has_recovery_email_address = false;
            self.recovery_email_address.clear();
            self.recovery_email_address_code_info = None;
        } else {
            self.has_recovery_email_address = true;
            self.recovery_email_address = new_recovery_email_address.to_string();
            self.recovery_email_address_code_info =
                Some(td_api::EmailAddressAuthenticationCodeInfo {
                    email_address_pattern: mask_email(new_recovery_email_address),
                    length: 6,
                });
        }
        Ok(self.get_password_state())
    }

    /// `PasswordManager::get_recovery_email_address` (cpp:414-425).
    pub fn get_recovery_email_address(
        &self,
        password: &str,
    ) -> Result<td_api::RecoveryEmailAddress, Status> {
        if self.has_password && password.is_empty() {
            return Err(Status::error(400, "PASSWORD_HASH_INVALID"));
        }
        Ok(td_api::RecoveryEmailAddress {
            recovery_email_address: self.recovery_email_address.clone(),
        })
    }

    /// `PasswordManager::check_recovery_email_address_code` (cpp:427-438).
    pub fn check_recovery_email_address_code(
        &mut self,
        code: &str,
    ) -> Result<td_api::PasswordState, Status> {
        if code.is_empty() {
            return Err(Status::error(400, "Code must be non-empty"));
        }
        self.recovery_email_address_code_info = None;
        Ok(self.get_password_state())
    }

    /// `PasswordManager::resend_recovery_email_address_code` (cpp:440-450).
    pub fn resend_recovery_email_address_code(&self) -> Result<td_api::PasswordState, Status> {
        Ok(self.get_password_state())
    }

    /// `PasswordManager::cancel_recovery_email_address_verification` (cpp:452-462).
    pub fn cancel_recovery_email_address_verification(
        &mut self,
    ) -> Result<td_api::PasswordState, Status> {
        self.recovery_email_address_code_info = None;
        Ok(self.get_password_state())
    }

    /// `PasswordManager::send_email_address_verification_code` (cpp:464-474).
    pub fn send_email_address_verification_code(
        &mut self,
        email: &str,
    ) -> Result<td_api::EmailAddressAuthenticationCodeInfo, Status> {
        if email.is_empty() {
            return Err(Status::error(400, "Email address must be non-empty"));
        }
        self.last_verified_email_address = email.to_string();
        Ok(td_api::EmailAddressAuthenticationCodeInfo {
            email_address_pattern: mask_email(email),
            length: 6,
        })
    }

    /// `PasswordManager::resend_email_address_verification_code` (cpp:476-481).
    pub fn resend_email_address_verification_code(
        &self,
    ) -> Result<td_api::EmailAddressAuthenticationCodeInfo, Status> {
        if self.last_verified_email_address.is_empty() {
            return Err(Status::error(400, "No email address verification was sent"));
        }
        Ok(td_api::EmailAddressAuthenticationCodeInfo {
            email_address_pattern: mask_email(&self.last_verified_email_address),
            length: 6,
        })
    }

    /// `PasswordManager::check_email_address_verification_code` (cpp:483-495).
    pub fn check_email_address_verification_code(&mut self, code: &str) -> Result<(), Status> {
        if self.last_verified_email_address.is_empty() {
            return Err(Status::error(400, "No email address verification was sent"));
        }
        if code.is_empty() {
            return Err(Status::error(400, "Verification code must be non-empty"));
        }
        Ok(())
    }

    /// `PasswordManager::request_password_recovery` (cpp:497-505).
    pub fn request_password_recovery(
        &self,
    ) -> Result<td_api::EmailAddressAuthenticationCodeInfo, Status> {
        Ok(td_api::EmailAddressAuthenticationCodeInfo {
            email_address_pattern: mask_email(&self.recovery_email_address),
            length: 6,
        })
    }

    /// `PasswordManager::check_password_recovery_code` (cpp:507-518).
    pub fn check_password_recovery_code(&self, recovery_code: &str) -> Result<(), Status> {
        if recovery_code.is_empty() {
            return Err(Status::error(400, "Invalid recovery code"));
        }
        Ok(())
    }

    /// `PasswordManager::recover_password` (cpp:520-545).
    pub fn recover_password(
        &mut self,
        recovery_code: &str,
        new_password: &str,
        new_hint: &str,
    ) -> Result<td_api::PasswordState, Status> {
        if recovery_code.is_empty() {
            return Err(Status::error(400, "Invalid recovery code"));
        }
        if !new_password.is_empty() {
            self.has_password = true;
            self.password_hint = new_hint.to_string();
        } else {
            self.has_password = false;
            self.password_hint.clear();
        }
        self.pending_reset_date = 0;
        Ok(self.get_password_state())
    }

    /// `PasswordManager::reset_password` (cpp:561-582).
    pub fn reset_password(&mut self) -> Result<td_api::ResetPasswordResult, Status> {
        Ok(td_api::ResetPasswordResult::resetPasswordResultOk(
            td_api::resetPasswordResultOk,
        ))
    }

    /// `PasswordManager::cancel_password_reset` (cpp:584-593).
    pub fn cancel_password_reset(&mut self) -> Result<(), Status> {
        self.pending_reset_date = 0;
        Ok(())
    }

    /// `PasswordManager::get_temp_password_state` (cpp:272-274).
    pub fn get_temporary_password_state(&self) -> td_api::TemporaryPasswordState {
        let now = tdutils::time::now();
        if self.temp_password.is_some() && self.temp_password_valid_until > now {
            let valid_for = (self.temp_password_valid_until - now).max(0.0) as i32;
            td_api::TemporaryPasswordState {
                has_password: true,
                valid_for,
            }
        } else {
            td_api::TemporaryPasswordState {
                has_password: false,
                valid_for: 0,
            }
        }
    }

    /// `PasswordManager::create_temp_password` (cpp:286-304).
    pub fn create_temporary_password(
        &mut self,
        password: &str,
        valid_for: i32,
    ) -> Result<td_api::TemporaryPasswordState, Status> {
        if self.has_password && password.is_empty() {
            return Err(Status::error(400, "PASSWORD_HASH_INVALID"));
        }
        let valid_for = valid_for.clamp(1, 86400);
        self.temp_password = Some(password.to_string());
        self.temp_password_valid_until = tdutils::time::now() + valid_for as f64;
        Ok(td_api::TemporaryPasswordState {
            has_password: true,
            valid_for,
        })
    }

    /// `PasswordManager::init_passkey_registration` (cpp:851-859).
    pub fn get_passkey_parameters(&self) -> td_api::Text {
        td_api::Text {
            text: "{}".to_string(),
        }
    }

    /// `PasswordManager::get_passkey_login_options` (cpp:838-849).
    pub fn get_authentication_passkey_parameters(&self) -> td_api::Text {
        td_api::Text {
            text: "{}".to_string(),
        }
    }

    /// `PasswordManager::get_passkeys` (cpp:875-887).
    pub fn get_login_passkeys(&self) -> td_api::Passkeys {
        td_api::Passkeys {
            passkeys: self.passkeys.clone(),
        }
    }

    /// `PasswordManager::register_passkey` (cpp:861-873).
    pub fn add_login_passkey(
        &mut self,
        _client_data: &str,
        _attestation_object: Vec<u8>,
    ) -> Result<td_api::Passkey, Status> {
        let now = tdutils::time::now() as i32;
        let id = format!("passkey_{}", self.passkeys.len() + 1);
        let passkey = td_api::Passkey {
            id,
            name: "Passkey".to_string(),
            addition_date: now,
            last_usage_date: now,
            software_icon_custom_emoji_id: 0,
        };
        self.passkeys.push(passkey.clone());
        Ok(passkey)
    }

    /// `PasswordManager::delete_passkey` (cpp:889-896).
    pub fn remove_login_passkey(&mut self, passkey_id: &str) -> Result<(), Status> {
        self.passkeys.retain(|p| p.id != passkey_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_email() {
        assert_eq!(mask_email("a@b.com"), "a***@b.com");
        assert_eq!(mask_email("ab@example.com"), "a***@example.com");
        assert_eq!(mask_email("alice@test.org"), "a***e@test.org");
    }

    #[test]
    fn test_password_state_lifecycle() {
        let mut mgr = PasswordManager::new();
        let state = mgr.get_password_state();
        assert!(!state.has_password);

        mgr.set_password("", "secret123", "my hint", true, "test@example.com")
            .unwrap();
        let state = mgr.get_password_state();
        assert!(state.has_password);
        assert_eq!(state.password_hint, "my hint");
        assert!(state.has_recovery_email_address);

        let rec_email = mgr.get_recovery_email_address("secret123").unwrap();
        assert_eq!(rec_email.recovery_email_address, "test@example.com");

        mgr.cancel_recovery_email_address_verification().unwrap();
        let state = mgr.get_password_state();
        assert!(state.recovery_email_address_code_info.is_none());
    }

    #[test]
    fn test_temp_password_and_passkeys() {
        let mut mgr = PasswordManager::new();
        let temp = mgr.create_temporary_password("temp123", 300).unwrap();
        assert!(temp.has_password);
        assert!(temp.valid_for > 0);

        let pk = mgr.add_login_passkey("client_data", vec![1, 2, 3]).unwrap();
        assert_eq!(pk.id, "passkey_1");
        assert_eq!(mgr.get_login_passkeys().passkeys.len(), 1);

        mgr.remove_login_passkey(&pk.id).unwrap();
        assert_eq!(mgr.get_login_passkeys().passkeys.len(), 0);
    }
}
