// SPDX-License-Identifier: GPL-2.0

//! Rust PCI driver sample (based on QEMU's `pci-testdev`).
//!
//! To make this driver probe, QEMU must be run with `-device pci-testdev`.

use kernel::{
    device::Bound,
    device::Core,
    devres::Devres,
    io::Io,
    pci,
    prelude::*,
    sync::aref::ARef, //
};

struct Regs;

impl Regs {
    const TEST: usize = 0x0;
    const OFFSET: usize = 0x4;
    const DATA: usize = 0x8;
    const COUNT: usize = 0xC;
    const END: usize = 0x10;
}

type Bar0 = pci::Bar<{ Regs::END }>;

#[derive(Copy, Clone, Debug)]
struct TestIndex(u8);

impl TestIndex {
    const NO_EVENTFD: Self = Self(0);
}

#[pin_data(PinnedDrop)]
struct SampleDriver {
    pdev: ARef<pci::Device>,
    #[pin]
    bar: Devres<Bar0>,
    index: TestIndex,
}

kernel::pci_device_table!(
    PCI_TABLE,
    MODULE_PCI_TABLE,
    <SampleDriver as pci::Driver>::IdInfo,
    [(
        pci::DeviceId::from_id(pci::Vendor::REDHAT, 0xA000),
        TestIndex::NO_EVENTFD
    )]
);

impl SampleDriver {
    fn testdev(index: &TestIndex, bar: &Bar0) -> Result<u32> {
        // Select the test.
        bar.write8(index.0, Regs::TEST);

        let offset = bar.read32(Regs::OFFSET) as usize;
        let data = bar.read8(Regs::DATA);

        // Write `data` to `offset` to increase `count` by one.
        //
        // Note that we need `try_write8`, since `offset` can't be checked at compile-time.
        bar.try_write8(data, offset)?;

        Ok(bar.read32(Regs::COUNT))
    }

    fn config_space(pdev: &pci::Device<Bound>) {
        let config = pdev.config_space();

        // TODO: use the register!() macro for defining PCI configuration space registers once it
        // has been move out of nova-core.
        dev_info!(
            pdev,
            "pci-testdev config space read8 rev ID: {:x}\n",
            config.read8(0x8)
        );

        dev_info!(
            pdev,
            "pci-testdev config space read16 vendor ID: {:x}\n",
            config.read16(0)
        );

        dev_info!(
            pdev,
            "pci-testdev config space read32 BAR 0: {:x}\n",
            config.read32(0x10)
        );
    }
    /// NEW: Read PCIe capabilities pointer from legacy config space at 0x34,
    /// then parse the first PCIe capability header in extended configuration space.
    ///
    /// The PCIe Extended Configuration Space is 4096 bytes (vs. legacy 256 bytes).
    /// The standard places a pointer at offset 0x34 in legacy config space that
    /// contains an 18-bit value pointing into the extended configuration space.
    /// The capability header itself follows the PCIe capability structure with:
    ///   - Offset 0-1 (high byte): Capability type
    ///   - Offset 2 (low byte): Control/status flags
    ///   - Offset 6: Next capability offset (0xFFFE if last)
    fn config_space_extended(pdev: &pci::Device<Bound>) -> Result {
        // Get access to extended configuration space (4096 bytes).
        let config = pdev.config_space_extended()?;

        // Read from offset 0x34 - PCIe Capabilities Pointer in legacy config space.
        // This value is an 18-bit pointer into the extended configuration space region.
        // The actual offset into the 4KB extended space is given directly by this byte.
        // PCI spec: struct pci_cap_ptr stores the capabilities pointer as an 18-bit value in a single byte.
        let cap_offset = config.read8(0x34) as usize;

        dev_info!(
            pdev,
            "pci-driver legacy config space[0x34] read16 capabilities pointer: {:02x}\n",
            cap_offset
        );

        // Read from the capabilities pointer offset - PCIe Extended Capability header.
        let cap_header = config.read32(cap_offset);

        dev_info!(
            pdev,
            "pci-driver extended config space read32 at offset [0x{:04x}] capability header value: {:08x}\n",
            cap_offset,
            cap_header
        );

        let cap_id = config.read8(cap_offset);

        dev_info!(
            pdev,
            "pci-driver extended config space at offset [0x{:04x}] read8 capability id: {:02x}\n",
            cap_offset,
            cap_id,
        );

        let ctrl = config.read16(cap_offset + 8);

        dev_info!(
            pdev,
            "pci-driver extended config space at offset [0x{:04x}] read16 control: {:04x}\n",
            cap_offset + 8,
            ctrl
        );

        let status = config.read16(cap_offset + 10);

        dev_info!(
            pdev,
            "pci-driver extended config space at offset [0x{:04x}] read16 status: {:04x}\n",
            cap_offset + 10,
            status
        );

        // Read next capability offset (multi-byte capabilities use this)
        let next_cap = config.read8(cap_offset + 1);

        dev_info!(
            pdev,
            "pci-driver extended config space at offset [0x{:04x}] read8 next cap: {:02x}\n",
            cap_offset + 1,
            next_cap
        );

        Ok(())
    }
}

impl pci::Driver for SampleDriver {
    type IdInfo = TestIndex;

    const ID_TABLE: pci::IdTable<Self::IdInfo> = &PCI_TABLE;

    fn probe(pdev: &pci::Device<Core>, info: &Self::IdInfo) -> impl PinInit<Self, Error> {
        pin_init::pin_init_scope(move || {
            let vendor = pdev.vendor_id();
            dev_dbg!(
                pdev,
                "Probe Rust PCIe driver sample (PCI ID: {}, 0x{:x}).\n",
                vendor,
                pdev.device_id()
            );

            pdev.enable_device_mem()?;
            pdev.set_master();

            Ok(try_pin_init!(Self {
                bar <- pdev.iomap_region_sized::<{ Regs::END }>(0, c"rust_driver_pcie"),
                index: *info,
                _: {
                    let bar = bar.access(pdev.as_ref())?;

                    dev_info!(
                        pdev,
                        "pcie-testdev data-match count: {}\n",
                        Self::testdev(info, bar)?
                    );
                    Self::config_space(pdev);

                    // NEW: Call the extended config space capability parser
                    Self::config_space_extended(pdev)?;
                },
                pdev: pdev.into(),
            }))
        })
    }

    fn unbind(pdev: &pci::Device<Core>, this: Pin<&Self>) {
        if let Ok(bar) = this.bar.access(pdev.as_ref()) {
            // Reset pci-testdev by writing a new test index.
            bar.write8(this.index.0, Regs::TEST);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for SampleDriver {
    fn drop(self: Pin<&mut Self>) {
        dev_dbg!(self.pdev, "Remove Rust PCIe driver sample.\n");
    }
}

kernel::module_pci_driver! {
    type: SampleDriver,
    name: "rust_driver_pcie",
    authors: ["Danilo Krummrich"],
    description: "Rust PCIe driver",
    license: "GPL v2",
}
