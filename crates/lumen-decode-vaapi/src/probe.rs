//! The decode capability probe.
//!
//! The probe opens the preferred VA-API device and asks the driver what it
//! can do: which profiles it offers and which entry points exist for each of
//! them. The result drives the backend fallback chain of the plan:
//!
//! ```text
//!   VA-API  ->  Vulkan Video (hardware)  ->  Vulkan Video (software)  ->  CPU
//! ```
//!
//! Only the VA-API rung can be answered from this crate: the Vulkan Video
//! decode crate does not exist in the workspace yet, so the Vulkan rungs are
//! reported as unavailable rather than guessed. The chain itself is data, so
//! the selection logic is testable without the hardware.

use lumen_ffi_va::VaDisplay;
use lumen_ffi_va::sys;

/// A video profile the decoder may support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Profile {
    /// H.264 Baseline.
    H264Baseline,
    /// H.264 Constrained Baseline.
    H264ConstrainedBaseline,
    /// H.264 Main.
    H264Main,
    /// H.264 High.
    H264High,
    /// H.264 High 4:2:2.
    H264High422,
    /// HEVC Main.
    HevcMain,
    /// HEVC Main 10.
    HevcMain10,
    /// AV1 Main.
    Av1Main,
    /// A profile this crate does not name.
    Other(i32),
}

impl Profile {
    /// Map a libva profile onto a named profile.
    #[must_use]
    pub fn from_va(profile: sys::VAProfile) -> Self {
        match profile {
            sys::VAProfileH264Baseline => Self::H264Baseline,
            sys::VAProfileH264ConstrainedBaseline => Self::H264ConstrainedBaseline,
            sys::VAProfileH264Main => Self::H264Main,
            sys::VAProfileH264High => Self::H264High,
            sys::VAProfileH264High422 => Self::H264High422,
            sys::VAProfileHEVCMain => Self::HevcMain,
            sys::VAProfileHEVCMain10 => Self::HevcMain10,
            sys::VAProfileAV1Profile0 => Self::Av1Main,
            other => Self::Other(other),
        }
    }

    /// The libva profile this name stands for.
    #[must_use]
    pub fn to_va(self) -> sys::VAProfile {
        match self {
            Self::H264Baseline => sys::VAProfileH264Baseline,
            Self::H264ConstrainedBaseline => sys::VAProfileH264ConstrainedBaseline,
            Self::H264Main => sys::VAProfileH264Main,
            Self::H264High => sys::VAProfileH264High,
            Self::H264High422 => sys::VAProfileH264High422,
            Self::HevcMain => sys::VAProfileHEVCMain,
            Self::HevcMain10 => sys::VAProfileHEVCMain10,
            Self::Av1Main => sys::VAProfileAV1Profile0,
            Self::Other(value) => value,
        }
    }

    /// A short name for reports and logs.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::H264Baseline => "h264-baseline".to_string(),
            Self::H264ConstrainedBaseline => "h264-constrained-baseline".to_string(),
            Self::H264Main => "h264-main".to_string(),
            Self::H264High => "h264-high".to_string(),
            Self::H264High422 => "h264-high-422".to_string(),
            Self::HevcMain => "hevc-main".to_string(),
            Self::HevcMain10 => "hevc-main10".to_string(),
            Self::Av1Main => "av1-main".to_string(),
            Self::Other(value) => format!("other-{value}"),
        }
    }

    /// Whether the profile carries more than eight bits per sample.
    #[must_use]
    pub fn is_ten_bit(self) -> bool {
        matches!(self, Self::HevcMain10 | Self::H264High422)
    }

    /// Whether the profile is an H.264 profile.
    #[must_use]
    pub fn is_h264(self) -> bool {
        matches!(
            self,
            Self::H264Baseline
                | Self::H264ConstrainedBaseline
                | Self::H264Main
                | Self::H264High
                | Self::H264High422
        )
    }
}

/// A libva entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryPoint {
    /// Video decode (`VAEntrypointVLD`).
    Decode,
    /// Encoded picture (`VAEntrypointEncPicture`).
    EncodePicture,
    /// Encoded slice (`VAEntrypointEncSlice`).
    EncodeSlice,
    /// An entry point this crate does not name.
    Other(u32),
}

impl EntryPoint {
    /// Map a libva entry point onto a named one.
    #[must_use]
    pub fn from_va(entrypoint: sys::VAEntrypoint) -> Self {
        match entrypoint {
            sys::VAEntrypointVLD => Self::Decode,
            sys::VAEntrypointEncPicture => Self::EncodePicture,
            sys::VAEntrypointEncSlice => Self::EncodeSlice,
            other => Self::Other(other),
        }
    }

    /// The libva entry point this name stands for.
    #[must_use]
    pub fn to_va(self) -> sys::VAEntrypoint {
        match self {
            Self::Decode => sys::VAEntrypointVLD,
            Self::EncodePicture => sys::VAEntrypointEncPicture,
            Self::EncodeSlice => sys::VAEntrypointEncSlice,
            Self::Other(value) => value,
        }
    }

    /// A short name for reports and logs.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Decode => "decode".to_string(),
            Self::EncodePicture => "encode-picture".to_string(),
            Self::EncodeSlice => "encode-slice".to_string(),
            Self::Other(value) => format!("other-{value}"),
        }
    }
}

/// What the driver offers for one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSupport {
    /// The profile.
    pub profile: Profile,
    /// The entry points the driver offers for it.
    pub entrypoints: Vec<EntryPoint>,
}

impl ProfileSupport {
    /// Whether the driver offers the decode entry point for this profile.
    #[must_use]
    pub fn can_decode(&self) -> bool {
        self.entrypoints.contains(&EntryPoint::Decode)
    }
}

/// A decode backend the client can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodeBackend {
    /// VA-API hardware decode.
    VaApi,
    /// Vulkan Video decode on a hardware queue.
    VulkanHardware,
    /// Vulkan Video decode on the software rasteriser (lavapipe).
    VulkanSoftware,
    /// A CPU decoder.
    Cpu,
}

impl DecodeBackend {
    /// A short name for reports and logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::VaApi => "va-api",
            Self::VulkanHardware => "vulkan-hardware",
            Self::VulkanSoftware => "vulkan-software",
            Self::Cpu => "cpu",
        }
    }
}

/// Which backends this machine can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendAvailability {
    /// A VA-API device that can decode the wanted profile.
    pub va_api: bool,
    /// A Vulkan Video hardware queue. No Vulkan Video decode crate exists in
    /// the workspace yet, so this is never set from a probe.
    pub vulkan_hardware: bool,
    /// A Vulkan Video software queue. Same reason: never set from a probe.
    pub vulkan_software: bool,
    /// A CPU decoder.
    pub cpu: bool,
}

impl BackendAvailability {
    /// The chain of the plan, in order.
    #[must_use]
    pub fn chain(&self) -> Vec<DecodeBackend> {
        let mut chain = Vec::new();
        if self.va_api {
            chain.push(DecodeBackend::VaApi);
        }
        if self.vulkan_hardware {
            chain.push(DecodeBackend::VulkanHardware);
        }
        if self.vulkan_software {
            chain.push(DecodeBackend::VulkanSoftware);
        }
        if self.cpu {
            chain.push(DecodeBackend::Cpu);
        }
        chain
    }
}

/// What the probe found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeProbe {
    /// The VA-API version the driver negotiated.
    pub va_version: (i32, i32),
    /// The driver's vendor string.
    pub vendor: String,
    /// The DRM node the probe opened.
    pub device: String,
    /// The profiles the driver offers, with their entry points.
    pub profiles: Vec<ProfileSupport>,
}

impl DecodeProbe {
    /// Probe the preferred VA-API device.
    ///
    /// The device is `LUMEN_VA_DEVICE` if it is set, then `renderD129`, then
    /// `renderD128`, then any other render node. The probe fails when none of
    /// them yields a display.
    ///
    /// # Errors
    /// Returns a description when no VA-API device can be opened or the
    /// driver refuses the profile query.
    pub fn probe() -> Result<Self, String> {
        let device = lumen_ffi_va::knobs::va_device()
            .map_or_else(|| "auto".to_string(), |path| path.display().to_string());
        Self::probe_device(&device)
    }

    /// Probe one named DRM node.
    ///
    /// # Errors
    /// Returns a description when the node cannot be opened or the driver
    /// refuses the profile query.
    pub fn probe_device(node: &str) -> Result<Self, String> {
        let display = if node == "auto" {
            VaDisplay::open_preferred().map_err(|e| e.to_string())?
        } else {
            VaDisplay::open(std::path::Path::new(node)).map_err(|e| e.to_string())?
        };
        let va_profiles = display.profiles().map_err(|e| e.to_string())?;
        let mut profiles = Vec::with_capacity(va_profiles.len());
        for va_profile in va_profiles {
            let entrypoints = display
                .entrypoints(va_profile)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(EntryPoint::from_va)
                .collect();
            profiles.push(ProfileSupport {
                profile: Profile::from_va(va_profile),
                entrypoints,
            });
        }
        Ok(Self {
            va_version: display.version(),
            vendor: display.vendor(),
            device: node.to_string(),
            profiles,
        })
    }

    /// Whether the driver offers `profile` at all.
    #[must_use]
    pub fn supports(&self, profile: Profile) -> bool {
        self.profiles.iter().any(|p| p.profile == profile)
    }

    /// Whether the driver can decode `profile`.
    #[must_use]
    pub fn can_decode(&self, profile: Profile) -> bool {
        self.profiles
            .iter()
            .any(|p| p.profile == profile && p.can_decode())
    }

    /// The highest H.264 profile the driver can decode.
    ///
    /// The order is the H.264 profile hierarchy: Constrained Baseline, then
    /// Baseline, Main, High, then High 4:2:2.
    #[must_use]
    pub fn highest_h264(&self) -> Option<Profile> {
        const LADDER: [Profile; 5] = [
            Profile::H264ConstrainedBaseline,
            Profile::H264Baseline,
            Profile::H264Main,
            Profile::H264High,
            Profile::H264High422,
        ];
        LADDER.into_iter().rfind(|p| self.can_decode(*p))
    }

    /// The ten-bit profiles the driver can decode.
    #[must_use]
    pub fn ten_bit_profiles(&self) -> Vec<Profile> {
        self.profiles
            .iter()
            .filter(|p| p.profile.is_ten_bit() && p.can_decode())
            .map(|p| p.profile)
            .collect()
    }

    /// The backends this machine can offer for `profile`.
    ///
    /// The VA-API rung is answered from the probe. The Vulkan rungs are
    /// answered from `vulkan`, because this crate cannot open a Vulkan
    /// device. The CPU rung is answered from `cpu`.
    #[must_use]
    pub fn availability(
        &self,
        profile: Profile,
        vulkan: &BackendAvailability,
        cpu: bool,
    ) -> BackendAvailability {
        BackendAvailability {
            va_api: self.can_decode(profile),
            vulkan_hardware: vulkan.vulkan_hardware,
            vulkan_software: vulkan.vulkan_software,
            cpu,
        }
    }

    /// The fallback chain for `profile`, in the order of the plan.
    #[must_use]
    pub fn fallback_chain(
        &self,
        profile: Profile,
        vulkan: &BackendAvailability,
        cpu: bool,
    ) -> Vec<DecodeBackend> {
        self.availability(profile, vulkan, cpu).chain()
    }
}

/// Choose the backend for `profile`.
///
/// The first backend of the chain that is available wins. The chain is the
/// plan's order: VA-API, Vulkan Video hardware, Vulkan Video software, CPU.
///
/// # Errors
/// Returns a description when no backend can decode the profile.
pub fn select_backend(
    probe: &DecodeProbe,
    profile: Profile,
    vulkan: &BackendAvailability,
    cpu: bool,
) -> Result<DecodeBackend, String> {
    let chain = probe.fallback_chain(profile, vulkan, cpu);
    chain
        .first()
        .copied()
        .ok_or_else(|| format!("no decode backend can decode {}", profile.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(profiles: &[ProfileSupport]) -> DecodeProbe {
        DecodeProbe {
            va_version: (2, 21),
            vendor: "test vendor".to_string(),
            device: "test".to_string(),
            profiles: profiles.to_vec(),
        }
    }

    fn support(profile: Profile, entrypoints: &[EntryPoint]) -> ProfileSupport {
        ProfileSupport {
            profile,
            entrypoints: entrypoints.to_vec(),
        }
    }

    fn no_vulkan() -> BackendAvailability {
        BackendAvailability {
            va_api: false,
            vulkan_hardware: false,
            vulkan_software: false,
            cpu: false,
        }
    }

    #[test]
    fn the_profile_names_round_trip_through_libva() {
        let names = [
            Profile::H264Baseline,
            Profile::H264ConstrainedBaseline,
            Profile::H264Main,
            Profile::H264High,
            Profile::H264High422,
            Profile::HevcMain,
            Profile::HevcMain10,
            Profile::Av1Main,
        ];
        for profile in names {
            assert_eq!(Profile::from_va(profile.to_va()), profile);
        }
        assert_eq!(Profile::from_va(sys::VAProfileMPEG2Main), Profile::Other(1));
        assert_eq!(Profile::Other(1).to_va(), sys::VAProfileMPEG2Main);
    }

    #[test]
    fn the_entry_point_names_round_trip_through_libva() {
        for entrypoint in [
            EntryPoint::Decode,
            EntryPoint::EncodePicture,
            EntryPoint::EncodeSlice,
        ] {
            assert_eq!(EntryPoint::from_va(entrypoint.to_va()), entrypoint);
        }
        assert_eq!(
            EntryPoint::from_va(sys::VAEntrypointVideoProc),
            EntryPoint::Other(10)
        );
    }

    #[test]
    fn a_profile_without_the_decode_entry_point_is_not_decodable() {
        let probe = probe_with(&[support(Profile::H264Main, &[EntryPoint::EncodePicture])]);
        assert!(probe.supports(Profile::H264Main));
        assert!(!probe.can_decode(Profile::H264Main));
        assert_eq!(probe.highest_h264(), None);
    }

    #[test]
    fn the_highest_h264_profile_follows_the_hierarchy() {
        let probe = probe_with(&[
            support(Profile::H264ConstrainedBaseline, &[EntryPoint::Decode]),
            support(Profile::H264Main, &[EntryPoint::Decode]),
        ]);
        assert_eq!(probe.highest_h264(), Some(Profile::H264Main));

        let probe = probe_with(&[
            support(Profile::H264ConstrainedBaseline, &[EntryPoint::Decode]),
            support(Profile::H264Main, &[EntryPoint::Decode]),
            support(Profile::H264High, &[EntryPoint::Decode]),
        ]);
        assert_eq!(probe.highest_h264(), Some(Profile::H264High));
    }

    #[test]
    fn the_fallback_chain_follows_the_order_of_the_plan() {
        let probe = probe_with(&[support(Profile::H264High, &[EntryPoint::Decode])]);
        let chain = probe.fallback_chain(
            Profile::H264High,
            &BackendAvailability {
                va_api: false,
                vulkan_hardware: true,
                vulkan_software: true,
                cpu: true,
            },
            true,
        );
        assert_eq!(
            chain,
            vec![
                DecodeBackend::VaApi,
                DecodeBackend::VulkanHardware,
                DecodeBackend::VulkanSoftware,
                DecodeBackend::Cpu
            ]
        );
    }

    #[test]
    fn the_selection_takes_the_first_available_backend() {
        let probe = probe_with(&[support(Profile::H264High, &[EntryPoint::Decode])]);
        let selected = select_backend(&probe, Profile::H264High, &no_vulkan(), true).unwrap();
        assert_eq!(selected, DecodeBackend::VaApi);

        // A profile the driver cannot decode falls through to the Vulkan rungs.
        let vulkan = BackendAvailability {
            va_api: false,
            vulkan_hardware: true,
            vulkan_software: true,
            cpu: true,
        };
        let selected = select_backend(&probe, Profile::Av1Main, &vulkan, true).unwrap();
        assert_eq!(selected, DecodeBackend::VulkanHardware);

        // With no Vulkan, the software rung is next.
        let vulkan = BackendAvailability {
            va_api: false,
            vulkan_hardware: false,
            vulkan_software: true,
            cpu: true,
        };
        let selected = select_backend(&probe, Profile::Av1Main, &vulkan, true).unwrap();
        assert_eq!(selected, DecodeBackend::VulkanSoftware);

        // With nothing but the CPU, the CPU rung is the last resort.
        let selected = select_backend(&probe, Profile::Av1Main, &no_vulkan(), true).unwrap();
        assert_eq!(selected, DecodeBackend::Cpu);
    }

    #[test]
    fn a_profile_no_backend_can_decode_is_an_error() {
        let probe = probe_with(&[]);
        let error = select_backend(&probe, Profile::Av1Main, &no_vulkan(), false)
            .expect_err("no backend is available");
        assert!(error.contains("av1-main"), "{error}");
    }

    #[test]
    fn ten_bit_profiles_are_listed() {
        let probe = probe_with(&[
            support(Profile::HevcMain, &[EntryPoint::Decode]),
            support(Profile::HevcMain10, &[EntryPoint::Decode]),
            support(Profile::H264High422, &[EntryPoint::Decode]),
        ]);
        assert_eq!(
            probe.ten_bit_profiles(),
            vec![Profile::HevcMain10, Profile::H264High422]
        );
        assert!(!Profile::HevcMain.is_ten_bit());
        assert!(Profile::H264Main.is_h264());
        assert!(!Profile::HevcMain.is_h264());
    }

    #[test]
    fn the_probe_reports_the_real_device() {
        let probe = match DecodeProbe::probe() {
            Ok(probe) => probe,
            Err(reason) => {
                eprintln!("SKIP the_probe_reports_the_real_device: {reason}");
                return;
            }
        };
        eprintln!(
            "VA-API {} on {} ({}): {:?}",
            probe.va_version.0,
            probe.device,
            probe.vendor,
            probe
                .profiles
                .iter()
                .map(|p| (p.profile.name(), p.entrypoints.len()))
                .collect::<Vec<_>>()
        );
        // A driver that cannot decode H.264 would be useless to this project.
        assert!(
            probe.can_decode(Profile::H264Main) || probe.can_decode(Profile::H264High),
            "the device does not decode H.264: {:?}",
            probe.profiles
        );
    }
}
