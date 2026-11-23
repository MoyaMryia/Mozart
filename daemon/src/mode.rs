//! Managed output policy. Individual resource operations keep their existing owners.

use crate::preset::PresetManager;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::state_file::{BridgeRestore, StateFile};
use mozart_core::config::Config;
use mozart_core::ctl::schema::{OutputMode, OutputModeStatus, OutputPhase, Route};
use std::sync::{Mutex, TryLockError};

/// Called under the transition lock. A newer request withdraws an uncommitted route intent.
pub fn request(state: &mut State, mode: OutputMode) -> OutputModeStatus {
    if let Some(current) = &state.output_mode {
        if current.requested == mode {
            return current.clone();
        }
    }
    state.set_route_desired(state.route_active());
    let next = OutputModeStatus {
        requested: mode,
        applied: None,
        phase: OutputPhase::ClosingSpeech,
        last_error: None,
    };
    state.output_mode = Some(next.clone());
    next
}

pub fn unmanage(state: &mut State) {
    if state.output_mode.take().is_some() {
        state.set_route_desired(state.route_active());
    }
}

pub fn tick(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
    state_file: &Mutex<StateFile>,
    bridge_restore: &Mutex<BridgeRestore>,
) {
    let Some(_transition) = services.try_transition() else {
        return;
    };
    let current = lock_state(state).output_mode.clone();
    let Some(mut current) = current else { return };
    if current.phase == OutputPhase::Applied {
        return;
    }
    let mut bridge = match bridge_restore.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return,
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
    };

    let phase = match current.phase {
        OutputPhase::ClosingSpeech => {
            services::advance_speech_stop(state, services, state_file, &mut bridge)
                .map(|_| Some(OutputPhase::Routing))
        }
        OutputPhase::Routing => {
            let route = if current.requested == OutputMode::Rvc {
                Route::Rvc
            } else {
                Route::Direct
            };
            services::route_set_locked(state, services, config, presets, route).map(|_| {
                let state = lock_state(state);
                if state.route_active() != route || state.realtime_return_pending() {
                    None
                } else if current.requested == OutputMode::Broadcast {
                    Some(OutputPhase::OpeningSpeech)
                } else {
                    Some(OutputPhase::Applied)
                }
            })
        }
        OutputPhase::OpeningSpeech => services::speech_set(services, true).and_then(|data| {
            if data["speech_enabled"] == true {
                bridge.speech = None; // The selected mode, not a legacy override, owns this setting.
                Ok(Some(OutputPhase::Applied))
            } else {
                Err((
                    mozart_core::ctl::CODE_INTERNAL.into(),
                    "bridge did not confirm new speech admission".into(),
                ))
            }
        }),
        OutputPhase::Applied => return,
    };
    match phase {
        Ok(next) => {
            current.last_error = None;
            if let Some(next) = next {
                current.phase = next;
            }
        }
        Err((code, reason)) => current.last_error = Some(format!("{code}: {reason}")),
    }
    if current.phase == OutputPhase::Applied {
        current.applied = Some(current.requested);
    }
    lock_state(state).output_mode = Some(current);
}

/// A restarted bridge must rejoin the selected policy instead of restoring its old user override.
pub fn bridge_restarted(state: &mut State) -> bool {
    let Some(mode) = state.output_mode.as_mut() else {
        return false;
    };
    if matches!(
        mode.phase,
        OutputPhase::Applied | OutputPhase::OpeningSpeech
    ) && mode.requested == OutputMode::Broadcast
    {
        // The prior bridge cannot grant new work. Fence its outstanding work before reopening.
        mode.phase = OutputPhase::ClosingSpeech;
        mode.applied = None;
    }
    true
}
