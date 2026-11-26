//! Managed output policy. Individual resource operations keep their existing owners.

use crate::preset::PresetManager;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::state_file::{BridgeRestore, StateFile};
use mozart_core::config::{Config, StartupMode};
use mozart_core::ctl::schema::{
    OutputMode, OutputModeStatus, OutputPhase, PlayoutPolicy, PlayoutSetRequest, Route,
};
use std::sync::{Mutex, TryLockError};

pub fn initialize(state: &mut State, config: &Config) {
    let requested = match config.daemon.startup_mode {
        StartupMode::Manual => return,
        StartupMode::Rvc if !config.rvc.model_id.is_empty() => OutputMode::Rvc,
        StartupMode::Rvc | StartupMode::Direct => OutputMode::Direct,
        StartupMode::Broadcast => OutputMode::Broadcast,
    };
    state.playout_policy = PlayoutPolicy::Silent;
    state.speech_drained = true;
    // A fresh supervised bridge has no prior admission. Direct and RVC can
    // start without waiting for bridge or TTS; the pre gate rejects TTS.
    let phase = if requested == OutputMode::Broadcast {
        OutputPhase::ClosingSpeech
    } else {
        OutputPhase::Routing
    };
    set_intent(state, requested, phase);
}

/// Called under the transition lock. A newer request withdraws an uncommitted route intent.
pub fn request(state: &mut State, mode: OutputMode) -> OutputModeStatus {
    if let Some(current) = &state.output_mode {
        if current.requested == mode {
            return current.clone();
        }
    }
    set_intent(state, mode, OutputPhase::ClosingSpeech)
}

fn set_intent(state: &mut State, mode: OutputMode, phase: OutputPhase) -> OutputModeStatus {
    state.set_route_desired(state.route_active());
    let next = OutputModeStatus {
        requested: mode,
        applied: None,
        phase,
        last_error: None,
    };
    state.output_mode = Some(next.clone());
    next
}

pub fn unmanage(state: &mut State) {
    if state.output_mode.take().is_some() {
        state.set_route_desired(state.route_active());
        state.playout_policy = PlayoutPolicy::Legacy;
        state.speech_drained = false;
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

    let broadcast = current.requested == OutputMode::Broadcast;
    let phase = match current.phase {
        OutputPhase::ClosingSpeech => {
            let drained = lock_state(state).speech_drained;
            if !broadcast && drained {
                // A superseded broadcast request may have started a redundant
                // fence, but never opened admission. Do not wait on that probe.
                bridge.speech_stop = None;
                bridge.clear_fence = None;
                lock_state(state).clear_degraded_prefix(services::SPEECH_STOP_REASON);
                Ok(Some(OutputPhase::Routing))
            } else {
                services::advance_speech_stop(state, services, state_file, &mut bridge).map(|_| {
                    lock_state(state).speech_drained = true;
                    Some(if broadcast {
                        OutputPhase::ConfiguringPlayout
                    } else {
                        OutputPhase::Routing
                    })
                })
            }
        }
        OutputPhase::ConfiguringPlayout => {
            let policy = if broadcast {
                PlayoutPolicy::BroadcastOnly
            } else {
                PlayoutPolicy::RealtimeOnly
            };
            // A lost reply may have changed pre already. Reassert the same policy on restart.
            let request = {
                let mut state = lock_state(state);
                state.playout_policy = policy;
                PlayoutSetRequest {
                    policy,
                    route: state.route_active(),
                    mute: state.mute(),
                }
            };
            services.assert_playout(request).map(|_| {
                Some(if broadcast {
                    OutputPhase::Routing
                } else {
                    OutputPhase::Applied
                })
            })
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
                } else if broadcast {
                    Some(OutputPhase::OpeningSpeech)
                } else {
                    Some(OutputPhase::ConfiguringPlayout)
                }
            })
        }
        OutputPhase::OpeningSpeech => services
            .tts_ready()
            .and_then(|()| {
                lock_state(state).speech_drained = false;
                services::speech_set(services, true)
            })
            .and_then(|data| {
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
        // Pre reconciliation owns reconnecting the current route and gate.
        OutputPhase::ReattachingPlayout | OutputPhase::Applied => return,
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

/// Called under the transition lock. Restarted pre begins silent with a fresh local cutoff.
pub fn observe_pre_playout(state: &mut State, pre: &mozart_core::ctl::schema::PreStatus) {
    if state.playout_startup_id.as_deref() == Some(pre.startup_id.as_str()) {
        return;
    }
    state.playout_startup_id = Some(pre.startup_id.clone());
    if state.playout_policy == PlayoutPolicy::BroadcastOnly {
        state.playout_policy = PlayoutPolicy::Silent;
        if let Some(mode) = state.output_mode.as_mut() {
            if mode.requested == OutputMode::Broadcast {
                mode.phase = OutputPhase::ClosingSpeech;
                mode.applied = None;
            }
        }
    } else if let Some(mode) = state.output_mode.as_mut() {
        if mode.phase == OutputPhase::Applied {
            mode.phase = OutputPhase::ReattachingPlayout;
            mode.applied = None;
        }
    }
}

pub(crate) fn finish_pre_reattach(state: &mut State, result: Result<(), String>) {
    let Some(mode) = state.output_mode.as_mut() else {
        return;
    };
    if mode.phase != OutputPhase::ReattachingPlayout {
        return;
    }
    mode.last_error = result.err();
    if mode.last_error.is_none() {
        mode.phase = OutputPhase::Applied;
        mode.applied = Some(mode.requested);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_defaults_to_direct_even_if_a_model_is_configured() {
        let mut config = Config::default();
        let mut state = State::new();
        initialize(&mut state, &config);
        assert_eq!(state.output_mode.unwrap().requested, OutputMode::Direct);

        config.rvc.model_id = "demo".into();
        let mut state = State::new();
        initialize(&mut state, &config);
        let mode = state.output_mode.unwrap();
        assert_eq!(mode.requested, OutputMode::Direct);
        assert_eq!(mode.phase, OutputPhase::Routing);
        assert_eq!(state.playout_policy, PlayoutPolicy::Silent);

        config.daemon.startup_mode = StartupMode::Rvc;
        let mut state = State::new();
        initialize(&mut state, &config);
        let mode = state.output_mode.unwrap();
        assert_eq!(mode.requested, OutputMode::Rvc);
        assert_eq!(mode.phase, OutputPhase::Routing);
        assert_eq!(state.playout_policy, PlayoutPolicy::Silent);

        config.rvc.model_id.clear();
        let mut state = State::new();
        initialize(&mut state, &config);
        assert_eq!(state.output_mode.unwrap().requested, OutputMode::Direct);
    }
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
