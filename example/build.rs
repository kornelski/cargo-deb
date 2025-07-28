use std::env;
use std::path::PathBuf;
use std::fs::{self, File};
use std::io::Write;

fn main() {
  if cfg!(feature = "example_non_debian_build") {
    panic!("Detected this example isn't built via cargo-deb, because example_non_debian_build feature is on. Build with --no-default-features");
  }
  if !cfg!(feature = "example_debian_build") {
    panic!("Detected this example isn't built via cargo-deb, because example_debian_build feature is off. Build with --features=example_debian_build");
  }

  // This is all awful, because Cargo doesn't specify a reliable CARGO_TARGET_DIR itself,
  // and the OUT_DIR hack depends on an implementation detail that is no longer true with build-dir
  let mut out_path = env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .map(|mut path| { path.push("release"); path })
    .filter(|path| path.exists())
    .unwrap_or_else(|| {
      let mut out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
      out_dir.pop();
      out_dir.pop();
      out_dir.pop();
      out_dir
    });
  out_path.push("assets");
  let _ = fs::create_dir_all(&out_path);

  File::create(out_path.join("5.txt")).and_then(|mut f| f.write_all(b"Hello generated asset 1")).expect("Could not write asset file");
  File::create(out_path.join("6.txt")).and_then(|mut f| f.write_all(b"Hello generated asset 2")).expect("Could not write asset file");
}
