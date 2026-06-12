//! Tiny `vX.Y.Z[-pre]` parser + ordering. The GitHub `/releases/latest`
//! endpoint never returns prereleases, so full semver precedence rules
//! (numeric vs alphanumeric identifiers, etc.) are deliberately out of
//! scope — between two prereleases with the same triple we simply refuse
//! to call either "newer".

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// Accepts `0.1.0`, `v0.1.0`, `V0.1.0-rc1`, with optional `+build`
    /// metadata (ignored). A missing patch component defaults to 0.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let s = s.split('+').next().unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, (!pre.is_empty()).then(|| pre.to_string())),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub fn is_newer_than(&self, current: &Self) -> bool {
        let a = (self.major, self.minor, self.patch);
        let b = (current.major, current.minor, current.patch);
        if a != b {
            return a > b;
        }
        // Equal triple: a release supersedes a prerelease; anything else
        // (release==release, pre vs pre, pre vs release) is not an upgrade.
        current.pre.is_some() && self.pre.is_none()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parse_forms() {
        assert_eq!(v("0.1.0"), v("v0.1.0"));
        assert_eq!(v("V1.2.3").patch, 3);
        assert_eq!(v("1.2").patch, 0);
        assert_eq!(v("1.2.3-rc1").pre.as_deref(), Some("rc1"));
        assert_eq!(v("1.2.3+abc").pre, None);
        assert_eq!(v("1.2.3-rc1+abc").pre.as_deref(), Some("rc1"));
        assert!(Version::parse("").is_none());
        assert!(Version::parse("nope").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
    }

    #[test]
    fn ordering() {
        assert!(v("0.2.0").is_newer_than(&v("0.1.9")));
        assert!(v("1.0.0").is_newer_than(&v("0.9.9")));
        assert!(v("0.1.1").is_newer_than(&v("0.1.0")));
        assert!(!v("0.1.0").is_newer_than(&v("0.1.0")));
        assert!(!v("0.1.0").is_newer_than(&v("0.2.0")));
        // Prerelease never beats the release with the same triple…
        assert!(!v("v0.1.0-rc1").is_newer_than(&v("0.1.0")));
        // …but the release beats its own prerelease.
        assert!(v("0.1.0").is_newer_than(&v("0.1.0-rc1")));
        // Two prereleases with equal triple: undecidable, so not newer.
        assert!(!v("0.1.0-rc2").is_newer_than(&v("0.1.0-rc1")));
    }

    #[test]
    fn display_round_trip() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc1").to_string(), "1.2.3-rc1");
    }
}
