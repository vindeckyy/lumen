//! Version comparison.
//!
//! A version is a dot separated list of numbers, with an optional suffix that makes it
//! pre-release. A version with more numbers is newer when the numbers before it are equal.

/// Compares two versions. `true` when `left` is newer than `right`.
#[must_use]
pub fn is_newer(left: &str, right: &str) -> bool {
    let (left_numbers, left_pre) = split(left);
    let (right_numbers, right_pre) = split(right);
    for (a, b) in left_numbers.iter().zip(right_numbers.iter()) {
        match a.cmp(b) {
            std::cmp::Ordering::Equal => {}
            other => return other.is_gt(),
        }
    }
    if left_numbers.len() != right_numbers.len() {
        return left_numbers.len() > right_numbers.len();
    }
    // The same numbers: a pre-release is older than the release.
    // A release is newer than a pre-release; two pre-releases are compared by their suffix.
    match (left_pre, right_pre) {
        (None, Some(_)) => true,
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The numbers of a version and its pre-release suffix.
fn split(version: &str) -> (Vec<u64>, Option<&str>) {
    let (numbers, pre) = match version.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (version, None),
    };
    (
        numbers
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect(),
        pre,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_number_is_newer() {
        assert!(is_newer("1.2.3", "1.2.2"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
    }

    #[test]
    fn the_same_version_is_not_newer() {
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2", "1.2.0"));
    }

    #[test]
    fn a_pre_release_is_older_than_the_release() {
        assert!(is_newer("1.2.3", "1.2.3-rc1"));
        assert!(!is_newer("1.2.3-rc1", "1.2.3"));
        assert!(is_newer("1.2.3-rc2", "1.2.3-rc1"));
    }

    #[test]
    fn a_lower_version_is_not_newer() {
        assert!(!is_newer("1.2.2", "1.2.3"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }
}
