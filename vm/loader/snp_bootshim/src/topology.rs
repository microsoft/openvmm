// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Allocation-free validation of the shared IGVM hardware schema.
//!
//! Host topology is unmeasured and is not attested. Attestation policy is
//! deferred for bring-up. It never changes the measured RAM acceptance list.

use crate::pcie::{Error, Properties, be64};
use acpi_core::snp::{Cpu, Ram, Topology, Uart};
use fdt::parser::{Node, Parser};
use loader_defs::linux::{
    SNP_BOOT_SHIM_DT_SIZE, SNP_BOOT_SHIM_MAX_CPUS, SNP_BOOT_SHIM_MAX_RAM_RECORDS,
};

fn exact<'a>(p: &Properties<'a>, name: &'static str, value: &[u8]) -> Result<(), Error<'a>> {
    if p.get(name) != Some(value) {
        return Err(Error::Property(name));
    }
    Ok(())
}

fn text<'a>(p: &Properties<'a>, name: &'static str) -> Result<(), Error<'a>> {
    if let Some(data) = p.get(name) {
        if data.last() != Some(&0)
            || data[..data.len() - 1].contains(&0)
            || core::str::from_utf8(data).is_err()
        {
            return Err(Error::Property(name));
        }
    }
    Ok(())
}

fn leaf<'a>(node: &Node<'a>) -> Result<(), Error<'a>> {
    if node.children().next().is_some() {
        return Err(Error::Unsupported("children of hardware leaf"));
    }
    Ok(())
}

/// The supported tree has at most three levels. Checking sibling names by
/// re-reading bounded iterators avoids a large name array on the boot stack.
fn children<'a>(
    node: &Node<'a>,
    count: &mut usize,
    mut visit: impl FnMut(Node<'a>) -> Result<(), Error<'a>>,
) -> Result<(), Error<'a>> {
    for (index, child) in node.children().enumerate() {
        let child = child?;
        *count += 1;
        if *count > 1024 {
            return Err(Error::DtLimit);
        }
        for previous in node.children().take(index) {
            if previous?.name == child.name {
                return Err(Error::Property("duplicate sibling node"));
            }
        }
        visit(child)?;
    }
    Ok(())
}

pub(crate) fn parse<'a>(
    dt: &'a [u8],
    expected_cpu_count: u32,
    ram_end: u64,
) -> Result<Topology, Error<'a>> {
    let size = Parser::read_total_size(dt)?;
    if size > SNP_BOOT_SHIM_DT_SIZE as usize {
        return Err(Error::DtLimit);
    }
    let parser = Parser::new(dt.get(..size).ok_or(Error::DtLimit)?)?;
    if parser.boot_cpuid_phys != 0 || parser.memory_reservations().next().is_some() {
        return Err(Error::Unsupported("BSP or memory reservation"));
    }
    let root = parser.root()?;
    let props = Properties::read(&root)?;
    props.only(&[
        "#address-cells",
        "#size-cells",
        "model",
        "compatible",
        "linux,pci-probe-only",
    ])?;
    if !root.name.is_empty()
        || props.cell("#address-cells")? != 2
        || props.cell("#size-cells")? != 2
    {
        return Err(Error::Property("root cells"));
    }
    text(&props, "model")?;
    if props.get("compatible").is_some() {
        exact(&props, "compatible", b"microsoft,hyperv\0")?;
    }
    let mut cpus = [Cpu {
        apic_id: 0,
        numa_node: 0,
        enabled: false,
    }; SNP_BOOT_SHIM_MAX_CPUS];
    let mut cpu_count = 0;
    let mut cpu_units = [false; SNP_BOOT_SHIM_MAX_CPUS];
    let mut ram = [(0u64, 0u64); SNP_BOOT_SHIM_MAX_RAM_RECORDS];
    let mut ram_count = 0;
    let mut uarts = [Uart {
        uid: 0,
        io_base: 0,
        length: 0,
        irq: 0,
    }; 4];
    let mut uart_count = 0;
    let mut root_count = 1;
    children(&root, &mut root_count, |node| {
        let p = Properties::read(&node)?;
        match node.name {
            "cpus" => {
                p.only(&["#address-cells", "#size-cells"])?;
                if p.cell("#address-cells")? != 1 || p.cell("#size-cells")? != 0 {
                    return Err(Error::Property("CPU cells"));
                }
                let mut count = 0;
                children(&node, &mut count, |cpu| {
                    leaf(&cpu)?;
                    let p = Properties::read(&cpu)?;
                    p.only(&["device_type", "reg", "numa-node-id", "status"])?;
                    let unit = cpu
                        .name
                        .strip_prefix("cpu@")
                        .and_then(|name| usize::from_str_radix(name, 16).ok())
                        .and_then(|unit| unit.checked_sub(1))
                        .filter(|&unit| unit < expected_cpu_count as usize)
                        .and_then(|unit| cpu_units.get_mut(unit))
                        .ok_or(Error::Property("CPU unit address"))?;
                    if core::mem::replace(unit, true) {
                        return Err(Error::Property("duplicate CPU unit address"));
                    }
                    exact(&p, "device_type", b"cpu\0")?;
                    exact(&p, "status", b"okay\0")?;
                    let slot = cpus.get_mut(cpu_count).ok_or(Error::DtLimit)?;
                    *slot = Cpu {
                        apic_id: p.cell("reg")?,
                        numa_node: p.cell("numa-node-id")?,
                        enabled: true,
                    };
                    cpu_count += 1;
                    Ok(())
                })?;
            }
            "pio-bus" => {
                p.only(&["compatible", "#address-cells", "#size-cells", "ranges"])?;
                exact(&p, "compatible", b"x86-pio-bus\0")?;
                exact(&p, "ranges", &[])?;
                if p.cell("#address-cells")? != 1 || p.cell("#size-cells")? != 1 {
                    return Err(Error::Property("PIO cells"));
                }
                let mut count = 0;
                children(&node, &mut count, |uart| {
                    leaf(&uart)?;
                    let p = Properties::read(&uart)?;
                    p.only(&[
                        "compatible",
                        "reg",
                        "interrupts",
                        "current-speed",
                        "clock-frequency",
                    ])?;
                    exact(&p, "compatible", b"ns16550\0")?;
                    let reg = p.required("reg", 16)?;
                    let base = be64(reg);
                    let index = x86defs::serial::COM_BASES
                        .iter()
                        .position(|&b| u64::from(b) == base)
                        .ok_or(Error::Unsupported("UART I/O base"))?;
                    let unit = uart
                        .name
                        .strip_prefix("serial@")
                        .ok_or(Error::Property("UART name"))?;
                    if u64::from_str_radix(unit, 16).ok() != Some(base) {
                        return Err(Error::Property("UART unit address"));
                    }
                    if p.cell("current-speed")? == 0 || p.cell("clock-frequency")? != 0 {
                        return Err(Error::Unsupported("UART clock or baud"));
                    }
                    let irq = u32::try_from(be64(p.required("interrupts", 8)?))
                        .map_err(|_| Error::Property("interrupts"))?;
                    *uarts.get_mut(uart_count).ok_or(Error::DtLimit)? = Uart {
                        uid: index as u8 + 1,
                        io_base: base as u16,
                        length: be64(&reg[8..]),
                        irq,
                    };
                    uart_count += 1;
                    Ok(())
                })?;
            }
            "chosen" => {
                leaf(&node)?;
                // Boot arguments and console selection are OpenHCL handoff
                // metadata. Linux uses its measured zero page, not these.
                p.only(&["bootargs", "stdout-path", "linux,pci-probe-only"])?;
                text(&p, "bootargs")?;
                text(&p, "stdout-path")?;
            }
            "openhcl" => openhcl(&node, &p)?,
            "bus" => buses(&node, &p, ram_end)?,
            name if name.starts_with("memory@") => {
                leaf(&node)?;
                p.only(&[
                    "device_type",
                    "reg",
                    "numa-node-id",
                    igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY,
                ])?;
                exact(&p, "device_type", b"memory\0")?;
                if p.cell("numa-node-id")? != 0
                    || p.cell(igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY)?
                        != u32::from(igvm_defs::MemoryMapEntryType::MEMORY.0)
                {
                    return Err(Error::Unsupported("memory NUMA node or IGVM type"));
                }
                let reg = p.required("reg", 16)?;
                let start = be64(reg);
                let len = be64(&reg[8..]);
                let end = start.checked_add(len).ok_or(Error::Range)?;
                if len == 0
                    || !start.is_multiple_of(4096)
                    || !len.is_multiple_of(4096)
                    || end > ram_end
                    || u64::from_str_radix(&name[7..], 16).ok() != Some(start)
                {
                    return Err(Error::Range);
                }
                *ram.get_mut(ram_count).ok_or(Error::DtLimit)? = (start, end);
                ram_count += 1;
            }
            name if name.starts_with("pcie@")
                || name == "pcie"
                || name.starts_with("pci@")
                || name == "pci" =>
            {
                // The PCIe converter validates every property and resource.
                leaf(&node)?;
                exact(&p, "compatible", b"pci-host-ecam-generic\0")?;
            }
            _ => return Err(Error::Unsupported("unknown hardware node")),
        }
        Ok(())
    })?;
    ram[..ram_count].sort_unstable();
    let mut end = 0;
    for &(start, next) in &ram[..ram_count] {
        if start != end {
            return Err(Error::Range);
        }
        end = next;
    }
    if end != ram_end || ram_count == 0 {
        return Err(Error::Property("RAM disagrees with measured interval"));
    }
    Topology::new(
        expected_cpu_count,
        parser.boot_cpuid_phys,
        &cpus[..cpu_count],
        Ram {
            start: 0,
            length: ram_end,
            numa_node: 0,
        },
        &uarts[..uart_count],
    )
    .map_err(|_| Error::Property("CPU or UART topology"))
}

fn openhcl<'a>(node: &Node<'a>, p: &Properties<'a>) -> Result<(), Error<'a>> {
    p.only(&["memory-allocation-mode"])?;
    exact(p, "memory-allocation-mode", b"host\0")?;
    let mut count = 0;
    children(node, &mut count, |node| {
        leaf(&node)?;
        let p = Properties::read(&node)?;
        match node.name {
            "entropy" => {
                p.only(&["reg"])?;
                if p.get("reg").is_none() {
                    return Err(Error::Property("entropy reg"));
                }
            }
            "keep-alive" => {
                p.only(&["device-types"])?;
                exact(&p, "device-types", b"nvme\0")?;
            }
            _ => return Err(Error::Unsupported("OpenHCL metadata")),
        }
        Ok(())
    })
}

fn buses<'a>(node: &Node<'a>, p: &Properties<'a>, ram_end: u64) -> Result<(), Error<'a>> {
    p.only(&["compatible", "#address-cells", "#size-cells", "ranges"])?;
    exact(p, "compatible", b"simple-bus\0")?;
    exact(p, "ranges", &[])?;
    if p.cell("#address-cells")? != 2 || p.cell("#size-cells")? != 2 {
        return Err(Error::Property("bus cells"));
    }
    let mut seen = [false; 2];
    let mut count = 0;
    children(node, &mut count, |node| {
        leaf(&node)?;
        let p = Properties::read(&node)?;
        let vtl_prop = igvm_defs::dt::IGVM_DT_VTL_PROPERTY;
        p.only(&[
            "compatible",
            "#address-cells",
            "#size-cells",
            "ranges",
            vtl_prop,
            "microsoft,message-connection-id",
        ])?;
        exact(&p, "compatible", b"microsoft,vmbus\0")?;
        if p.cell("#address-cells")? != 2 || p.cell("#size-cells")? != 2 {
            return Err(Error::Property("VMBus cells"));
        }
        let index = match p.cell(vtl_prop)? {
            0 => 0,
            2 => 1,
            _ => return Err(Error::Unsupported("VMBus VTL")),
        };
        if core::mem::replace(&mut seen[index], true) {
            return Err(Error::Property("duplicate VMBus VTL"));
        }
        let prefix = if index == 0 {
            "vmbus-vtl0"
        } else {
            "vmbus-vtl2"
        };
        if node.name != prefix
            && !node
                .name
                .strip_prefix(prefix)
                .is_some_and(|s| s.starts_with('@'))
        {
            return Err(Error::Property("VMBus name"));
        }
        if p.cell("microsoft,message-connection-id")? != if index == 0 { 1 } else { 4 } {
            return Err(Error::Unsupported("VMBus redirection"));
        }
        // These describe OpenHCL partition handoff, not an enabled guest
        // device. Do not synthesize VMBus AML from their presence.
        let ranges = p.get("ranges").ok_or(Error::Property("ranges"))?;
        if !ranges.len().is_multiple_of(24)
            || ranges.len() > 48
            || (index == 1 && !ranges.is_empty())
        {
            return Err(Error::Unsupported("VMBus ranges"));
        }
        let mut previous_end = 0;
        for r in ranges.chunks_exact(24) {
            let base = be64(r);
            let len = be64(&r[16..]);
            let end = base.checked_add(len).ok_or(Error::Range)?;
            if base != be64(&r[8..]) || !base.is_multiple_of(4096) || !len.is_multiple_of(4096) {
                return Err(Error::Range);
            }
            if len == 0 {
                continue;
            }
            if base < ram_end || base < previous_end {
                return Err(Error::Range);
            }
            previous_end = end;
        }
        Ok(())
    })
}
