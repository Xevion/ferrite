//! Task stacks from firmware pages, each with a not-present guard page below it: through the firmware's
//! page attribute protocol, or, where the firmware refuses, by editing the page tables directly.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::ManuallyDrop;
use core::num::NonZeroUsize;
use core::ptr::NonNull;

use corosensei::stack::{Stack, StackPointer};
use ferrite_efi::paging::GuardPage;
use ferrite_hal::PhysAddr;
use ferrite_lkpi::sched::{CANARY_WORDS, PAINT, StackAllocError, StackSource};
use uefi::boot::{self, AllocateType, MemoryType, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::mem::memory_map::MemoryAttribute;
use uefi::proto::ProtocolPointer;
use uefi::proto::security::MemoryProtection;
use uefi::proto::unsafe_protocol;
use uefi::{Status, StatusExt as _};

const PAGE: usize = 4096;
const WORD: usize = size_of::<u64>();
/// `EFI_MEMORY_RP`: reads and writes fault, which on x86 means the page is not present.
const MEMORY_RP: u64 = 0x2000;

/// `EFI_CPU_ARCH_PROTOCOL`, of which only `SetMemoryAttributes` is called. The other members are
/// function pointers held as plain words so the layout matches.
#[repr(C)]
#[unsafe_protocol("26baccb1-6f42-11d4-bce7-0080c73c8881")]
struct CpuArch {
    flush_data_cache: usize,
    enable_interrupt: usize,
    disable_interrupt: usize,
    get_interrupt_state: usize,
    init: usize,
    register_interrupt_handler: usize,
    get_timer_value: usize,
    set_memory_attributes: unsafe extern "efiapi" fn(*const Self, u64, u64, u64) -> Status,
    number_of_timers: u32,
    dma_buffer_alignment: u32,
}

/// How a page is made to fault, whichever protocol the firmware offers.
enum Backend {
    CpuArch(ManuallyDrop<ScopedProtocol<CpuArch>>),
    MemoryAttribute(ManuallyDrop<ScopedProtocol<MemoryProtection>>),
}

/// A shared open of `P` that is never closed, because some firmware does not track a `GetProtocol`
/// open and fails the close.
fn open<P: ProtocolPointer + ?Sized>() -> Option<ManuallyDrop<ScopedProtocol<P>>> {
    let handle = boot::get_handle_for_protocol::<P>().ok()?;
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: a shared open whose only calls are the page attribute ones below.
    unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok().map(ManuallyDrop::new)
}

impl Backend {
    fn probe() -> Option<Self> {
        open::<CpuArch>().map(Self::CpuArch).or_else(|| open::<MemoryProtection>().map(Self::MemoryAttribute))
    }

    fn name(&self) -> &'static str {
        match self {
            Self::CpuArch(_) => "CPU Arch protocol SetMemoryAttributes",
            Self::MemoryAttribute(_) => "Memory Attribute protocol",
        }
    }

    /// Make the page at `page` fault on any access, or restore it.
    fn set_not_present(&self, page: usize, faulting: bool) -> uefi::Result {
        let base = page as u64;
        match self {
            Self::CpuArch(cpu) => {
                let attributes = if faulting { MEMORY_RP } else { 0 };
                let cpu: &CpuArch = cpu;
                // SAFETY: the protocol is the firmware's, and the page is one this program owns.
                unsafe { (cpu.set_memory_attributes)(cpu, base, PAGE as u64, attributes) }.to_result()
            },
            Self::MemoryAttribute(protocol) => {
                let range = base..base + PAGE as u64;
                if faulting {
                    protocol.set_memory_attributes(range, MemoryAttribute::READ_PROTECT)
                } else {
                    protocol.clear_memory_attributes(range, MemoryAttribute::READ_PROTECT)
                }
            },
        }
    }
}

/// Stack ranges the source has handed out and what its stacks reached, for the end-of-run report.
#[derive(Default)]
struct Registry {
    /// `(limit, base)` of each stack in use.
    live: Vec<(usize, usize)>,
    allocated: usize,
    guarded: usize,
    deepest: usize,
}

/// Bytes from `base` down to the deepest word the task wrote, as the scheduler paints stacks.
fn used(limit: usize, base: usize) -> usize {
    let words = (base - limit) / WORD;
    let untouched = (CANARY_WORDS..words)
        // SAFETY: the range is a stack the scheduler painted, still allocated and not running.
        .take_while(|&i| unsafe { core::ptr::with_exposed_provenance::<u64>(limit + i * WORD).read() } == PAINT)
        .count();
    base - (limit + (CANARY_WORDS.min(words) + untouched) * WORD)
}

#[derive(Clone, Copy, Debug)]
pub struct StackReport {
    pub allocated: usize,
    pub guarded: usize,
    /// The most any stack, live or released, has used.
    pub deepest: usize,
    /// What each stack offers above its guard page.
    pub size: usize,
}

/// A view of the source's stacks that outlives its move into the scheduler.
#[derive(Clone)]
pub struct StackStats {
    registry: Rc<RefCell<Registry>>,
    size: usize,
}

impl StackStats {
    pub fn report(&self) -> StackReport {
        let registry = self.registry.borrow();
        let live = registry.live.iter().map(|&(limit, base)| used(limit, base)).max().unwrap_or(0);
        StackReport {
            allocated: registry.allocated,
            guarded: registry.guarded,
            deepest: registry.deepest.max(live),
            size: self.size,
        }
    }
}

/// How a stack's lowest page was made to fault.
enum Guard {
    /// Through the firmware's protocol, which has to be asked to undo it.
    Firmware,
    /// In the page tables directly; present again when this drops.
    Tables(GuardPage),
}

/// A run of firmware pages: a guard page, then the stack.
pub struct PageStack {
    low: NonNull<u8>,
    size: usize,
    guard: Option<Guard>,
}

impl PageStack {
    fn limit_addr(&self) -> usize {
        self.low.as_ptr().addr() + PAGE
    }
}

// SAFETY: the range from `limit` to `base` is page aligned, writable, owned by this value until the source
// frees it, and above a page that is either a guard or spare.
unsafe impl Stack for PageStack {
    fn base(&self) -> StackPointer {
        NonZeroUsize::new(self.limit_addr() + self.size).unwrap_or(NonZeroUsize::MIN)
    }

    fn limit(&self) -> StackPointer {
        NonZeroUsize::new(self.limit_addr()).unwrap_or(NonZeroUsize::MIN)
    }
}

/// Stacks of `size` bytes from `AllocatePages`, each with a guard page below.
///
/// The guard page is made not-present, so an overrun faults at once: through the firmware's protocol
/// when it has one that works, else in the page tables. Should both fail it is spare, and the
/// scheduler's canary words catch the overrun when the task next switches out.
pub struct PageStacks {
    /// `None` once the firmware has no protocol or refused one guard.
    backend: Option<Backend>,
    /// Cleared when editing the page tables failed, so later stacks go without.
    tables: bool,
    registry: Rc<RefCell<Registry>>,
    size: usize,
}

impl PageStacks {
    pub fn new(size: usize) -> Self {
        let backend = Backend::probe();
        match &backend {
            Some(backend) => log::info!("task stack guard pages: {}", backend.name()),
            None => log::info!("task stack guard pages: the page tables, since the firmware has no protocol"),
        }
        Self { backend, tables: true, registry: Rc::default(), size }
    }

    pub fn stats(&self) -> StackStats {
        StackStats { registry: self.registry.clone(), size: self.size }
    }

    fn guard(&mut self, page: usize) -> Option<Guard> {
        if let Some(backend) = &self.backend {
            match backend.set_not_present(page, true) {
                Ok(()) => return Some(Guard::Firmware),
                Err(error) => {
                    log::info!("task stack guard pages: {} refused one ({error}); the page tables", backend.name());
                    self.backend = None;
                },
            }
        }
        if !self.tables {
            return None;
        }
        // SAFETY: the allocation's lowest page, below the stack, which nothing uses.
        match unsafe { GuardPage::new(PhysAddr::new(page as u64)) } {
            Ok(guard) => Some(Guard::Tables(guard)),
            Err(error) => {
                log::warn!("making a stack guard page not-present failed: {error}; later stacks go without");
                self.tables = false;
                None
            },
        }
    }
}

impl StackSource for PageStacks {
    type Stack = PageStack;

    fn allocate(&mut self, size: usize) -> Result<PageStack, StackAllocError> {
        let size = size.next_multiple_of(PAGE);
        let low = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, size / PAGE + 1).map_err(
            |error| {
                log::error!("allocating a {size}-byte task stack failed: {error}");
                StackAllocError { size }
            },
        )?;
        let guard = self.guard(low.as_ptr().addr());
        let stack = PageStack { low, size, guard };
        let mut registry = self.registry.borrow_mut();
        registry.live.push((stack.limit_addr(), stack.limit_addr() + size));
        registry.allocated += 1;
        registry.guarded += usize::from(stack.guard.is_some());
        drop(registry);
        Ok(stack)
    }

    fn release(&mut self, stack: PageStack) {
        let limit = stack.limit_addr();
        let reached = used(limit, limit + stack.size);
        let mut registry = self.registry.borrow_mut();
        registry.live.retain(|&(l, _)| l != limit);
        registry.deepest = registry.deepest.max(reached);
        drop(registry);
        match stack.guard {
            Some(Guard::Firmware) => {
                let page = stack.low.as_ptr().addr();
                let restored = self.backend.as_ref().map_or(Ok(()), |backend| backend.set_not_present(page, false));
                if let Err(error) = restored {
                    log::error!("restoring the stack guard page at {page:#x} failed: {error}; its pages are kept");
                    return;
                }
            },
            Some(Guard::Tables(guard)) => drop(guard),
            None => {},
        }
        // SAFETY: the pages are the allocation `allocate` made, and the scheduler no longer runs on them.
        if let Err(error) = unsafe { boot::free_pages(stack.low, stack.size / PAGE + 1) } {
            log::warn!("freeing a task stack failed: {error}");
        }
    }
}
