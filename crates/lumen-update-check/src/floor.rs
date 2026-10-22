//! The serial floor: a manifest that is older than the one already seen is a replay.

use crate::Error;

/// The highest serial that was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Floor {
    serial: u64,
}

impl Floor {
    /// A floor that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { serial: 0 }
    }

    /// A floor at `serial`.
    #[must_use]
    pub const fn at(serial: u64) -> Self {
        Self { serial }
    }

    /// The serial the floor is at.
    #[must_use]
    pub const fn serial(self) -> u64 {
        self.serial
    }

    /// Raises the floor to `serial` when it is higher.
    pub fn raise(&mut self, serial: u64) {
        if serial > self.serial {
            self.serial = serial;
        }
    }
}

/// Checks that `serial` is not below the floor.
///
/// # Errors
/// Returns an error when the serial is below the floor, which means the manifest is a replay.
pub fn check(serial: u64, floor: &Floor) -> Result<(), Error> {
    if serial < floor.serial {
        return Err(Error::Replayed {
            got: serial,
            floor: floor.serial,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_manifest_passes_and_raises_the_floor() {
        let mut floor = Floor::new();
        check(5, &floor).expect("the first manifest passes");
        floor.raise(5);
        check(6, &floor).expect("a newer manifest passes");
        floor.raise(6);
        assert_eq!(floor.serial(), 6);
    }

    #[test]
    fn an_older_manifest_is_a_replay() {
        let floor = Floor::at(9);
        let error = check(8, &floor).expect_err("the replay is refused");
        assert_eq!(error, Error::Replayed { got: 8, floor: 9 });
    }

    #[test]
    fn the_same_manifest_passes() {
        let floor = Floor::at(9);
        assert!(check(9, &floor).is_ok());
    }
}
