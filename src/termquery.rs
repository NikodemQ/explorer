//! Asks the terminal what it can draw, the way Yazi does, but never waits on it for long.
//!
//! The questions end with a device attributes request (DA1), which every terminal answers, so
//! reading stops as soon as that answer arrives. A terminal that answers nothing costs the
//! timeout once, and no reader is left behind to swallow keys later.

use std::{
    io::{self, Write},
    os::fd::AsFd,
    time::{Duration, Instant},
};

/// What the terminal said about itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Answers {
    /// The terminal accepted a kitty graphics query.
    pub kitty: bool,
    /// It also read a picture from a temporary file of ours, so it runs on this machine and
    /// pictures need not travel through the connection.
    pub kitty_files: bool,
    /// It inflates zlib-compressed pictures, which then cross a slow connection in fewer bytes.
    pub kitty_zlib: bool,
    /// DA1 listed sixel graphics (attribute 4).
    pub sixel: bool,
    /// The name and version from XTVERSION, such as `foot(1.16.2)` or `WezTerm 20240203`.
    pub name: Option<String>,
    /// Pixel size of one cell, from `CSI 16 t`.
    pub cell: Option<(u16, u16)>,
    /// DA1 arrived, so the other answers are complete.
    pub complete: bool,
}

const KITTY_QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const XTVERSION: &str = "\x1b[>0q";
const CELL_SIZE: &str = "\x1b[16t";
const DA1: &str = "\x1b[c";

/// A kitty graphics query for a one pixel picture sent compressed.
fn kitty_zlib_query() -> String {
    use std::io::Write as _;
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = zlib.write_all(&[0; 3]);
    let data = base64_simd::STANDARD.encode_to_string(zlib.finish().unwrap_or_default());
    format!("\x1b_Gi=33,s=1,v=1,a=q,t=d,f=24,o=z;{data}\x1b\\")
}

/// A kitty graphics query for a one pixel picture in the file at `path`.
fn kitty_file_query(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let name = base64_simd::STANDARD.encode_to_string(path.as_os_str().as_bytes());
    format!("\x1b_Gi=32,s=1,v=1,a=q,t=t,f=24;{name}\x1b\\")
}

/// The bytes to send, with a question about reading pictures from the file at `probe` when given.
/// Inside tmux the questions about graphics go through to the outer terminal, which tmux only
/// allows with its `allow-passthrough` option on.
pub fn questions(in_tmux: bool, probe: Option<&std::path::Path>) -> String {
    let wrap = |q: &str| {
        if in_tmux {
            format!("\x1bPtmux;{}\x1b\\", q.replace('\x1b', "\x1b\x1b"))
        } else {
            q.to_string()
        }
    };
    format!(
        "{}{}{}{}{}{DA1}",
        wrap(KITTY_QUERY),
        wrap(&kitty_zlib_query()),
        probe
            .map(|p| wrap(&kitty_file_query(p)))
            .unwrap_or_default(),
        wrap(XTVERSION),
        wrap(CELL_SIZE)
    )
}

/// Reads the answers out of what the terminal sent back. Anything else, such as keys typed at
/// the same moment, is ignored.
pub fn parse(bytes: &[u8]) -> Answers {
    let text = String::from_utf8_lossy(bytes);
    let mut answers = Answers::default();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find('\x1b') {
        rest = &rest[start..];
        if let Some(body) = rest.strip_prefix("\x1b_G") {
            let end = body.find("\x1b\\").unwrap_or(body.len());
            let reply = &body[..end];
            if reply.ends_with(";OK") {
                answers.kitty |= reply.starts_with("i=31");
                answers.kitty_files |= reply.starts_with("i=32");
                answers.kitty_zlib |= reply.starts_with("i=33");
            }
            rest = &body[end..];
        } else if let Some(body) = rest.strip_prefix("\x1bP>|") {
            let end = body.find("\x1b\\").unwrap_or(body.len());
            answers.name = Some(body[..end].to_string());
            rest = &body[end..];
        } else if let Some(body) = rest.strip_prefix("\x1b[?") {
            let end = body.find('c').unwrap_or(body.len());
            let attributes = &body[..end];
            answers.sixel = attributes.split(';').any(|a| a == "4");
            answers.complete = end < body.len();
            rest = &body[end..];
        } else if let Some(body) = rest.strip_prefix("\x1b[6;") {
            let end = body.find('t').unwrap_or(body.len());
            let mut numbers = body[..end].split(';').filter_map(|n| n.parse::<u16>().ok());
            if let (Some(height), Some(width)) = (numbers.next(), numbers.next())
                && height > 0
                && width > 0
            {
                answers.cell = Some((width, height));
            }
            rest = &body[end..];
        } else {
            rest = &rest[1..];
        }
    }
    answers
}

/// Sends the questions and collects the answers until DA1 arrives or `timeout` passes.
/// The terminal must be in raw mode, and nothing else may be reading stdin yet.
pub fn ask(in_tmux: bool, timeout: Duration) -> io::Result<Answers> {
    // The terminal deletes the file once it has read it, like the pictures sent this way later.
    // The name must say what it is for, and the file must be in the temporary folder.
    let probe = std::env::temp_dir().join(format!(
        "tty-graphics-protocol-tx-{}-probe",
        std::process::id()
    ));
    let probe = std::fs::write(&probe, [0u8; 3]).is_ok().then_some(probe);
    let answers = ask_with(in_tmux, timeout, probe.as_deref());
    if let Some(probe) = probe {
        let _ = std::fs::remove_file(probe);
    }
    answers
}

fn ask_with(
    in_tmux: bool,
    timeout: Duration,
    probe: Option<&std::path::Path>,
) -> io::Result<Answers> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut out = io::stdout();
    out.write_all(questions(in_tmux, probe).as_bytes())?;
    out.flush()?;
    let stdin = io::stdin();
    let fd = stdin.as_fd();
    let deadline = Instant::now() + timeout;
    let mut received = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let wait = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        if poll(&mut fds, Some(&wait))? == 0 {
            break;
        }
        let n = rustix::io::read(fd, &mut chunk)?;
        if n == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..n]);
        if parse(&received).complete {
            break;
        }
    }
    Ok(parse(&received))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_reports_graphics_its_name_and_cell_size() {
        let reply = b"\x1b_Gi=31;OK\x1b\\\x1bP>|kitty(0.35.2)\x1b\\\x1b[6;20;10t\x1b[?62;c";
        let a = parse(reply);
        assert!(a.kitty && a.complete && !a.sixel);
        assert_eq!(a.name.as_deref(), Some("kitty(0.35.2)"));
        assert_eq!(a.cell, Some((10, 20)));
    }

    #[test]
    fn a_sixel_terminal_lists_attribute_4() {
        let a = parse(b"\x1bP>|foot(1.16.2)\x1b\\\x1b[?62;4;22c");
        assert!(a.sixel && !a.kitty && a.complete);
        assert_eq!(a.name.as_deref(), Some("foot(1.16.2)"));
        assert!(!parse(b"\x1b[?62;14;22c").sixel, "14 is not 4");
    }

    #[test]
    fn a_kitty_error_reply_is_not_support() {
        let a = parse(b"\x1b_Gi=31;EINVAL:unsupported\x1b\\\x1b[?1;2c");
        assert!(!a.kitty);
    }

    #[test]
    fn a_terminal_that_answers_only_da1_is_complete_with_nothing_else() {
        let a = parse(b"\x1b[?1;2c");
        assert_eq!(
            a,
            Answers {
                complete: true,
                ..Answers::default()
            }
        );
    }

    #[test]
    fn keys_typed_in_between_and_partial_answers_are_harmless() {
        let a = parse(b"jk\x1b[6;18;9tq\x1bP>|WezTerm 20240203\x1b\\x\x1b[?6");
        assert_eq!(a.cell, Some((9, 18)));
        assert_eq!(a.name.as_deref(), Some("WezTerm 20240203"));
        assert!(!a.complete, "DA1 has not finished");
        assert_eq!(parse(b"").cell, None);
        assert_eq!(parse(b"\x1b[6;0;0t").cell, None, "zero sizes are not sizes");
    }

    #[test]
    fn inside_tmux_the_graphics_questions_are_passed_through_and_da1_is_not() {
        let q = questions(true, None);
        assert!(q.starts_with("\x1bPtmux;\x1b\x1b_Gi=31"), "{q:?}");
        assert!(q.ends_with("\x1b[c"));
        assert!(!questions(false, None).contains("tmux;"));
    }

    #[test]
    fn a_terminal_that_reads_the_probe_file_can_take_pictures_as_files() {
        let q = questions(false, Some(std::path::Path::new("/tmp/x")));
        assert!(q.contains("i=32,s=1,v=1,a=q,t=t,f=24;L3RtcC94"), "{q:?}");
        let a = parse(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b_Gi=33;OK\x1b\\\x1b[?62;c");
        assert!(a.kitty && a.kitty_files && a.kitty_zlib);
        let a = parse(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EBADF:no such file\x1b\\\x1b[?62;c");
        assert!(a.kitty && !a.kitty_files, "over SSH the file is not there");
    }
}
