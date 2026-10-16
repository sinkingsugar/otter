//! otter-text's descriptions and catalog entries; also run with the `docs`
//! feature off.

use otter_core::Catalog;

#[test]
fn every_shard_is_described_and_joins_the_core_catalog() {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG, otter_text::CATALOG]).unwrap();
  let index = catalog.index_json();
  for ty in otter_text::CATALOG {
    let name = ty.desc.name;
    assert!(index.contains(&format!("\"name\":\"{name}\"")), "{name}");
    let json = catalog.describe_json(name).unwrap();
    assert!(json.contains("\"targets\":\"all\""), "{json}");
    // Only the identifiers draw random numbers; nothing here does I/O or
    // reads the clock.
    let random = matches!(name, "UUID" | "NanoID");
    assert!(
      json.contains(&format!(
        "\"effects\":{{\"suspends\":false,\"io\":false,\"time\":false,\"random\":{random},\"unknown\":false}}"
      )),
      "{json}"
    );
  }
  let regex = catalog.describe_json("Regex.Replace").unwrap();
  assert!(regex.contains("\"name\":\"regex\",\"index\":0"), "{regex}");
  assert!(
    regex.contains("\"name\":\"replacement\",\"index\":1"),
    "{regex}"
  );
  assert!(
    catalog
      .describe_json("NanoID")
      .unwrap()
      .contains("\"default\":21")
  );
  assert_eq!(catalog.search("regex").len(), 3);
}
