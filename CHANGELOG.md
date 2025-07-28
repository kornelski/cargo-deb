For more details see https://github.com/kornelski/cargo-deb/commits/main/

# 3.4.0

* Added support for `build-dir` Cargo feature

# 3.3.0

* Switched systemd location to the preferred `/usr/lib/systemd` instead of `/lib/systemd`
* Improved warnings and error messages
* Added `systemd/` is a fallback location for systemd units if `maintainer-scripts` is not set

# 3.2.0

* Added colored terminal output

# 3.1.0

* Added `--compress-debug-symbols=zlib` and `--compress-debug-symbols=zstd`. If the algorithm is not specified, it defaults to `zstd` for `debug = "full"` (for debuggers) and `zlib` otherwise (for compatibility with panic backtraces).
* When `--separate-debug-symbols` or `--dbgsym` are used with a package that doesn't have debug symbols configured, the package will be built with some debug symbols enabled anyway.
* When `strip --strip-unneeded --remove-section=…` fails, it's retried without the extra arguments.

# 3.0.0

* `--dbgsym` option to generate extra `-dbgsym.ddeb` package with separated debug info.
  Install with `cargo install cargo-deb --features default_enable_dbgsym` to make it enabled by default.
* Warnings for conflicting options, like `--separate-debug-symbols` and `--no-strip`.
* Support for `CARGO_PROFILE_RELEASE_DEBUG` and `CARGO_PROFILE_RELEASE_STRIP`.
* `[profile.release]` is read from the root of the workspace, matching Cargo's behavior.
* `.cargo/config.toml` is searched relative to the current dir, rather than workspace root.
* current dir is not changed when using `--manifest-path`. `cd` to the manifest's dir to get the previous behavior.
* `"$auto"` can be explicitly added to `assets` to get the default assets in addition to custom ones.
* Improved handling of `name` of config variants `[package.metadata.deb.variants.*]`
* lack of `package.authors` and `copyright` is only a warning, not a fatal error.
* Support for default custom release profiles, like `[package.metadata.deb] profile = "dist"`
* `-vv` is now used to print `rustc` invocations during build

# 2.12

* Improved auto-detection default packages in workspaces
* Fixed handling of asset paths with `/*/` directory components

# 2.11

* Support for GCC-style and Debian-style multi-arch lib directories
* Support naming systemd units
* Pass through `--features` flag
* Faster Cargo metadata parsing

# 2.10

* Support for `CARGO_BUILD_TARGET` in addition to `--target`.
* Consistent syntax for all package interrelationship fields in Cargo manifest.
* Systemd uses deb package name instead of Cargo crate name, when they're different.
* tmpfiles are forced to have a .conf extension, like systemd requires.
* Fixed detection of objcopy and strip in cross-compilation when `config.toml` is absent.

# 2.9.3

* Support for multiarch lib dir
* Support for Rust edition 2024

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

