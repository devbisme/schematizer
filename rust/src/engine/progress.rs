//! Tell the user which step is being worked on.

use std::cell::RefCell;
use std::io::{IsTerminal, Write};

enum Sink {
    Off,
    Stderr { tty: bool, shown: bool },
    Func(Box<dyn FnMut(&str)>),
    /// Messages from a worker thread, sent to the thread that shows them.
    Channel(std::sync::mpsc::Sender<String>),
}

struct State {
    sink: Sink,
    prefixes: Vec<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State { sink: Sink::Off, prefixes: Vec::new() });
}

/// Show progress as a status line on stderr.
pub fn enable_stderr() {
    STATE.with(|s| s.borrow_mut().sink = Sink::Stderr { tty: std::io::stderr().is_terminal(), shown: false });
}

/// Send each progress message to a function.
pub fn enable_func(f: Box<dyn FnMut(&str)>) {
    STATE.with(|s| s.borrow_mut().sink = Sink::Func(f));
}

/// Send this thread's progress messages to another thread.
pub fn enable_channel(tx: std::sync::mpsc::Sender<String>) {
    STATE.with(|s| s.borrow_mut().sink = Sink::Channel(tx));
}

/// True if progress messages are going anywhere from this thread.
pub fn is_enabled() -> bool {
    STATE.with(|s| !matches!(s.borrow().sink, Sink::Off))
}

/// Stop showing progress, clearing the status line.
pub fn disable() {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if let Sink::Stderr { tty: true, shown: true } = st.sink {
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        }
        st.sink = Sink::Off;
        st.prefixes.clear();
    });
}

/// Report the step being worked on.
pub fn report(message: &str) {
    let mut text = STATE.with(|s| s.borrow().prefixes.join(" "));
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(message);
    report_raw(&text);
}

/// Show a message as it is, without this thread's prefixes.
pub fn report_raw(text: &str) {
    let text = text.to_string();
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        match &mut st.sink {
            Sink::Off => {}
            Sink::Stderr { tty, shown } => {
                let line = format!("schematizer: {}...", text);
                if *tty {
                    let shown_text: String = line.chars().take(100).collect();
                    eprint!("\r\x1b[K{}", shown_text);
                    *shown = true;
                } else {
                    eprintln!("{}", line);
                }
                let _ = std::io::stderr().flush();
            }
            Sink::Func(f) => f(&text),
            Sink::Channel(tx) => {
                let _ = tx.send(text);
            }
        }
    });
}

/// Put a note in front of the messages reported until the guard is dropped.
pub fn prefixed(note: &str) -> PrefixGuard {
    STATE.with(|s| s.borrow_mut().prefixes.push(note.to_string()));
    PrefixGuard
}

pub struct PrefixGuard;

impl Drop for PrefixGuard {
    fn drop(&mut self) {
        STATE.with(|s| {
            s.borrow_mut().prefixes.pop();
        });
    }
}
