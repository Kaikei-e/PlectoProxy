The body-reading contract: identical to `filter` PLUS the `on-request-body` export. A filter that
inspects or transforms the request body targets this world; the PRESENCE of `on-request-body` is
the signal that makes the host buffer the body and run this hook (buffer-then-decide, ADR 000025).
Absence (the base `filter` world) means the body streams straight through. The deferred
true-streaming increment swaps the `list<u8>` for `stream<u8>`. (Spelled out rather than
`include filter` — WIT does not propagate an included world's type `use` into a new export's
scope, so the shared shape is duplicated here deliberately.)