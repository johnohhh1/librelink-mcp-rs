//! LibreLinkUp regions: the names `setup` accepts and their API hosts.

/// Canonical stored names, in the order `setup` lists them.
const REGIONS: [&str; 14] = [
    "global", "ae", "ap", "au", "ca", "de", "eu", "eu2", "fr", "jp", "us", "la", "ru", "cn",
];

/// The stored spelling of a user-entered region, or `None` if unknown.
/// Blank input means `global`.
pub(crate) fn canonical_region(input: &str) -> Option<&'static str> {
    let normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Some("global");
    }
    REGIONS.into_iter().find(|region| *region == normalized)
}

/// Production base URL for a canonical region name. Matches the hosts the
/// previously used `libre_link_up_api_client` 0.3.3 crate mapped to.
pub(crate) fn region_base_url(region: &str) -> Option<&'static str> {
    Some(match region {
        "global" => "https://api.libreview.io",
        "ae" => "https://api-ae.libreview.io",
        "ap" => "https://api-ap.libreview.io",
        "au" => "https://api-au.libreview.io",
        "ca" => "https://api-ca.libreview.io",
        "de" => "https://api-de.libreview.io",
        "eu" => "https://api-eu.libreview.io",
        "eu2" => "https://api-eu2.libreview.io",
        "fr" => "https://api-fr.libreview.io",
        "jp" => "https://api-jp.libreview.io",
        "us" => "https://api-us.libreview.io",
        "la" => "https://api-la.libreview.io",
        "ru" => "https://api.libreview.ru",
        "cn" => "https://api-cn.myfreestyle.cn",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, valid_api_version};

    #[test]
    fn accepts_only_known_regions() {
        assert_eq!(canonical_region("EU2"), Some("eu2"));
        assert_eq!(canonical_region(" us "), Some("us"));
        assert_eq!(canonical_region(""), Some("global"));
        assert_eq!(canonical_region("unknown"), None);
        assert!(valid_api_version("4.16.0"));
        assert!(!valid_api_version("4.16.0\r\npassword:secret"));
        assert!(!valid_api_version("latest"));
        let old_settings: Settings =
            serde_json::from_str(r#"{"target_low":70,"target_high":180,"patient_name":null}"#)
                .unwrap();
        assert_eq!(old_settings.api_version, "4.16.0");
    }

    #[test]
    fn every_region_has_a_base_url() {
        for region in REGIONS {
            assert!(region_base_url(region).is_some(), "{region}");
        }
        assert_eq!(region_base_url("global"), Some("https://api.libreview.io"));
        assert_eq!(region_base_url("eu2"), Some("https://api-eu2.libreview.io"));
        assert_eq!(region_base_url("cn"), Some("https://api-cn.myfreestyle.cn"));
        assert_eq!(region_base_url("unknown"), None);
    }
}
