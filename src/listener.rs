use anstream::{AutoStream, ColorChoice};
use anstyle::{Style, AnsiColor};
use std::error::Error;
use std::io::{Write, StderrLock};
use std::path::Path;

#[cfg_attr(test, mockall::automock)]
pub trait Listener: Send + Sync {
    fn warning(&self, s: String);
    fn info(&self, s: String);

    fn progress(&self, operation: &str, detail: String) {
        self.info(format!("{operation}: {detail}"))
    }

    fn error(&self, error: &dyn Error) {
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "cargo-deb: {error}");
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
    pub quiet: bool,
    pub color: ColorChoice,
}

impl StdErrListener {
    fn label(&self, label: &str, style: Style, text: &str) {
        let mut out = AutoStream::new(std::io::stderr(), self.color).lock();
        self.label_locked(&mut out, label, style, text);
    }

    fn label_locked(&self, out: &mut AutoStream<StderrLock<'static>>, label: &str, style: Style, text: &str) {
        let text = text.strip_prefix(label).and_then(|t| t.strip_prefix(": ")).unwrap_or(text);
        let mut lines = text.lines();
        if let Some(line) = lines.next() {
            let _ = writeln!(*out, "{style}{label}{style:#}: {line}");
        }
        for line in lines {
            let _ = writeln!(*out, "{:width$}{line}", "", width = label.len() + 2);
        }
    }
}

impl Listener for StdErrListener {
    fn warning(&self, s: String) {
        if !self.quiet {
            self.label("warning", Style::new().bold().fg_color(Some(AnsiColor::Yellow.into())), &s);
        }
    }

    fn info(&self, s: String) {
        if self.verbose {
            self.label("info", Style::new().bold().fg_color(Some(AnsiColor::Cyan.into())), &s);
        }
    }

    fn error(&self, err: &dyn Error) {
        let mut cause = err.source();
        let mut causes = String::new();
        let mut max_causes = 3;
        while let Some(err) = cause {
            max_causes -= 1;
            if max_causes == 0 {
                break;
            }
            causes = format!("{err}\n\n{causes}");
            cause = err.source();
        }
        let causes = causes.trim_end();

        let mut out = AutoStream::new(std::io::stderr(), self.color).lock();
        if !causes.is_empty() {
            self.label_locked(&mut out, "error", Style::new().fg_color(Some(AnsiColor::Red.into())), causes);
        }
        self.label_locked(&mut out, "error", Style::new().bold().fg_color(Some(AnsiColor::Red.into())), &err.to_string());
    }

    fn progress(&self, operation: &str, detail: String) {
        if self.verbose {
            let mut out = AutoStream::new(std::io::stderr(), self.color).lock();
            let style = Style::new().bold().fg_color(Some(AnsiColor::Green.into()));
            let _ = writeln!(out, "{style}{operation:>12}{style:#} {detail}");
        }
    }
}

pub(crate) struct PrefixedListener<'l>(pub &'static str, pub &'l dyn Listener);
impl Listener for PrefixedListener<'_> {
    fn warning(&self, mut s: String) {
        s.insert_str(0, self.0);
        self.1.warning(s);
    }

    fn error(&self, err: &dyn Error) {
        self.1.error(err);
    }

    fn info(&self, mut s: String) {
        s.insert_str(0, self.0);
        self.1.info(s);
    }

    fn progress(&self, operation: &str, mut s: String) {
        s.insert_str(0, self.0);
        self.1.progress(operation, s);
    }
}
