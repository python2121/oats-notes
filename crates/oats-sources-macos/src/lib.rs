//! The macOS [`AudioProducers`]: which processes are playing sound, from Core Audio's process
//! objects (macOS 14.2+: `kAudioHardwarePropertyProcessObjectList`, then per object
//! `kAudioProcessPropertyIsRunningOutput`, `kAudioProcessPropertyPID` and
//! `kAudioProcessPropertyBundleID`). Plain property reads on the HAL: no TCC permission, no
//! audio stream, no prompt — a few dozen microseconds per poll.
//!
//! On other platforms [`MacProducers::new`] returns `None` (there is no Core Audio).

pub use oats_sources::{AudioProducers, NoProducers, Producer};

#[cfg(target_os = "macos")]
mod macos;

/// Core Audio's view of the audio producers.
#[derive(Debug)]
pub struct MacProducers {
    own_pid: i32,
}

impl MacProducers {
    /// `None` when Core Audio has no process objects (before macOS 14.2) or this isn't macOS.
    pub fn new() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            if macos::process_objects().is_none() {
                log::info!(
                    "Core Audio has no process objects (macOS < 14.2?): no producer detection"
                );
                return None;
            }
            Some(Self {
                own_pid: std::process::id() as i32,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// The platform's best producer source: Core Audio's, else [`NoProducers`].
    pub fn boxed() -> Box<dyn AudioProducers> {
        match Self::new() {
            Some(p) => Box::new(p),
            None => Box::new(NoProducers),
        }
    }
}

impl AudioProducers for MacProducers {
    fn running_output(&mut self) -> Vec<Producer> {
        #[cfg(target_os = "macos")]
        {
            let mut out: Vec<Producer> = macos::process_objects()
                .unwrap_or_default()
                .into_iter()
                .filter_map(macos::producer_if_playing)
                .filter(|p| p.pid != self.own_pid)
                .collect();
            out.sort_by_key(|p| p.pid);
            out
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = self.own_pid;
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads Core Audio's process list (property reads only: no stream, no permission).
    #[test]
    fn lists_producers_without_ourselves() {
        let Some(mut p) = MacProducers::new() else {
            return; // not macOS 14.2+
        };
        let own = std::process::id() as i32;
        let now = p.running_output();
        assert!(now.iter().all(|x| x.pid != own));
        assert!(now.windows(2).all(|w| w[0].pid <= w[1].pid));
    }
}
