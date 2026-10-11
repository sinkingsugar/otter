#[cfg(feature = "acceptance")]
// Generated token streams lose source newlines and macro lint exemptions.
// These style lints run on the original suites in the workspace instead.
#[allow(
  clippy::possible_missing_else,
  clippy::type_complexity,
  clippy::missing_const_for_thread_local
)]
mod acceptance {
  mod prototype {
    include!(concat!(env!("OUT_DIR"), "/prototype.rs"));
  }
  mod metadata {
    include!(concat!(env!("OUT_DIR"), "/metadata.rs"));
  }
  mod host_contract {
    include!(concat!(env!("OUT_DIR"), "/host_contract.rs"));
  }
  mod registry {
    include!(concat!(env!("OUT_DIR"), "/registry.rs"));
  }
  mod functions {
    include!(concat!(env!("OUT_DIR"), "/functions.rs"));
  }
  mod values {
    include!(concat!(env!("OUT_DIR"), "/values.rs"));
  }
  mod lang {
    include!(concat!(env!("OUT_DIR"), "/lang.rs"));
  }
  mod trampoline {
    include!(concat!(env!("OUT_DIR"), "/trampoline.rs"));
  }
  pub fn run() {
    // Allocate the compose stack at boot: allocating a large task stack
    // after earlier suites can fail from heap fragmentation on C3.
    for (name, run) in [
      ("prototype", prototype::run_suite as fn()),
      ("metadata", metadata::run_suite as fn()),
      ("host_contract", host_contract::run_suite as fn()),
      ("registry", registry::run_suite as fn()),
      ("functions", functions::run_suite as fn()),
      ("values", values::run_suite as fn()),
      ("lang", lang::run_suite as fn()),
      ("trampoline", trampoline::run_suite as fn()),
    ] {
      run();
      // SAFETY: queries of this running task and the SDK heap.
      let (stack, heap) = unsafe {
        (
          esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()),
          esp_idf_sys::esp_get_minimum_free_heap_size(),
        )
      };
      println!("acceptance suite {name}: main stack min free {stack} B, heap min free {heap} B");
      std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("Shards ESP32 acceptance suites passed");
  }
}

use std::collections::HashMap;

use otter_core::{Catalog, Outcome, Var};
use otter_lang::{Program, Source};

fn main() {
  esp_idf_sys::link_patches();

  let catalog = Catalog::new(&[otter_core::shards::CATALOG]).expect("core catalog");
  let program = Program::load(
    Source::new("smoke.shs", include_str!("../smoke.shs")),
    &catalog,
    &HashMap::new(),
  )
  .unwrap_or_else(|(_, diagnostics)| panic!("load failed: {diagnostics:?}"));
  let report = program.run().expect("compose and run");
  assert_eq!(report.outcomes.len(), 1);
  assert_eq!(report.outcomes[0].1, Some(Outcome::Completed(Var::Int(42))));
  assert!(report.spawned_failures.is_empty());
  assert!(report.ticks >= 2, "Pause must suspend and resume");
  let ticks = report.ticks;
  drop(report);
  drop(program);
  drop(catalog);
  #[cfg(feature = "acceptance")]
  acceptance::run();
  // Low-water marks since boot: parse, compose and run all happened above.
  // Stack depth follows the code path, so emulator runs measure it too.
  // SAFETY: plain FreeRTOS/ESP-IDF queries; a null handle is this task.
  let (stack_free, heap_free) = unsafe {
    (
      esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()),
      esp_idf_sys::esp_get_minimum_free_heap_size(),
    )
  };
  println!(
    "Shards ESP32 smoke test passed: 42 ({} ticks); main stack min free {} of {} B, heap min free {} B",
    ticks,
    stack_free,
    esp_idf_sys::CONFIG_ESP_MAIN_TASK_STACK_SIZE,
    heap_free
  );
}
