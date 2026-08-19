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

## Licences

### The Apache-2.0 fixture

`custom-fingerprint.envelope` wraps a payload copyright Sentry (Functional Software,
Inc.), licensed under the Apache License, Version 2.0. Its full text is at
[`LICENSE`](../../../../LICENSE) in the root of this repository, and at
<http://www.apache.org/licenses/LICENSE-2.0>.

The payload is unmodified. The envelope framing around it — the header line and the
item header line — was written here.

    Copyright Functional Software, Inc. and Sentry contributors

    Licensed under the Apache License, Version 2.0 (the "License");
    you may not use this file except in compliance with the License.
    You may obtain a copy of the License at

        http://www.apache.org/licenses/LICENSE-2.0

    Unless required by applicable law or agreed to in writing, software
    distributed under the License is distributed on an "AS IS" BASIS,
    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
    See the License for the specific language governing permissions and
    limitations under the License.

### The MIT fixtures

Everything else wraps a payload copyright Bugsink, licensed under the MIT licence,
and unmodified.

    Copyright (c) Bugsink and contributors

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE.
