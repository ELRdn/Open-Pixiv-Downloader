# Source and license notices

The Python core, its tests, and the synthetic fixtures are imported from the
user-supplied **Pixiv Archive Lab 0.1.0a1** archive. Subsequent changes add batch
input, bounded concurrent work scheduling, coordinated database/network
access, and a guided terminal interface. Its MIT copyright notice is preserved
in [LICENSE](LICENSE).

Original archive SHA-256:
`7b9ee8afd0557f904b4e1791ae5a0ad002728f7d701a94a50332e0e72bea6777`.

The original [README](docs/archive-lab/README.md),
[provenance notice](docs/archive-lab/NOTICE.md), and
[license](docs/archive-lab/LICENSE) are retained as historical baseline documents.
Their claims of independent implementation are the original author's provenance
statement, not a formal clean-room certification by this repository.

The Windows metadata probe is an additional MIT-licensed diagnostic tool.
Its notice is in [tools/windows-probe/LICENSE](tools/windows-probe/LICENSE).

The current Rust implementation in `rust/` ports the MIT Python archive contract
and novel renderer, and adds a Ratatui interface and complete ugoira ZIP reads
for CRC validation. It does not invoke the Python implementation at runtime.
The terminal wordmark uses character patterns defined in this project. Cargo
dependencies retain their own licenses; exact resolved versions are recorded
in `Cargo.lock`.

The comparison examined fixed revisions of other projects. Their source trees,
package caches, credentials, and downloaded Pixiv works are not included here.
See [the comparison](docs/COMPARISON_JA.md) for exact revisions and license
declarations. The alternative PxArchive alpha was evaluated but is not included
as an application implementation.

MIT licensing covers the code and the included synthetic fixtures. It does not
grant rights to third-party works or permission to access external services.

Self replacement uses [self-replace](https://github.com/mitsuhiko/self-replace),
resolved to version 1.5.1 in Cargo.lock, under Apache-2.0. Its license is
included in [LICENSES/self-replace-Apache-2.0.txt](LICENSES/self-replace-Apache-2.0.txt).
