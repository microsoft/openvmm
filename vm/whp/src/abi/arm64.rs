// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

use super::WHV_PROCESSOR_FEATURES;
use super::WHV_REGISTER_NAME;
use super::WHV_RUN_VP_EXIT_REASON;

// AArch64 System Register Descriptions: General-purpose registers
pub const WHvArm64RegisterX0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020000);
pub const WHvArm64RegisterX1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020001);
pub const WHvArm64RegisterX2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020002);
pub const WHvArm64RegisterX3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020003);
pub const WHvArm64RegisterX4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020004);
pub const WHvArm64RegisterX5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020005);
pub const WHvArm64RegisterX6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020006);
pub const WHvArm64RegisterX7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020007);
pub const WHvArm64RegisterX8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020008);
pub const WHvArm64RegisterX9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020009);
pub const WHvArm64RegisterX10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000A);
pub const WHvArm64RegisterX11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000B);
pub const WHvArm64RegisterX12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000C);
pub const WHvArm64RegisterX13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000D);
pub const WHvArm64RegisterX14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000E);
pub const WHvArm64RegisterX15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002000F);
pub const WHvArm64RegisterX16: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020010);
pub const WHvArm64RegisterX17: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020011);
pub const WHvArm64RegisterX18: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020012);
pub const WHvArm64RegisterX19: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020013);
pub const WHvArm64RegisterX20: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020014);
pub const WHvArm64RegisterX21: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020015);
pub const WHvArm64RegisterX22: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020016);
pub const WHvArm64RegisterX23: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020017);
pub const WHvArm64RegisterX24: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020018);
pub const WHvArm64RegisterX25: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020019);
pub const WHvArm64RegisterX26: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001A);
pub const WHvArm64RegisterX27: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001B);
pub const WHvArm64RegisterX28: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001C);
pub const WHvArm64RegisterFp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001D);
pub const WHvArm64RegisterLr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001E);
pub const WHvArm64RegisterPc: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020022);
pub const WHvArm64RegisterXzr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002FFFE);

// AArch64 System Register Descriptions: Floating-point registers
pub const WHvArm64RegisterQ0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030000);
pub const WHvArm64RegisterQ1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030001);
pub const WHvArm64RegisterQ2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030002);
pub const WHvArm64RegisterQ3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030003);
pub const WHvArm64RegisterQ4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030004);
pub const WHvArm64RegisterQ5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030005);
pub const WHvArm64RegisterQ6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030006);
pub const WHvArm64RegisterQ7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030007);
pub const WHvArm64RegisterQ8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030008);
pub const WHvArm64RegisterQ9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030009);
pub const WHvArm64RegisterQ10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000A);
pub const WHvArm64RegisterQ11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000B);
pub const WHvArm64RegisterQ12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000C);
pub const WHvArm64RegisterQ13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000D);
pub const WHvArm64RegisterQ14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000E);
pub const WHvArm64RegisterQ15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003000F);
pub const WHvArm64RegisterQ16: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030010);
pub const WHvArm64RegisterQ17: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030011);
pub const WHvArm64RegisterQ18: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030012);
pub const WHvArm64RegisterQ19: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030013);
pub const WHvArm64RegisterQ20: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030014);
pub const WHvArm64RegisterQ21: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030015);
pub const WHvArm64RegisterQ22: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030016);
pub const WHvArm64RegisterQ23: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030017);
pub const WHvArm64RegisterQ24: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030018);
pub const WHvArm64RegisterQ25: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00030019);
pub const WHvArm64RegisterQ26: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001A);
pub const WHvArm64RegisterQ27: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001B);
pub const WHvArm64RegisterQ28: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001C);
pub const WHvArm64RegisterQ29: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001D);
pub const WHvArm64RegisterQ30: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001E);
pub const WHvArm64RegisterQ31: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0003001F);

// AArch64 System Register Descriptions: Special-purpose registers
pub const WHvArm64RegisterCurrentEl: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021003);
pub const WHvArm64RegisterDaif: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021004);
pub const WHvArm64RegisterDit: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021005);
pub const WHvArm64RegisterPstate: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020023);
pub const WHvArm64RegisterElrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040015);
pub const WHvArm64RegisterFpcr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040012);
pub const WHvArm64RegisterFpsr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040013);
pub const WHvArm64RegisterNzcv: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021006);
pub const WHvArm64RegisterPan: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021007);
pub const WHvArm64RegisterSp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002001F);
pub const WHvArm64RegisterSpEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020020);
pub const WHvArm64RegisterSpEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00020021);
pub const WHvArm64RegisterSpSel: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021008);
pub const WHvArm64RegisterSpsrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040014);
pub const WHvArm64RegisterSsbs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00021009);
pub const WHvArm64RegisterTco: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002100A);
pub const WHvArm64RegisterUao: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002100B);

// AArch64 System Register Descriptions: ID Registers
pub const WHvArm64RegisterIdAa64Dfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022028);
pub const WHvArm64RegisterIdAa64Dfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022029);
pub const WHvArm64RegisterIdAa64Isar0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022030);
pub const WHvArm64RegisterIdAa64Isar1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022031);
pub const WHvArm64RegisterIdAa64Isar2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022032);
pub const WHvArm64RegisterIdAa64Mmfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022038);
pub const WHvArm64RegisterIdAa64Mmfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022039);
pub const WHvArm64RegisterIdAa64Mmfr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002203A);
pub const WHvArm64RegisterIdAa64Mmfr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002203B);
pub const WHvArm64RegisterIdAa64Mmfr4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002203C);
pub const WHvArm64RegisterIdAa64Pfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022020);
pub const WHvArm64RegisterIdAa64Pfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022021);
pub const WHvArm64RegisterIdAa64Pfr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022022);
pub const WHvArm64RegisterIdAa64Smfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022025);
pub const WHvArm64RegisterIdAa64Zfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022024);
pub const WHvArm64RegisterIdDfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002200A);
pub const WHvArm64RegisterIdIsar0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022010);
pub const WHvArm64RegisterIdIsar1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022011);
pub const WHvArm64RegisterIdIsar2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022012);
pub const WHvArm64RegisterIdIsar3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022013);
pub const WHvArm64RegisterIdIsar4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022014);
pub const WHvArm64RegisterIdIsar5El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022015);
pub const WHvArm64RegisterIdMmfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022016);
pub const WHvArm64RegisterIdMmfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022017);
pub const WHvArm64RegisterIdMmfr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022018);
pub const WHvArm64RegisterIdMmfr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00022019);
pub const WHvArm64RegisterIdPfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002201A);
pub const WHvArm64RegisterIdPfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002201B);
pub const WHvArm64RegisterIdPfr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0002201C);

// AArch64 System Register Descriptions: General system control registers
pub const WHvArm64RegisterApdAKeyHiEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040026);
pub const WHvArm64RegisterApdAKeyLoEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040027);
pub const WHvArm64RegisterApdBKeyHiEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040028);
pub const WHvArm64RegisterApdBKeyLoEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040029);
pub const WHvArm64RegisterApgAKeyHiEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002A);
pub const WHvArm64RegisterApgAKeyLoEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002B);
pub const WHvArm64RegisterApiAKeyHiEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002C);
pub const WHvArm64RegisterApiAKeyLoEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002D);
pub const WHvArm64RegisterApiBKeyHiEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002E);
pub const WHvArm64RegisterApiBKeyLoEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004002F);
pub const WHvArm64RegisterCcsidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040030);
pub const WHvArm64RegisterCcsidr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040031);
pub const WHvArm64RegisterClidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040032);
pub const WHvArm64RegisterContextidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004000D);
pub const WHvArm64RegisterCpacrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040004);
pub const WHvArm64RegisterCsselrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040035);
pub const WHvArm64RegisterCtrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040036);
pub const WHvArm64RegisterDczidEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040038);
pub const WHvArm64RegisterEsrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040008);
pub const WHvArm64RegisterFarEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040009);
pub const WHvArm64RegisterIsrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004004A);
pub const WHvArm64RegisterMairEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004000B);
pub const WHvArm64RegisterMidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040051);
pub const WHvArm64RegisterMpidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040001);
pub const WHvArm64RegisterMvfr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040052);
pub const WHvArm64RegisterMvfr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040053);
pub const WHvArm64RegisterMvfr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040054);
pub const WHvArm64RegisterParEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004000A);
pub const WHvArm64RegisterRevidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040055);
pub const WHvArm64RegisterRgsrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040056);
pub const WHvArm64RegisterRndr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040057);
pub const WHvArm64RegisterRndrrs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040058);
pub const WHvArm64RegisterSctlrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040002);
pub const WHvArm64RegisterTcrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040007);
pub const WHvArm64RegisterTpidrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040011);
pub const WHvArm64RegisterTpidrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004000E);
pub const WHvArm64RegisterTpidrroEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040010);
pub const WHvArm64RegisterTtbr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040005);
pub const WHvArm64RegisterTtbr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00040006);
pub const WHvArm64RegisterVbarEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0004000C);

// AArch64 System Register Descriptions: Debug Registers
pub const WHvArm64RegisterDbgbcr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050000);
pub const WHvArm64RegisterDbgbcr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050001);
pub const WHvArm64RegisterDbgbcr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050002);
pub const WHvArm64RegisterDbgbcr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050003);
pub const WHvArm64RegisterDbgbcr4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050004);
pub const WHvArm64RegisterDbgbcr5El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050005);
pub const WHvArm64RegisterDbgbcr6El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050006);
pub const WHvArm64RegisterDbgbcr7El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050007);
pub const WHvArm64RegisterDbgbcr8El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050008);
pub const WHvArm64RegisterDbgbcr9El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050009);
pub const WHvArm64RegisterDbgbcr10El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000A);
pub const WHvArm64RegisterDbgbcr11El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000B);
pub const WHvArm64RegisterDbgbcr12El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000C);
pub const WHvArm64RegisterDbgbcr13El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000D);
pub const WHvArm64RegisterDbgbcr14El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000E);
pub const WHvArm64RegisterDbgbcr15El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005000F);
pub const WHvArm64RegisterDbgbvr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050020);
pub const WHvArm64RegisterDbgbvr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050021);
pub const WHvArm64RegisterDbgbvr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050022);
pub const WHvArm64RegisterDbgbvr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050023);
pub const WHvArm64RegisterDbgbvr4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050024);
pub const WHvArm64RegisterDbgbvr5El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050025);
pub const WHvArm64RegisterDbgbvr6El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050026);
pub const WHvArm64RegisterDbgbvr7El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050027);
pub const WHvArm64RegisterDbgbvr8El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050028);
pub const WHvArm64RegisterDbgbvr9El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050029);
pub const WHvArm64RegisterDbgbvr10El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002A);
pub const WHvArm64RegisterDbgbvr11El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002B);
pub const WHvArm64RegisterDbgbvr12El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002C);
pub const WHvArm64RegisterDbgbvr13El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002D);
pub const WHvArm64RegisterDbgbvr14El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002E);
pub const WHvArm64RegisterDbgbvr15El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005002F);
pub const WHvArm64RegisterDbgprcrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050045);
pub const WHvArm64RegisterDbgwcr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050010);
pub const WHvArm64RegisterDbgwcr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050011);
pub const WHvArm64RegisterDbgwcr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050012);
pub const WHvArm64RegisterDbgwcr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050013);
pub const WHvArm64RegisterDbgwcr4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050014);
pub const WHvArm64RegisterDbgwcr5El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050015);
pub const WHvArm64RegisterDbgwcr6El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050016);
pub const WHvArm64RegisterDbgwcr7El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050017);
pub const WHvArm64RegisterDbgwcr8El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050018);
pub const WHvArm64RegisterDbgwcr9El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050019);
pub const WHvArm64RegisterDbgwcr10El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001A);
pub const WHvArm64RegisterDbgwcr11El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001B);
pub const WHvArm64RegisterDbgwcr12El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001C);
pub const WHvArm64RegisterDbgwcr13El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001D);
pub const WHvArm64RegisterDbgwcr14El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001E);
pub const WHvArm64RegisterDbgwcr15El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005001F);
pub const WHvArm64RegisterDbgwvr0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050030);
pub const WHvArm64RegisterDbgwvr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050031);
pub const WHvArm64RegisterDbgwvr2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050032);
pub const WHvArm64RegisterDbgwvr3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050033);
pub const WHvArm64RegisterDbgwvr4El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050034);
pub const WHvArm64RegisterDbgwvr5El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050035);
pub const WHvArm64RegisterDbgwvr6El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050036);
pub const WHvArm64RegisterDbgwvr7El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050037);
pub const WHvArm64RegisterDbgwvr8El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050038);
pub const WHvArm64RegisterDbgwvr9El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050039);
pub const WHvArm64RegisterDbgwvr10El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003A);
pub const WHvArm64RegisterDbgwvr11El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003B);
pub const WHvArm64RegisterDbgwvr12El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003C);
pub const WHvArm64RegisterDbgwvr13El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003D);
pub const WHvArm64RegisterDbgwvr14El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003E);
pub const WHvArm64RegisterDbgwvr15El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005003F);
pub const WHvArm64RegisterMdrarEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005004C);
pub const WHvArm64RegisterMdscrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005004D);
pub const WHvArm64RegisterOsdlrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005004E);
pub const WHvArm64RegisterOslarEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050052);
pub const WHvArm64RegisterOslsrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00050053);

// AArch64 System Register Descriptions: Performance Monitors Registers
pub const WHvArm64RegisterPmccfiltrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052000);
pub const WHvArm64RegisterPmccntrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052001);
pub const WHvArm64RegisterPmceid0El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052002);
pub const WHvArm64RegisterPmceid1El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052003);
pub const WHvArm64RegisterPmcntenclrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052004);
pub const WHvArm64RegisterPmcntensetEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052005);
pub const WHvArm64RegisterPmcrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052006);
pub const WHvArm64RegisterPmevcntr0El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052007);
pub const WHvArm64RegisterPmevcntr1El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052008);
pub const WHvArm64RegisterPmevcntr2El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052009);
pub const WHvArm64RegisterPmevcntr3El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200A);
pub const WHvArm64RegisterPmevcntr4El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200B);
pub const WHvArm64RegisterPmevcntr5El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200C);
pub const WHvArm64RegisterPmevcntr6El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200D);
pub const WHvArm64RegisterPmevcntr7El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200E);
pub const WHvArm64RegisterPmevcntr8El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005200F);
pub const WHvArm64RegisterPmevcntr9El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052010);
pub const WHvArm64RegisterPmevcntr10El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052011);
pub const WHvArm64RegisterPmevcntr11El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052012);
pub const WHvArm64RegisterPmevcntr12El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052013);
pub const WHvArm64RegisterPmevcntr13El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052014);
pub const WHvArm64RegisterPmevcntr14El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052015);
pub const WHvArm64RegisterPmevcntr15El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052016);
pub const WHvArm64RegisterPmevcntr16El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052017);
pub const WHvArm64RegisterPmevcntr17El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052018);
pub const WHvArm64RegisterPmevcntr18El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052019);
pub const WHvArm64RegisterPmevcntr19El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201A);
pub const WHvArm64RegisterPmevcntr20El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201B);
pub const WHvArm64RegisterPmevcntr21El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201C);
pub const WHvArm64RegisterPmevcntr22El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201D);
pub const WHvArm64RegisterPmevcntr23El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201E);
pub const WHvArm64RegisterPmevcntr24El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005201F);
pub const WHvArm64RegisterPmevcntr25El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052020);
pub const WHvArm64RegisterPmevcntr26El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052021);
pub const WHvArm64RegisterPmevcntr27El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052022);
pub const WHvArm64RegisterPmevcntr28El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052023);
pub const WHvArm64RegisterPmevcntr29El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052024);
pub const WHvArm64RegisterPmevcntr30El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052025);
pub const WHvArm64RegisterPmevtyper0El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052026);
pub const WHvArm64RegisterPmevtyper1El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052027);
pub const WHvArm64RegisterPmevtyper2El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052028);
pub const WHvArm64RegisterPmevtyper3El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052029);
pub const WHvArm64RegisterPmevtyper4El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202A);
pub const WHvArm64RegisterPmevtyper5El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202B);
pub const WHvArm64RegisterPmevtyper6El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202C);
pub const WHvArm64RegisterPmevtyper7El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202D);
pub const WHvArm64RegisterPmevtyper8El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202E);
pub const WHvArm64RegisterPmevtyper9El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005202F);
pub const WHvArm64RegisterPmevtyper10El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052030);
pub const WHvArm64RegisterPmevtyper11El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052031);
pub const WHvArm64RegisterPmevtyper12El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052032);
pub const WHvArm64RegisterPmevtyper13El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052033);
pub const WHvArm64RegisterPmevtyper14El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052034);
pub const WHvArm64RegisterPmevtyper15El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052035);
pub const WHvArm64RegisterPmevtyper16El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052036);
pub const WHvArm64RegisterPmevtyper17El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052037);
pub const WHvArm64RegisterPmevtyper18El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052038);
pub const WHvArm64RegisterPmevtyper19El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052039);
pub const WHvArm64RegisterPmevtyper20El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203A);
pub const WHvArm64RegisterPmevtyper21El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203B);
pub const WHvArm64RegisterPmevtyper22El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203C);
pub const WHvArm64RegisterPmevtyper23El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203D);
pub const WHvArm64RegisterPmevtyper24El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203E);
pub const WHvArm64RegisterPmevtyper25El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005203F);
pub const WHvArm64RegisterPmevtyper26El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052040);
pub const WHvArm64RegisterPmevtyper27El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052041);
pub const WHvArm64RegisterPmevtyper28El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052042);
pub const WHvArm64RegisterPmevtyper29El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052043);
pub const WHvArm64RegisterPmevtyper30El0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052044);
pub const WHvArm64RegisterPmintenclrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052045);
pub const WHvArm64RegisterPmintensetEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052046);
pub const WHvArm64RegisterPmovsclrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052048);
pub const WHvArm64RegisterPmovssetEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00052049);
pub const WHvArm64RegisterPmselrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005204A);
pub const WHvArm64RegisterPmswincEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005204B);
pub const WHvArm64RegisterPmuserenrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005204C);
pub const WHvArm64RegisterPmxevcntrEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005204D);
pub const WHvArm64RegisterPmxevtyperEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005204E);

// AArch64 System Register Descriptions: Generic Timer Registers
pub const WHvArm64RegisterCntfrqEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00058000);
pub const WHvArm64RegisterCntkctlEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00058008);
pub const WHvArm64RegisterCntvCtlEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005800E);
pub const WHvArm64RegisterCntvCvalEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0005800F);
pub const WHvArm64RegisterCntvTvalEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00058010);
pub const WHvArm64RegisterCntvctEl0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00058011);

// ARM GIC (System Registers): AArch64 System Register Descriptions
pub const WHvArm64RegisterIccAp1R0El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060000);
pub const WHvArm64RegisterIccAp1R1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060001);
pub const WHvArm64RegisterIccAp1R2El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060002);
pub const WHvArm64RegisterIccAp1R3El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060003);
pub const WHvArm64RegisterIccAsgi1REl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060004);
pub const WHvArm64RegisterIccBpr1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060005);
pub const WHvArm64RegisterIccCtlrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060006);
pub const WHvArm64RegisterIccDirEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060007);
pub const WHvArm64RegisterIccEoir1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060008);
pub const WHvArm64RegisterIccHppir1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00060009);
pub const WHvArm64RegisterIccIar1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000A);
pub const WHvArm64RegisterIccIgrpen1El1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000B);
pub const WHvArm64RegisterIccPmrEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000C);
pub const WHvArm64RegisterIccRprEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000D);
pub const WHvArm64RegisterIccSgi1REl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000E);
pub const WHvArm64RegisterIccSreEl1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0006000F);

// GICR
pub const WHvArm64RegisterGicrBaseGpa: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00063000);

// Synic registers
pub const WHvRegisterSint0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0000);
pub const WHvRegisterSint1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0001);
pub const WHvRegisterSint2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0002);
pub const WHvRegisterSint3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0003);
pub const WHvRegisterSint4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0004);
pub const WHvRegisterSint5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0005);
pub const WHvRegisterSint6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0006);
pub const WHvRegisterSint7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0007);
pub const WHvRegisterSint8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0008);
pub const WHvRegisterSint9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0009);
pub const WHvRegisterSint10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000A);
pub const WHvRegisterSint11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000B);
pub const WHvRegisterSint12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000C);
pub const WHvRegisterSint13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000D);
pub const WHvRegisterSint14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000E);
pub const WHvRegisterSint15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A000F);
pub const WHvRegisterScontrol: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0010);
pub const WHvRegisterSversion: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0011);
pub const WHvRegisterSifp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0012);
pub const WHvRegisterSipp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A0013);
pub const WHvRegisterEom: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x000A00140);

// Hypervisor defined registers
pub const WHvRegisterVpRuntime: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00090000);
pub const WHvRegisterGuestOsId: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00090002);
pub const WHvRegisterVpAssistPage: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00090013);
pub const WHvRegisterReferenceTsc: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00090017);
pub const WHvRegisterReferenceTscSequence: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0009001A);
pub const WHvRegisterPendingEvent0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00010004);
pub const WHvRegisterPendingEvent1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00010005);
pub const WHvRegisterDeliverabilityNotifications: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00010006);
pub const WHvRegisterInternalActivityState: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000004);
pub const WHvRegisterPendingEvent2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00010008);
pub const WHvRegisterPendingEvent3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00010009);

pub const WHvRegisterSiefp: WHV_REGISTER_NAME = WHvRegisterSifp;
pub const WHvRegisterSimp: WHV_REGISTER_NAME = WHvRegisterSipp;
pub const WHvRegisterPendingEvent: WHV_REGISTER_NAME = WHvRegisterPendingEvent0;

pub const WHvRunVpExitReasonNone: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00000000);
pub const WHvRunVpExitReasonUnmappedGpa: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000000);
pub const WHvRunVpExitReasonGpaIntercept: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000001);
pub const WHvRunVpExitReasonUnrecoverableException: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000021);
pub const WHvRunVpExitReasonInvalidVpRegisterValue: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000020);
pub const WHvRunVpExitReasonUnsupportedFeature: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000022);
pub const WHvRunVpExitReasonSynicSintDeliverable: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x80000062);
pub const WHvRunVpExitReasonArm64Reset: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x8001000c);
pub const WHvRunVpExitReasonHypercall: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x80000050);
pub const WHvRunVpExitReasonCanceled: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0xFFFFFFFF);

#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct WHV_RUN_VP_EXIT_CONTEXT_u {
    pub message: [u8; 256],
}

impl WHV_PROCESSOR_FEATURES {
    // Bits for features reported by ID_AA64MMFR0_EL1
    /// 16 bits ASID.
    pub const Asid16: Self = Self(1 << 0);
    /// Indicated support for 16KB memory translation granule.
    pub const TGran16: Self = Self(1 << 1);
    /// Indicated support for 64KB memory translation granule.
    pub const TGran64: Self = Self(1 << 2);

    // Bits for features reported by ID_AA64MMFR1_EL1
    /// Hardware updates to Access flag.
    pub const Haf: Self = Self(1 << 3);
    /// Hardware updates to Dirty state.
    pub const Hdbs: Self = Self(1 << 4);
    /// Privileged access never (ARMv8.1).
    pub const Pan: Self = Self(1 << 5);
    /// AT S1E1RP and AT S1E1WP supported.
    pub const AtS1E1: Self = Self(1 << 6);

    // Bits for features reported by ID_AA64MMFR2_EL1

    /// PSTATE override of Unprivileged Load/Store (ARMv8.2)
    pub const Uao: Self = Self(1 << 7);

    // Bits for features reported by ID_AA64PFR0_EL1

    /// If Aarch32 is supported at El0.
    pub const El0Aarch32: Self = Self(1 << 8);
    /// Floating point support.
    pub const Fp: Self = Self(1 << 9);
    /// Floating point half-precision support.
    pub const FpHp: Self = Self(1 << 10);
    /// AdvSIMD is implemented.
    pub const AdvSimd: Self = Self(1 << 11);
    /// AdvSIMD with half precision floating point support.
    pub const AdvSimdHp: Self = Self(1 << 12);
    /// System register interface to versions 3 and 4 of the GIC CPU interface is implemented.
    pub const GicV3V4: Self = Self(1 << 13);
    /// System register interface to version 4.1 of the GIC CPU interface is implemented.
    pub const GicV41: Self = Self(1 << 14);
    /// Ras
    pub const Ras: Self = Self(1 << 15);

    // Bits for features reported by ID_AA64DFR0_EL1

    /// PMUv3 implemented.
    pub const PmuV3: Self = Self(1 << 16);
    /// PMUv3 for Armv8.1 implemented.
    pub const PmuV3ArmV81: Self = Self(1 << 17);
    /// PMUv3 for Armv8.4 implemented.
    pub const PmuV3ArmV84: Self = Self(1 << 18);
    /// PMUv3 for Armv8.5 implemented.
    pub const PmuV3ArmV85: Self = Self(1 << 19);

    // Bits for features reported by ID_AA64ISAR0_EL1

    /// AES(AESE, AESD, AESMC, AESIMC) instructions.
    pub const Aes: Self = Self(1 << 20);
    /// Polynomial multiply instructions PMULL/PMULL2
    pub const PolyMul: Self = Self(1 << 21);
    /// Sha1 instructions implemented.
    pub const Sha1: Self = Self(1 << 22);
    /// Sha256 instructions implemented.
    pub const Sha256: Self = Self(1 << 23);
    /// Sha512 instructions implemented.
    pub const Sha512: Self = Self(1 << 24);
    /// CRC instructions implemented.
    pub const Crc32: Self = Self(1 << 25);
    /// Atomic instructions implemented.
    pub const Atomic: Self = Self(1 << 26);
    /// SQRDMLAH, SQRDMLSH instructions implemented.
    pub const Rdm: Self = Self(1 << 27);
    /// Sha3 instructions implemented.
    pub const Sha3: Self = Self(1 << 28);
    /// SM3 instructions implemented.
    pub const Sm3: Self = Self(1 << 29);
    /// SM4 instructions implemented.
    pub const Sm4: Self = Self(1 << 30);
    /// UDOT and SDOT Dot product instructions implemented.
    pub const Dp: Self = Self(1 << 31);
    /// FMLAL and FMLSL instructions implemented.
    pub const Fhm: Self = Self(1 << 32);

    // Bits for features reported by ID_AA64ISAR1_EL1

    /// Clean data cache by address to point of persistence.
    pub const DcCvap: Self = Self(1 << 33);
    /// Clean data cache by address to point of deep persistence.
    pub const DcCvadp: Self = Self(1 << 34);

    // Pointer authentication using QARMA algo.

    /// HaveEnhancedPAC, HaveEnhancedPAC2 return false
    pub const ApaBase: Self = Self(1 << 35);
    /// HaveEnhancedPAC -> true, HaveEnhancedPAC2 -> false.
    pub const ApaEp: Self = Self(1 << 36);
    /// HaveEnhancedPAC -> false, HaveEnhancedPAC2 -> true, HaveFPAC -> false, HaveFPACCombined -> false.
    pub const ApaEp2: Self = Self(1 << 37);
    /// HaveEnhancedPAC -> false, HaveEnahancedPAC2 -> true, HaveFPAC -> true, HaveFPACCombined -> false.
    pub const ApaEp2Fp: Self = Self(1 << 38);
    /// HaveEnhancedPAC -> false, HaveEnahancedPAC2 -> true, HaveFPAC -> true, HaveFPACCombined -> true.
    pub const ApaEp2Fpc: Self = Self(1 << 39);

    /// FJCVTZS instruction implemented. Support for JS conversion from double to integer.
    pub const Jscvt: Self = Self(1 << 40);
    /// Complex number instructions(FCMLA and FCADD) instructions are implemented.
    pub const Fcma: Self = Self(1 << 41);
    /// ARMv8.3-RCPC. LDAPR* instructions.
    pub const RcpcV83: Self = Self(1 << 42);
    /// ARMv8.4-RCPC. LDAPUR* and STLUR* instructions.
    pub const RcpcV84: Self = Self(1 << 43);
    /// Generic code authentication using QARMA algo.
    pub const Gpa: Self = Self(1 << 44);

    // Features reported by CTR_EL0

    /// If L1 Instruction cache is PIPT.
    pub const L1ipPipt: Self = Self(1 << 45);

    // Features reported by DCZID_EL0

    /// Data zero instructions permitted.
    pub const DzPermitted: Self = Self(1 << 46);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_CONTROL {
    pub TargetPartition: u64,
    pub InterruptControl: u64,
    pub DestinationAddress: u64,
    pub RequestedVector: u32,
    pub TargetVtl: u8,
    pub ReservedZ0: u8,
    pub ReservedZ1: u16,
}

pub const INTERRUPT_CONTROL_ASSERTED: u64 = 1 << 34;
