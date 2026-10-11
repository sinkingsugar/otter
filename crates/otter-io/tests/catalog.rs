//! `Http.Get`'s description and catalog entry: available without creating
//! the runtime, a client or a request. Its own test binary, so no other test
//! starts the runtime first.

use otter_core::args::decode;
use otter_core::{Arg, Catalog, ParamValue, Var};
use otter_io::http::GET_DESC;
use otter_io::runtime::runtime_started;

#[test]
fn http_get_is_described_without_starting_anything() {
  let catalog = Catalog::new(&[otter_core::shards::CATALOG, otter_io::CATALOG]).unwrap();
  let json = catalog.describe_json("Http.Get").unwrap();
  assert!(json.contains("\"targets\":\"native-only\""), "{json}");
  assert!(json.contains("\"name\":\"url\",\"index\":0"));
  assert!(json.contains("\"required\":true"));
  assert!(json.contains("\"name\":\"timeout\",\"index\":1"));
  assert!(json.contains("\"default\":10"));
  assert!(
    json
      .contains("\"output\":{\"kind\":\"fixed\",\"type\":{\"name\":\"String\",\"basic_type\":52}}")
  );
  assert!(catalog.index_json().contains("\"name\":\"Http.Get\""));
  assert_eq!(catalog.search("http").len(), 1);
  assert!(
    !runtime_started(),
    "describing Http.Get must not start the runtime"
  );
}

#[test]
fn omitted_and_explicit_defaults_decode_the_same() {
  let url = || ParamValue::Value(Var::string("http://127.0.0.1:1/ok"));
  let omitted = decode(&GET_DESC, &[Arg::pos(url())]).unwrap();
  let explicit = decode(
    &GET_DESC,
    &[
      Arg::pos(url()),
      Arg::named("timeout", ParamValue::Value(Var::Int(10))),
    ],
  )
  .unwrap();
  assert_eq!(omitted.int("timeout"), Some(10));
  assert_eq!(explicit.int("timeout"), Some(10));
  assert_eq!(omitted.get("url"), explicit.get("url"));
  // A wrong literal type is caught by the shared decoder, from the same
  // declaration the catalog documents.
  let err = decode(
    &GET_DESC,
    &[
      Arg::pos(url()),
      Arg::named("timeout", ParamValue::Value(Var::string("10"))),
    ],
  )
  .err()
  .unwrap();
  let d = err.diagnostic().unwrap();
  assert_eq!(
    (d.code, d.param.as_deref()),
    ("wrong-argument-type", Some("timeout"))
  );
  assert!(!runtime_started());
}
