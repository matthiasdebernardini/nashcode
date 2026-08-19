# Where these fixtures came from

The event payloads are vendored from [bugsink/event-samples][samples], which collects
real Sentry-protocol events from several codebases and records the licence of each
directory. The envelopes here wrap those payloads the way an SDK's transport does —
one envelope header line, one item header line, one payload — and nothing in the
payloads is edited.

| File | Payload source | Copyright and licence |
|---|---|---|
| `python-exception.envelope` | `generated/sentry-python-capture-exception-add-full-stack.json` | Bugsink, MIT |
| `python-exception.envelope.gz` | the same, gzipped | Bugsink, MIT |
| `unknown-item.envelope` | the same, behind a `profile_chunk` and a `client_report` item | Bugsink, MIT |
| `log-message.envelope` | `bugsink/111.json` | Bugsink, MIT |
| `custom-fingerprint.envelope` | `sentry/custom-fingerprint.json` | Sentry, Apache-2.0 |

The `profile_chunk` and `client_report` item payloads in `unknown-item.envelope` are
written here, not vendored.

Why each one is in the suite:

- **python-exception** — the ordinary case, with a full stack and in-app frames.
- **python-exception.envelope.gz** — the same bytes under `content-encoding: gzip`,
  which is what every server-side SDK sends by default.
- **unknown-item** — the rule the protocol calls out first: a server must skip an item
  type it does not know rather than fail the envelope.
- **log-message** — an event with a `logentry` and no exception at all.
- **custom-fingerprint** — an explicit SDK `fingerprint`, and a numeric `timestamp`
  rather than an RFC 3339 string. Both forms are legal and both have to parse.

[samples]: https://github.com/bugsink/event-samples
