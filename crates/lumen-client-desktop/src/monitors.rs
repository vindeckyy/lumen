//! The monitor list of the client.
//!
//! The host sends one [`ControlMessage::Monitor`] per output it can
//! capture. The client keeps the list, shows it and picks one of the
//! entries. The choice decides which output the host captures, so the
//! client has to be able to name the output it wants.

use lumen_host_service::control::ControlMessage;

/// One output the host can capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// The index of the output in the host's enumeration.
    pub index: u32,
    /// The width of the output in pixels.
    pub width: u32,
    /// The height of the output in pixels.
    pub height: u32,
    /// The refresh rate of the output in hertz.
    pub refresh_hz: u32,
}

impl Monitor {
    /// The size and the refresh rate, as a single line.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{}x{}@{}", self.width, self.height, self.refresh_hz)
    }
}

/// The outputs the host offers and the one the client picked.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MonitorList {
    monitors: Vec<Monitor>,
    selected: Option<u32>,
}

impl MonitorList {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the output of a control message.
    ///
    /// A message for an output that is already in the list replaces it, so
    /// a host that repeats its list does not grow the client's list.
    pub fn apply(&mut self, index: u32, width: u32, height: u32, refresh_hz: u32) {
        let monitor = Monitor {
            index,
            width,
            height,
            refresh_hz,
        };
        match self
            .monitors
            .iter_mut()
            .find(|monitor| monitor.index == index)
        {
            Some(existing) => *existing = monitor,
            None => self.monitors.push(monitor),
        }
        if self.selected.is_none() {
            self.selected = Some(index);
        }
    }

    /// Take a control message apart and add the output it names.
    pub fn apply_message(&mut self, message: &ControlMessage) {
        if let ControlMessage::Monitor {
            index,
            width,
            height,
            refresh_hz,
        } = message
        {
            self.apply(*index, *width, *height, *refresh_hz);
        }
    }

    /// The outputs the host offers.
    #[must_use]
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// How many outputs the host offers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// Whether the host offered no output.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// The output the client picked.
    #[must_use]
    pub fn selected(&self) -> Option<&Monitor> {
        let index = self.selected?;
        self.monitors.iter().find(|monitor| monitor.index == index)
    }

    /// The index of the output the client picked.
    #[must_use]
    pub fn selected_index(&self) -> Option<u32> {
        self.selected
    }

    /// Pick an output.
    ///
    /// The first output is picked when there is none, so a session starts
    /// with something on the screen.
    pub fn select(&mut self, index: u32) -> bool {
        if self.monitors.iter().any(|monitor| monitor.index == index) {
            self.selected = Some(index);
            true
        } else {
            false
        }
    }

    /// The list as the lines a client shows.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.monitors
            .iter()
            .map(|monitor| {
                let marker = if Some(monitor.index) == self.selected {
                    '*'
                } else {
                    ' '
                };
                format!("{marker} {} {}", monitor.index, monitor.describe())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_is_added_and_replaced() {
        let mut list = MonitorList::new();
        list.apply(0, 1920, 1080, 60);
        list.apply(1, 2560, 1440, 120);
        assert_eq!(list.len(), 2);
        list.apply(1, 1280, 720, 60);
        assert_eq!(list.len(), 2, "an output must not be added twice");
        assert_eq!(list.monitors()[1].width, 1280);
    }

    #[test]
    fn the_first_output_is_picked_by_default() {
        let mut list = MonitorList::new();
        assert!(list.selected().is_none());
        list.apply(3, 1920, 1080, 60);
        assert_eq!(list.selected_index(), Some(3));
    }

    #[test]
    fn an_output_can_be_picked() {
        let mut list = MonitorList::new();
        list.apply(0, 1920, 1080, 60);
        list.apply(1, 2560, 1440, 120);
        assert!(list.select(1));
        assert_eq!(list.selected_index(), Some(1));
        assert_eq!(list.selected().unwrap().describe(), "2560x1440@120");
        // An output the host does not offer cannot be picked.
        assert!(!list.select(9));
        assert_eq!(list.selected_index(), Some(1));
    }

    #[test]
    fn the_list_shows_the_picked_output() {
        let mut list = MonitorList::new();
        list.apply(0, 1920, 1080, 60);
        list.apply(1, 2560, 1440, 120);
        list.select(1);
        let lines = list.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(' '), "the first line is not picked");
        assert!(lines[1].starts_with('*'), "the second line is picked");
    }

    #[test]
    fn a_control_message_is_taken_apart() {
        let mut list = MonitorList::new();
        list.apply_message(&ControlMessage::Monitor {
            index: 2,
            width: 3840,
            height: 2160,
            refresh_hz: 30,
        });
        assert_eq!(list.len(), 1);
        assert_eq!(list.monitors()[0].width, 3840);
        // A message that names no monitor changes nothing.
        list.apply_message(&ControlMessage::Bye);
        assert_eq!(list.len(), 1);
    }
}
