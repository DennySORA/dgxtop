<!-- ops-tools:workflow:v1:start -->
# Engineering workflow

protocol 1 · tickets-v1 · 1 ticket

Read the [board](BOARD.md) and [handoff](handoff.md), then the relevant ticket, requirements, and evidence.

The engine originally generated this section. Ticket status is authoritative in tickets/*/ticket.json.
<!-- ops-tools:workflow:v1:end -->

These process records and generated Markdown projections were manually translated into English at the user's explicit request. This project uses English. Do not regenerate these projections in Chinese; the current engine has a fixed Chinese renderer. Its private engine and projection hashes were not changed by this translation.

Identifiers, revisions, event timestamps, gate outcomes, and historical source/test snapshot hashes are preserved. The source segment is explicitly labeled as an English translation, with its content hash and byte count updated; the source manifest retains the original Chinese input hash and batch key. Historical completion statements and evidence describe their recorded snapshots and do not validate subsequent English UI or inline-control changes. The blocked runner-isolation limitation remains in effect.

## dgxtop v2 rewrite (hand-written, 2026-10-04)

The program was rewritten from scratch according to `spec/dgxtop-v2/`. The work record, ticket
status and implementation decisions are in [v2-rewrite/README.md](v2-rewrite/README.md) and
[v2-rewrite/DECISIONS.md](v2-rewrite/DECISIONS.md). These entries were written by hand because
the workflow engine is not installed on this machine; they are not engine-managed state.

TKT-000001 (process-list scrolling and GPU filter on the legacy code) is superseded: the legacy
code it changed no longer exists. Its requirement (scroll to the last row, GPU column first,
per-GPU filter, selection survives refresh) is carried into the v2 processes view (T10).
