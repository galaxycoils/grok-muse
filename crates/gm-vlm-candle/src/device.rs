use candle_core::{DType, Device};
use gm_llm::{VlmDevice, VlmPrecision};
use tracing::{info, warn};

/// Runtime device context managing Apple Silicon Metal acceleration and fallback CPU execution.
#[derive(Debug, Clone)]
pub struct VlmDeviceContext {
    pub device: Device,
    pub active_device_kind: VlmDevice,
    pub requested_device_kind: VlmDevice,
    pub is_accelerated: bool,
}

impl VlmDeviceContext {
    /// Resolve the hardware execution device, prioritizing Apple Silicon Metal acceleration
    /// when requested and gracefully falling back to CPU execution if unavailable.
    pub fn resolve(requested: VlmDevice) -> Self {
        match requested {
            VlmDevice::Metal => {
                #[cfg(feature = "metal")]
                {
                    match Device::new_metal(0) {
                        Ok(metal_dev) => {
                            info!(
                                "Initialized Apple Silicon Metal acceleration for local VLM (Device 0)"
                            );
                            Self {
                                device: metal_dev,
                                active_device_kind: VlmDevice::Metal,
                                requested_device_kind: VlmDevice::Metal,
                                is_accelerated: true,
                            }
                        }
                        Err(err) => {
                            warn!(
                                "Metal initialization failed ({err}); falling back to CPU execution"
                            );
                            Self {
                                device: Device::Cpu,
                                active_device_kind: VlmDevice::Cpu,
                                requested_device_kind: VlmDevice::Metal,
                                is_accelerated: false,
                            }
                        }
                    }
                }
                #[cfg(not(feature = "metal"))]
                {
                    info!(
                        "Metal feature not compiled in current build profile; defaulting to CPU execution"
                    );
                    Self {
                        device: Device::Cpu,
                        active_device_kind: VlmDevice::Cpu,
                        requested_device_kind: VlmDevice::Metal,
                        is_accelerated: false,
                    }
                }
            }
            VlmDevice::Cuda => {
                warn!(
                    "CUDA device requested but not supported on Apple Silicon / macOS; falling back to CPU execution"
                );
                Self {
                    device: Device::Cpu,
                    active_device_kind: VlmDevice::Cpu,
                    requested_device_kind: VlmDevice::Cuda,
                    is_accelerated: false,
                }
            }
            VlmDevice::Cpu => {
                info!("Using CPU execution for local VLM inference");
                Self {
                    device: Device::Cpu,
                    active_device_kind: VlmDevice::Cpu,
                    requested_device_kind: VlmDevice::Cpu,
                    is_accelerated: false,
                }
            }
        }
    }

    /// Map `VlmPrecision` enum to Candle tensor `DType`.
    pub fn candle_dtype(&self, precision: VlmPrecision) -> DType {
        match precision {
            VlmPrecision::Fp16 => {
                // If on CPU and F16 is slow or unsupported for some CPU ops, F32 is safer on CPU
                if matches!(self.active_device_kind, VlmDevice::Cpu) {
                    DType::F32
                } else {
                    DType::F16
                }
            }
            VlmPrecision::Bf16 => {
                if matches!(self.active_device_kind, VlmDevice::Cpu) {
                    DType::F32
                } else {
                    DType::BF16
                }
            }
            VlmPrecision::Q4_0 | VlmPrecision::Q4_K => DType::F32,
        }
    }
}
