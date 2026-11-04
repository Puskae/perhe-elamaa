//! Admin-hideable bottom-bar features.
//!
//! A family admin can hide any of these tabs. Koti (home) and Minä (me) are
//! never hideable, so they are deliberately not in this enum. Hiding a
//! feature hides its tab and its Koti card for the whole family and mutes
//! that feature's push notifications. The data itself is kept, so un-hiding
//! brings everything back unchanged.

use serde::{Deserialize, Serialize};

/// One hideable feature. The serde names (snake_case) are the wire/DB keys
/// and must stay equal to [`Feature::key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Grocery,
    Calendar,
    Chores,
    Food,
    Notes,
    Chat,
    Announcements,
}

impl Feature {
    /// Every feature, in bottom-bar order (Announcements last).
    pub const ALL: [Feature; 7] = [
        Feature::Grocery,
        Feature::Calendar,
        Feature::Chores,
        Feature::Food,
        Feature::Notes,
        Feature::Chat,
        Feature::Announcements,
    ];

    /// Stable string key; identical to the serde name.
    pub fn key(self) -> &'static str {
        match self {
            Feature::Grocery => "grocery",
            Feature::Calendar => "calendar",
            Feature::Chores => "chores",
            Feature::Food => "food",
            Feature::Notes => "notes",
            Feature::Chat => "chat",
            Feature::Announcements => "announcements",
        }
    }

    /// Inverse of [`Feature::key`]; `None` for an unknown key.
    pub fn from_key(s: &str) -> Option<Feature> {
        // `into_iter()` on an array yields the values by copy; `find` stops
        // at the first match.
        Feature::ALL.into_iter().find(|f| f.key() == s)
    }

    /// Finnish UI label (the tab text).
    pub fn label_fi(self) -> &'static str {
        match self {
            Feature::Grocery => "Ostokset",
            Feature::Calendar => "Kalenteri",
            Feature::Chores => "Kotityöt",
            Feature::Food => "Ruoka",
            Feature::Notes => "Muistiot",
            Feature::Chat => "Chatti",
            Feature::Announcements => "Ilmoitukset",
        }
    }
}

/// Response of `GET /api/family/features` and body of `PUT` to the same path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyFeatures {
    /// Features hidden family-wide. Everything not listed is shown.
    pub hidden: Vec<Feature>,
}

impl FamilyFeatures {
    pub fn is_hidden(&self, f: Feature) -> bool {
        self.hidden.contains(&f)
    }

    /// Sorted (by enum order, thanks to the derived `Ord`) and deduplicated.
    /// Takes `self` by value (`mut self`) so callers can chain it cheaply.
    pub fn normalized(mut self) -> Self {
        self.hidden.sort();
        // `dedup` only removes *adjacent* duplicates — hence the sort first.
        self.hidden.dedup();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_key_round_trip() {
        for f in Feature::ALL {
            assert_eq!(Feature::from_key(f.key()), Some(f));
        }
    }

    #[test]
    fn unknown_key_is_none() {
        assert_eq!(Feature::from_key("koti"), None);
        assert_eq!(Feature::from_key(""), None);
        assert_eq!(Feature::from_key("Grocery"), None);
    }

    #[test]
    fn serde_name_equals_key() {
        for f in Feature::ALL {
            let json = serde_json::to_string(&f).unwrap();
            assert_eq!(json, format!("\"{}\"", f.key()));
            let back: Feature = serde_json::from_str(&json).unwrap();
            assert_eq!(back, f);
        }
    }

    #[test]
    fn family_features_serde() {
        let ff: FamilyFeatures = serde_json::from_str(r#"{"hidden":["chores"]}"#).unwrap();
        assert_eq!(ff.hidden, vec![Feature::Chores]);
        assert!(ff.is_hidden(Feature::Chores));
        assert!(!ff.is_hidden(Feature::Chat));
        assert_eq!(
            serde_json::to_string(&ff).unwrap(),
            r#"{"hidden":["chores"]}"#
        );
    }

    #[test]
    fn normalized_sorts_and_dedups() {
        let ff = FamilyFeatures {
            hidden: vec![Feature::Chat, Feature::Grocery, Feature::Chat],
        }
        .normalized();
        assert_eq!(ff.hidden, vec![Feature::Grocery, Feature::Chat]);
    }
}
