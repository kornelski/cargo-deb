use std::io::Write;
use std::path::Path;

#[cfg_attr(test, mockall::automock)]
pub trait Listener: Send + Sync {
    fn warning(&self, s: String);
    fn info(&self, s: String);

    fn progress(&self, operation: &str, detail: String) {
        self.info(format!("{operation}: {detail}"))
    }

    /// Notified when finished writing .deb file (possibly before install)
    fn generated_archive(&self, path: &Path) {
        println!("{}", path.display());
    }
}

pub struct NoOpListener;
impl Listener for NoOpListener {
    fn info(&self, _s: String) {}
    fn warning(&self, _s: String) {}
    fn progress(&self, _op: &str, _s: String) {}
    fn generated_archive(&self, _: &Path) {}
}

pub struct StdErrListener {
    pub verbose: bool,
}
impl Listener for StdErrListener {
    fn warning(&self, s: String) {
        let mut out = std::io::stderr().lock();
        for (i, line) in s.lines().enumerate() {
            let _ = writeln!(out, "{}{line}", if i == 0 { "warning: " } else { "         " });
        }
    }

    fn info(&self, s: String) {
        if self.verbose {
            let mut out = std::io::stderr().lock();
            for (i, line) in s.lines().enumerate() {
                let _ = writeln!(out, "{}{line}", if i == 0 { "info: " } else { "      " });
            }
        }
    }

    fn progress(&self, operation: &str, detail: String) {
        if self.verbose {
            let mut out = std::io::stderr().lock();
            let _ = writeln!(out, "{operation:>12} {detail}");
        }
    }
}

pub(crate) struct PrefixedListener<'l>(pub &'static str, pub &'l dyn Listener);
impl Listener for PrefixedListener<'_> {
    fn warning(&self, mut s: String) {
        s.insert_str(0, self.0);
        self.1.warning(s);
    }

    fn info(&self, mut s: String) {
        s.insert_str(0, self.0);
        self.1.info(s);
    }
}
