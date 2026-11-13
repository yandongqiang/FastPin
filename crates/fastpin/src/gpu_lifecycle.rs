//! GPU device lifecycle: an explicit, testable state machine plus the
//! process-wide generation stamps shared by capture resources.
//!
//! GPUI owns the single D3D11 device and recreates it after a TDR. The app
//! borrows that device, so every resource it creates (textures, the freeze
//! swap chain, WGC live sessions) belongs to one device generation. When the
//! device is lost the resources are not usable on the replacement device and
//! must be rejected instead of copied across devices.
//!
//! The transitions here are pure and never panic. Invalid transitions return
//! an error and leave the state untouched, so a late/duplicate platform
//! callback cannot corrupt the state.

use std::sync::Mutex;

/// How the device was lost. Reported from `GetDeviceRemovedReason`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeviceLostKind {
    /// `DXGI_ERROR_DEVICE_REMOVED` — adapter unplugged / driver update.
    #[default]
    Removed,
    /// `DXGI_ERROR_DEVICE_RESET` — TDR reset.
    Reset,
    /// `DXGI_ERROR_DEVICE_HUNG` — the GPU stopped responding.
    Hung,
    /// Another failure (driver internal error, unknown HRESULT).
    Other,
}

impl DeviceLostKind {
    pub fn label(self) -> &'static str {
        match self {
            DeviceLostKind::Removed => "DEVICE_REMOVED",
            DeviceLostKind::Reset => "DEVICE_RESET",
            DeviceLostKind::Hung => "DEVICE_HUNG",
            DeviceLostKind::Other => "DEVICE_LOST",
        }
    }

    #[cfg(target_os = "windows")]
    pub fn from_hresult(code: windows::core::HRESULT) -> Self {
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
        };
        if code == DXGI_ERROR_DEVICE_REMOVED {
            DeviceLostKind::Removed
        } else if code == DXGI_ERROR_DEVICE_RESET {
            DeviceLostKind::Reset
        } else if code == DXGI_ERROR_DEVICE_HUNG {
            DeviceLostKind::Hung
        } else {
            DeviceLostKind::Other
        }
    }
}

/// The lifecycle state of the borrowed GPUI device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuLifecycleState {
    /// The device is healthy and resources of `generation` may be used.
    Ready { generation: u64 },
    /// `GetDeviceRemovedReason` reported a loss; a reset is expected.
    Lost {
        generation: u64,
        reason: DeviceLostKind,
    },
    /// A reset is in progress. No GPU resource is usable and no new GPU work
    /// may start.
    Resetting { generation: u64 },
    /// The device was recreated with `generation`; resources of the old
    /// generation are stale, but new work may start on the new device.
    Recreated { generation: u64 },
}

impl Default for GpuLifecycleState {
    fn default() -> Self {
        GpuLifecycleState::Ready { generation: 0 }
    }
}

impl GpuLifecycleState {
    pub fn generation(self) -> u64 {
        match self {
            GpuLifecycleState::Ready { generation }
            | GpuLifecycleState::Lost { generation, .. }
            | GpuLifecycleState::Resetting { generation }
            | GpuLifecycleState::Recreated { generation } => generation,
        }
    }

    /// A resource stamped with `generation` may be used.
    pub fn is_usable(self, generation: u64) -> bool {
        match self {
            GpuLifecycleState::Ready {
                generation: current,
            }
            | GpuLifecycleState::Recreated {
                generation: current,
            } => current == generation,
            GpuLifecycleState::Lost { .. } | GpuLifecycleState::Resetting { .. } => false,
        }
    }

    /// New GPU work (captures, live sessions, freeze visuals) may start.
    pub fn accepts_new_work(self) -> bool {
        matches!(
            self,
            GpuLifecycleState::Ready { .. } | GpuLifecycleState::Recreated { .. }
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            GpuLifecycleState::Ready { .. } => "ready",
            GpuLifecycleState::Lost { .. } => "lost",
            GpuLifecycleState::Resetting { .. } => "resetting",
            GpuLifecycleState::Recreated { .. } => "recreated",
        }
    }
}

/// Why a transition was refused. Returned instead of panicking; the caller
/// can log it and keep going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuLifecycleError {
    /// `begin_reset` when a reset is already in progress.
    AlreadyResetting,
    /// `device_recreated` while no loss/reset was observed.
    NotLost,
    /// A proposed generation is not newer than the current one.
    StaleGeneration { current: u64, proposed: u64 },
    /// `acknowledge_recovery` outside `Recreated`.
    NotRecovering,
}

impl std::fmt::Display for GpuLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuLifecycleError::AlreadyResetting => write!(f, "GPU reset already in progress"),
            GpuLifecycleError::NotLost => write!(f, "GPU device was not reported lost"),
            GpuLifecycleError::StaleGeneration { current, proposed } => write!(
                f,
                "GPU generation {proposed} is not newer than the current generation {current}"
            ),
            GpuLifecycleError::NotRecovering => write!(f, "GPU device is not awaiting recovery"),
        }
    }
}

/// Pure lifecycle machine. Production code drives one process-wide instance
/// through the free functions below; tests construct independent instances.
#[derive(Clone, Copy, Debug)]
pub struct GpuLifecycle {
    state: GpuLifecycleState,
    /// The newest generation a device was successfully created for.
    generation: u64,
    resets: u32,
}

impl GpuLifecycle {
    pub const fn new(generation: u64) -> Self {
        Self {
            state: GpuLifecycleState::Ready { generation },
            generation,
            resets: 0,
        }
    }

    pub fn state(&self) -> GpuLifecycleState {
        self.state
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn reset_count(&self) -> u32 {
        self.resets
    }

    /// Marks the start of a device reset. `Ready`/`Recreated`/`Lost` may
    /// enter it; a second `begin_reset` is refused without changing state.
    pub fn begin_reset(&mut self) -> Result<GpuLifecycleState, GpuLifecycleError> {
        match self.state {
            GpuLifecycleState::Resetting { .. } => Err(GpuLifecycleError::AlreadyResetting),
            GpuLifecycleState::Ready { generation }
            | GpuLifecycleState::Recreated { generation }
            | GpuLifecycleState::Lost { generation, .. } => {
                self.state = GpuLifecycleState::Resetting { generation };
                self.resets = self.resets.wrapping_add(1);
                Ok(self.state)
            }
        }
    }

    /// Records a loss reported by `GetDeviceRemovedReason`. A loss while
    /// already resetting is expected and keeps the state resetting.
    pub fn device_lost(
        &mut self,
        reason: DeviceLostKind,
    ) -> Result<GpuLifecycleState, GpuLifecycleError> {
        match self.state {
            GpuLifecycleState::Resetting { generation } => {
                self.state = GpuLifecycleState::Resetting { generation };
            }
            GpuLifecycleState::Ready { generation }
            | GpuLifecycleState::Recreated { generation }
            | GpuLifecycleState::Lost { generation, .. } => {
                self.state = GpuLifecycleState::Lost { generation, reason };
            }
        }
        Ok(self.state)
    }

    /// Records a successfully recreated device. Requires a newer generation;
    /// the old generation's resources stay stale.
    pub fn device_recreated(
        &mut self,
        generation: u64,
    ) -> Result<GpuLifecycleState, GpuLifecycleError> {
        if generation <= self.generation {
            return Err(GpuLifecycleError::StaleGeneration {
                current: self.generation,
                proposed: generation,
            });
        }
        if !matches!(
            self.state,
            GpuLifecycleState::Resetting { .. } | GpuLifecycleState::Lost { .. }
        ) {
            return Err(GpuLifecycleError::NotLost);
        }
        self.generation = generation;
        self.state = GpuLifecycleState::Recreated { generation };
        Ok(self.state)
    }

    /// Self-heal for a platform generation that advanced without the app
    /// observing a reset begin (a missed callback must not wedge capture).
    pub fn observe_generation(&mut self, generation: u64) -> GpuLifecycleState {
        if generation > self.generation {
            self.generation = generation;
            self.state = GpuLifecycleState::Recreated { generation };
        }
        self.state
    }

    /// The freeze visual / auxiliary resources were rebuilt for the new
    /// device; the lifecycle can return to normal.
    pub fn acknowledge_recovery(&mut self) -> Result<GpuLifecycleState, GpuLifecycleError> {
        match self.state {
            GpuLifecycleState::Recreated { generation } => {
                self.state = GpuLifecycleState::Ready { generation };
                Ok(self.state)
            }
            _ => Err(GpuLifecycleError::NotRecovering),
        }
    }

    pub fn is_usable(&self, generation: u64) -> bool {
        self.state.is_usable(generation)
    }

    pub fn accepts_new_work(&self) -> bool {
        self.state.accepts_new_work()
    }

    pub fn is_resetting(&self) -> bool {
        matches!(self.state, GpuLifecycleState::Resetting { .. })
    }
}

/// Should the overlay rebuild and re-publish its DComp freeze visual?
///
/// `visual_generation` is the device generation the currently attached freeze
/// visual belongs to (`None` when there is no visual). During a loss/reset the
/// visual cannot be rebuilt; it is rebuilt once the state is usable again.
pub fn should_rebuild_freeze(state: GpuLifecycleState, visual_generation: Option<u64>) -> bool {
    match state {
        GpuLifecycleState::Ready { generation } | GpuLifecycleState::Recreated { generation } => {
            visual_generation != Some(generation)
        }
        GpuLifecycleState::Lost { .. } | GpuLifecycleState::Resetting { .. } => false,
    }
}

static LIFECYCLE: Mutex<GpuLifecycle> = Mutex::new(GpuLifecycle::new(0));

fn lock() -> std::sync::MutexGuard<'static, GpuLifecycle> {
    LIFECYCLE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Resets the process-wide lifecycle. Called once when the GPUI app starts.
pub fn init(generation: u64) {
    *lock() = GpuLifecycle::new(generation);
}

pub fn snapshot() -> GpuLifecycleState {
    lock().state()
}

pub fn current_generation() -> u64 {
    lock().generation()
}

pub fn begin_reset() -> Result<GpuLifecycleState, GpuLifecycleError> {
    lock().begin_reset()
}

pub fn note_device_lost(reason: DeviceLostKind) -> Result<GpuLifecycleState, GpuLifecycleError> {
    lock().device_lost(reason)
}

pub fn note_device_recreated(generation: u64) -> Result<GpuLifecycleState, GpuLifecycleError> {
    lock().device_recreated(generation)
}

pub fn observe_generation(generation: u64) -> GpuLifecycleState {
    lock().observe_generation(generation)
}

pub fn acknowledge_recovery() -> Result<GpuLifecycleState, GpuLifecycleError> {
    lock().acknowledge_recovery()
}

pub fn is_usable(generation: u64) -> bool {
    lock().is_usable(generation)
}

pub fn accepts_new_work() -> bool {
    lock().accepts_new_work()
}

pub fn is_resetting() -> bool {
    lock().is_resetting()
}

/// Classifies and records a device loss from a failed GPU call. Returns true
/// when the failure was a device loss (and the lifecycle moved to
/// `Resetting`); a plain `E_INVALIDARG`-style failure returns false.
#[cfg(target_os = "windows")]
pub fn note_failed_device_operation(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> bool {
    let Some(reason) = device_removed_reason(device) else {
        return false;
    };
    crate::perf::log_app(&format!(
        "gpu device lost ({}) detected from a failed D3D call",
        reason.label()
    ));
    let _ = note_device_lost(reason);
    let _ = begin_reset();
    true
}

/// `GetDeviceRemovedReason` probe: `None` means the device is healthy.
#[cfg(target_os = "windows")]
pub fn device_removed_reason(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Option<DeviceLostKind> {
    match unsafe { device.GetDeviceRemovedReason() } {
        Ok(()) => None,
        Err(e) => Some(DeviceLostKind::from_hresult(e.code())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_ready_and_usable() {
        let life = GpuLifecycle::new(7);
        assert_eq!(life.state(), GpuLifecycleState::Ready { generation: 7 });
        assert!(life.is_usable(7));
        assert!(!life.is_usable(6));
        assert!(life.accepts_new_work());
        assert_eq!(life.reset_count(), 0);
    }

    #[test]
    fn begin_reset_rejects_new_work_and_keeps_generation() {
        let mut life = GpuLifecycle::new(3);
        assert_eq!(
            life.begin_reset(),
            Ok(GpuLifecycleState::Resetting { generation: 3 })
        );
        assert!(!life.is_usable(3));
        assert!(!life.accepts_new_work());
        assert_eq!(life.reset_count(), 1);
    }

    #[test]
    fn duplicate_begin_reset_is_refused_without_panic() {
        let mut life = GpuLifecycle::new(1);
        assert!(life.begin_reset().is_ok());
        assert_eq!(life.begin_reset(), Err(GpuLifecycleError::AlreadyResetting));
        assert_eq!(life.state(), GpuLifecycleState::Resetting { generation: 1 });
        assert_eq!(life.reset_count(), 1);
    }

    #[test]
    fn device_lost_transitions_ready_to_lost() {
        let mut life = GpuLifecycle::new(2);
        assert_eq!(
            life.device_lost(DeviceLostKind::Reset),
            Ok(GpuLifecycleState::Lost {
                generation: 2,
                reason: DeviceLostKind::Reset,
            })
        );
        assert!(!life.is_usable(2));
        assert!(!life.accepts_new_work());
    }

    #[test]
    fn device_lost_while_resetting_stays_resetting() {
        let mut life = GpuLifecycle::new(4);
        life.begin_reset().unwrap();
        assert_eq!(
            life.device_lost(DeviceLostKind::Removed),
            Ok(GpuLifecycleState::Resetting { generation: 4 })
        );
        assert_eq!(life.reset_count(), 1);
    }

    #[test]
    fn device_recreated_requires_a_newer_generation() {
        let mut life = GpuLifecycle::new(5);
        life.begin_reset().unwrap();
        assert_eq!(
            life.device_recreated(5),
            Err(GpuLifecycleError::StaleGeneration {
                current: 5,
                proposed: 5,
            })
        );
        assert_eq!(
            life.device_recreated(4),
            Err(GpuLifecycleError::StaleGeneration {
                current: 5,
                proposed: 4,
            })
        );
        assert_eq!(life.state(), GpuLifecycleState::Resetting { generation: 5 });
    }

    #[test]
    fn device_recreated_without_loss_is_refused() {
        let mut life = GpuLifecycle::new(1);
        assert_eq!(life.device_recreated(2), Err(GpuLifecycleError::NotLost));
        assert_eq!(life.state(), GpuLifecycleState::Ready { generation: 1 });
    }

    #[test]
    fn recreation_restores_new_work_but_old_resources_stay_stale() {
        let mut life = GpuLifecycle::new(10);
        life.begin_reset().unwrap();
        assert_eq!(
            life.device_recreated(11),
            Ok(GpuLifecycleState::Recreated { generation: 11 })
        );
        assert!(life.accepts_new_work());
        assert!(life.is_usable(11));
        assert!(
            !life.is_usable(10),
            "resources from the old device must be rejected"
        );
        assert_eq!(life.generation(), 11);
    }

    #[test]
    fn lost_then_recreated_also_recovers() {
        let mut life = GpuLifecycle::new(1);
        life.device_lost(DeviceLostKind::Hung).unwrap();
        assert_eq!(
            life.device_recreated(2),
            Ok(GpuLifecycleState::Recreated { generation: 2 })
        );
        assert!(life.is_usable(2));
    }

    #[test]
    fn acknowledge_recovery_returns_to_ready() {
        let mut life = GpuLifecycle::new(1);
        life.begin_reset().unwrap();
        life.device_recreated(2).unwrap();
        assert_eq!(
            life.acknowledge_recovery(),
            Ok(GpuLifecycleState::Ready { generation: 2 })
        );
        assert!(life.is_usable(2));
    }

    #[test]
    fn acknowledge_recovery_outside_recreated_is_refused() {
        let mut life = GpuLifecycle::new(1);
        assert_eq!(
            life.acknowledge_recovery(),
            Err(GpuLifecycleError::NotRecovering)
        );
        life.begin_reset().unwrap();
        assert_eq!(
            life.acknowledge_recovery(),
            Err(GpuLifecycleError::NotRecovering)
        );
    }

    #[test]
    fn observe_generation_self_heals_a_missed_reset_begin() {
        let mut life = GpuLifecycle::new(3);
        assert_eq!(
            life.observe_generation(4),
            GpuLifecycleState::Recreated { generation: 4 }
        );
        assert!(life.is_usable(4));
        assert!(!life.is_usable(3));
        // A duplicate/older observation changes nothing.
        assert_eq!(
            life.observe_generation(4),
            GpuLifecycleState::Recreated { generation: 4 }
        );
        assert_eq!(life.generation(), 4);
    }

    #[test]
    fn freeze_visual_rebuild_decision() {
        let ready = GpuLifecycleState::Ready { generation: 5 };
        assert!(should_rebuild_freeze(ready, None));
        assert!(!should_rebuild_freeze(ready, Some(5)));
        assert!(should_rebuild_freeze(ready, Some(4)));

        let recreated = GpuLifecycleState::Recreated { generation: 6 };
        assert!(should_rebuild_freeze(recreated, Some(5)));
        assert!(!should_rebuild_freeze(recreated, Some(6)));

        assert!(!should_rebuild_freeze(
            GpuLifecycleState::Lost {
                generation: 5,
                reason: DeviceLostKind::Reset,
            },
            Some(5)
        ));
        assert!(!should_rebuild_freeze(
            GpuLifecycleState::Resetting { generation: 5 },
            Some(4)
        ));
    }

    #[test]
    fn a_stale_resource_check_matches_the_state_machine() {
        let mut life = GpuLifecycle::new(2);
        assert!(life.is_usable(2));
        life.begin_reset().unwrap();
        assert!(!life.is_usable(2));
        life.device_recreated(3).unwrap();
        assert!(!life.is_usable(2));
        assert!(life.is_usable(3));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn device_lost_hresult_classification() {
        use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        };
        assert_eq!(
            DeviceLostKind::from_hresult(DXGI_ERROR_DEVICE_REMOVED),
            DeviceLostKind::Removed
        );
        assert_eq!(
            DeviceLostKind::from_hresult(DXGI_ERROR_DEVICE_RESET),
            DeviceLostKind::Reset
        );
        assert_eq!(
            DeviceLostKind::from_hresult(DXGI_ERROR_DEVICE_HUNG),
            DeviceLostKind::Hung
        );
        assert_eq!(
            DeviceLostKind::from_hresult(DXGI_ERROR_DRIVER_INTERNAL_ERROR),
            DeviceLostKind::Other
        );
        assert_eq!(DeviceLostKind::from_hresult(E_FAIL), DeviceLostKind::Other);
        assert_eq!(
            DeviceLostKind::from_hresult(E_INVALIDARG),
            DeviceLostKind::Other
        );
    }
}
