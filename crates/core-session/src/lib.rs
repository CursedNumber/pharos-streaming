// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Session FSM + capability negotiation (BATTLE_PLAN §3). Skeleton for M0;
//! fleshed out alongside the QUIC transport at M1.

use core_proto::ControlMsg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Disconnected,
    Handshaking,
    Streaming,
    Paused,
}

#[derive(Debug)]
pub struct Session {
    pub state: SessionState,
}

impl Session {
    pub fn new() -> Self {
        Self {
            state: SessionState::Disconnected,
        }
    }

    /// Placeholder driver: M1 turns this into the real negotiation FSM.
    pub fn on_control(&mut self, _msg: &ControlMsg) {
        // TODO(M1): drive state transitions from control messages.
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
