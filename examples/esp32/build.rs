fn main() {
  embuild::espidf::sysenv::output();
  println!("cargo:rustc-check-cfg=cfg(feature, values(\"docs\", \"output-checks\"))");
  #[cfg(feature = "acceptance")]
  acceptance();
}

// ESP-IDF has no libtest harness. Call the shared suites' test functions
// directly, preserving each test's cfg attributes. Changes to the actual
// tests automatically enter the firmware suite.
#[cfg(feature = "acceptance")]
fn acceptance() {
  use quote::{ToTokens, quote};
  use std::{fs, path::PathBuf};
  use syn::{Item, parse_quote};
  let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
  for (name, path) in [
    ("prototype", "../../crates/otter-core/tests/prototype.rs"),
    ("metadata", "../../crates/otter-core/tests/metadata.rs"),
    (
      "host_contract",
      "../../crates/otter-core/tests/host_contract.rs",
    ),
    ("registry", "../../crates/otter-core/tests/registry.rs"),
    ("functions", "../../crates/otter-core/tests/functions.rs"),
    ("values", "../../crates/otter-core/tests/values.rs"),
    ("lang", "../../crates/otter-lang/tests/lang.rs"),
    ("trampoline", "../../crates/otter-core/tests/trampoline.rs"),
  ] {
    println!("cargo:rerun-if-changed={path}");
    // This shared frontend fixture is relative to tests/lang.rs; retain its
    // location when compiling the generated file from OUT_DIR.
    let source = fs::read_to_string(path).unwrap().replace(
      "\"../../../examples/esp32/smoke.shs\"",
      &format!("{:?}", fs::canonicalize("smoke.shs").unwrap()),
    );
    fn runnable(items: &mut Vec<Item>) {
      let mut calls = Vec::new();
      for item in items.iter_mut() {
        match item {
          Item::Fn(f) if f.attrs.iter().any(|a| a.path().is_ident("test")) => {
            f.attrs.retain(|a| !a.path().is_ident("test"));
            let cfgs: Vec<_> = f
              .attrs
              .iter()
              .filter(|a| a.path().is_ident("cfg"))
              .collect();
            let name = &f.sig.ident;
            calls.push(quote! { #(#cfgs)* {
              // SAFETY: queries of the SDK heap. The low-water mark so far:
              // a drop from one line to the next belongs to the test between.
              let (heap, low) = unsafe {
                (
                  esp_idf_sys::esp_get_free_heap_size(),
                  esp_idf_sys::esp_get_minimum_free_heap_size(),
                )
              };
              println!(
                "acceptance: {} (heap free {heap} B, min free {low} B)",
                stringify!(#name)
              );
              #name();
              std::thread::sleep(std::time::Duration::from_millis(10));
            } });
          }
          Item::Mod(m) => {
            if let Some((_, children)) = &mut m.content {
              runnable(children);
              let name = &m.ident;
              let cfgs: Vec<_> = m
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("cfg"))
                .collect();
              calls.push(quote! { #(#cfgs)* #name::run_suite(); });
            }
          }
          _ => {}
        }
      }
      items.push(parse_quote! { pub fn run_suite() { #(#calls)* } });
    }
    let mut file = syn::parse_file(&source).unwrap();
    runnable(&mut file.items);
    // Inner doc comments belong to the original file, not an include! expansion.
    file.attrs.clear();
    fs::write(
      out.join(format!("{name}.rs")),
      file.into_token_stream().to_string(),
    )
    .unwrap();
  }
}
