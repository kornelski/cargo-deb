For more details see https://github.com/kornelski/cargo-deb/commits/main/

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

