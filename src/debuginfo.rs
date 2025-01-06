use crate::assets::{Asset, AssetSource, IsBuilt, ProcessedFrom};
use crate::config::{Config, DebugSymbols, PackageConfig};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::parse::cargo::CargoConfig;
use rayon::prelude::*;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::{fs, io};

fn ensure_success(status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, status.to_string()))
    }
}

/// Strips the binary that was created with cargo
pub fn strip_binaries(config: &mut Config, package_deb: &mut PackageConfig, rust_target_triple: Option<&str>, listener: &dyn Listener) -> CDResult<()> {
    let mut cargo_config = None;
    let objcopy_tmp;
    let strip_tmp;
    let mut objcopy_cmd = Path::new("objcopy");
    let mut strip_cmd = Path::new("strip");

    if let Some(rust_target_triple) = rust_target_triple {
        cargo_config = config.cargo_config().ok().flatten();
        if let Some(cmd) = target_specific_command(cargo_config.as_ref(), "objcopy", rust_target_triple) {
            listener.info(format!("Using '{}' for '{rust_target_triple}'", cmd.display()));
            objcopy_tmp = cmd;
            objcopy_cmd = &objcopy_tmp;
        }

        if let Some(cmd) = target_specific_command(cargo_config.as_ref(), "strip", rust_target_triple) {
            listener.info(format!("Using '{}' for '{rust_target_triple}'", cmd.display()));
            strip_tmp = cmd;
            strip_cmd = &strip_tmp;
        }
    }

    let stripped_binaries_output_dir = config.default_deb_output_dir();
    let (separate_debug_symbols, compress_debug_symbols) = match config.debug_symbols {
        DebugSymbols::Keep | DebugSymbols::Strip => (false, false),
        DebugSymbols::Separate { compress } => (true, compress),
    };

    let lib_dir_base = package_deb.library_install_dir(config.rust_target_triple());
    let added_debug_assets = package_deb.built_binaries_mut().into_par_iter().enumerate()
        .filter(|(_, asset)| !asset.source.archive_as_symlink_only()) // data won't be included, so nothing to strip
        .map(|(i, asset)| {
        let (new_source, new_debug_asset) = if let Some(path) = asset.source.path() {
            if !path.exists() {
                return Err(CargoDebError::StripFailed(path.to_owned(), "The file doesn't exist".into()));
            }

            let conf_path = cargo_config.as_ref().map(|c| c.path())
                .unwrap_or_else(|| Path::new(".cargo/config"));
            let file_name = path.file_stem().ok_or(CargoDebError::Str("bad path"))?.to_string_lossy();
            let stripped_temp_path = stripped_binaries_output_dir.join(format!("{file_name}.tmp{i}-stripped"));
            let _ = fs::remove_file(&stripped_temp_path);

            log::debug!("stripping with {} from {} into {}", strip_cmd.display(), path.display(), stripped_temp_path.display());
            Command::new(strip_cmd)
               // same as dh_strip
               .args(["--strip-unneeded", "--remove-section=.comment", "--remove-section=.note"])
               .arg("-o").arg(&stripped_temp_path)
               .arg(path)
               .status()
               .and_then(ensure_success)
               .map_err(|err| {
                    if let Some(target) = rust_target_triple {
                        CargoDebError::StripFailed(path.to_owned(), format!("{}: {}.\nhint: Target-specific strip commands are configured in [target.{}] strip = {{ path = \"{}\" }} in {}", strip_cmd.display(), err, target, strip_cmd.display(), conf_path.display()))
                    } else {
                        CargoDebError::CommandFailed(err, "strip")
                    }
                })?;

            if !stripped_temp_path.exists() {
                return Err(CargoDebError::StripFailed(path.to_owned(), format!("{} command failed to create output '{}'", strip_cmd.display(), stripped_temp_path.display())));
            }

            let new_debug_asset = if separate_debug_symbols && asset.c.is_built() {
                log::debug!("extracting debug info with {} from {}", objcopy_cmd.display(), path.display());

                // parse the ELF and use debug-id-based path if available
                let debug_target_path = get_target_debug_path(asset, path, &lib_dir_base)?;

                // --add-gnu-debuglink reads the file path given, so it can't get to-be-installed target path
                // and the recommended fallback solution is to give it relative path in the same dir
                let debug_temp_path = stripped_temp_path.with_file_name(debug_target_path.file_name().ok_or(CargoDebError::Str("bad path"))?);

                let _ = fs::remove_file(&debug_temp_path);
                let mut args: &[_] = &["--only-keep-debug", "--compress-debug-sections=zstd"];
                if !compress_debug_symbols {
                    args = &args[..1];
                }
                Command::new(objcopy_cmd)
                    .args(args)
                    .arg(path)
                    .arg(&debug_temp_path)
                    .status()
                    .and_then(ensure_success)
                    .map_err(|err| {
                        if let Some(target) = rust_target_triple {
                            CargoDebError::StripFailed(path.to_owned(), format!("{}: {}.\nhint: Target-specific strip commands are configured in [target.{}] objcopy = {{ path =\"{}\" }} in {}", objcopy_cmd.display(), err, target, objcopy_cmd.display(), conf_path.display()))
                        } else {
                            CargoDebError::CommandFailed(err, "objcopy")
                        }
                    })?;

                let relative_debug_temp_path = debug_temp_path.file_name().ok_or(CargoDebError::Str("bad path"))?;
                log::debug!("linking debug info with {} from {} into {:?}", objcopy_cmd.display(), stripped_temp_path.display(), relative_debug_temp_path);
                Command::new(objcopy_cmd)
                    .current_dir(debug_temp_path.parent().ok_or(CargoDebError::Str("bad path"))?)
                    .arg("--add-gnu-debuglink")
                    // intentionally relative - the file name must match debug_target_path
                    .arg(relative_debug_temp_path)
                    .arg(&stripped_temp_path)
                    .status()
                    .and_then(ensure_success)
                    .map_err(|err| CargoDebError::CommandFailed(err, "objcopy"))?;

                Some(Asset::new(
                    AssetSource::Path(debug_temp_path),
                    debug_target_path,
                    0o644,
                    IsBuilt::No,
                    false,
                ).processed(if compress_debug_symbols { "compress"} else {"separate"}, path.to_path_buf()))
            } else {
                None // no new asset
            };
            listener.info(format!("Stripped '{}'", path.display()));

            (AssetSource::Path(stripped_temp_path), new_debug_asset)
        } else {
            // This is unexpected - emit a warning if we come across it
            listener.warning(format!("Found built asset with non-path source '{asset:?}'"));
            return Ok(None);
        };
        log::debug!("Replacing asset {} with stripped asset {}", asset.source.path().unwrap().display(), new_source.path().unwrap().display());
        let old_source = std::mem::replace(&mut asset.source, new_source);
        asset.processed_from = Some(ProcessedFrom {
            original_path: old_source.into_path(),
            action: "strip",
        });
        Ok::<_, CargoDebError>(new_debug_asset)
    }).collect::<Result<Vec<_>, _>>()?;

    package_deb.assets.resolved
        .extend(added_debug_assets.into_iter().flatten());

    Ok(())
}

fn target_specific_command<'a>(cargo_config: Option<&'a CargoConfig>, command_name: &str, target_triple: &str) -> Option<Cow<'a, Path>> {
    if let Some(cmd) = cargo_config.and_then(|c| c.explicit_target_specific_command(command_name, target_triple)) {
        return Some(cmd.into());
    }

    let debian_target_triple = crate::debian_triple_from_rust_triple(target_triple);
    if let Some(linker) = cargo_config.and_then(|c| c.explicit_linker_command(target_triple)) {
        if linker.parent().is_some() {
            let linker_file_name = linker.file_name()?.to_str()?;
            // checks whether it's `/usr/bin/triple-ld` or `/custom-toolchain/ld`
            let strip_path = if linker_file_name.starts_with(&debian_target_triple) {
                linker.with_file_name(format!("{debian_target_triple}-{command_name}"))
            } else {
                linker.with_file_name(command_name)
            };
            if strip_path.exists() {
                return Some(strip_path.into());
            }
        }
    }
    let path = PathBuf::from(format!("/usr/bin/{debian_target_triple}-{command_name}"));
    if path.exists() {
        return Some(path.into());
    }
    None
}

fn get_target_debug_path(asset: &Asset, asset_path: &Path, lib_dir_base: &Path) -> Result<PathBuf, CargoDebError> {
    let target_debug_path = match elf_gnu_debug_id(asset_path, lib_dir_base) {
        Ok(Some(path)) => {
            log::debug!("got gnu debug-id: {} for {}", path.display(), asset_path.display());
            path
        },
        Ok(None) => {
            log::debug!("debug-id not found in {}", asset_path.display());
            asset.c.default_debug_target_path(lib_dir_base)
        },
        Err(e) => {
            log::debug!("elf: {e} in {}", asset_path.display());
            asset.c.default_debug_target_path(lib_dir_base)
        },
    };
    Ok(target_debug_path)
}

#[cfg(not(feature = "debug-id"))]
fn elf_gnu_debug_id(_: &Path, _: &Path) -> io::Result<Option<PathBuf>> {
    Ok(None)
}

#[cfg(feature = "debug-id")]
fn elf_gnu_debug_id(elf_file_path: &Path, lib_dir_base: &Path) -> Result<Option<PathBuf>, elf::ParseError> {
    use elf::endian::AnyEndian;
    use elf::note::Note;
    use elf::ElfStream;

    let mut stream = ElfStream::<AnyEndian, _>::open_stream(fs::File::open(elf_file_path)?)?;
    let Some(abi_shdr) = stream.section_header_by_name(".note.gnu.build-id")?
        else { return Ok(None) };

    let abi_shdr = *abi_shdr;
    for note in stream.section_data_as_notes(&abi_shdr)? {
        if let Note::GnuBuildId(note) = note {
            if let Some((byte, rest)) = note.0.split_first() {
                let mut s = format!("debug/.build-id/{byte:02x}/");
                for b in rest {
                    use std::fmt::Write;
                    write!(&mut s, "{b:02x}").unwrap();
                }
                s.push_str(".debug");
                return Ok(Some(lib_dir_base.join(s)));
            }
        }
    }
    Ok(None)
}
