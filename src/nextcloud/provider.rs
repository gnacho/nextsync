//! The sync provider an account is bound to.
//!
//! `Provider` selects the server dialect (Nextcloud vs OpenCloud) for the
//! WebDAV requests and for the native engine configuration. It is persisted
//! in `settings.json` as a lowercase string.

use std::fmt;
use std::str::FromStr;

/// The sync provider an account is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// A Nextcloud server (the historical default).
    #[default]
    Nextcloud,
    /// An OpenCloud server.
    OpenCloud,
}

impl Provider {
    /// Stable lowercase name (`"nextcloud"` or `"opencloud"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nextcloud => "nextcloud",
            Self::OpenCloud => "opencloud",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error produced when an unknown provider name is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown provider: {}", self.0)
    }
}

impl std::error::Error for ProviderError {}

impl FromStr for Provider {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "nextcloud" => Ok(Self::Nextcloud),
            "opencloud" => Ok(Self::OpenCloud),
            other => Err(ProviderError(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_string_forms_and_defaults() {
        assert_eq!(Provider::Nextcloud.as_str(), "nextcloud");
        assert_eq!(Provider::OpenCloud.as_str(), "opencloud");
        assert_eq!(Provider::default(), Provider::Nextcloud);
    }

    #[test]
    fn provider_serde_roundtrip_lowercase() {
        assert_eq!(
            serde_json::to_string(&Provider::OpenCloud).unwrap(),
            "\"opencloud\""
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"opencloud\"").unwrap(),
            Provider::OpenCloud
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"nextcloud\"").unwrap(),
            Provider::Nextcloud
        );
    }

    #[test]
    fn provider_from_str_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            "nextcloud".parse::<Provider>().unwrap(),
            Provider::Nextcloud
        );
        assert_eq!(
            "OpenCloud".parse::<Provider>().unwrap(),
            Provider::OpenCloud
        );
        assert!("kde".parse::<Provider>().is_err());
    }
}
