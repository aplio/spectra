use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};

pub const FRAME_DURATION_60_FPS: Duration = Duration::from_millis(16);

pub fn poll_event_for(timeout: Duration) -> io::Result<Option<Event>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    Ok(Some(event::read()?))
}

pub fn poll_event_until(deadline: Instant) -> io::Result<Option<Event>> {
    let now = Instant::now();
    let Some(remaining) = deadline.checked_duration_since(now) else {
        return Ok(None);
    };
    if remaining.is_zero() {
        return Ok(None);
    }
    poll_event_for(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadline_returns_none() {
        let result = poll_event_until(Instant::now() - Duration::from_millis(1))
            .expect("poll until expired deadline");
        assert!(result.is_none());
    }
}
