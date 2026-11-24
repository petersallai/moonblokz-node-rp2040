# moonblokz-node-rp2040

MoonBlokz node firmware for RP2040 boards with an SX1262 LoRa radio (Raspberry Pi Pico + Waveshare board). The board binary of the node (DEC-288): it owns the executor, the core assignment, the peripherals, the stacks and the capacity profile, and runs the chain through [`moonblokz-node-runtime`](https://github.com/petersallai/moonblokz-node-runtime).

- **Core 0** runs the blockchain task, which owns the storage: on the RP2040 only Core 0 may erase or program flash (DEC-289).
- **Core 1** runs the USB console (later also the radio).

## Build

The chain's identity is baked into the build (DEC-291, DEC-292). Both variables must name files; the build fails without them.

| Variable | Content |
|---|---|
| `MOONBLOKZ_NODE_ZERO_PUBLIC_KEY` | node #0's public key, raw bytes: 32 (`schnorr`) / 96 (`bls-min-sig`) / 48 (`bls-min-pk`). A wrong size is a compile error. |
| `MOONBLOKZ_GENESIS_CHAIN_CONFIG` | the genesis chain-config **content** (no signature), used only by node #0's `/G_` command. |

Produce both with `config-encoder` (in `moonblokz-configuration/tools`), built with the same scheme feature as the firmware:

```sh
cargo run -p moonblokz-config-encoder --no-default-features --features schnorr -- \
    chain.cfg --key node-zero.hex --content genesis-config.bin --public-key node-zero.pk
```

Then build (absolute paths; the scheme is one of `schnorr` (default), `bls-min-sig`, `bls-min-pk`):

```sh
MOONBLOKZ_NODE_ZERO_PUBLIC_KEY=$PWD/node-zero.pk \
MOONBLOKZ_GENESIS_CHAIN_CONFIG=$PWD/genesis-config.bin \
cargo build --release --no-default-features --features schnorr
```

Add `,stack-report` to the features for a measurement build. It paints both cores' stacks, logs their high-water marks every 5 s, and logs a Core 1 heartbeat. Never ship it.

Convert `target/thumbv6m-none-eabi/release/moonblokz-node-rp2040` to UF2 (for example `picotool uf2 convert` or `elf2uf2-rs`) and copy it to the board in BOOTSEL mode.

## Console commands (USB serial)

| Command | When | Effect |
|---|---|---|
| `/I_<node_id>_<private_key_hex>_` | not provisioned | Erases all storage and stores the node identity. The key (64 hex digits) is checked for the scheme first (DEC-294). Accepted at once on an uninitialized control plane; otherwise only after `/E_`. |
| `/E_` | the control plane holds data this firmware cannot use | Arms the erase: the next `/I_` may erase all storage, the stored chain included. |
| `/G_<initial_total_network_currency>_` | node #0, booted on empty storage | Founds the chain (FR54): blocks #0 and #1. The currency must be positive. Refused anywhere else (DEC-292). |

## Recovery

A node that cannot serve its stored chain says so in its boot log (`boot: Rejected(...)`, or `Control plane unreadable (...)` before provisioning). The usual cause is a firmware that does not match the stored chain: another anchor, signature scheme or block-size tier, or a `MAX_NODES` below the chain's node cap. **Flash the matching build**; nothing is lost. Only when the stored data is truly unusable, start over: before provisioning (an unreadable control plane), send `/E_` and then `/I_`; after a boot that reports `boot: Rejected(...)` the console does not accept `/E_`, so erase the flash (for example `picotool erase`) and provision again. Either way all storage, the chain included, is lost. A Core 0 stack overflow halts the node (HardFault); power-cycle it.

## Hardware run (Story 12.1 AC8)

Build each scheme with `,stack-report` and watch the USB log.

1. **A node other than #0.** Flash it and send `/I_<id>_<key>_`. After `Node <id> initialized` the boot continues without a reboot: `Node <id> booting the blockchain`, then `boot: StartedCollecting`. With no radio there is no Stored head, so no tick follows (`NextCall::Idle`).
2. **Node #0.** Flash it with the anchor and genesis-config files made from node #0's key, send `/I_0_<key>_`, wait for `boot: StartedCollecting`, then send `/G_<currency>_`. It logs `genesis: block #0 [..], block #1 [..]`. This is the first real flash write by the blockchain.
3. **Reboot node #0.** It logs `boot: ResumedReady` with no `BackendIo`. A `/G_` now logs `genesis refused: not node #0 booted on empty storage`.
4. **Stacks.** Every 5 s the node logs `stack high-water: core0 <n> B, core1 <m> B of 8192`, and `core1 heartbeat` keeps arriving while Core 0 computes during `/G_`. Record the highest marks per scheme against `footprint.py`'s `stack_required` (Core 0) and the 8 KiB `CORE1_STACK`.
5. **Record** the outcomes, the marks, and the boot-to-`boot:` latency (O-023) in the story's Dev Agent Record.

## Footprint

`python3 moonblokz-node-rp2040/tools/footprint/footprint.py --scheme S` (from the workspace root) builds the node and the footprint probe. It reports:
- the 264 KB inequality, including the blockchain task's pool and the Core 0 stack gate (DEC-289);
- the largest `MAX_NODES` that fits.

It writes its own dummy anchor and config files.
