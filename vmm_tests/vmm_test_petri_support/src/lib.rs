// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support code for the `petri`-based tests in `vmm_tests`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use petri::ArtifactHandle;
use petri::AsArtifactHandle;
use petri_artifacts_common::tags::IsOpenhclIgvm;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_vmm_test::artifacts as hvlite_artifacts;

/// See [`ListTestDepsArtifactResolver`](list_test_deps_resolver::ListTestDepsArtifactResolver)
pub mod list_test_deps_resolver {
    use petri::ErasedArtifactHandle;

    /// POC of a "dry run" resolver that simply lists test dependencies, and
    /// panics.
    // FUTURE: Future test infra updates will update / introduce a new resolver
    // that is able to emit structured test dependency manifests (JSON?) that
    // tooling can ingest in order to auto-build/fetch dependencies.
    #[derive(Default)]
    pub struct ListTestDepsArtifactResolver {
        artifacts: parking_lot::Mutex<Vec<ErasedArtifactHandle>>,
    }

    impl petri::TestArtifactResolverBackend for ListTestDepsArtifactResolver {
        fn resolve(&self, id: ErasedArtifactHandle) -> anyhow::Result<std::path::PathBuf> {
            self.artifacts.lock().push(id);
            Ok(std::path::PathBuf::new())
        }

        fn finalize(mut self: Box<Self>) {
            for artifact in self.artifacts.get_mut() {
                println!("{:?}", artifact)
            }
            panic!("done listing test artifacts")
        }
    }
}

/// Helper methods to streamline requesting common sets of dependencies via
/// [`petri::TestArtifactResolver`]
pub trait TestArtifactResolverExt {
    /// Helper method to require standard HvLite test dependencies.
    fn require_hvlite_standard(self, pipette_flavor: Option<(MachineArch, OsFlavor)>) -> Self;

    /// Helper method to require standard OpenHCL test dependencies.
    fn require_openhcl_standard<A>(self, openhcl_image: ArtifactHandle<A>) -> Self
    where
        A: IsOpenhclIgvm;
}

impl TestArtifactResolverExt for petri::TestArtifactResolver {
    /// Helper method to require standard HvLite test dependencies.
    fn require_hvlite_standard(self, pipette_flavor: Option<(MachineArch, OsFlavor)>) -> Self {
        let s = self
            .require(petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE)
            .require(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
        if let Some((arch, flavor)) = pipette_flavor {
            let artifact = match (arch, flavor) {
                (MachineArch::X86_64, OsFlavor::Windows) => {
                    petri_artifacts_common::artifacts::PIPETTE_WINDOWS_X64.erase()
                }
                (MachineArch::X86_64, OsFlavor::Linux) => {
                    petri_artifacts_common::artifacts::PIPETTE_LINUX_X64.erase()
                }
                (MachineArch::Aarch64, OsFlavor::Windows) => {
                    petri_artifacts_common::artifacts::PIPETTE_WINDOWS_AARCH64.erase()
                }
                (MachineArch::Aarch64, OsFlavor::Linux) => {
                    petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64.erase()
                }
                (_, OsFlavor::FreeBsd) => panic!("pipette not supported on FreeBSD guests"),
                (_, OsFlavor::Uefi) => panic!("pipette not supported on UEFI guests"),
            };
            s.require(artifact)
        } else {
            s
        }
    }

    /// Helper method to require standard OpenHCL test dependencies.
    fn require_openhcl_standard<A>(self, openhcl_image: ArtifactHandle<A>) -> Self
    where
        A: IsOpenhclIgvm,
    {
        self.require(openhcl_image.erase())
            .require(hvlite_artifacts::OPENHCL_DUMP_DIRECTORY)
            .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64) // For VTL2 Pipette
    }
}
