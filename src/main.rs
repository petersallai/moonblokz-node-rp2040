// MoonBlokz node firmware for the Raspberry Pi Pico (RP2040) with a Waveshare
// SX1262 LoRa board.
//
// Core assignment (DEC-289): Core 0 runs the blockchain task, which owns the
// storage, because on the RP2040 only Core 0 may erase or program flash
// (`embassy-rp` `flash.rs` `in_ram`); Core 1 runs the USB console (later also
// the radio), so it stays responsive while Core 0 verifies for seconds.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use embassy_executor::{Executor, Spawner};
use embassy_rp::Peri;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::Stack;
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use moonblokz_crypto::{Crypto, CryptoTrait, PRIVATE_KEY_SIZE, PUBLIC_KEY_SIZE};
use moonblokz_node_runtime::LocalCommand;
use moonblokz_storage::{INIT_PARAMS_SIZE, MoonblokzStorage, StorageError, StorageTrait};
use static_cell::StaticCell;

/// Raw USB console lines, from the console on Core 1 to Core 0.
type CommandChannel =
    Channel<CriticalSectionRawMutex, [u8; rp_usb_console::USB_READ_BUFFER_SIZE], 4>;
static COMMAND_CHANNEL: CommandChannel = Channel::new();
/// Parsed local commands for the runtime (DEC-292).
static LOCAL_COMMANDS: Channel<CriticalSectionRawMutex, LocalCommand, 1> = Channel::new();
/// Set by the blockchain task once provisioning is done, so that only one task
/// reads `COMMAND_CHANNEL` at a time.
static BOOTED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The RP2040 flash size, `__flash_size` in `memory.x`. The backend lays out its
/// slots from this and the start address of the storage region.
const FLASH_SIZE: usize = 2048 * 1024;
type NodeStorage = MoonblokzStorage<FLASH_SIZE>;

/// The chain's trust anchor, node #0's public key, baked at build (FR69,
/// DEC-291). A file of the wrong size for the scheme is a compile error.
const NODE_ZERO_PUBLIC_KEY: [u8; PUBLIC_KEY_SIZE] =
    *include_bytes!(env!("MOONBLOKZ_NODE_ZERO_PUBLIC_KEY"));
/// The genesis chain-config content, used only by node #0's `/G_` (DEC-292).
static GENESIS_CHAIN_CONFIG: &[u8] = include_bytes!(env!("MOONBLOKZ_GENESIS_CHAIN_CONFIG"));

// The capacity profile per scheme (DEC-254): a `MAX_NODES` whose static RAM and
// stacks fit 264 KB, beside the architecture §5 defaults. Story 12.1 re-fitted
// it to 800 / 240 / 95 against the real rp2040 storage and the blockchain task
// on Core 0; Story 12.2's stack diet restores 1000 / 258 / 211 exactly, the
// surplus kept as headroom (DEC-300). Keep in step with the footprint probe's
// `PROFILE_MAX_NODES`.
#[cfg(feature = "schnorr")]
const MAX_NODES: usize = 1000;
#[cfg(feature = "bls-min-sig")]
const MAX_NODES: usize = 258;
#[cfg(feature = "bls-min-pk")]
const MAX_NODES: usize = 211;
const SNAKE_CHAIN_LENGTH_MAX: u32 = 500;
const MAX_BLOCKS: usize = 600;
const MAX_BRANCH_COUNT: usize = 40;
const MAX_BLOCK_UTXO_OUTPUT: usize = 256;

// Core 1 runs the USB console (later the radio) on its own stack; the
// blockchain task runs on Core 0's main stack (DEC-289). Measured with the
// `stack-report` feature.
const CORE1_STACK_SIZE: usize = 8 * 1024;
static mut CORE1_STACK: Stack<CORE1_STACK_SIZE> = Stack::new();
static EXECUTOR_CORE1: StaticCell<Executor> = StaticCell::new();

/// The flash offset of the storage region, `__storage_flash_offset` in
/// `memory.x`. The region runs to the end of flash.
fn storage_flash_offset() -> usize {
    unsafe extern "C" {
        // Defined in the linker script.
        unsafe static __storage_flash_offset: u32;
    }

    core::ptr::addr_of!(__storage_flash_offset) as usize
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Reboot the node in the case of a panic. This is a simple way to recover from unexpected errors in an embedded environment.
    rp2040_hal::reset()
}

/// Parse a parameter from a command string.
/// Command format: `/I_param0_param1_param2_` (trailing underscore required).
/// Returns the parameter at the given index, or `None` if out of range.
fn parse_command_parameter(command: &str, parameter_index: usize) -> Option<&str> {
    // Skip the command prefix (e.g. "/I") by finding the first '_'
    let after_prefix = command.find('_').map(|i| &command[i + 1..])?;

    let mut current = after_prefix;
    let mut idx = 0;

    loop {
        match current.find('_') {
            Some(pos) => {
                if idx == parameter_index {
                    return Some(&current[..pos]);
                }
                current = &current[pos + 1..];
                idx += 1;
            }
            None => {
                // Last (or only) segment
                if idx == parameter_index {
                    return if current.is_empty() {
                        None
                    } else {
                        Some(current)
                    };
                }
                return None;
            }
        }
    }
}

/// The trimmed text of a console line, or `None` if it is not UTF-8. The
/// console reuses its line buffer, so the bytes after the first NUL are stale.
fn command_text(bytes: &[u8; rp_usb_console::USB_READ_BUFFER_SIZE]) -> Option<&str> {
    let line = bytes.split(|byte| *byte == 0).next().unwrap_or(&[]);
    core::str::from_utf8(line).ok().map(str::trim)
}

/// What one provisioning console line did.
enum Provision {
    Stored,
    EraseArmed,
    Ignored,
}

/// Waits for a `/I_<node_id>_<private_key_hex>_` command and writes the node's
/// identity to the storage control plane. Returns once it is stored.
///
/// `/I_` erases the whole storage region. When the control plane holds data
/// this firmware cannot use (`erase_armed` false), usually because the firmware
/// does not match the stored chain, `/I_` is refused until `/E_` confirms the
/// erase: flashing the matching build recovers the node without losing the
/// chain (Story 12.1 DN1).
async fn wait_for_initialization(storage: &mut NodeStorage, mut erase_armed: bool) {
    loop {
        let command_bytes = COMMAND_CHANNEL.receive().await;
        match provision(storage, &command_bytes, erase_armed) {
            Provision::Stored => return,
            Provision::EraseArmed => erase_armed = true,
            Provision::Ignored => {}
        }
    }
}

/// Handles one console line during provisioning. The key is checked with the
/// scheme's `Crypto::new` first (DEC-294). No error path echoes the line: it
/// may carry the key.
///
/// Out of line, like [`load_identity`], so its temporaries do not sit in the
/// blockchain task's poll frame under every later blockchain call.
#[inline(never)]
fn provision(
    storage: &mut NodeStorage,
    command_bytes: &[u8; rp_usb_console::USB_READ_BUFFER_SIZE],
    erase_armed: bool,
) -> Provision {
    let Some(trimmed) = command_text(command_bytes) else {
        log::warn!("The command is not UTF-8");
        return Provision::Ignored;
    };
    if trimmed == "/E_" {
        log::warn!("Erase armed: the next /I_ erases all storage, the stored chain included");
        return Provision::EraseArmed;
    }
    if !trimmed.starts_with("/I_") {
        log::warn!("Not provisioned: only /I_<node_id>_<private_key>_ is accepted");
        return Provision::Ignored;
    }
    if !erase_armed {
        log::warn!(
            "The storage holds data this firmware cannot use: flash the firmware that matches it, \
             or send /E_ to let /I_ erase all storage"
        );
        return Provision::Ignored;
    }
    let (Some(node_id_str), Some(key_hex)) = (
        parse_command_parameter(trimmed, 0),
        parse_command_parameter(trimmed, 1),
    ) else {
        log::warn!("Usage: /I_<node_id>_<private_key_hex>_");
        return Provision::Ignored;
    };
    let Ok(node_id) = node_id_str.parse::<u32>() else {
        log::warn!("The node id must be a decimal u32");
        return Provision::Ignored;
    };
    if key_hex.len() != 2 * PRIVATE_KEY_SIZE {
        log::warn!(
            "Private key hex must be {} characters, got {}",
            2 * PRIVATE_KEY_SIZE,
            key_hex.len()
        );
        return Provision::Ignored;
    }
    let mut private_key = [0u8; PRIVATE_KEY_SIZE];
    let key_bytes = key_hex.as_bytes();
    for (i, byte) in private_key.iter_mut().enumerate() {
        match (
            hex_nibble(key_bytes[i * 2]),
            hex_nibble(key_bytes[i * 2 + 1]),
        ) {
            (Some(h), Some(l)) => *byte = (h << 4) | l,
            _ => {
                log::warn!("Invalid hex in private key");
                return Provision::Ignored;
            }
        }
    }
    if Crypto::new(private_key).is_err() {
        log::warn!("The private key is not a valid key for this signature scheme");
        return Provision::Ignored;
    }
    match storage.init(private_key, node_id, [0u8; INIT_PARAMS_SIZE]) {
        Ok(()) => {
            log::info!("Node {} initialized", node_id);
            Provision::Stored
        }
        Err(error) => {
            log_storage_error("Failed to store the node identity", &error);
            Provision::Ignored
        }
    }
}

/// Reads `own_node_id` and the signing key from the control plane and builds
/// the crypto instance; the key goes straight into it and is not kept (FR68).
/// An error (logged) sends the node back to provisioning; it carries whether
/// `/I_` may erase without an `/E_` confirmation: only an uninitialized
/// control plane holds nothing to lose.
///
/// Both control-plane reads run here, on Core 0, before the storage moves into
/// the blockchain, so any replica repair they write happens on the core that
/// may write flash. Out of line, so the control-plane data (about 2 KB) does
/// not sit in the blockchain task's poll frame.
#[inline(never)]
fn load_identity(storage: &mut NodeStorage) -> Result<(u32, Crypto), bool> {
    let own_node_id = match storage.load_control_data() {
        Ok(control) => control.own_node_id,
        Err(StorageError::ControlPlaneUninitialized) => {
            log::info!("Node is not initialized; waiting for /I_<node_id>_<private_key>_");
            return Err(true);
        }
        Err(error) => {
            log_storage_error("Control plane unreadable", &error);
            return Err(false);
        }
    };
    let key = match storage.load_private_key() {
        Ok(key) => key,
        Err(error) => {
            log_storage_error("Signing key unreadable", &error);
            return Err(false);
        }
    };
    match Crypto::new(key) {
        Ok(crypto) => Ok((own_node_id, crypto)),
        Err(_) => {
            log::error!(
                "The stored signing key is not valid for this firmware's signature scheme: \
                 flash the matching firmware, or send /E_ then /I_ to re-provision"
            );
            Err(false)
        }
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Logs a storage error by name (`StorageError` has no `Debug` outside tests),
/// with what the operator can do.
fn log_storage_error(context: &str, error: &StorageError) {
    let name = match error {
        StorageError::BackendIo { code } => {
            log::error!(
                "{} (BackendIo {}): check the flash, or send /E_ then /I_ to erase and re-provision",
                context,
                code
            );
            return;
        }
        StorageError::ControlPlaneUninitialized => "ControlPlaneUninitialized",
        StorageError::ControlPlaneCorrupted => "ControlPlaneCorrupted",
        StorageError::ControlPlaneIncompatible => "ControlPlaneIncompatible",
        StorageError::InvalidIndex => "InvalidIndex",
        StorageError::BlockAbsent => "BlockAbsent",
        StorageError::IntegrityFailure => "IntegrityFailure",
        StorageError::ChainConfigurationAlreadySet => "ChainConfigurationAlreadySet",
        StorageError::InvalidConfiguration => "InvalidConfiguration",
    };
    log::error!(
        "{} ({}): flash the firmware that matches this node's storage, or send /E_ then /I_ to erase and re-provision",
        context,
        name
    );
}

/// The blockchain task (Core 0). It builds the storage and the crypto instance
/// itself, so they are not also held by `main`'s future (Story 12.1 AC2), and
/// the `Blockchain` lives in this task's `TaskStorage` through `run` (DEC-290).
#[embassy_executor::task]
async fn blockchain_task(flash: Peri<'static, FLASH>) {
    // In our case we use the whole flash storage for MoonBlokz. In other setups, you might want to reserve some flash space for other purposes and adjust the offset and size accordingly.
    let mut storage = match NodeStorage::new(flash, storage_flash_offset()) {
        Ok(storage) => storage,
        Err(_) => {
            // A misconfigured storage region is a build error, not a runtime
            // condition: report it and stop, rather than reboot in a loop.
            log::error!("Storage region is not page-aligned");
            loop {
                Timer::after_secs(1).await;
            }
        }
    };

    let (own_node_id, crypto) = loop {
        match load_identity(&mut storage) {
            Ok(identity) => break identity,
            Err(erase_armed) => wait_for_initialization(&mut storage, erase_armed).await,
        }
    };

    // Unpredictable, as DEC-230 requires (DEC-296).
    let prng_seed = RoscRng.next_u64();
    log::info!("Node {} booting the blockchain", own_node_id);
    BOOTED.signal(());

    // The blockchain borrows the storage, which stays in this task's future
    // once instead of being moved into every async layer (Story 12.1 AC7).
    moonblokz_node_runtime::run::<
        Crypto,
        &mut NodeStorage,
        CriticalSectionRawMutex,
        1,
        MAX_NODES,
        SNAKE_CHAIN_LENGTH_MAX,
        MAX_BLOCKS,
        MAX_BRANCH_COUNT,
        MAX_BLOCK_UTXO_OUTPUT,
    >(
        crypto,
        &mut storage,
        own_node_id,
        NODE_ZERO_PUBLIC_KEY,
        prng_seed,
        GENESIS_CHAIN_CONFIG,
        LOCAL_COMMANDS.receiver(),
    )
    .await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // First: a Core 0 stack overflow becomes a HardFault, which halts the core,
    // instead of running silently into `.bss`, where the blockchain lives (a
    // reset needs a watchdog; deferred-work). Core 1 gets its guard from
    // `spawn_core1`. The result is logged once the console runs.
    let stack_guard = embassy_rp::install_core0_stack_guard();
    #[cfg(feature = "stack-report")]
    stack_report::paint();

    let peripherals = embassy_rp::init(Default::default());

    let usb = peripherals.USB;
    embassy_rp::multicore::spawn_core1(
        peripherals.CORE1,
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR_CORE1.init(Executor::new());
            executor1.run(|spawner| {
                // On Core 1, so the USB interrupt is enabled on Core 1's NVIC.
                rp_usb_console::start(
                    spawner,
                    log::LevelFilter::Debug,
                    usb,
                    Some(COMMAND_CHANNEL.sender()),
                );
                #[cfg(feature = "stack-report")]
                spawner.spawn(stack_report::heartbeat()).unwrap();
            });
        },
    );

    //waiting a bit to let the terminal connect to the device
    let mut debug_indicator = Output::new(peripherals.PIN_25, Level::Low);
    debug_indicator.set_high();
    Timer::after_secs(5).await;
    debug_indicator.set_low();

    log::info!("Starting up");
    if stack_guard.is_err() {
        log::warn!("Core 0 stack guard not installed");
    }
    spawner.spawn(blockchain_task(peripherals.FLASH)).unwrap();
    #[cfg(feature = "stack-report")]
    spawner.spawn(stack_report::report()).unwrap();

    // After provisioning, the console's commands go to the runtime.
    BOOTED.wait().await;
    loop {
        let command_bytes = COMMAND_CHANNEL.receive().await;
        let Some(trimmed) = command_text(&command_bytes) else {
            continue;
        };
        if !trimmed.starts_with("/G_") {
            log::warn!("After boot only /G_<initial_total_network_currency>_ is accepted");
            continue;
        }
        match parse_command_parameter(trimmed, 0).map(str::parse::<u64>) {
            // A chain with no currency is never what the operator meant.
            Some(Ok(currency)) if currency > 0 => {
                LOCAL_COMMANDS
                    .send(LocalCommand::Genesis { currency })
                    .await
            }
            _ => log::warn!("Usage: /G_<initial_total_network_currency>_ (a positive integer)"),
        }
    }
}

/// Painted-stack high-water marks for both cores (Story 12.1 AC8). Not built in
/// release firmware.
#[cfg(feature = "stack-report")]
mod stack_report {
    use embassy_time::Timer;

    use super::{CORE1_STACK, CORE1_STACK_SIZE};

    const PAINT: u32 = 0xA5A5_A5A5;
    /// The MPU guard covers 32 bytes at the bottom of each stack (rounded up to
    /// a 32-byte boundary); stay clear of it.
    const GUARD: usize = 64;

    unsafe extern "C" {
        // cortex-m-rt: Core 0's stack runs from `_stack_start` down to `_stack_end`.
        static _stack_start: u32;
        static _stack_end: u32;
    }

    fn core0_range() -> (usize, usize) {
        let bottom = core::ptr::addr_of!(_stack_end) as usize + GUARD;
        let top = core::ptr::addr_of!(_stack_start) as usize;
        (bottom, top)
    }

    fn core1_range() -> (usize, usize) {
        let bottom = core::ptr::addr_of!(CORE1_STACK) as usize + GUARD;
        (bottom, bottom - GUARD + CORE1_STACK_SIZE)
    }

    /// Paints Core 0's free stack below the current frame and all of Core 1's
    /// stack; Core 1 has not started yet.
    pub fn paint() {
        let (bottom, _) = core0_range();
        let below_sp = (cortex_m::register::msp::read() as usize - 256) & !3;
        fill(bottom, below_sp);
        let (bottom, top) = core1_range();
        fill(bottom, top);
    }

    fn fill(from: usize, to: usize) {
        let mut word = from as *mut u32;
        while (word as usize) < to {
            // SAFETY: the range lies in a stack's unused part (below the live
            // frame, or Core 1's stack before it starts), word-aligned.
            unsafe {
                word.write_volatile(PAINT);
                word = word.add(1);
            }
        }
    }

    /// Bytes from the lowest overwritten word to the top of the range.
    fn used((bottom, top): (usize, usize)) -> usize {
        let mut word = bottom as *const u32;
        // SAFETY: reads inside the stack range, word-aligned.
        while (word as usize) < top && unsafe { word.read_volatile() } == PAINT {
            word = unsafe { word.add(1) };
        }
        top - word as usize
    }

    /// Logs both high-water marks every 5 s (Core 0).
    #[embassy_executor::task]
    pub async fn report() {
        loop {
            Timer::after_secs(5).await;
            log::info!(
                "stack high-water: core0 {} B, core1 {} B of {}",
                used(core0_range()),
                used(core1_range()),
                CORE1_STACK_SIZE
            );
        }
    }

    /// Shows that Core 1 keeps logging while Core 0 computes.
    #[embassy_executor::task]
    pub async fn heartbeat() {
        loop {
            Timer::after_secs(5).await;
            log::info!("core1 heartbeat");
        }
    }
}
