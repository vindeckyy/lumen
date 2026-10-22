//! Thread scheduling priority for the data plane.
//!
//! The capture, encode and send threads should not be descheduled by a game that uses every
//! core: a thread that wakes up late submits its work late, and the picture is late whatever
//! the pipeline does. The idea is punktfunk's `pf-frame/src/thread_qos.rs` (MIT OR Apache-2.0,
//! Copyright (c) 2026 unom - Enrico Bühler).
//!
//! The nice value is best effort: lowering it needs `CAP_SYS_NICE` or a raised
//! `RLIMIT_NICE`, so the call may fail and say so. Realtime policies are not used: a realtime
//! thread can preempt the compositor.

use rustix::process::{getpriority_process, setpriority_process};

/// The nice value of the capture and encode threads.
pub const NICE_CRITICAL: i32 = -10;

/// The nice value of the send thread.
pub const NICE_ABOVE_NORMAL: i32 = -5;

/// Raises the priority of the calling thread.
///
/// `critical` is the capture and encode thread, which should not wait behind anything; the
/// send thread is a step lower. Returns the nice value the thread has afterwards.
pub fn boost_thread_priority(critical: bool) -> i32 {
    let nice = if critical {
        NICE_CRITICAL
    } else {
        NICE_ABOVE_NORMAL
    };
    // PRIO_PROCESS with no pid is the calling thread.
    let _ = setpriority_process(None, nice);
    current_nice()
}

/// The nice value of the calling thread.
#[must_use]
pub fn current_nice() -> i32 {
    getpriority_process(None).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nice_value_of_the_calling_thread_is_readable() {
        // The test process may not lower its own nice value, so the value is only read.
        assert!(current_nice() <= 0);
    }
}
