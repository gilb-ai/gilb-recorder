# gilb-db

SQLite-backed storage for gilb (sessions, actions, tree snapshots, app
budgets, health events). Every PRAGMA is justified inline in
`src/lib.rs::open_db` — they are tuned for steady-state low-latency capture,
which is not the same set you would pick for a read-heavy database.

## Throughput benchmark

`tests/throughput_bench.rs` measures the synthetic insert rate of the
single-row, unbatched `actions::insert_action` path. It establishes a
baseline so a future batched write queue has something concrete to defend
its improvements against.

```sh
cargo test -p gilb-db throughput_bench -- --nocapture
```

Inserts 10K varied actions (Key / Click / FocusChange) one at a time and
prints the achieved rate. Asserts `> 1000 inserts/sec` as a smoke check;
absolute throughput is machine-dependent.

## Other commands

```sh
cargo test  -p gilb-db                       # all tests
cargo build -p gilb-db                       # build only this crate
```
