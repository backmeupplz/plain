//! Terminal prompts: a list to pick from with the arrow keys, a line of text, hidden input for keys and
//! passwords. Raw mode is on only while a list is up; text is read in the terminal's own line mode, so
//! editing, pasting and long lines work as usual.
use std::io::Write;

pub fn tty() -> bool {
    #[cfg(unix)]
    return unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 };
    #[cfg(not(unix))]
    false
}

fn paint(code: &str, s: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() { s.to_string() } else { format!("\x1b[{code}m{s}\x1b[0m") }
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}

/// The terminal's settings as they were, put back on drop.
#[cfg(unix)]
struct Mode(libc::termios);

#[cfg(unix)]
impl Mode {
    fn set(change: impl FnOnce(&mut libc::termios)) -> Option<Mode> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let old = Mode(t);
            change(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(old)
        }
    }
}

#[cfg(unix)]
impl Drop for Mode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

#[cfg(unix)]
extern "C" fn nothing(_: libc::c_int) {}

/// While alive, ctrl-c doesn't end this process: it ends the command the menu runs (children get the default
/// handler back when they start), or cancels a prompt, since the read waiting for a line is not restarted.
pub struct Interrupt(#[cfg(unix)] libc::sigaction);

impl Interrupt {
    pub fn catch() -> Interrupt {
        #[cfg(unix)]
        unsafe {
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = nothing as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut new.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &new, &mut old);
            Interrupt(old)
        }
        #[cfg(not(unix))]
        Interrupt()
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut())
        };
    }
}

/// One line from stdin, trimmed. None: ctrl-c, ctrl-d or no more input.
fn read_line() -> Option<String> {
    #[cfg(unix)]
    {
        // a byte at a time: from a pipe, one read could take the next answer's line too
        let mut buf = Vec::new();
        while buf.last() != Some(&b'\n') {
            let mut b = 0u8;
            if unsafe { libc::read(0, (&mut b as *mut u8).cast(), 1) } <= 0 {
                return None;
            }
            buf.push(b);
        }
        Some(String::from_utf8_lossy(&buf).trim().to_string())
    }
    #[cfg(not(unix))]
    {
        let mut s = String::new();
        (std::io::stdin().read_line(&mut s).ok()? > 0).then(|| s.trim().to_string())
    }
}

fn ask(prompt: &str, extra: &str) {
    eprint!("{} {}{extra} ", green("?"), bold(prompt));
}

/// A line of text; `default` when left empty. None: cancelled.
pub fn input(prompt: &str, default: Option<&str>) -> Option<String> {
    let _i = Interrupt::catch();
    ask(prompt, &default.map(|d| dim(&format!(" ({d})"))).unwrap_or_default());
    let Some(s) = read_line() else {
        eprintln!();
        return None;
    };
    Some(if s.is_empty() { default.unwrap_or_default().to_string() } else { s })
}

/// A line typed without echo. None: cancelled.
pub fn secret(prompt: &str) -> Option<String> {
    let _i = Interrupt::catch();
    ask(prompt, &dim(" (hidden)"));
    #[cfg(unix)]
    let _m = Mode::set(|t| t.c_lflag &= !libc::ECHO);
    let s = read_line();
    eprintln!();
    s
}

pub fn confirm(prompt: &str) -> bool {
    input(&format!("{prompt} [y/N]"), None).is_some_and(|a| matches!(a.to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(unix)]
fn size() -> (usize, usize) {
    let mut w: libc::winsize = unsafe { std::mem::zeroed() };
    match unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut w) } {
        0 if w.ws_col > 0 => (w.ws_col as usize, w.ws_row as usize),
        _ => (80, 24),
    }
}

/// Pick one of `items` (label, hint), starting at `start`. None: esc, q or ctrl-c.
pub fn select(prompt: &str, items: &[(String, String)], start: usize) -> Option<usize> {
    #[cfg(not(unix))]
    {
        for (i, (l, h)) in items.iter().enumerate() {
            eprintln!("  {}. {l}  {}", i + 1, dim(h));
        }
        return input(prompt, Some(&(start + 1).to_string()))?.parse::<usize>().ok().filter(|n| (1..=items.len()).contains(n)).map(|n| n - 1);
    }
    #[cfg(unix)]
    {
        if items.is_empty() {
            return None;
        }
        let _m = Mode::set(|t| {
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
        })?;
        let (cols, rows) = size();
        let width = items.iter().map(|i| i.0.chars().count()).max().unwrap_or(0);
        let shown = items.len().min(rows.saturating_sub(4).max(3));
        let (mut at, mut top, mut drawn) = (start.min(items.len() - 1), 0, 0);
        let mut err = std::io::stderr();
        let picked = loop {
            top = top.min(at).max((at + 1).saturating_sub(shown));
            let mut out = if drawn > 0 { format!("\x1b[{drawn}A\r\x1b[J") } else { "\x1b[?25l".into() };
            out += &format!("{} {}\n", green("?"), bold(prompt));
            for (i, (label, hint)) in items.iter().enumerate().skip(top).take(shown) {
                let pad = " ".repeat(width - label.chars().count());
                let hint: String = hint.chars().take(cols.saturating_sub(width + 6)).collect();
                out += &match i == at {
                    true => format!("{} {}{pad}  {}\n", green("❯"), bold(&green(label)), hint),
                    false => format!("  {label}{pad}  {}\n", dim(&hint)),
                };
            }
            let more = if shown < items.len() { format!(" · {}/{}", at + 1, items.len()) } else { String::new() };
            out += &dim(&format!("  ↑↓ move · enter pick · esc back{more}\n"));
            drawn = shown + 2;
            err.write_all(out.as_bytes()).ok();
            let mut b = [0u8; 16];
            let n = unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) };
            match &b[..n.max(0) as usize] {
                [27, b'[' | b'O', b'A'] | [b'k'] => at = (at + items.len() - 1) % items.len(),
                [27, b'[' | b'O', b'B'] | [b'j'] => at = (at + 1) % items.len(),
                [b'\r' | b'\n'] => break Some(at),
                [] | [27] | [3] | [4] | [b'q'] => break None,
                _ => {}
            }
        };
        // the list folds into one line: the question and the answer
        let answer = picked.map(|i| green(&items[i].0)).unwrap_or_else(|| dim("cancelled"));
        eprint!("\x1b[{drawn}A\r\x1b[J{} {} {answer}\n\x1b[?25h", green("?"), bold(prompt));
        picked
    }
}
