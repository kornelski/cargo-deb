use crate::error::CDResult;
use std::path::{Path, PathBuf};
use std::{env, fs};

pub struct CargoConfig {
    path: PathBuf,
    config: toml::Value,
}

impl CargoConfig {
    pub fn new<P: AsRef<Path>>(project_path: P) -> CDResult<Option<Self>> {
        Self::new_(project_path.as_ref())
    }

    fn new_(project_path: &Path) -> CDResult<Option<Self>> {
        let mut project_path = project_path;
        loop {
            if let Some(conf) = Self::try_parse(&project_path.join(".cargo"))? {
                return Ok(Some(conf));
            }
            if let Some(parent) = project_path.parent() {
                project_path = parent;
            } else {
                break;
            }
        }
        if let Some(home) = env::var_os("CARGO_HOME").map(PathBuf::from) {
            if let Some(conf) = Self::try_parse(&home)? {
                return Ok(Some(conf));
            }
        }
        #[allow(deprecated)]
        if let Some(home) = env::home_dir() {
            if let Some(conf) = Self::try_parse(&home.join(".cargo"))? {
                return Ok(Some(conf));
            }
        }
        if let Some(conf) = Self::try_parse(Path::new("/etc/.cargo"))? {
            return Ok(Some(conf));
        }
        Ok(None)
    }

    fn try_parse(dir_path: &Path) -> CDResult<Option<Self>> {
        let mut path = dir_path.join("config.toml");
        if !path.exists() {
            path.set_file_name("config");
            if !path.exists() {
                return Ok(None);
            }
        }
        Ok(Some(Self::from_str(&fs::read_to_string(&path)?, path)?))
    }

    fn from_str(input: &str, path: PathBuf) -> CDResult<Self> {
        let config = toml::from_str(input)?;
        Ok(Self { path, config })
    }

    fn target_conf(&self, target_triple: &str) -> Option<&toml::value::Table> {
        let target = self.config.get("target")?.as_table()?;
        target.get(target_triple)?.as_table()
    }

    pub fn explicit_target_specific_command(&self, command_name: &str, target_triple: &str) -> Option<&Path> {
        let top = self.target_conf(target_triple)?.get(command_name)?;
        top.as_str().or_else(|| top.get("path")?.as_str()).map(Path::new)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn explicit_linker_command(&self, target_triple: &str) -> Option<&Path> {
        self.target_conf(target_triple)?.get("linker")?.as_str().map(Path::new)
    }
}

#[test]
fn parse_strip() {
    let c = CargoConfig::from_str(r#"
[target.i686-unknown-dragonfly]
linker = "magic-ld"
strip = "magic-strip"

[target.'foo']
strip = { path = "strip2" }
"#, ".".into()).unwrap();

    assert_eq!("magic-strip", c.explicit_target_specific_command("strip", "i686-unknown-dragonfly").unwrap().as_os_str());
    assert_eq!("strip2", c.explicit_target_specific_command("strip", "foo").unwrap().as_os_str());
    assert_eq!(None, c.explicit_target_specific_command("strip", "bar"));
}

#[test]
fn parse_objcopy() {
    let c = CargoConfig::from_str(r#"
[target.i686-unknown-dragonfly]
linker = "magic-ld"
objcopy = "magic-objcopy"

[target.'foo']
objcopy = { path = "objcopy2" }
"#, ".".into()).unwrap();

    assert_eq!("magic-objcopy", c.explicit_target_specific_command("objcopy", "i686-unknown-dragonfly").unwrap().as_os_str());
    assert_eq!("objcopy2", c.explicit_target_specific_command("objcopy", "foo").unwrap().as_os_str());
    assert_eq!(None, c.explicit_target_specific_command("objcopy", "bar"));
}
