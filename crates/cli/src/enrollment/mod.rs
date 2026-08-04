//! Compile-time enrollment provider registry.
//!
//! Profiles describe runtime advertisement semantics in the protocol crate;
//! providers obtain the credentials needed by one of those profiles. Keeping the
//! registries separate lets several enrollment transports target the same profile.

mod apple_watch;
pub(crate) mod mgmt;

use omarchy_watch_unlock_protocol::Profile;

pub struct Request<'a> {
    pub adapter: Option<&'a str>,
    pub timeout_secs: u64,
    pub id: &'a str,
    pub save: bool,
    /// Set to ask a long-running enrollment to stop early and clean up.
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

pub struct Provider {
    id: &'static str,
    profile: &'static Profile,
    description: &'static str,
    run: for<'a> fn(&Request<'a>) -> Result<(), String>,
}

impl Provider {
    pub const fn new(
        id: &'static str,
        profile: &'static Profile,
        description: &'static str,
        run: for<'a> fn(&Request<'a>) -> Result<(), String>,
    ) -> Self {
        Self {
            id,
            profile,
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
        let assurance = if profile.attests_device_state() {
            "device-state attestation"
        } else {
            "proximity only"
        };
        println!("{} ({assurance})", profile.id());
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
}
