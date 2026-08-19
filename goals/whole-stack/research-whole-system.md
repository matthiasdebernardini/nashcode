# Research: whole-system source — app to container to host OS

How to pin and browse every byte of source a deployed system runs, without forcing
Nix or Bazel on anyone. Conclusion up front: this has been solved four times
(Nix/Guix, Yocto, Debsources, Software Heritage); for Ubuntu hosts the plumbing is
public, and the cheapest 80% is dpkg inventory + snapshot timestamp + lazy
per-package source fetch.

1. **Nix/Guix — steal the shape, skip the tool.** `flake.lock` pins the transitive
   input graph by content hash; every source fetch in nixpkgs is (URL, sha256), so
   a system's full closure is enumerable. The portable ideas: one lockfile per
   layer pinned by content hash; content-addressed storage so `ubuntu:24.04` is
   stored once across all users; a single identifier naming the whole distro state.

2. **Yocto — the two-artifact split.** The only ecosystem that routinely ships
   "every byte of source in this image" (GPL compliance): a manifest (`create-spdx`
   per image) plus a materialized source tree (`archiver.bbclass`), manifest as
   index, tree hydrated on demand. Their own warning: archive what is *in the
   image*, not everything downloaded.

3. **Ubuntu/Debian — the highest-feasibility chain.**
   1. `dpkg-query -W` → exact installed versions (binary and source package).
   2. **snapshot.ubuntu.com** (since 2023-03) / snapshot.debian.org: a timestamp
      pins the entire archive state — the nixpkgs-commit equivalent.
   3. Source per package: `apt-get source pkg=ver`; when superseded, the snapshot
      machine-readable API (`/mr/package/<src>/<ver>/srcfiles?fileinfo=1`,
      `/mr/file/<sha256>/download`).
   4. Git form: **git.launchpad.net/ubuntu/+source/<pkg>**, every upload tagged
      `import/<version>` (kernel: the Launchpad kernel git, tags `Ubuntu-*`).
   5. Browsable prior art: **sources.debian.org** (Debsources) — the whole archive
      unpacked, indexed in Postgres, JSON API, self-hostable. The product shape to
      copy.
   Failure modes: git-ubuntu shows packaging history, not upstream commit graphs;
   snapshots rate-limit; nothing before Mar 2023; container images lack `deb-src`
   lines.

4. **Container images.** syft inventories any image into purls
   (`pkg:deb/ubuntu/nginx@...`) → feed the Debian chain. BuildKit attestations,
   when present, add VCS repo + revision and base-image digests (`mode=max` adds
   the Dockerfile itself) — but they are unsigned by default and usually absent.
   Design for absence: syft-of-the-image is the universal path, provenance the
   bonus.

5. **Software Heritage — fallback tier only.** Intrinsic SWHIDs, daily Debian
   ingestion, Vault cooking for retrieval. Rate-limited, petabyte-class to mirror.
   Wire SWHID lookup behind snapshot/Launchpad; never self-host.

6. **buildinfo — the toolchain row.** Debian `.buildinfo` records the complete
   pinned build environment and is *executable* (debrebuild/debootsnap reconstruct
   the env from snapshots at archive scale). Show it as the toolchain row; never
   actually rebuild.

7. **Binary self-description.** Go binaries embed module versions + VCS revision
   (`go version -m`); `cargo auditable` embeds the dep tree (versions, no commit).
   For a Rust app, `Cargo.lock` + crates.io hashes is exact source identity.

## The ranked ladder (cheapest path)

1. Inventory: syft the image / dpkg the host; record a snapshot timestamp — the
   "system commit".
2. App layer: `Cargo.lock` + the deploying git rev (already known to nashcode).
3. OS layer: dpkg list → `apt-get source` → snapshot `/mr/` → Launchpad
   `import/<version>`, in that order.
4. Browse: Debsources model — unpack lazily on first view, content-addressed,
   shared across stacks.
5. Fallbacks: SWHID for vanished upstreams; buildinfo for the toolchain row.
6. Never: mirror SWH, rebuild anything, require Nix/Bazel.

Weakest links, in order: the kernel (use Launchpad kernel git, not source
packages); third-party APT repos and `curl | sh` installs (no snapshot exists —
label unpinned, honestly); pre-2023 Ubuntu states.

## Sources

- https://determinate.systems/blog/nix-flakes-explained/
- https://docs.yoctoproject.org/dev/dev-manual/sbom.html
- https://ubuntu.com/server/docs/how-to/software/snapshot-service/
- https://wiki.debian.org/BisectDebian (snapshot `/mr/` endpoints)
- https://ubuntu.com/blog/git-ubuntu-more-on-the-imported-repositories
- https://sources.debian.org/doc/api/
- https://docs.docker.com/build/metadata/attestations/slsa-provenance/
- https://www.augmentedmind.de/2025/03/16/docker-image-attestation-buildkit/
- https://github.com/anchore/syft/issues/1408
- https://docs.softwareheritage.org/sysadm/mirror-operations/index.html
- https://wiki.debian.org/ReproducibleBuilds/BuildinfoFiles / https://reproduce.debian.net/
- https://docs.deps.dev/api/v3alpha/
- https://github.com/rust-secure-code/cargo-auditable
