<!-- @generated model list, produced out-of-tree from the committed
vmm_core/virt_kvm/src/arch/x86_64/cpu_models.rs table. Do not edit the model
tables below by hand. The prose above the generated marker is hand-written. -->

# Guest CPU models

`--hypervisor kvm:cpu=<model>` presents a named CPU model to the guest. The
backend masks the host CPUID down to the model's feature set, so the guest sees
the intersection of the host's features and the model's: guest features = host
AND model. It also reports the model's vendor and family/model/stepping. The
masking never adds a feature the host lacks; it only takes features away, which
is what a defined feature baseline (or live migration to a host that runs the
same model) needs.

`cpu=host` or `cpu=max` (the default when `cpu=` is omitted) pass the host
features through unchanged. An unknown model name is rejected at startup.

The model set matches the named Intel and AMD generations in wide use, plus the
versioned variants (for example `Haswell-v3` or `Cascadelake-Server-v5`) and the
x86-64 psABI micro-architecture levels. The exact feature bits each model
defines live in the `X86_CPU_MODELS` table in
`vmm_core/virt_kvm/src/arch/x86_64/cpu_models.rs`.

## x86-64 micro-architecture levels

The x86-64 psABI defines cumulative micro-architecture levels. Each level is a
fixed feature baseline rather than a specific part, useful when you want a
portable floor without naming a generation.

* `x86-64-v2`
* `x86-64-v2-AES`
* `x86-64-v3`
* `x86-64-v4`

## Named models by vendor

### Intel

* `486`
* `486-v1`
* `Broadwell`
* `Broadwell-IBRS`
* `Broadwell-noTSX`
* `Broadwell-noTSX-IBRS`
* `Broadwell-v1`
* `Broadwell-v2`
* `Broadwell-v3`
* `Broadwell-v4`
* `Cascadelake-Server`
* `Cascadelake-Server-noTSX`
* `Cascadelake-Server-v1`
* `Cascadelake-Server-v2`
* `Cascadelake-Server-v3`
* `Cascadelake-Server-v4`
* `Cascadelake-Server-v5`
* `ClearwaterForest`
* `ClearwaterForest-v1`
* `ClearwaterForest-v2`
* `ClearwaterForest-v3`
* `Conroe`
* `Conroe-v1`
* `Cooperlake`
* `Cooperlake-v1`
* `Cooperlake-v2`
* `core2duo`
* `core2duo-v1`
* `coreduo`
* `coreduo-v1`
* `Denverton`
* `Denverton-v1`
* `Denverton-v2`
* `Denverton-v3`
* `DiamondRapids`
* `DiamondRapids-v1`
* `GraniteRapids`
* `GraniteRapids-v1`
* `GraniteRapids-v2`
* `GraniteRapids-v3`
* `GraniteRapids-v4`
* `GraniteRapids-v5`
* `Haswell`
* `Haswell-IBRS`
* `Haswell-noTSX`
* `Haswell-noTSX-IBRS`
* `Haswell-v1`
* `Haswell-v2`
* `Haswell-v3`
* `Haswell-v4`
* `Icelake-Server`
* `Icelake-Server-noTSX`
* `Icelake-Server-v1`
* `Icelake-Server-v2`
* `Icelake-Server-v3`
* `Icelake-Server-v4`
* `Icelake-Server-v5`
* `Icelake-Server-v6`
* `Icelake-Server-v7`
* `IvyBridge`
* `IvyBridge-IBRS`
* `IvyBridge-v1`
* `IvyBridge-v2`
* `KnightsMill`
* `KnightsMill-v1`
* `kvm32`
* `kvm32-v1`
* `kvm64`
* `kvm64-v1`
* `n270`
* `n270-v1`
* `Nehalem`
* `Nehalem-IBRS`
* `Nehalem-v1`
* `Nehalem-v2`
* `Penryn`
* `Penryn-v1`
* `pentium`
* `pentium-v1`
* `pentium2`
* `pentium2-v1`
* `pentium3`
* `pentium3-v1`
* `SandyBridge`
* `SandyBridge-IBRS`
* `SandyBridge-v1`
* `SandyBridge-v2`
* `SapphireRapids`
* `SapphireRapids-v1`
* `SapphireRapids-v2`
* `SapphireRapids-v3`
* `SapphireRapids-v4`
* `SapphireRapids-v5`
* `SapphireRapids-v6`
* `SierraForest`
* `SierraForest-v1`
* `SierraForest-v2`
* `SierraForest-v3`
* `SierraForest-v4`
* `SierraForest-v5`
* `Skylake-Client`
* `Skylake-Client-IBRS`
* `Skylake-Client-noTSX-IBRS`
* `Skylake-Client-v1`
* `Skylake-Client-v2`
* `Skylake-Client-v3`
* `Skylake-Client-v4`
* `Skylake-Server`
* `Skylake-Server-IBRS`
* `Skylake-Server-noTSX-IBRS`
* `Skylake-Server-v1`
* `Skylake-Server-v2`
* `Skylake-Server-v3`
* `Skylake-Server-v4`
* `Skylake-Server-v5`
* `Snowridge`
* `Snowridge-v1`
* `Snowridge-v2`
* `Snowridge-v3`
* `Snowridge-v4`
* `Westmere`
* `Westmere-IBRS`
* `Westmere-v1`
* `Westmere-v2`

### AMD

* `athlon`
* `athlon-v1`
* `EPYC`
* `EPYC-Genoa`
* `EPYC-Genoa-v1`
* `EPYC-Genoa-v2`
* `EPYC-IBPB`
* `EPYC-Milan`
* `EPYC-Milan-v1`
* `EPYC-Milan-v2`
* `EPYC-Milan-v3`
* `EPYC-Rome`
* `EPYC-Rome-v1`
* `EPYC-Rome-v2`
* `EPYC-Rome-v3`
* `EPYC-Rome-v4`
* `EPYC-Rome-v5`
* `EPYC-Turin`
* `EPYC-Turin-v1`
* `EPYC-v1`
* `EPYC-v2`
* `EPYC-v3`
* `EPYC-v4`
* `EPYC-v5`
* `Opteron_G1`
* `Opteron_G1-v1`
* `Opteron_G2`
* `Opteron_G2-v1`
* `Opteron_G3`
* `Opteron_G3-v1`
* `Opteron_G4`
* `Opteron_G4-v1`
* `Opteron_G5`
* `Opteron_G5-v1`
* `phenom`
* `phenom-v1`

### Hygon

* `Dhyana`
* `Dhyana-v1`
* `Dhyana-v2`

### Centaur / Zhaoxin

* `YongFeng`
* `YongFeng-v1`
* `YongFeng-v2`
* `YongFeng-v3`
