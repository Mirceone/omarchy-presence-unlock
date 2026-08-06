//! Compile-time enrollment provider registry.
//!
//! Profiles describe runtime advertisement semantics in the protocol crate;
//! providers obtain the credentials needed by one of those profiles. Keeping the
//! registries separate lets several enrollment transports target the same profile.

mod apple_watch;
pub(crate) mod mgmt;

use omarchy_watch_unlock_protocol::Profile;

/// How far an enrollment has got, reported as it happens.
///
/// The order of the variants is the order they occur in, and [`Phase::rank`]
/// is what lets a caller draw everything before the current phase as finished
/// without tracking the sequence itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The Bluetooth adapter is open and powered.
    AdapterReady,
    /// The privileged pairing monitor is listening for key material.
    MonitorReady,
    /// This computer is advertising under the carried name.
    Advertising(String),
    /// The device connected, under the advertised name `BlueZ` has for it.
    Connected(Option<String>),
    /// Secure pairing completed.
    Bonded,
    /// The device distributed the identity key that makes it recognisable.
    IdentityReceived,
    /// The identity resolves the address the device was using, and has been
    /// saved when the request asked for it.
    Verified,
}

impl Phase {
    /// Position in the sequence. Phases with a lower rank have already
    /// happened by the time this one is reported.
    #[must_use]
    pub const fn rank(&self) -> u8 {
        match self {
            Self::AdapterReady => 0,
            Self::MonitorReady => 1,
            Self::Advertising(_) => 2,
            Self::Connected(_) => 3,
            Self::Bonded => 4,
            Self::IdentityReceived => 5,
            Self::Verified => 6,
        }
    }

    /// What this phase is called in a checklist or a log line.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::AdapterReady => "Bluetooth adapter ready".to_string(),
            Self::MonitorReady => "Secure pairing monitor ready".to_string(),
            Self::Advertising(name) => format!("Advertising as \u{201c}{name}\u{201d}"),
            Self::Connected(_) => "Device connected".to_string(),
            Self::Bonded => "Secure pairing completed".to_string(),
            Self::IdentityReceived => "Device identity received".to_string(),
            Self::Verified => "Device identity verified".to_string(),
        }
    }

    /// What the flow was doing when it reached this phase, for a failure
    /// report: the phase reached names the step that then went wrong.
    #[must_use]
    pub const fn waiting_for(&self) -> &'static str {
        match self {
            Self::AdapterReady => "Starting the privileged pairing monitor",
            Self::MonitorReady => "Advertising this computer as a pairable peripheral",
            Self::Advertising(_) => "Waiting for the device to connect",
            Self::Connected(_) => "Completing secure pairing",
            Self::Bonded => "Waiting for remote Identity Resolving Key",
            Self::IdentityReceived => "Verifying enrollment",
            Self::Verified => "Finishing up",
        }
    }
}

/// One thing an enrollment put back after it finished or was cancelled.
///
/// Reported rather than logged because a failure screen has to be able to say
/// what state the machine was left in, and a cancelled flow has to be able to
/// promise that nothing was left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanup {
    pub label: &'static str,
    pub ok: bool,
}

/// Anything an enrollment wants to say while it runs. Enrollments never print:
/// the interactive wizard repaints a live checklist from these, and the
/// non-interactive CLI prints them as they arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Phase(Phase),
    Cleanup(Cleanup),
}

/// Where an enrollment reports to.
///
/// `Sync` because the flow is driven by a multi-threaded runtime, which may
/// move the future holding this reference between worker threads.
pub type Sink<'a> = &'a (dyn Fn(Progress) + Sync);

pub struct Request<'a> {
    pub adapter: Option<&'a str>,
    pub timeout_secs: u64,
    pub id: &'a str,
    pub save: bool,
    /// Set to ask a long-running enrollment to stop early and clean up.
    pub cancel: &'a std::sync::atomic::AtomicBool,
    pub progress: Sink<'a>,
}

/// The words one guided enrollment is presented with.
///
/// Copy lives with the provider rather than in the wizard so the wizard stays
/// one generic flow: adding a provider adds its menu entry and its
/// instructions together, and the screens never grow a per-device branch.
pub struct Guide {
    /// What this provider enrolls, in the user's words. Shown in the menu, so
    /// it never mentions transports, profiles, or providers.
    pub label: &'static str,
    /// One line saying what this computer is about to do.
    pub summary: &'static str,
    /// What to do on the device being enrolled. `{name}` is replaced with the
    /// name this computer advertises under.
    pub steps: &'static [&'static str],
    /// The same instruction condensed to one line, shown while waiting.
    pub hint: &'static str,
}

pub struct Provider {
    id: &'static str,
    profile: &'static Profile,
    guide: Guide,
    description: &'static str,
    run: for<'a> fn(&Request<'a>) -> Result<(), String>,
}

impl Provider {
    pub const fn new(
        id: &'static str,
        profile: &'static Profile,
        guide: Guide,
        description: &'static str,
        run: for<'a> fn(&Request<'a>) -> Result<(), String>,
    ) -> Self {
        Self {
            id,
            profile,
            guide,
            description,
            run,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn profile(&self) -> &'static Profile {
        self.profile
    }

    #[must_use]
    pub const fn guide(&self) -> &Guide {
        &self.guide
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.guide.label
    }

    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }
}

pub static PROVIDERS: [&Provider; 1] = [&apple_watch::PROVIDER];

#[must_use]
pub fn find(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().copied().find(|provider| provider.id == id)
}

pub fn enroll(provider: &str, request: &Request<'_>) -> Result<(), String> {
    let provider = find(provider).ok_or_else(|| {
        let supported = PROVIDERS
            .iter()
            .map(|provider| provider.id())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown enrollment provider {provider}; supported: {supported}")
    })?;
    (provider.run)(request)
}

pub fn print_catalog() {
    for profile in omarchy_watch_unlock_protocol::profile::PROFILES {
        println!("{} — {}", profile.id(), profile.label());
        println!(
            "  {}",
            if profile.attests_device_state() {
                "reports whether the device is itself unlocked"
            } else {
                "says nothing about the device's own lock state"
            }
        );
        for provider in PROVIDERS
            .iter()
            .filter(|provider| provider.profile().id() == profile.id())
        {
            println!(
                "  enroll with {}: {}",
                provider.id(),
                provider.description()
            );
        }
    }
}

pub fn run_mgmt_helper(adapter_index: u16) -> Result<(), String> {
    mgmt::run_helper(adapter_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_unique_and_target_registered_profiles() {
        for (index, provider) in PROVIDERS.iter().enumerate() {
            assert!(
                PROVIDERS[..index]
                    .iter()
                    .all(|other| other.id() != provider.id())
            );
            assert!(
                omarchy_watch_unlock_protocol::profile::find(provider.profile().id()).is_some()
            );
            assert!(!provider.description().is_empty());
        }
    }

    /// The guided screens render this copy directly, so a provider missing any
    /// of it would show a blank instruction list rather than fail to build.
    #[test]
    fn every_provider_carries_the_copy_the_guided_flow_renders() {
        for provider in PROVIDERS {
            let guide = provider.guide();
            assert!(!guide.label.is_empty());
            assert!(!guide.summary.is_empty());
            assert!(!guide.hint.is_empty());
            assert!(!guide.steps.is_empty());
            assert!(guide.steps.iter().all(|step| !step.is_empty()));
        }
    }

    /// A phase must be reported as later than everything it comes after, or a
    /// checklist drawn from `rank` would show finished work as pending.
    #[test]
    fn phases_rank_in_the_order_they_happen() {
        let sequence = [
            Phase::AdapterReady,
            Phase::MonitorReady,
            Phase::Advertising("hci0".into()),
            Phase::Connected(None),
            Phase::Bonded,
            Phase::IdentityReceived,
            Phase::Verified,
        ];
        for pair in sequence.windows(2) {
            assert!(
                pair[0].rank() < pair[1].rank(),
                "{:?} must rank before {:?}",
                pair[0],
                pair[1]
            );
            assert!(!pair[0].describe().is_empty());
            assert!(!pair[0].waiting_for().is_empty());
        }
    }
}
