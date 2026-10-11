//! Warns when a native release build lacks the block alignment this
//! workspace's `.cargo/config.toml` sets: the runtime's measured speed
//! depends on it (docs/runtime-performance-overview.md, lesson 7), and a
//! host building `otter-core` from its own workspace does not inherit the
//! flag. Cargo shows the warning for path and workspace dependencies.

fn main() {
  println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
  let var = |name| std::env::var(name).unwrap_or_default();
  let native = matches!(var("CARGO_CFG_TARGET_ARCH").as_str(), "aarch64" | "x86_64");
  if native
    && var("PROFILE") == "release"
    && !var("CARGO_ENCODED_RUSTFLAGS").contains("align-all-nofallthru-blocks")
  {
    println!(
      "cargo:warning=otter-core is built without `-C llvm-args=-align-all-nofallthru-blocks=4`: \
       the VM's speed then depends on code placement (up to 40 percent either way). Add it to \
       your workspace's .cargo/config.toml as otter's does (docs/embedding.md)."
    );
  }
}
