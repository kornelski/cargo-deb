For more details see https://github.com/kornelski/cargo-deb/commits/main/

# 2.8.0

* Don't add Vcs-* to the binary control file, since lintian doesn't like it.
* Don't generate sha256sums files, since lintian doesn't like it either.

# 2.7.0

* `assets` entries in `Cargo.toml` can use a verbose syntax `{ source = "path", dest = "path", mode = "644" }`
* Improved handling of symlinked assets

# 2.6.0

 * `--maintainer` overrides maintainer field and makes `authors` in TOML optional.
 * Fixed use of `--manifest-path` used from out-of-workspace directories having `.cargo` dir

# 2.5.0

 * `--offline`, `--locked`, `--frozen` passed to `cargo metadata` and `cargo build`
 * `--cargo-build="custom build"` splits on spaces, allowing custom cargo subcommands

# 2.4.0

 * If run in a workspace without any package name specified, it picks a default workspace member.
 * Package with `publish = false` without any `license` specified defaults to "UNLICENSED".
 * `conffiles` is generated for all assets with `etc/` as their destination.

This release has big internal refactorings, so watch out for subtle bugs.

# 2.3.0

 * Added `--compress-debug-symbols` option when using `--separate-debug-symbols`.
 * Fixed `separate-debug-symbols` option missing from `Cargo.toml` metadata.
 * Changed location of separated debug symbols to use GNU debug-id paths, when the debug-id section is available.
 * `strip` command is called with the same args as `dh_strip` uses.

# 2.2.0

 * Changed digests from MD5 to SHA256

# 2.1.0

 * Added `--rsyncable` that tries to make very large packages compressed in a more deterministic way.

# 2.0.0

 * Added a default `-1` revision suffix to versions, to indicate these aren't official ("native") Debian packages.

