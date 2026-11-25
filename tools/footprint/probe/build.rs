//! Links `memory.x`, and adapts the storage stub to the trait of the
//! workspace being measured (`FOOTPRINT_WS`): `read_block_into` (DEC-136)
//! or the older by-value `read_block`.
fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("memory.x"), include_bytes!("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo::rustc-check-cfg=cfg(storage_read_into)");
    println!("cargo:rerun-if-env-changed=FOOTPRINT_WS");
    // `footprint.py` measures the blockchain's static RAM per node by building the
    // probe at other node counts than the scheme's profile.
    println!("cargo:rerun-if-env-changed=FOOTPRINT_MAX_NODES");
    let max_nodes = match std::env::var("FOOTPRINT_MAX_NODES") {
        Ok(n) => format!("Some({}usize)", n.parse::<usize>().expect("FOOTPRINT_MAX_NODES")),
        Err(_) => "None".to_string(),
    };
    std::fs::write(out.join("max_nodes.rs"), max_nodes).unwrap();
    let ws = std::env::var("FOOTPRINT_WS").expect("FOOTPRINT_WS");
    let lib = std::fs::read_to_string(format!("{ws}/moonblokz-storage/src/lib.rs")).unwrap();
    if lib.contains("fn read_block_into(") {
        println!("cargo:rustc-cfg=storage_read_into");
    }
}
