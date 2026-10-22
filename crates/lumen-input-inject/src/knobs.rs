//! The environment knobs this crate reads.
//!
//! Every `LUMEN_*` read lives here, so the list of what can change the behaviour of the
//! injector is in one place.

/// Whether a test may move the real pointer on the X display: `LUMEN_XTEST_MOVE_POINTER=1`.
///
/// The test that uses it moves the pointer of whatever display `DISPLAY` names, so it runs
/// only against a throwaway display such as Xvfb.
#[must_use]
pub fn xtest_may_move_pointer() -> bool {
    std::env::var("LUMEN_XTEST_MOVE_POINTER").as_deref() == Ok("1")
}
