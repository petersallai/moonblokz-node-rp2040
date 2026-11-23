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
| `/I_<node_id>_<private_key_hex>_` | the control plane is uninitialized or unreadable | Stores the node identity. The key (64 hex digits) is checked for the scheme first (DEC-294). |
| `/G_<initial_total_network_currency>_` | node #0, booted on empty storage | Founds the chain (FR54): blocks #0 and #1. Refused anywhere else (DEC-292). |

## Footprint

`python3 moonblokz-node-rp2040/tools/footprint/footprint.py --scheme S` (from the workspace root) builds the node and the footprint probe. It reports:
- the 264 KB inequality, including the blockchain task's pool and the Core 0 stack gate (DEC-289);
- the largest `MAX_NODES` that fits.

It writes its own dummy anchor and config files.
