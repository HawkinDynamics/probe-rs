use probe_rs_target::RawFlashAlgorithm;
use tracing::Level;

use super::{FlashAlgorithm, FlashBuilder, FlashError, FlashFill, FlashPage, FlashProgress};
use crate::config::NvmRegion;
use crate::error::Error;
use crate::flashing::encoder::FlashEncoder;
use crate::flashing::{FlashLayout, FlashSector};
use crate::memory::MemoryInterface;
use crate::rtt::{self, Rtt, ScanRegion};
use crate::CoreStatus;
use crate::{core::CoreRegisters, session::Session, Core, InstructionSet};
use std::marker::PhantomData;
use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

/// The timeout for init/uninit routines.
const INIT_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) trait Operation {
    const OPERATION: u32;
    const NAME: &'static str;
}

pub(super) struct Erase;

impl Operation for Erase {
    const OPERATION: u32 = 1;
    const NAME: &'static str = "Erase";
}

pub(super) struct Program;

impl Operation for Program {
    const OPERATION: u32 = 2;
    const NAME: &'static str = "Program";
}

pub(super) struct Verify;

impl Operation for Verify {
    const OPERATION: u32 = 3;
    const NAME: &'static str = "Verify";
}

/// A structure to control the flash of an attached microchip.
///
/// Once constructed it can be used to program date to the flash.
pub(super) struct Flasher<'session> {
    session: &'session mut Session,
    core_index: usize,
    flash_algorithm: FlashAlgorithm,
    progress: FlashProgress,
}

/// The byte used to fill the stack when checking for stack overflows.
const STACK_FILL_BYTE: u8 = 0x56;

impl<'session> Flasher<'session> {
    pub(super) fn new(
        session: &'session mut Session,
        core_index: usize,
        raw_flash_algorithm: &RawFlashAlgorithm,
        progress: FlashProgress,
    ) -> Result<Self, FlashError> {
        let target = session.target();

        let flash_algorithm = FlashAlgorithm::assemble_from_raw_with_core(
            raw_flash_algorithm,
            &target.cores[core_index].name,
            target,
        )?;

        let mut this = Self {
            session,
            core_index,
            flash_algorithm,
            progress,
        };

        this.load()?;

        Ok(this)
    }

    pub(super) fn flash_algorithm(&self) -> &FlashAlgorithm {
        &self.flash_algorithm
    }

    pub(super) fn double_buffering_supported(&self) -> bool {
        self.flash_algorithm.page_buffers.len() > 1
    }

    fn load(&mut self) -> Result<(), FlashError> {
        tracing::debug!("Initializing the flash algorithm.");
        let algo = &self.flash_algorithm;

        // Attach to memory and core.
        let mut core = self
            .session
            .core(self.core_index)
            .map_err(FlashError::Core)?;

        // TODO: we probably want a full system reset here to make sure peripherals don't interfere.
        tracing::debug!("Reset and halt core {}", self.core_index);
        core.reset_and_halt(Duration::from_millis(500))
            .map_err(FlashError::ResetAndHalt)?;

        // TODO: Possible special preparation of the target such as enabling faster clocks for the flash e.g.

        // Load flash algorithm code into target RAM.
        tracing::debug!("Downloading algorithm code to {:#010x}", algo.load_address);

        core.write_32(algo.load_address, algo.instructions.as_slice())
            .map_err(FlashError::Core)?;

        let mut data = vec![0; algo.instructions.len()];
        core.read_32(algo.load_address, &mut data)
            .map_err(FlashError::Core)?;

        for (offset, (original, read_back)) in algo.instructions.iter().zip(data.iter()).enumerate()
        {
            if original == read_back {
                continue;
            }

            tracing::error!(
                "Failed to verify flash algorithm. Data mismatch at address {:#010x}",
                algo.load_address + (4 * offset) as u64
            );
            tracing::error!("Original instruction: {:#010x}", original);
            tracing::error!("Readback instruction: {:#010x}", read_back);

            tracing::error!("Original: {:x?}", &algo.instructions);
            tracing::error!("Readback: {:x?}", &data);

            return Err(FlashError::FlashAlgorithmNotLoaded);
        }

        if algo.stack_overflow_check {
            // Fill the stack with known data.
            let stack_bottom = algo.stack_top - algo.stack_size;
            let fill = vec![STACK_FILL_BYTE; algo.stack_size as usize];
            core.write_8(stack_bottom, &fill)
                .map_err(FlashError::Core)?;
        }

        tracing::debug!("RAM contents match flashing algo blob.");

        Ok(())
    }

    pub(super) fn init<O: Operation>(
        &mut self,
        clock: Option<u32>,
    ) -> Result<ActiveFlasher<'_, O>, FlashError> {
        // Attach to memory and core.
        let mut core = self
            .session
            .core(self.core_index)
            .map_err(FlashError::Core)?;

        let instruction_set = core.instruction_set().map_err(FlashError::Core)?;

        tracing::debug!("Preparing Flasher for operation {}", O::NAME);
        let mut flasher = ActiveFlasher::<O> {
            core,
            instruction_set,
            rtt: None,
            progress: &self.progress,
            flash_algorithm: &self.flash_algorithm,
            _operation: PhantomData,
        };

        flasher.init(clock)?;

        Ok(flasher)
    }

    pub(super) fn run_erase_all(&mut self) -> Result<(), FlashError> {
        self.progress.started_erasing();
        let result = if self.session.has_sequence_erase_all() {
            fn run(flasher: &mut Flasher) -> Result<(), FlashError> {
                flasher
                    .session
                    .sequence_erase_all()
                    .map_err(|e| FlashError::ChipEraseFailed {
                        source: Box::new(e),
                    })?;
                // We need to reload the flasher, since the debug sequence erase
                // may have invalidated any previously invalid state
                flasher.load()
            }

            run(self)
        } else {
            self.run_erase(|active| active.erase_all())
        };

        match result.is_ok() {
            true => self.progress.finished_erasing(),
            false => self.progress.failed_erasing(),
        }

        result
    }

    pub(super) fn run_erase<T, F>(&mut self, f: F) -> Result<T, FlashError>
    where
        F: FnOnce(&mut ActiveFlasher<'_, Erase>) -> Result<T, FlashError> + Sized,
    {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub(super) fn run_program<T, F>(&mut self, f: F) -> Result<T, FlashError>
    where
        F: FnOnce(&mut ActiveFlasher<'_, Program>) -> Result<T, FlashError> + Sized,
    {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub(super) fn run_verify<T, F>(&mut self, f: F) -> Result<T, FlashError>
    where
        F: FnOnce(&mut ActiveFlasher<'_, Verify>) -> Result<T, FlashError> + Sized,
    {
        // TODO: Fix those values (None, None).
        let mut active = self.init(None)?;
        let r = f(&mut active)?;
        active.uninit()?;
        Ok(r)
    }

    pub(super) fn is_chip_erase_supported(&self) -> bool {
        self.session.has_sequence_erase_all() || self.flash_algorithm().pc_erase_all.is_some()
    }

    /// Program the contents of given `FlashBuilder` to the flash.
    ///
    /// If `restore_unwritten_bytes` is `true`, all bytes of a sector,
    /// that are not to be written during flashing will be read from the flash first
    /// and written again once the sector is erased.
    pub(super) fn program(
        &mut self,
        region: &NvmRegion,
        flash_builder: &FlashBuilder,
        restore_unwritten_bytes: bool,
        enable_double_buffering: bool,
        skip_erasing: bool,
    ) -> Result<(), FlashError> {
        tracing::debug!("Starting program procedure.");
        // Convert the list of flash operations into flash sectors and pages.
        let mut flash_layout = self.flash_layout(region, flash_builder, restore_unwritten_bytes)?;

        tracing::debug!("Double Buffering enabled: {:?}", enable_double_buffering);
        tracing::debug!(
            "Restoring unwritten bytes enabled: {:?}",
            restore_unwritten_bytes
        );

        // Read all fill areas from the flash.
        self.progress.started_filling();

        if restore_unwritten_bytes {
            for fill in flash_layout.fills.iter() {
                let t = Instant::now();
                let page = &mut flash_layout.pages[fill.page_index()];
                let result = self.fill_page(page, fill);

                // If we encounter an error, catch it, gracefully report the failure and return the error.
                if result.is_err() {
                    self.progress.failed_filling();
                    return result;
                } else {
                    self.progress.page_filled(fill.size(), t.elapsed());
                }
            }
        }

        // We successfully finished filling.
        self.progress.finished_filling();

        let flash_encoder = FlashEncoder::new(self.flash_algorithm.transfer_encoding, flash_layout);

        // Skip erase if necessary (i.e. chip erase was done before)
        if !skip_erasing {
            // Erase all necessary sectors
            self.sector_erase(&flash_encoder)?;
        }

        // Flash all necessary pages.
        if self.double_buffering_supported() && enable_double_buffering {
            self.program_double_buffer(&flash_encoder)?;
        } else {
            self.program_simple(&flash_encoder)?;
        };

        Ok(())
    }

    /// Fills all the bytes of `current_page`.
    ///
    /// If `restore_unwritten_bytes` is `true`, all bytes of the page,
    /// that are not to be written during flashing will be read from the flash first
    /// and written again once the page is programmed.
    pub(super) fn fill_page(
        &mut self,
        page: &mut FlashPage,
        fill: &FlashFill,
    ) -> Result<(), FlashError> {
        let page_offset = (fill.address() - page.address()) as usize;
        let page_slice = &mut page.data_mut()[page_offset..][..fill.size() as usize];
        self.run_verify(|active| {
            active
                .core
                .read(fill.address(), page_slice)
                .map_err(FlashError::Core)
        })
    }

    /// Programs the pages given in `flash_layout` into the flash.
    fn program_simple(&mut self, flash_encoder: &FlashEncoder) -> Result<(), FlashError> {
        self.progress
            .started_programming(flash_encoder.program_size());

        let result = self.run_program(|active| {
            for page in flash_encoder.pages() {
                active
                    .program_page(page)
                    .map_err(|error| FlashError::PageWrite {
                        page_address: page.address(),
                        source: Box::new(error),
                    })?;
            }
            Ok(())
        });

        match result.is_ok() {
            true => self.progress.finished_programming(),
            false => self.progress.failed_programming(),
        }

        result
    }

    /// Perform an erase of all sectors given in `flash_layout`.
    fn sector_erase(&mut self, flash_encoder: &FlashEncoder) -> Result<(), FlashError> {
        self.progress.started_erasing();

        let result = self.run_erase(|active| {
            for sector in flash_encoder.sectors() {
                active
                    .erase_sector(sector)
                    .map_err(|e| FlashError::EraseFailed {
                        sector_address: sector.address(),
                        source: Box::new(e),
                    })?;
            }
            Ok(())
        });

        match result.is_ok() {
            true => self.progress.finished_erasing(),
            false => self.progress.failed_erasing(),
        }

        result
    }

    /// Flash a program using double buffering.
    ///
    /// This uses two buffers to increase the flash speed.
    /// While the data from one buffer is programmed, the
    /// data for the next page is already downloaded
    /// into the next buffer.
    ///
    /// This is only possible if the RAM is large enough to
    /// fit at least two page buffers. See [Flasher::double_buffering_supported].
    fn program_double_buffer(&mut self, flash_encoder: &FlashEncoder) -> Result<(), FlashError> {
        let mut current_buf = 0;
        self.progress
            .started_programming(flash_encoder.program_size());

        let result = self.run_program(|active| {
            let mut t = Instant::now();
            let mut last_page_address = 0;
            for page in flash_encoder.pages() {
                // At the start of each loop cycle load the next page buffer into RAM.
                let buffer_address = active.load_page_buffer(page.data(), current_buf)?;

                // Then wait for the active RAM -> Flash copy process to finish.
                // Also check if it finished properly. If it didn't, return an error.
                active.wait_for_write_end(last_page_address)?;

                last_page_address = page.address();
                active.progress.page_programmed(page.size(), t.elapsed());

                t = Instant::now();

                // Start the next copy process.
                active.start_program_page_with_buffer(
                    buffer_address,
                    page.address(),
                    page.size() as u64,
                )?;

                // Swap the buffers
                if current_buf == 1 {
                    current_buf = 0;
                } else {
                    current_buf = 1;
                }
            }

            active.wait_for_write_end(last_page_address)
        });

        match result.is_ok() {
            true => self.progress.finished_programming(),
            false => self.progress.failed_programming(),
        }

        result
    }

    pub(super) fn flash_layout(
        &self,
        region: &NvmRegion,
        flash_builder: &FlashBuilder,
        restore_unwritten_bytes: bool,
    ) -> Result<FlashLayout, FlashError> {
        flash_builder.build_sectors_and_pages(
            region,
            &self.flash_algorithm,
            restore_unwritten_bytes,
        )
    }
}

struct Registers {
    pc: u32,
    r0: Option<u32>,
    r1: Option<u32>,
    r2: Option<u32>,
    r3: Option<u32>,
}

impl Debug for Registers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:#010x} ({:?}, {:?}, {:?}, {:?})",
            self.pc, self.r0, self.r1, self.r2, self.r3
        )
    }
}

fn into_reg(val: u64) -> Result<u32, FlashError> {
    let reg_value: u32 = val
        .try_into()
        .map_err(|_| FlashError::RegisterValueNotSupported(val))?;

    Ok(reg_value)
}

pub(super) struct ActiveFlasher<'op, O: Operation> {
    core: Core<'op>,
    instruction_set: InstructionSet,
    rtt: Option<Rtt>,
    progress: &'op FlashProgress,
    flash_algorithm: &'op FlashAlgorithm,
    _operation: PhantomData<O>,
}

impl<O: Operation> ActiveFlasher<'_, O> {
    #[tracing::instrument(name = "Call to flash algorithm init", skip(self, clock))]
    pub(super) fn init(&mut self, clock: Option<u32>) -> Result<(), FlashError> {
        let algo = &self.flash_algorithm;

        // Skip init routine if not present.
        let Some(pc_init) = algo.pc_init else {
            return Ok(());
        };

        let address = self.flash_algorithm.flash_properties.address_range.start;
        let error_code = self
            .call_function_and_wait(
                &Registers {
                    pc: into_reg(pc_init)?,
                    r0: Some(into_reg(address)?),
                    r1: clock.or(Some(0)),
                    r2: Some(O::OPERATION),
                    r3: None,
                },
                true,
                INIT_TIMEOUT,
            )
            .map_err(|error| FlashError::Init(Box::new(error)))?;

        if error_code != 0 {
            return Err(FlashError::RoutineCallFailed {
                name: "init",
                error_code,
            });
        }

        Ok(())
    }

    pub(super) fn uninit(&mut self) -> Result<(), FlashError> {
        tracing::debug!("Running uninit routine.");
        let algo = &self.flash_algorithm;

        // Skip uninit routine if not present.
        let Some(pc_uninit) = algo.pc_uninit else {
            return Ok(());
        };

        let error_code = self
            .call_function_and_wait(
                &Registers {
                    pc: into_reg(pc_uninit)?,
                    r0: Some(O::OPERATION),
                    r1: None,
                    r2: None,
                    r3: None,
                },
                false,
                INIT_TIMEOUT,
            )
            .map_err(|error| FlashError::Uninit(Box::new(error)))?;

        if error_code != 0 {
            return Err(FlashError::RoutineCallFailed {
                name: "uninit",
                error_code,
            });
        }

        Ok(())
    }

    fn call_function_and_wait(
        &mut self,
        registers: &Registers,
        init: bool,
        duration: Duration,
    ) -> Result<u32, FlashError> {
        self.call_function(registers, init)?;
        self.wait_for_completion(duration)
    }

    fn call_function(&mut self, registers: &Registers, init: bool) -> Result<(), FlashError> {
        tracing::debug!("Calling routine {:?}, init={})", registers, init);

        let algo = &self.flash_algorithm;
        let regs: &'static CoreRegisters = self.core.registers();

        let registers = [
            (self.core.program_counter(), Some(registers.pc)),
            (regs.argument_register(0), registers.r0),
            (regs.argument_register(1), registers.r1),
            (regs.argument_register(2), registers.r2),
            (regs.argument_register(3), registers.r3),
            (
                regs.core_register(9),
                if init {
                    Some(into_reg(algo.static_base)?)
                } else {
                    None
                },
            ),
            (
                self.core.stack_pointer(),
                if init {
                    Some(into_reg(algo.stack_top)?)
                } else {
                    None
                },
            ),
            (
                self.core.return_address(),
                // For ARM Cortex-M cores, we have to add 1 to the return address,
                // to ensure that we stay in Thumb mode.
                if self.instruction_set == InstructionSet::Thumb2 {
                    Some(into_reg(algo.load_address + 1)?)
                } else {
                    Some(into_reg(algo.load_address)?)
                },
            ),
        ];

        for (description, value) in registers {
            if let Some(v) = value {
                self.core.write_core_reg(description, v).map_err(|error| {
                    FlashError::Core(Error::WriteRegister {
                        register: description.to_string(),
                        source: Box::new(error),
                    })
                })?;

                if tracing::enabled!(Level::DEBUG) {
                    let value: u32 = self.core.read_core_reg(description).map_err(|error| {
                        FlashError::Core(Error::ReadRegister {
                            register: description.to_string(),
                            source: Box::new(error),
                        })
                    })?;

                    tracing::debug!(
                        "content of {} {:#x}: {:#010x} should be: {:#010x}",
                        description.name(),
                        description.id.0,
                        value,
                        v
                    );
                }
            }
        }

        // Ensure RISC-V `ebreak` instructions enter debug mode,
        // this is necessary for soft breakpoints to work.
        self.core
            .debug_on_sw_breakpoint(true)
            .map_err(FlashError::Core)?;

        // Resume target operation.
        self.core.run().map_err(FlashError::Run)?;

        if let Some(rtt_address) = self.flash_algorithm.rtt_control_block {
            match rtt::try_attach_to_rtt(
                &mut self.core,
                Duration::from_secs(1),
                &ScanRegion::Exact(rtt_address),
            ) {
                Ok(rtt) => self.rtt = Some(rtt),
                Err(rtt::Error::NoControlBlockLocation) => {}
                Err(error) => tracing::error!("RTT could not be initialized: {error}"),
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub(super) fn wait_for_completion(&mut self, timeout: Duration) -> Result<u32, FlashError> {
        tracing::debug!("Waiting for routine call completion.");
        let regs = self.core.registers();

        // Wait until halted state is active again.
        let start = Instant::now();

        loop {
            match self
                .core
                .status()
                .map_err(FlashError::UnableToReadCoreStatus)?
            {
                CoreStatus::Halted(_) => {
                    // Once the core is halted we know for sure all RTT data is written
                    // so we can read all of it.
                    self.read_rtt()?;
                    break;
                }
                CoreStatus::LockedUp => {
                    return Err(FlashError::UnexpectedCoreStatus {
                        status: CoreStatus::LockedUp,
                    });
                }
                _ => {} // All other statuses are okay: we'll just keep polling.
            }
            self.read_rtt()?;
            if start.elapsed() >= timeout {
                return Err(FlashError::Core(Error::Timeout));
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        self.check_for_stack_overflow()?;

        self.core
            .read_core_reg::<u32>(regs.result_register(0))
            .map_err(|error| {
                FlashError::Core(Error::ReadRegister {
                    register: regs.result_register(0).to_string(),
                    source: Box::new(error),
                })
            })
    }

    fn read_rtt(&mut self) -> Result<(), FlashError> {
        let Some(rtt) = &mut self.rtt else {
            return Ok(());
        };

        for channel in rtt.up_channels().iter() {
            let mut buffer = vec![0; channel.buffer_size()];
            match channel.read(&mut self.core, &mut buffer) {
                Ok(read) if read > 0 => {
                    let message = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let channel = channel.name().unwrap_or("unnamed");
                    tracing::debug!("RTT({channel}): {message}");
                    self.progress.message(message);
                }
                Ok(_) => (),
                Err(error) => tracing::debug!("Reading RTT failed: {error}"),
            }
        }

        Ok(())
    }

    fn check_for_stack_overflow(&mut self) -> Result<(), FlashError> {
        let algo = &self.flash_algorithm;

        if !algo.stack_overflow_check {
            return Ok(());
        }

        let stack_bottom = algo.stack_top - algo.stack_size;
        let read_back = self
            .core
            .read_word_8(stack_bottom)
            .map_err(FlashError::Core)?;

        if read_back != STACK_FILL_BYTE {
            return Err(FlashError::StackOverflowDetected { operation: O::NAME });
        }

        Ok(())
    }
}

impl<'probe> ActiveFlasher<'probe, Erase> {
    pub(super) fn erase_all(&mut self) -> Result<(), FlashError> {
        tracing::debug!("Erasing entire chip.");
        let algo = &self.flash_algorithm;

        let Some(pc_erase_all) = algo.pc_erase_all else {
            return Err(FlashError::ChipEraseNotSupported);
        };

        let result = self
            .call_function_and_wait(
                &Registers {
                    pc: into_reg(pc_erase_all)?,
                    r0: None,
                    r1: None,
                    r2: None,
                    r3: None,
                },
                false,
                Duration::from_secs(30),
            )
            .map_err(|error| FlashError::ChipEraseFailed {
                source: Box::new(error),
            })?;

        if result != 0 {
            Err(FlashError::ChipEraseFailed {
                source: Box::new(FlashError::RoutineCallFailed {
                    name: "chip_erase",
                    error_code: result,
                }),
            })
        } else {
            Ok(())
        }
    }

    pub(super) fn erase_sector(&mut self, sector: &FlashSector) -> Result<(), FlashError> {
        let address = sector.address();
        tracing::info!("Erasing sector at address {:#010x}", address);
        let t1 = Instant::now();

        let error_code = self.call_function_and_wait(
            &Registers {
                pc: into_reg(self.flash_algorithm.pc_erase_sector)?,
                r0: Some(into_reg(address)?),
                r1: None,
                r2: None,
                r3: None,
            },
            false,
            Duration::from_millis(
                self.flash_algorithm.flash_properties.erase_sector_timeout as u64,
            ),
        )?;
        tracing::info!(
            "Done erasing sector. Result is {}. This took {:?}",
            error_code,
            t1.elapsed()
        );

        if error_code != 0 {
            Err(FlashError::RoutineCallFailed {
                name: "erase_sector",
                error_code,
            })
        } else {
            self.progress.sector_erased(sector.size(), t1.elapsed());
            Ok(())
        }
    }
}

impl<'p> ActiveFlasher<'p, Program> {
    /// Transfers the buffer bytes to RAM.
    fn load_data(&mut self, address: u64, bytes: &[u8]) -> Result<(), FlashError> {
        tracing::debug!(
            "Loading {} bytes of data into RAM at address {:#010x}\n",
            bytes.len(),
            address
        );
        // TODO: Prevent security settings from locking the device.

        // In case some of the previous preprocessing forgets to pad the last page,
        // we will fill the missing bytes with the erased byte value.
        let empty = self.flash_algorithm.flash_properties.erased_byte_value;
        let words: Vec<u32> = bytes
            .chunks(std::mem::size_of::<u32>())
            .map(|a| {
                u32::from_le_bytes([
                    a[0],
                    a.get(1).copied().unwrap_or(empty),
                    a.get(2).copied().unwrap_or(empty),
                    a.get(3).copied().unwrap_or(empty),
                ])
            })
            .collect();

        let t1 = Instant::now();

        self.core
            .write_32(address, &words)
            .map_err(FlashError::Core)?;

        tracing::info!(
            "Took {:?} to download {} byte page into ram",
            t1.elapsed(),
            bytes.len()
        );

        Ok(())
    }

    pub(super) fn program_page(&mut self, page: &FlashPage) -> Result<(), FlashError> {
        let t1 = Instant::now();

        let address = page.address();
        let bytes = page.data();

        tracing::info!(
            "Flashing page at address {:#08x} with size: {}",
            address,
            bytes.len()
        );

        // Transfer the bytes to RAM.
        let begin_data = self.load_page_buffer(bytes, 0)?;

        self.start_program_page_with_buffer(begin_data, address, bytes.len() as u64)?;
        self.wait_for_write_end(address)?;

        tracing::info!("Flashing took: {:?}", t1.elapsed());

        self.progress.page_programmed(page.size(), t1.elapsed());
        Ok(())
    }

    pub(super) fn start_program_page_with_buffer(
        &mut self,
        buffer_address: u64,
        page_address: u64,
        data_size: u64,
    ) -> Result<(), FlashError> {
        self.call_function(
            &Registers {
                pc: into_reg(self.flash_algorithm.pc_program_page)?,
                r0: Some(into_reg(page_address)?),
                r1: Some(into_reg(data_size)?),
                r2: Some(into_reg(buffer_address)?),
                r3: None,
            },
            false,
        )
        .map_err(|error| FlashError::PageWrite {
            page_address,
            source: Box::new(error),
        })?;

        Ok(())
    }

    /// Returns the address of the buffer that was used.
    pub(super) fn load_page_buffer(
        &mut self,
        bytes: &[u8],
        buffer_number: usize,
    ) -> Result<u64, FlashError> {
        // Ensure the buffer number is valid, otherwise there is a bug somewhere
        // in the flashing code.
        assert!(
            buffer_number < self.flash_algorithm.page_buffers.len(),
            "Trying to use non-existing buffer ({}/{}) for flashing. This is a bug. Please report it.",
            buffer_number, self.flash_algorithm.page_buffers.len()
        );

        let buffer_address = self.flash_algorithm.page_buffers[buffer_number];
        self.load_data(buffer_address, bytes)?;

        Ok(buffer_address)
    }

    fn wait_for_write_end(&mut self, last_page_address: u64) -> Result<(), FlashError> {
        let timeout = Duration::from_millis(
            self.flash_algorithm.flash_properties.program_page_timeout as u64,
        );
        self.wait_for_completion(timeout)
            .and_then(|result| {
                if result == 0 {
                    Ok(())
                } else {
                    Err(FlashError::RoutineCallFailed {
                        name: "program_page",
                        error_code: result,
                    })
                }
            })
            .map_err(|error| FlashError::PageWrite {
                page_address: last_page_address,
                source: Box::new(error),
            })
    }
}
