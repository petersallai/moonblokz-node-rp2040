// This example runs on the Raspberry Pi Pico with a Waveshare board containing a Semtech Sx1262 radio.
// It demonstrates LORA P2P send functionality.

#![no_std]
#![no_main]

use embassy_time::Timer;

use embassy_executor::Executor;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use moonblokz_storage::{INIT_PARAMS_SIZE, MoonblokzStorage, StorageTrait};
use static_cell::StaticCell;

type CommandChannel =
    Channel<CriticalSectionRawMutex, [u8; rp_usb_console::USB_READ_BUFFER_SIZE], 4>;
static COMMAND_CHANNEL: CommandChannel = Channel::new();

use core::panic::PanicInfo;

/// The RP2040 flash size, `__flash_size` in `memory.x`. The backend lays out its
/// slots from this and the start address of the storage region.
const FLASH_SIZE: usize = 2048 * 1024;
type NodeStorage = MoonblokzStorage<FLASH_SIZE>;

// Core 1 requires its own stack and executor allocated statically. Core 1
// hosts the blockchain task, and Embassy tasks share their core's stack, so
// this is sized per signature scheme to the blockchain's measured peak plus
// ~25 % (architecture §8, DEC-137), rounded up to a KiB. The peak is
// `process_genesis` with `schnorr` (25 692 B → 32 KiB) and `bls-min-pk`
// (56 552 B, hash to G2 → 70 KiB), and with `bls-min-sig` the FR3 pass
// (`initialize_from_storage`, 46 584 B → 57 KiB), where Story 6.7's inline
// evidence verification runs (D6, DEC-263; tools/footprint). The chain's
// capacity profile that fits beside it in 264 KB (`MAX_NODES` 1000 / 258 / 211,
// DEC-254) lives in the footprint probe's `Blockchain` instantiation until
// this node instantiates the blockchain itself (the bridge, AR12 / Epic 12).
#[cfg(feature = "schnorr")]
const CORE1_STACK_SIZE: usize = 32 * 1024;
#[cfg(feature = "bls-min-sig")]
const CORE1_STACK_SIZE: usize = 57 * 1024;
#[cfg(feature = "bls-min-pk")]
const CORE1_STACK_SIZE: usize = 70 * 1024;
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

#[embassy_executor::task]
async fn core1_task() {
    loop {
        Timer::after_secs(1).await;
    }
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

/// Waits for a `/I_<node_id>_<private_key_hex>_` command and writes the node's
/// identity to the storage control plane. Returns once it is stored.
async fn wait_for_initialization(
    command_receiver: embassy_sync::channel::Receiver<
        'static,
        CriticalSectionRawMutex,
        [u8; rp_usb_console::USB_READ_BUFFER_SIZE],
        4,
    >,
    storage: &mut NodeStorage,
) {
    loop {
        let command_bytes = command_receiver.receive().await;
        if let Ok(command) = core::str::from_utf8(&command_bytes) {
            let trimmed = command.trim();
            if !trimmed.starts_with("/I_") {
                continue;
            }
            let param0 = parse_command_parameter(trimmed, 0);
            let param1 = parse_command_parameter(trimmed, 1);

            match (param0, param1) {
                (Some(node_id_str), Some(key_hex)) => {
                    let node_id = match node_id_str.parse::<u32>() {
                        Ok(id) => id,
                        Err(_) => {
                            log::warn!("Failed to parse node_id: {}", node_id_str);
                            continue;
                        }
                    };
                    let mut private_key = [0u8; 32];
                    if key_hex.len() != 64 {
                        log::warn!(
                            "Private key hex must be 64 characters, got {}",
                            key_hex.len()
                        );
                        continue;
                    }
                    let mut valid = true;
                    let key_bytes = key_hex.as_bytes();
                    let mut i = 0;
                    while i < 32 {
                        let hi = hex_nibble(key_bytes[i * 2]);
                        let lo = hex_nibble(key_bytes[i * 2 + 1]);
                        if let (Some(h), Some(l)) = (hi, lo) {
                            private_key[i] = (h << 4) | l;
                        } else {
                            valid = false;
                            break;
                        }
                        i += 1;
                    }
                    if !valid {
                        log::warn!("Invalid hex in private key: {}", key_hex);
                        continue;
                    }
                    match storage.init(private_key, node_id, [0u8; INIT_PARAMS_SIZE]) {
                        Ok(()) => {
                            log::info!("Node {} initialized", node_id);
                            return;
                        }
                        Err(_) => log::error!("Failed to store the node identity"),
                    }
                }
                _ => {
                    log::warn!(
                        "Received initialization command with insufficient parameters: {}",
                        trimmed
                    );
                }
            }
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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let peripherals = embassy_rp::init(Default::default());

    rp_usb_console::start(
        spawner,
        log::LevelFilter::Debug,
        peripherals.USB,
        Some(COMMAND_CHANNEL.sender()),
    );

    let command_receiver = COMMAND_CHANNEL.receiver();

    //waiting a bit to let the terminal connect to the device
    let mut debug_indicator = Output::new(peripherals.PIN_25, Level::Low);
    debug_indicator.set_high();
    Timer::after_secs(5).await;
    debug_indicator.set_low();

    log::info!("Starting up");
    //Storage initialization and loading of parameters
    log::debug!("Initializing storage");
    //In our case we use the whole flash storage for MoonBlokz. In other setups, you might want to reserve some flash space for other purposes and adjust the offset and size accordingly.
    let mut storage = match NodeStorage::new(peripherals.FLASH, storage_flash_offset()) {
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

    // The control plane holds the node identity. Its full contents are read
    // later by the blockchain's `initialize_from_storage`; here only whether
    // it exists matters.
    if storage.load_control_data().is_err() {
        log::info!("Node is not initialized; waiting for /I_<node_id>_<private_key>_");
        wait_for_initialization(command_receiver, &mut storage).await;
    }

    embassy_rp::multicore::spawn_core1(
        peripherals.CORE1,
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR_CORE1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(core1_task()).unwrap();
            });
        },
    );

    loop {
        Timer::after_secs(1).await;
    }
}
