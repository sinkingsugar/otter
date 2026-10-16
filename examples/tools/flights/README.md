# Google Flights tools

Two tools a host loads at run time (`otter_lang::Tool`, [docs/embedding.md §6](../../../docs/embedding.md#6-tools)), ported from the 1.x Google Flights tool. They read the public Google Flights website; no API key.

| File | Tool | What it does |
|---|---|---|
| `search-flights.shs` | `google_flights_search` | One-way or round-trip search between two airports on given dates: prices, airlines, flight numbers, times, durations, stops, layovers and a booking link. |
| `cheap-dates.shs` | `google_flights_cheap_dates` | The cheapest dates over a range of up to 45 days: one real search per date, each date's cheapest fare. |
| `flights.shs` | | Shared by both (`@include`d): the `tfs` itinerary token, defensive reads of the page's data, formatting, the fetch with its retry. |

## How they work

Google's JSON RPCs (`batchexecute`, `GetCalendarGraph`) are blocked for cookieless callers from most addresses, so the tools take the route a browser takes:

1. Build the `tfs` itinerary token: a small protobuf message (`ToLEB128`, `Bytes.Join`), base64-encoded and percent-encoded, byte for byte what the fast-flights project builds.
2. `Http.Get` `/travel/flights/search?tfs=...` with a browser's headers.
3. Cut the `ds:1` script block out of the page (`String.Split`) and parse it (`FromJson`). The data is positional and undocumented, so every read is defensive: a malformed row is skipped and logged, not fatal.

Requests go one at a time. Concurrent requests multiplexed over one HTTP/2 connection get degraded pages (a `ds:1` block without rows), which looks like rate limiting but is not. A degraded page is retried 0.8 s later (twice for a search, once per date in a date scan); a search then fails with a message saying so, and a date scan reports the dates that returned no fare.

## Running them

The `otter` binary has TLS (rustls) by default:

```sh
cargo build --release -p otter-cli
otter tool examples/tools/flights/search-flights.shs        # the definition, as JSON
otter tool examples/tools/flights/search-flights.shs '{"from": "KIX", "to": "NRT", "date": "2026-11-20", "max-stops": 0}'
otter tool examples/tools/flights/cheap-dates.shs '{"from": "KIX", "to": "NRT", "from-date": "2026-11-20", "to-date": "2026-11-27"}'
```

## Tests

`crates/otter-cli/tests/cli.rs` (`mod flights`) loads both tools and checks their definitions and argument checks offline. The live searches are ignored by default, since they need the network and Google may throttle:

```sh
cargo test -p otter-cli --test cli -- --ignored --test-threads 1 flights
```

The page layout is Google's and changes without notice; when the live tests fail with no rows, compare a saved page's `ds:1` block against the positions `search-flights.shs` reads.
