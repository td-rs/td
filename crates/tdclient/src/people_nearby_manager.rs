//! `PeopleNearbyManager` (td/telegram/PeopleNearbyManager.{h,cpp}) — the
//! M.31 Wave-3 slice. The 27-line C++ actor has no behavior beyond its
//! constructor side effect and the parent reset in `tear_down`, so the
//! port collapses it to the constructor side effect (the recorded
//! collapse, run at the C++ construction site in `Td::init`).

/// The `G()`/`td_` surface of the collapsed manager.
pub trait PeopleNearbyManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->erase`.
    fn pmc_erase(&mut self, key: &str);
}

/// The `PeopleNearbyManager` constructor (PeopleNearbyManager.cpp:12-18):
/// a non-bot user erases the two location-visibility expire dates.
pub fn init(env: &mut impl PeopleNearbyManagerEnv) {
    if !env.is_bot() {
        env.pmc_erase("location_visibility_expire_date");
        env.pmc_erase("pending_location_visibility_expire_date");
    }
}
