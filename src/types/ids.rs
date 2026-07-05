//! Entity ID newtypes.
//!
//! Four ID kinds — [`SpecId`], [`TaskId`], [`PhaseRunId`], [`DecisionId`] — each a
//! newtype over `Arc<str>` (not `String`). These IDs travel through 15 [`BoiEvent`]
//! variants × ~3 ID slots and every `PhaseContext` build; `String` heap-allocates on
//! each clone, `Arc<str>` is one atomic increment (Batch A review — L1).
//!
//! [`BoiEvent`]: crate::types::event::BoiEvent
//!
//! ## Format
//!
//! IDs are Crockford base32 (lowercase `0123456789abcdefghjkmnpqrstvwxyz` — excludes
//! the confusables `i`/`l`/`o`/`u`): an uppercase type prefix (`S`/`T`/`P`/`D`)
//! followed by an 8-char random base32 body. Total 9 chars, e.g. `Sxk3m9p2q`.
//!
//! ID *generation* (random base32 + collision-retry capped at 5) lives in Phase 3
//! (`repo::ids`), not here — this module defines the parse/validate surface only.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Crockford base32, lowercase, no confusables — excludes `i` / `l` / `o` / `u`.
const ID_ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";

/// Length of the random body following the type prefix.
const ID_BODY_LEN: usize = 8;

/// Total ID length: 1 prefix char + [`ID_BODY_LEN`] body chars.
const ID_TOTAL_LEN: usize = ID_BODY_LEN + 1;

/// An ID failed validation against the Crockford-base32 format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// Total length is not exactly `ID_TOTAL_LEN` characters.
    #[error("id '{got}' has length {len}, expected {ID_TOTAL_LEN}")]
    WrongLength {
        /// The rejected string.
        got: String,
        /// Its actual length.
        len: usize,
    },
    /// The first character is not the kind's uppercase prefix.
    #[error("id '{got}' has prefix '{found}', expected '{expected}'")]
    WrongPrefix {
        /// The rejected string.
        got: String,
        /// Prefix character that was found.
        found: char,
        /// Prefix character that was required.
        expected: char,
    },
    /// A body character is not in the Crockford base32 alphabet (e.g. an
    /// excluded confusable `i`/`l`/`o`/`u`, or an uppercase letter).
    #[error("id '{got}' has invalid body char '{bad}' (not in Crockford base32)")]
    InvalidBodyChar {
        /// The rejected string.
        got: String,
        /// The offending character.
        bad: char,
    },
}

/// Validate `s` as an ID with the given uppercase `prefix`.
fn validate(s: &str, prefix: char) -> Result<(), IdError> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() != ID_TOTAL_LEN {
        return Err(IdError::WrongLength {
            got: s.to_owned(),
            len: chars.len(),
        });
    }
    if chars[0] != prefix {
        return Err(IdError::WrongPrefix {
            got: s.to_owned(),
            found: chars[0],
            expected: prefix,
        });
    }
    for &c in &chars[1..] {
        if !ID_ALPHABET.contains(c) {
            return Err(IdError::InvalidBodyChar {
                got: s.to_owned(),
                bad: c,
            });
        }
    }
    Ok(())
}

/// Generate the four ID newtypes — identical surface, differing only by prefix.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Arc<str>);

        impl $name {
            /// The uppercase type prefix for this ID kind.
            pub const PREFIX: char = $prefix;

            /// Parse and validate a string into this ID.
            ///
            /// Validates: total length `ID_TOTAL_LEN`, the exact uppercase
            /// prefix, and that every body character is in the Crockford
            /// base32 alphabet. Returns [`IdError`] otherwise.
            pub fn new(s: impl AsRef<str>) -> Result<Self, IdError> {
                let s = s.as_ref();
                validate(s, Self::PREFIX)?;
                Ok(Self(Arc::from(s)))
            }

            /// Borrow the ID as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // e.g. SpecId("Sxk3m9p2q")
                write!(f, "{}({:?})", stringify!($name), &*self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                struct IdVisitor;
                impl Visitor<'_> for IdVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(
                            f,
                            "a {}-char Crockford-base32 id prefixed '{}'",
                            ID_TOTAL_LEN,
                            $prefix,
                        )
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$name, E> {
                        $name::new(v).map_err(E::custom)
                    }
                }
                de.deserialize_str(IdVisitor)
            }
        }
    };
}

define_id! {
    /// Identifier for a spec — `S` + 8 Crockford-base32 chars, e.g. `Sxk3m9p2q`.
    SpecId, 'S'
}
define_id! {
    /// Identifier for a task — `T` + 8 Crockford-base32 chars, e.g. `Txk3m9p2q`.
    TaskId, 'T'
}
define_id! {
    /// Identifier for a phase run — `P` + 8 Crockford-base32 chars, e.g. `Pxk3m9p2q`.
    PhaseRunId, 'P'
}
define_id! {
    /// Identifier for a decision — `D` + 8 Crockford-base32 chars, e.g. `Dxk3m9p2q`.
    DecisionId, 'D'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_pattern_accepted() {
        assert!(SpecId::new("Sxk3m9p2q").is_ok());
        assert!(TaskId::new("Tabcdefgh").is_ok());
        assert!(PhaseRunId::new("P0123456789".get(..9).unwrap()).is_ok());
        assert!(DecisionId::new("Dvwxyz000").is_ok());
    }

    #[test]
    fn wrong_prefix_rejected() {
        // A TaskId-shaped string is not a valid SpecId.
        let err = SpecId::new("Txk3m9p2q").unwrap_err();
        assert!(matches!(
            err,
            IdError::WrongPrefix {
                found: 'T',
                expected: 'S',
                ..
            }
        ));
        // Lowercase prefix is also wrong.
        assert!(matches!(
            SpecId::new("sxk3m9p2q").unwrap_err(),
            IdError::WrongPrefix { .. }
        ));
    }

    #[test]
    fn wrong_body_length_rejected() {
        // Plan says "7 + 9 chars": a 7-char body (8 total) and a 9-char body
        // (10 total) — both differ from the valid 8-char body / 9 total.
        assert!(matches!(
            SpecId::new("Sxk3m9p2").unwrap_err(),
            IdError::WrongLength { len: 8, .. }
        ));
        assert!(matches!(
            SpecId::new("Sxk3m9p2qz").unwrap_err(),
            IdError::WrongLength { len: 10, .. }
        ));
    }

    #[test]
    fn excluded_confusables_rejected() {
        // Crockford base32 excludes i / l / o / u — each must be rejected.
        for bad in ['i', 'l', 'o', 'u'] {
            let body: String = std::iter::repeat_n(bad, ID_BODY_LEN).collect();
            let candidate = format!("S{body}");
            let err = SpecId::new(&candidate).unwrap_err();
            assert!(
                matches!(err, IdError::InvalidBodyChar { bad: c, .. } if c == bad),
                "expected confusable '{bad}' to be rejected, got {err:?}",
            );
        }
    }

    #[test]
    fn uppercase_body_rejected() {
        // Body must be lowercase; an uppercase base32 letter is invalid.
        assert!(matches!(
            SpecId::new("SXk3m9p2q").unwrap_err(),
            IdError::InvalidBodyChar { bad: 'X', .. }
        ));
    }

    #[test]
    fn serde_roundtrip() {
        let id = TaskId::new("Tabc12345").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"Tabc12345\"");
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        // Deserializing an invalid id is an error, not a silent accept.
        assert!(serde_json::from_str::<TaskId>("\"Sabc12345\"").is_err());
    }

    #[test]
    fn fromstr_display_are_inverses() {
        let original = "Pqrstvwxy";
        let parsed: PhaseRunId = original.parse().unwrap();
        assert_eq!(parsed.to_string(), original);
        // Round-trip the other direction too.
        let id = DecisionId::new("D9876543z").unwrap();
        let reparsed: DecisionId = id.to_string().parse().unwrap();
        assert_eq!(id, reparsed);
    }

    #[test]
    fn debug_shows_kind_and_value() {
        let id = SpecId::new("Sxk3m9p2q").unwrap();
        assert_eq!(format!("{id:?}"), "SpecId(\"Sxk3m9p2q\")");
    }
}
