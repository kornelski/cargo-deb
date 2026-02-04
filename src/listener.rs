use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use std::error::Error;
use std::io::{StderrLock, Write};
use std::path::Path;

#[cfg_attr(test, mockall::automock)]
pub trait Listener: Send + Sync {
    fn warning(&self, s: String);
    fn info(&self, s: String);

    fn progress(&self, operation: &str, detail: String) {
        self.info(format!("{operation}: {detail}"));
    }

    #[allow(unused_parens)]
    fn error(&self, error: &(dyn Error + 'static)) {
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
        Self::label_locked(&mut out, label, 0, style, text);
    }

    fn label_locked(out: &mut AutoStream<StderrLock<'static>>, label: &str, indent: u8, style: Style, text: &str) {
        let text = text.strip_prefix(label).and_then(|t| t.strip_prefix(": ")).unwrap_or(text).trim_end();
        let mut lines = text.lines();
        if let Some(line) = lines.next() {
            let _ = writeln!(*out, "{:width$}{style}{label}{style:#}: {line}", "", width = indent as usize);
        }
        for line in lines {
            let _ = writeln!(*out, "{:width$}{line}", "", width = indent as usize + label.len() + 2);
        }
    }

    fn error_with_notes(out: &mut AutoStream<StderrLock<'static>>, err: &(dyn Error + 'static), primary_error: bool) {
        let err_msg = err.to_string();
        let mut messages = err_msg.split("\nnote: ");
        let err_msg = messages.next().unwrap_or_default();

        if primary_error {
            Self::label_locked(&mut *out, "error", 0, Style::new().bold().fg_color(Some(AnsiColor::Red.into())), err_msg);
        } else {
            Self::label_locked(&mut *out, "cause", 2, Style::new().fg_color(Some(AnsiColor::Red.into())), err_msg);
        }

        for note in messages.map(|n| n.trim_start()).filter(|n| !n.is_empty()) {
            Self::label_locked(out, "note", if primary_error { 0 } else { 3 }, Style::new().fg_color(Some(AnsiColor::Cyan.into())), note);
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

    fn error(&self, err: &(dyn Error + 'static)) {
        let mut out = AutoStream::new(std::io::stderr(), self.color).lock();
        Self::error_with_notes(&mut out, err, true);

        let mut cause = err.source();
        let mut max_causes = 5;
        while let Some(err) = cause {
            max_causes -= 1;
            if max_causes == 0 {
                break;
            }
            Self::error_with_notes(&mut out, err, false);
            cause = err.source();
        }
    }

    fn progress(&self, operation: &str, detail: String) {
        if self.verbose {
            let mut out = AutoStream::new(std::io::stderr(), self.color).lock();
            let style = Style::new().bold().fg_color(Some(AnsiColor::Green.into()));
            let _ = writeln!(out, "{style}{operation:>12}{style:#} {detail}");
        }
    }
}

pub(crate) struct PrefixedListener<'l>(pub &'l str, pub &'l dyn Listener);
impl Listener for PrefixedListener<'_> {
    fn warning(&self, mut s: String) {
        s.insert_str(0, self.0);
        self.1.warning(s);
    }

    fn error(&self, err: &(dyn Error + 'static)) {
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
