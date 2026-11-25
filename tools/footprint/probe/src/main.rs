//! Footprint probe (tools/footprint): the blockchain module as the firmware
//! would link it, at the scheme's RP2040 profile, behind opaque storage and
//! inputs. One `#[inline(never)]` root per API entry point, for the
//! stack-depth analysis.
//!
//! The profiles (Story 6.17, D12, DEC-254) live here until the node
//! instantiates the blockchain itself (the bridge, AR12 / Epic 12): `schnorr`
//! keeps the architecture 5 defaults, and each BLS variant caps `MAX_NODES`
//! at the largest count whose static RAM, Core 1 stack and the rest of the
//! system constraints §3.2 allocation fit 264 KB.
#![no_std]
#![no_main]

use core::hint::black_box;
use core::mem::MaybeUninit;
use cortex_m_rt::entry;
use embassy_rp as _;
use moonblokz_chain_types::{Block, BlockView, MAX_BLOCK_SIZE};
use moonblokz_configuration::{ChainConfiguration, NoopConfigChangeSink};
use moonblokz_crypto::{Crypto, CryptoTrait, MULTI_SIGNATURE_SIZE, PRIVATE_KEY_SIZE, PUBLIC_KEY_SIZE};
use moonblokz_storage::{ControlPlaneData, INIT_PARAMS_SIZE, StorageError, StorageTrait};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// The bytes of one flash-mapped slot.
static FLASH: [u8; MAX_BLOCK_SIZE] = [0u8; MAX_BLOCK_SIZE];

/// Storage whose answers the optimizer cannot predict.
struct Opaque;

impl StorageTrait for Opaque {
    fn init(&mut self, _k: [u8; PRIVATE_KEY_SIZE], _n: u32, _p: [u8; INIT_PARAMS_SIZE]) -> Result<(), StorageError> {
        black_box(Ok(()))
    }
    fn save_block(&mut self, idx: u32, block: &Block) -> Result<(), StorageError> {
        black_box((idx, block.serialized_bytes()));
        black_box(Ok(()))
    }
    #[cfg(storage_read_into)]
    fn read_block_into(&self, idx: u32, out: &mut [u8; MAX_BLOCK_SIZE]) -> Result<(), StorageError> {
        // A flash-mapped slot copied into the caller's buffer.
        black_box(idx);
        out.copy_from_slice(black_box(&FLASH[..]));
        black_box(Ok(()))
    }
    #[cfg(not(storage_read_into))]
    fn read_block(&self, idx: u32) -> Result<Block, StorageError> {
        // The pre-DEC-136 trait: a backend returns an owned block.
        black_box(idx);
        Block::from_bytes(black_box(&FLASH[..])).map_err(|_| StorageError::BackendIo { code: 1 })
    }
    fn capacity(&self) -> u32 {
        black_box(600)
    }
    fn set_chain_configuration(&mut self, block: &Block) -> Result<(), StorageError> {
        black_box(block.serialized_bytes());
        black_box(Ok(()))
    }
    fn load_control_data(&mut self) -> Result<ControlPlaneData, StorageError> {
        black_box(Err(StorageError::BackendIo { code: 2 }))
    }
}

/// The scheme's node capacity (`MAX_NODES`), the profile's one lever: the
/// largest count whose blockchain static RAM (about 58.5 / 57.8 / 60.0 KB
/// plus 50 / 114 / 66 B a node), its Core 1 stack and the rest of system
/// constraints §3.2 fit 264 KB (Story 6.17, DEC-254; re-fitted by Story 6.6
/// for the approval accumulator, and by Story 6.7 for `bls-min-sig`'s Core 1
/// of 57 KiB, DEC-263), and by Story 12.1 against the node's own build (the
/// real storage, the blockchain task on Core 0), restored to 1000 / 258 / 211
/// by Story 12.2's stack diet (DEC-300). `footprint.py --scheme` checks the
/// fit and prints the largest count (`budget.max_nodes_fit`). Keep in step
/// with the node's `MAX_NODES` (`src/main.rs`).
#[cfg(feature = "schnorr")]
const PROFILE_MAX_NODES: usize = 1000;
#[cfg(feature = "bls-min-sig")]
const PROFILE_MAX_NODES: usize = 258;
#[cfg(feature = "bls-min-pk")]
const PROFILE_MAX_NODES: usize = 211;

/// The profile's count, or `FOOTPRINT_MAX_NODES` when `footprint.py` sets it.
#[cfg(feature = "with-chain")]
const MAX_NODES: usize = match include!(concat!(env!("OUT_DIR"), "/max_nodes.rs")) {
    Some(n) => n,
    None => PROFILE_MAX_NODES,
};

#[cfg(feature = "with-chain")]
type Chain = moonblokz_blockchain::Blockchain<
    Crypto,
    Opaque,
    ChainConfiguration<NoopConfigChangeSink>,
    MAX_NODES,
    500,
    600,
    40,
    256,
>;

#[cfg(feature = "with-chain")]
static mut SLOT: MaybeUninit<Chain> = MaybeUninit::uninit();

#[entry]
fn main() -> ! {
    let crypto = Crypto::new(black_box([1u8; PRIVATE_KEY_SIZE])).ok().unwrap();
    #[cfg(feature = "with-chain")]
    {
        let limits = black_box(Chain::BUILD_LIMITS);
        let config = ChainConfiguration::new(NoopConfigChangeSink, limits);
        let key = black_box([0u8; PUBLIC_KEY_SIZE]);
        // SAFETY: single-threaded entry, the only reference to SLOT.
        let slot = unsafe { &mut *core::ptr::addr_of_mut!(SLOT) };
        let bc = Chain::init(slot, crypto, Opaque, config, black_box(5), key, black_box(7));
        probe_restart(bc);
        probe_genesis(bc);
        loop {
            probe_receive(bc);
            probe_support(bc);
            probe_tick(bc);
        }
    }
    #[cfg(not(feature = "with-chain"))]
    {
        // The slot image is in both builds, so it cancels out of the delta.
        black_box(&crypto);
        black_box(&FLASH);
        loop {}
    }
}

#[cfg(feature = "with-chain")]
#[unsafe(no_mangle)]
#[inline(never)]
fn probe_restart(bc: &mut Chain) {
    black_box(bc.initialize_from_storage(black_box(0)));
}

#[cfg(feature = "with-chain")]
#[unsafe(no_mangle)]
#[inline(never)]
fn probe_genesis(bc: &mut Chain) {
    black_box(bc.process_genesis(black_box(1_000_000), black_box(&[0u8; 16][..])).is_ok());
}

#[cfg(feature = "with-chain")]
#[unsafe(no_mangle)]
#[inline(never)]
fn probe_receive(bc: &mut Chain) {
    // The bridge's receive buffer lives outside the blockchain task's frames.
    static mut RX: [u8; MAX_BLOCK_SIZE] = [0u8; MAX_BLOCK_SIZE];
    // SAFETY: single-threaded probe, the only reference to RX.
    let rx = unsafe { &*core::ptr::addr_of!(RX) };
    if let Ok(view) = BlockView::from_bytes(black_box(&rx[..])) {
        black_box(bc.receive_block(view, black_box(1)));
    }
}

#[cfg(feature = "with-chain")]
#[unsafe(no_mangle)]
#[inline(never)]
fn probe_support(bc: &mut Chain) {
    // The bridge's radio receive buffer lives outside the blockchain task's frames.
    // The support body: three ids, the hash and the scheme's multi-signature.
    const BODY: usize = 44 + MULTI_SIGNATURE_SIZE;
    static mut RX: [u8; BODY] = [0u8; BODY];
    // SAFETY: single-threaded probe, the only reference to RX.
    let rx = unsafe { &*core::ptr::addr_of!(RX) };
    let len = black_box(BODY);
    if let Some(view) = moonblokz_blockchain::SupportView::from_bytes(black_box(&rx[..len])) {
        black_box(bc.receive_support(view, black_box(1)));
    }
}

#[cfg(feature = "with-chain")]
#[unsafe(no_mangle)]
#[inline(never)]
fn probe_tick(bc: &mut Chain) {
    black_box(bc.on_tick(black_box(1)));
}
