// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use alloc::vec::Vec;
use core::num::Wrapping;
use zerocopy::IntoBytes;

#[derive(Copy, Clone)]
pub struct OemInfo {
    pub oem_id: [u8; 6],
    pub oem_tableid: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: [u8; 4],
    pub creator_revision: u32,
}

pub struct Table<'a> {
    revision: u8,
    oem_tableid: Option<[u8; 8]>,
    signature: [u8; 4],
    base: &'a [u8],
    extra: &'a [&'a [u8]],
}

impl<'a> Table<'a> {
    pub fn new<T: acpi_spec::Table>(revision: u8, oem_tableid: Option<[u8; 8]>, t: &'a T) -> Self {
        Self {
            revision,
            oem_tableid,
            signature: T::SIGNATURE,
            base: t.as_bytes(),
            extra: &[],
        }
    }

    pub fn new_dyn<T: acpi_spec::Table>(
        revision: u8,
        oem_tableid: Option<[u8; 8]>,
        t: &'a T,
        extra: &'a [&'a [u8]],
    ) -> Self {
        Self {
            revision,
            oem_tableid,
            signature: T::SIGNATURE,
            base: t.as_bytes(),
            extra,
        }
    }

    pub fn append_to_vec(&self, oem: &OemInfo, v: &mut Vec<u8>) -> usize {
        let len = size_of::<acpi_spec::Header>()
            + self.base.len()
            + self.extra.iter().fold(0, |x, y| x + y.len());
        let mut header = acpi_spec::Header {
            signature: self.signature,
            length: (len as u32).into(),
            revision: self.revision,
            checksum: 0,
            oem_id: oem.oem_id,
            oem_tableid: self.oem_tableid.unwrap_or(oem.oem_tableid),
            oem_revision: oem.oem_revision.into(),
            creator_id: u32::from_le_bytes(oem.creator_id).into(),
            creator_revision: oem.creator_revision.into(),
        };
        let sum = checksum(header.as_bytes())
            + checksum(self.base.as_bytes())
            + self.extra.iter().fold(Wrapping(0), |x, y| x + checksum(y));
        header.checksum = (-sum).0;
        let orig_len = v.len();
        v.extend_from_slice(header.as_bytes());
        v.extend_from_slice(self.base.as_bytes());
        for x in self.extra.iter() {
            v.extend_from_slice(x);
        }
        assert_eq!(checksum(&v[orig_len..]), Wrapping(0));
        len
    }

    pub fn to_vec(&self, oem: &OemInfo) -> Vec<u8> {
        let mut v = Vec::new();
        self.append_to_vec(oem, &mut v);
        v
    }
}

pub struct Builder {
    v: Vec<u8>,
    tables: Vec<u64>,
    base_addr: u64,
    oem: OemInfo,
}

fn checksum(data: &[u8]) -> Wrapping<u8> {
    let mut sum = Wrapping(0u8);
    for i in data.iter() {
        sum += Wrapping(*i);
    }
    sum
}

impl Builder {
    pub fn new(base_addr: u64, oem: OemInfo) -> Self {
        Builder {
            v: Vec::new(),
            tables: Vec::new(),
            base_addr,
            oem,
        }
    }

    /// Adds an existing table's guest physical address to the final XSDT.
    ///
    /// This does not read, copy, or relocate the table. The caller must validate
    /// the address and ensure the table remains available there. Only register
    /// tables that belong in the XSDT, not the DSDT or another XSDT.
    pub fn add_existing_table(&mut self, address: u64) {
        self.tables.push(address);
    }

    pub fn append(&mut self, table: &Table<'_>) -> u64 {
        let addr = self.base_addr + self.v.len() as u64;
        let len = table.append_to_vec(&self.oem, &mut self.v);
        if !len.is_multiple_of(8) {
            self.v.extend_from_slice(&[0; 8][..8 - len % 8]);
        }
        if table.signature != *b"XSDT" && table.signature != *b"DSDT" {
            self.tables.push(addr);
        }
        addr
    }

    pub fn append_raw(&mut self, data: &[u8]) -> u64 {
        let offset = self.v.len() as u64;
        let signature = &data[0..4];
        if signature != *b"XSDT" && signature != *b"DSDT" {
            self.tables.push(self.base_addr + offset);
        }
        self.v.extend_from_slice(data);
        if !data.len().is_multiple_of(8) {
            self.v.extend_from_slice(&[0; 8][..8 - data.len() % 8]);
        }
        self.base_addr + offset
    }

    fn rsdp(&self, xsdt: u64) -> acpi_spec::Rsdp {
        let mut r = acpi_spec::Rsdp {
            signature: *b"RSD PTR ",                     // [u8; 8], // "RSD PTR "
            checksum: 0,                                 // u8, // first 20 bytes
            oem_id: self.oem.oem_id,                     // [u8; 6],
            revision: 2,                                 // u8, // 2
            rsdt: 0,                                     // u32,
            length: size_of::<acpi_spec::Rsdp>() as u32, // u32,
            xsdt,                                        // u64,
            xchecksum: 0,                                // u8, // full checksum
            rsvd: [0, 0, 0],                             // [u8; 3],
        };
        let sum = checksum(&r.as_bytes()[0..20]);
        r.checksum = (-sum).0;
        let xsum = checksum(r.as_bytes());
        r.xchecksum = (-xsum).0;
        assert_eq!(checksum(&r.as_bytes()[0..20]), Wrapping(0));
        assert_eq!(checksum(r.as_bytes()), Wrapping(0));
        r
    }

    pub fn build(mut self) -> (Vec<u8>, Vec<u8>) {
        let tables = core::mem::take(&mut self.tables);
        let xsdt = self.append(&Table {
            signature: *b"XSDT",
            revision: 1,
            oem_tableid: None,
            base: tables.as_slice().as_bytes(),
            extra: &[],
        });
        let rsdp = self.rsdp(xsdt);
        (rsdp.as_bytes().to_vec(), self.v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    #[test]
    fn xsdt_retains_existing_and_appended_tables() {
        let base = 0x2000_0000;
        let existing_address = 0x1000_0000;
        let oem = OemInfo {
            oem_id: *b"MSFTVM",
            oem_tableid: *b"TESTACPI",
            oem_revision: 1,
            creator_id: *b"MSFT",
            creator_revision: 1,
        };
        let mut builder = Builder::new(base, oem);
        builder.add_existing_table(existing_address);
        let mcfg = acpi_spec::mcfg::McfgHeader::new();
        let table = Table::new(acpi_spec::mcfg::MCFG_REVISION, None, &mcfg);
        let appended_address = builder.append(&table);
        assert_eq!(appended_address, base);

        let (rsdp_bytes, tables) = builder.build();
        let rsdp = acpi_spec::Rsdp::read_from_bytes(&rsdp_bytes).unwrap();
        assert_eq!(checksum(&rsdp_bytes[..20]), Wrapping(0));
        assert_eq!(checksum(&rsdp_bytes), Wrapping(0));

        let appended_bytes = table.to_vec(&oem);
        let xsdt_offset = (rsdp.xsdt - base) as usize;
        assert_eq!(xsdt_offset, appended_bytes.len().next_multiple_of(8));
        assert_eq!(&tables[..appended_bytes.len()], appended_bytes);

        let (header, entries) =
            acpi_spec::Header::read_from_prefix(&tables[xsdt_offset..]).unwrap();
        assert_eq!(header.signature, *b"XSDT");
        let xsdt_length = header.length.get() as usize;
        assert_eq!(xsdt_length, size_of::<acpi_spec::Header>() + 16);
        assert_eq!(
            checksum(&tables[xsdt_offset..xsdt_offset + xsdt_length]),
            Wrapping(0)
        );
        assert_eq!(
            &entries[..16],
            [existing_address, appended_address].as_bytes()
        );
        assert_eq!(tables.len(), xsdt_offset + xsdt_length.next_multiple_of(8));
    }
}
