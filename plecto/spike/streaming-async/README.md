# spike: streaming-async

`stream<u8>` body 契約の書き味を確認した spike（ADR 000021 / 000025、実施 2026-06-27）。
`wasm32-wasip2` + wit-bindgen async で自前 WIT の `stream<u8>` がコンパイル可能なことを確認し、
生成 trait が低レベル（`RawStreamReader<&'static StreamVtable<u8>>`）で書き味が悪いという発見が
**v1 を buffered `list<u8>` に切り替える判断**（ADR 000025）の根拠になった。

- ワークスペース外・CI 対象外（意図的）。ビルドが腐っていても本体には影響しない。
- 現行の実験実装は `crates/host/src/streaming.rs`（feature `streaming-body`）にある。
- `stream<u8>` への差し替え増分（true-streaming）を検討するときに再訪する。それまで削除しない。
