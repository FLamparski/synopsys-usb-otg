use usb_device::{Result, UsbDirection, UsbError};
use usb_device::bus::{UsbBusAllocator, PollResult};
use usb_device::endpoint::{EndpointType, EndpointAddress};
use crate::transition::{EndpointConfig, EndpointDescriptor};
use crate::ral::{read_reg, write_reg, modify_reg, otg_global, otg_device, otg_pwrclk};

use crate::target::UsbRegisters;
use crate::target::interrupt::{self, Mutex, CriticalSection};
use crate::endpoint::{EndpointIn, EndpointOut};
use crate::endpoint_memory::{EndpointMemoryAllocator, EndpointBufferState};
use crate::UsbPeripheral;

/// USB peripheral driver for STM32 microcontrollers.
pub struct UsbBus<USB> {
    peripheral: USB,
    regs: Mutex<UsbRegisters<USB>>,
    allocator: EndpointAllocator,
}

impl<USB: UsbPeripheral> UsbBus<USB> {
    /// Constructs a new USB peripheral driver.
    pub fn new(peripheral: USB, ep_memory: &'static mut [u32]) -> UsbBusAllocator<Self> {
        let bus = UsbBus {
            peripheral,
            regs: Mutex::new(UsbRegisters::new()),
            allocator: EndpointAllocator::new(ep_memory),
        };

        UsbBusAllocator::new(bus)
    }

    pub fn free(self) -> USB {
        self.peripheral
    }

    pub fn configure_all(&self, cs: &CriticalSection) {
        let regs = self.regs.borrow(cs);

        // Rx FIFO
        // This calculation doesn't correspond to one in a Reference Manual.
        // In fact, the required number of words is higher than indicated in RM.
        // The following numbers are pessimistic and were figured out empirically.
        let rx_fifo_size = if USB::HIGH_SPEED {
            self.allocator.memory_allocator.total_rx_buffer_size_words() + 30
        } else {
            // F429 requires 35+ words for the (EP0[8] + EP2[64]) setup
            // F446 requires 39+ words for the same setup
            self.allocator.memory_allocator.total_rx_buffer_size_words() + 30
        };
        write_reg!(otg_global, regs.global, GRXFSIZ, rx_fifo_size as u32);
        let mut fifo_top = rx_fifo_size;

        // Tx FIFO #0
        let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(0);

        #[cfg(feature = "fs")]
        write_reg!(otg_global, regs.global, DIEPTXF0,
            TX0FD: fifo_size as u32,
            TX0FSA: fifo_top as u32
        );
        #[cfg(feature = "hs")]
        write_reg!(otg_global, regs.global, GNPTXFSIZ,
            TX0FD: fifo_size as u32,
            TX0FSA: fifo_top as u32
        );

        fifo_top += fifo_size;

        // Tx FIFO #1
        let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(1);
        write_reg!(otg_global, regs.global, DIEPTXF1,
            INEPTXFD: fifo_size as u32,
            INEPTXSA: fifo_top as u32
        );
        fifo_top += fifo_size;

        // Tx FIFO #2
        let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(2);
        write_reg!(otg_global, regs.global, DIEPTXF2,
            INEPTXFD: fifo_size as u32,
            INEPTXSA: fifo_top as u32
        );
        fifo_top += fifo_size;

        // Tx FIFO #3
        let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(3);
        write_reg!(otg_global, regs.global, DIEPTXF3,
            INEPTXFD: fifo_size as u32,
            INEPTXSA: fifo_top as u32
        );
        fifo_top += fifo_size;

        #[cfg(feature = "stm32f446xx")]
        {
            // Tx FIFO #4
            let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(4);
            write_reg!(otg_global, regs.global, DIEPTXF4,
                INEPTXFD: fifo_size as u32,
                INEPTXSA: fifo_top as u32
            );
            fifo_top += fifo_size;

            // Tx FIFO #5
            let fifo_size = self.allocator.memory_allocator.tx_fifo_size_words(5);
            write_reg!(otg_global, regs.global, DIEPTXF5,
                INEPTXFD: fifo_size as u32,
                INEPTXSA: fifo_top as u32
            );
            fifo_top += fifo_size;
        }

        assert!(fifo_top as u32 <= crate::ral::otg_fifo::FIFO_DEPTH_WORDS);

        // Flush Rx & Tx FIFOs
        modify_reg!(otg_global, regs.global, GRSTCTL, RXFFLSH: 1, TXFFLSH: 1, TXFNUM: 0x10);
        while read_reg!(otg_global, regs.global, GRSTCTL, RXFFLSH, TXFFLSH) != (0, 0) {}

        for ep in &self.allocator.endpoints_in {
            if let Some(ep) = ep {
                // enabling EP TX interrupt
                modify_reg!(otg_device, regs.device, DAINTMSK, |v| v | (0x0001 << ep.address().index()));

                ep.configure(cs);
            }
        }

        for ep in &self.allocator.endpoints_out {
            if let Some(ep) = ep {
                if ep.address().index() == 0 {
                    // enabling RX interrupt from EP0
                    modify_reg!(otg_device, regs.device, DAINTMSK, |v| v | 0x00010000);
                }

                ep.configure(cs);
            }
        }
    }

    pub fn deconfigure_all(&self, cs: &CriticalSection) {
        let regs = self.regs.borrow(cs);

        // disable interrupts
        modify_reg!(otg_device, regs.device, DAINTMSK, IEPM: 0, OEPM: 0);

        for ep in &self.allocator.endpoints_in {
            if let Some(ep) = ep {
                ep.deconfigure(cs);
            }
        }

        for ep in &self.allocator.endpoints_out {
            if let Some(ep) = ep {
                ep.deconfigure(cs);
            }
        }
    }
}

pub struct EndpointAllocator {
    bitmap_in: u8,
    bitmap_out: u8,
    endpoints_in: [Option<EndpointIn>; 4],
    endpoints_out: [Option<EndpointOut>; 4],
    memory_allocator: EndpointMemoryAllocator,
}

impl EndpointAllocator {
    const ENDPOINT_COUNT: u8 = 4;

    fn new(memory: &'static mut [u32]) -> Self {
        Self {
            bitmap_in: 0,
            bitmap_out: 0,
            // [None; 4] requires Copy
            endpoints_in: [None, None, None, None],
            endpoints_out: [None, None, None, None],
            memory_allocator: EndpointMemoryAllocator::new(memory),
        }
    }

    fn alloc_number(bitmap: &mut u8, number: Option<u8>) -> Result<u8> {
        if let Some(number) = number {
            if number >= Self::ENDPOINT_COUNT {
                return Err(UsbError::InvalidEndpoint);
            }
            if *bitmap & (1 << number) == 0 {
                *bitmap |= 1 << number;
                Ok(number)
            } else {
                Err(UsbError::InvalidEndpoint)
            }
        } else {
            // Skip EP0
            for number in 1..Self::ENDPOINT_COUNT {
                if *bitmap & (1 << number) == 0 {
                    *bitmap |= 1 << number;
                    return Ok(number)
                }
            }
            Err(UsbError::EndpointOverflow)
        }
    }

    fn alloc(bitmap: &mut u8, config: &EndpointConfig, direction: UsbDirection) -> Result<EndpointDescriptor> {
        let number = Self::alloc_number(bitmap, config.number)?;
        let address = EndpointAddress::from_parts(number as usize, direction);
        Ok(EndpointDescriptor {
            address,
            ep_type: config.ep_type,
            max_packet_size: config.max_packet_size,
            interval: config.interval
        })
    }

    fn alloc_in(&mut self, config: &EndpointConfig) -> Result<EndpointIn> {
        let descr = Self::alloc(&mut self.bitmap_in, config, UsbDirection::In)?;

        self.memory_allocator.allocate_tx_buffer(descr.address.index() as u8, descr.max_packet_size as usize)?;
        let ep = EndpointIn::new(descr);

        Ok(ep)
    }

    fn alloc_out(&mut self, config: &EndpointConfig) -> Result<EndpointOut> {
        let descr = Self::alloc(&mut self.bitmap_out, config, UsbDirection::Out)?;

        let buffer = self.memory_allocator.allocate_rx_buffer(descr.max_packet_size as usize)?;
        let ep = EndpointOut::new(descr, buffer);

        Ok(ep)
    }

    fn alloc_ep(
        &mut self,
        ep_dir: UsbDirection,
        ep_addr: Option<EndpointAddress>,
        ep_type: EndpointType,
        max_packet_size: u16,
        interval: u8) -> Result<EndpointAddress>
    {
        let ep_type = unsafe { core::mem::transmute(ep_type) };
        let number = ep_addr.map(|a| a.index() as u8);

        let config = EndpointConfig {
            ep_type,
            max_packet_size,
            interval,
            number,
            pair_of: None
        };
        match ep_dir {
            UsbDirection::Out => {
                let ep = self.alloc_out(&config)?;
                let address = ep.address();
                self.endpoints_out[address.index()] = Some(ep);
                Ok(address)
            },
            UsbDirection::In => {
                let ep = self.alloc_in(&config)?;
                let address = ep.address();
                self.endpoints_in[address.index()] = Some(ep);
                Ok(address)
            },
        }
    }
}

impl<USB: UsbPeripheral> usb_device::bus::UsbBus for UsbBus<USB> {
    fn alloc_ep(
        &mut self,
        ep_dir: UsbDirection,
        ep_addr: Option<EndpointAddress>,
        ep_type: EndpointType,
        max_packet_size: u16,
        interval: u8) -> Result<EndpointAddress>
    {
        self.allocator.alloc_ep(ep_dir, ep_addr, ep_type, max_packet_size, interval)
    }

    fn enable(&mut self) {
        // Enable USB_OTG in RCC
        USB::enable();

        interrupt::free(|cs| {
            let regs = self.regs.borrow(cs);

            // Wait for AHB ready
            while read_reg!(otg_global, regs.global, GRSTCTL, AHBIDL) == 0 {}

            // Configure OTG as device
            #[cfg(feature = "fs")]
            modify_reg!(otg_global, regs.global, GUSBCFG,
                SRPCAP: 0, // SRP capability is not enabled
                TRDT: 0x6, // ??? USB turnaround time
                FDMOD: 1 // Force device mode
            );
            #[cfg(feature = "hs")]
            modify_reg!(otg_global, regs.global, GUSBCFG,
                SRPCAP: 0, // SRP capability is not enabled
                TRDT: 0x9, // ??? USB turnaround time
                TOCAL: 0x1,
                FDMOD: 1, // Force device mode
                PHYSEL: 1
            );

            // Configuring Vbus sense and SOF output
            //write_reg!(otg_global, regs.global, GCCFG, VBUSBSEN: 1);
            write_reg!(otg_global, regs.global, GCCFG, 1 << 21); // set NOVBUSSENS

            // Enable PHY clock
            write_reg!(otg_pwrclk, regs.pwrclk, PCGCCTL, 0);

            // Soft disconnect device
            modify_reg!(otg_device, regs.device, DCTL, SDIS: 1);

            // Setup USB FS speed [and frame interval]
            modify_reg!(otg_device, regs.device, DCFG,
                DSPD: 0b11 // Device speed: Full speed
            );

            // unmask EP interrupts
            write_reg!(otg_device, regs.device, DIEPMSK, XFRCM: 1);

            // unmask core interrupts
            write_reg!(otg_global, regs.global, GINTMSK,
                USBRST: 1, ENUMDNEM: 1,
                USBSUSPM: 1, WUIM: 1,
                IEPINT: 1, RXFLVLM: 1
            );

            // clear pending interrupts
            write_reg!(otg_global, regs.global, GINTSTS, 0xffffffff);

            // unmask global interrupt
            modify_reg!(otg_global, regs.global, GAHBCFG, GINT: 1);

            // connect(true)
            modify_reg!(otg_global, regs.global, GCCFG, PWRDWN: 1);
            modify_reg!(otg_device, regs.device, DCTL, SDIS: 0);
        });
    }

    fn reset(&self) {
        interrupt::free(|cs| {
            let regs = self.regs.borrow(cs);

            self.configure_all(cs);

            modify_reg!(otg_device, regs.device, DCFG, DAD: 0);
        });
    }

    fn set_device_address(&self, addr: u8) {
        interrupt::free(|cs| {
            let regs = self.regs.borrow(cs);

            modify_reg!(otg_device, regs.device, DCFG, DAD: addr as u32);
        });
    }

    fn write(&self, ep_addr: EndpointAddress, buf: &[u8]) -> Result<usize> {
        if !ep_addr.is_in() || ep_addr.index() >= 4 {
            return Err(UsbError::InvalidEndpoint);
        }
        if let Some(ep) = &self.allocator.endpoints_in[ep_addr.index()] {
            ep.write(buf).map(|_| buf.len())
        } else {
            Err(UsbError::InvalidEndpoint)
        }
    }

    fn read(&self, ep_addr: EndpointAddress, buf: &mut [u8]) -> Result<usize> {
        if !ep_addr.is_out() || ep_addr.index() >= 4 {
            return Err(UsbError::InvalidEndpoint);
        }

        if let Some(ep) = &self.allocator.endpoints_out[ep_addr.index()] {
            ep.read(buf)
        } else {
            Err(UsbError::InvalidEndpoint)
        }
    }

    fn set_stalled(&self, ep_addr: EndpointAddress, stalled: bool) {
        if ep_addr.index() >= 4 {
            return;
        }

        crate::endpoint::set_stalled(ep_addr, stalled)
    }

    fn is_stalled(&self, ep_addr: EndpointAddress) -> bool {
        if ep_addr.index() >= 4 {
            return true;
        }

        crate::endpoint::is_stalled(ep_addr)
    }

    fn suspend(&self) {
        // Nothing to do here?
    }

    fn resume(&self) {
        // Nothing to do here?
    }

    fn poll(&self) -> PollResult {
        interrupt::free(|cs| {
            let regs = self.regs.borrow(cs);

            #[cfg(not(feature = "stm32f446xx"))]
            let core_id = read_reg!(otg_global, regs.global, CID);

            // The CID register is named slightly differently in these crates, this should be
            // fixed upstream. For now, use this hack to get the right register.
            #[cfg(feature = "stm32f446xx")]
            let core_id = read_reg!(otg_global, regs.global, OTG_CID);

            let (wakeup, suspend, enum_done, reset, iep, rxflvl) = read_reg!(otg_global, regs.global, GINTSTS,
                WKUPINT, USBSUSP, ENUMDNE, USBRST, IEPINT, RXFLVL
            );

            if reset != 0 {
                write_reg!(otg_global, regs.global, GINTSTS, USBRST: 1);

                self.deconfigure_all(cs);

                // Flush RX
                modify_reg!(otg_global, regs.global, GRSTCTL, RXFFLSH: 1);
                while read_reg!(otg_global, regs.global, GRSTCTL, RXFFLSH) == 1 {}
            }

            if enum_done != 0 {
                write_reg!(otg_global, regs.global, GINTSTS, ENUMDNE: 1);

                PollResult::Reset
            } else if wakeup != 0 {
                // Clear the interrupt
                write_reg!(otg_global, regs.global, GINTSTS, WKUPINT: 1);

                PollResult::Resume
            } else if suspend != 0 {
                write_reg!(otg_global, regs.global, GINTSTS, USBSUSP: 1);

                PollResult::Suspend
            } else {
                let mut ep_out = 0;
                let mut ep_in_complete = 0;
                let mut ep_setup = 0;

                use crate::ral::{endpoint_in, endpoint_out};

                // RXFLVL & IEPINT flags are read-only, there is no need to clear them
                if rxflvl != 0 {
                    let (epnum, data_size, status) = read_reg!(otg_global, regs.global, GRXSTSR, EPNUM, BCNT, PKTSTS);
                    match status {
                        0x02 => { // OUT received
                            ep_out |= 1 << epnum;
                        }
                        0x06 => { // SETUP received
                            // flushing TX if something stuck in control endpoint
                            let ep = endpoint_in::instance(epnum as u8);
                            if read_reg!(endpoint_in, ep, DIEPTSIZ, PKTCNT) != 0 {
                                modify_reg!(otg_global, regs.global, GRSTCTL, TXFNUM: epnum, TXFFLSH: 1);
                                while read_reg!(otg_global, regs.global, GRSTCTL, TXFFLSH) == 1 {}
                            }
                            ep_setup |= 1 << epnum;
                        }
                        0x03 | 0x04 => { // OUT completed | SETUP completed
                            // Re-enable the endpoint, F429-like chips only
                            if core_id == 0x0000_1200 || core_id == 0x0000_1100 {
                                let ep = endpoint_out::instance(epnum as u8);
                                modify_reg!(endpoint_out, ep, DOEPCTL, CNAK: 1, EPENA: 1);
                            }
                            read_reg!(otg_global, regs.global, GRXSTSP); // pop GRXSTSP
                        }
                        _ => {
                            read_reg!(otg_global, regs.global, GRXSTSP); // pop GRXSTSP
                        }
                    }

                    if status == 0x02 || status == 0x06 {
                        if let Some(ep) = &self.allocator.endpoints_out[epnum as usize] {
                            let mut buffer = ep.buffer.borrow(cs).borrow_mut();
                            if buffer.state() == EndpointBufferState::Empty {
                                read_reg!(otg_global, regs.global, GRXSTSP); // pop GRXSTSP

                                let is_setup = status == 0x06;
                                buffer.fill_from_fifo(data_size as u16, is_setup).ok();

                                // Re-enable the endpoint, F446-like chips only
                                if core_id == 0x0000_2000 || core_id == 0x0000_2100 {
                                    let ep = endpoint_out::instance(epnum as u8);
                                    modify_reg!(endpoint_out, ep, DOEPCTL, CNAK: 1, EPENA: 1);
                                }
                            }
                        }
                    }
                }

                if iep != 0 {
                    for ep in &self.allocator.endpoints_in {
                        if let Some(ep) = ep {
                            let ep_regs = endpoint_in::instance(ep.address().index() as u8);
                            if read_reg!(endpoint_in, ep_regs, DIEPINT, XFRC) != 0 {
                                write_reg!(endpoint_in, ep_regs, DIEPINT, XFRC: 1);
                                ep_in_complete |= 1 << ep.address().index();
                            }
                        }
                    }
                }

                for ep in &self.allocator.endpoints_out {
                    if let Some(ep) = ep {
                        match ep.buffer_state() {
                            EndpointBufferState::DataOut => {
                                ep_out |= 1 << ep.address().index();
                            },
                            EndpointBufferState::DataSetup => {
                                ep_setup |= 1 << ep.address().index();
                            },
                            EndpointBufferState::Empty => {},
                        }
                    }
                }

                if (ep_in_complete | ep_out | ep_setup) != 0 {
                    PollResult::Data { ep_out, ep_in_complete, ep_setup }
                } else {
                    PollResult::None
                }
            }
        })
    }

    const QUIRK_SET_ADDRESS_BEFORE_STATUS: bool = true;
}
