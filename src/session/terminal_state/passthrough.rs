use super::*;

#[derive(Debug, Default)]
pub(super) enum TmuxPassthroughState {
    #[default]
    Ground,
    StartEscape,
    Prefix {
        matched: usize,
    },
    Payload {
        payload: Vec<u8>,
        escaped: bool,
    },
}

impl TerminalState {
    pub(super) fn filter_tmux_passthrough(&mut self, bytes: &[u8]) -> Vec<u8> {
        const TMUX_PREFIX: &[u8] = b"tmux;";
        let mut filtered = Vec::with_capacity(bytes.len());

        for &byte in bytes {
            match &mut self.tmux_passthrough_state {
                TmuxPassthroughState::Ground => {
                    if byte == 0x1b {
                        self.tmux_passthrough_state = TmuxPassthroughState::StartEscape;
                    } else {
                        filtered.push(byte);
                    }
                }
                TmuxPassthroughState::StartEscape => {
                    if byte == b'P' {
                        self.tmux_passthrough_state = TmuxPassthroughState::Prefix { matched: 0 };
                    } else {
                        filtered.push(0x1b);
                        filtered.push(byte);
                        self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                    }
                }
                TmuxPassthroughState::Prefix { matched } => {
                    if TMUX_PREFIX
                        .get(*matched)
                        .is_some_and(|expected| *expected == byte)
                    {
                        *matched += 1;
                        if *matched == TMUX_PREFIX.len() {
                            self.tmux_passthrough_state = TmuxPassthroughState::Payload {
                                payload: Vec::new(),
                                escaped: false,
                            };
                        }
                        continue;
                    }

                    filtered.push(0x1b);
                    filtered.push(b'P');
                    filtered.extend_from_slice(&TMUX_PREFIX[..*matched]);
                    filtered.push(byte);
                    self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                }
                TmuxPassthroughState::Payload { payload, escaped } => {
                    if *escaped {
                        match byte {
                            0x1b => payload.push(0x1b),
                            b'\\' => {
                                if !payload.is_empty() {
                                    self.grid.passthrough_queue.push(std::mem::take(payload));
                                }
                                self.tmux_passthrough_state = TmuxPassthroughState::Ground;
                                continue;
                            }
                            _ => {
                                payload.push(0x1b);
                                payload.push(byte);
                            }
                        }
                        *escaped = false;
                        continue;
                    }

                    if byte == 0x1b {
                        *escaped = true;
                    } else {
                        payload.push(byte);
                    }
                }
            }
        }

        filtered
    }
}
