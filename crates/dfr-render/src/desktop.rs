//! The desktop user the deck's session helpers run as, and how to reach them.
//!
//! The optional helpers (volume, brightness, now playing, voice level,
//! visualizer) run in the graphical user's session and each listens on the
//! abstract socket `<name>-<uid>`. The daemon starts at boot, before anyone
//! logs in, and the seat's user can change, so the user is resolved on every
//! dial instead of once at startup.

use std::{
    env, fs,
    os::{linux::net::SocketAddrExt, unix::net::SocketAddr},
};

/// Set this in the service environment to pin the desktop user instead of
/// following the active seat (for example on a machine with several users).
pub const DESKTOP_UID_ENV: &str = "TINY_DFR_DESKTOP_UID";

/// logind's record of seat0. sd-login's `sd_seat_get_active()` reads this
/// same file; reading it directly keeps libsystemd out of the daemon.
const SEAT0_STATE: &str = "/run/systemd/seats/seat0";

/// The first regular user, used when neither the override nor logind names
/// one (no one logged in yet, or logind unavailable).
pub const FALLBACK_DESKTOP_UID: u32 = 1000;

/// The UID of the desktop user: the explicit override, else the active user
/// of seat0, else [`FALLBACK_DESKTOP_UID`].
pub fn desktop_uid() -> u32 {
    resolve_desktop_uid(
        env::var(DESKTOP_UID_ENV).ok().as_deref(),
        fs::read_to_string(SEAT0_STATE).ok().as_deref(),
    )
}

fn resolve_desktop_uid(override_uid: Option<&str>, seat_state: Option<&str>) -> u32 {
    override_uid
        .and_then(|value| value.trim().parse().ok())
        .or_else(|| seat_state.and_then(active_uid))
        .unwrap_or(FALLBACK_DESKTOP_UID)
}

fn active_uid(seat_state: &str) -> Option<u32> {
    seat_state
        .lines()
        .find_map(|line| line.strip_prefix("ACTIVE_UID="))
        .and_then(|value| value.trim().parse().ok())
        // A root greeter session is never the deck's desktop user.
        .filter(|&uid| uid != 0)
}

/// Where a session helper listens: the desktop user's socket for a helper
/// name, resolved on each dial, or a fixed address and UID (tests).
#[derive(Clone, Debug)]
pub enum Endpoint {
    Desktop(&'static str),
    Fixed(SocketAddr, u32),
}

impl Endpoint {
    /// The socket address to dial and the UID its listener must run as.
    pub fn resolve(&self) -> (SocketAddr, u32) {
        match self {
            Endpoint::Desktop(name) => {
                let uid = desktop_uid();
                let address = SocketAddr::from_abstract_name(format!("{name}-{uid}").as_bytes())
                    .expect("helper names are valid abstract socket names");
                (address, uid)
            }
            Endpoint::Fixed(address, uid) => (address.clone(), *uid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEAT: &str =
        "# This is private data. Do not parse.\nIS_SEAT0=1\nACTIVE=1\nACTIVE_UID=1001\nUIDS=1001\n";

    #[test]
    fn follows_the_active_seat_user() {
        assert_eq!(resolve_desktop_uid(None, Some(SEAT)), 1001);
    }

    #[test]
    fn explicit_override_wins_over_the_seat() {
        assert_eq!(resolve_desktop_uid(Some("1002"), Some(SEAT)), 1002);
    }

    #[test]
    fn unusable_override_falls_back_to_the_seat() {
        assert_eq!(resolve_desktop_uid(Some("alice"), Some(SEAT)), 1001);
    }

    #[test]
    fn no_session_or_root_greeter_uses_the_fallback() {
        assert_eq!(resolve_desktop_uid(None, None), FALLBACK_DESKTOP_UID);
        assert_eq!(
            resolve_desktop_uid(None, Some("ACTIVE_UID=0\n")),
            FALLBACK_DESKTOP_UID
        );
        assert_eq!(
            resolve_desktop_uid(None, Some("IS_SEAT0=1\n")),
            FALLBACK_DESKTOP_UID
        );
    }

    #[test]
    fn desktop_endpoint_names_the_socket_after_the_uid() {
        let (address, uid) = Endpoint::Desktop("dfr-test").resolve();
        assert_eq!(
            address.as_abstract_name().unwrap(),
            format!("dfr-test-{uid}").as_bytes()
        );
    }
}
