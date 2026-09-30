//! Drives the real binary in a pseudo-terminal and reads back what a terminal would show.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const COLS: u16 = 100;
pub const ROWS: u16 = 24;
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    _master: Box<dyn MasterPty + Send>,
    _home: tempfile::TempDir,
    pub cwd_file: PathBuf,
}

#[derive(Default)]
pub struct Opts<'a> {
    pub config: Option<&'a str>,
    /// What the terminal replies when the program asks about it, as a real terminal would.
    pub terminal_answers: Option<&'static [u8]>,
    pub args: Vec<String>,
    pub env: Vec<(&'a str, String)>,
}

impl Session {
    pub fn spawn(root: &Path) -> Session {
        Session::spawn_with(root, Opts::default())
    }

    pub fn spawn_with(root: &Path, opts: Opts) -> Session {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("config");
        // The test terminal never answers the graphics query, so tests draw pictures with half blocks.
        let config = match opts.config {
            Some(config) if config.contains("images") => config.to_string(),
            Some(config) => format!("images = \"halfblocks\"\n{config}"),
            None => "images = \"halfblocks\"\n".to_string(),
        };
        fs::create_dir_all(config_dir.join("tx")).unwrap();
        fs::write(config_dir.join("tx/config.toml"), config).unwrap();
        let cwd_file = home.path().join("cwd");

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tx"));
        cmd.arg("--cwd-file");
        cmd.arg(&cwd_file);
        cmd.args(&opts.args);
        cmd.arg(root);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // macOS trashes into the real ~/.Trash whatever HOME says, so undo must look there too.
        if cfg!(not(target_os = "macos")) {
            cmd.env("HOME", home.path());
        }
        cmd.env("XDG_CONFIG_HOME", &config_dir);
        cmd.env("XDG_STATE_HOME", home.path().join("state"));
        // The machine running the tests may itself be reached over SSH, which changes how long the
        // program waits for the terminal. Tests that want SSH say so in `env`.
        // Nor should the terminal running the tests leak in and decide the picture protocol.
        for name in [
            "SSH_CONNECTION",
            "SSH_TTY",
            "SSH_CLIENT",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "ITERM_SESSION_ID",
            "KITTY_WINDOW_ID",
            "WEZTERM_EXECUTABLE",
        ] {
            cmd.env_remove(name);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let sink = Arc::clone(&parser);
        let replier = Arc::clone(&writer);
        let mut answers = opts.terminal_answers;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut seen = Vec::new();
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buf[..n]);
                if let Some(reply) = answers {
                    seen.extend_from_slice(&buf[..n]);
                    if seen.windows(3).any(|w| w == b"\x1b[c") {
                        let mut w = replier.lock().unwrap();
                        w.write_all(reply).unwrap();
                        w.flush().unwrap();
                        answers = None;
                    }
                }
            }
        });
        Session {
            child,
            writer,
            parser,
            _master: pair.master,
            _home: home,
            cwd_file,
        }
    }

    /// The home directory the app runs with, where its trash lives.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn home(&self) -> &Path {
        self._home.path()
    }

    pub fn send(&mut self, bytes: &str) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    pub fn rows(&self) -> Vec<String> {
        let parser = self.parser.lock().unwrap();
        parser.screen().rows(0, COLS).collect()
    }

    pub fn screen(&self) -> String {
        self.rows().join("\n")
    }

    pub fn fg(&self, row: u16, col: u16) -> vt100::Color {
        let parser = self.parser.lock().unwrap();
        parser
            .screen()
            .cell(row, col)
            .map_or(vt100::Color::Default, |c| c.fgcolor())
    }

    pub fn inverse(&self, row: u16, col: u16) -> bool {
        let parser = self.parser.lock().unwrap();
        parser.screen().cell(row, col).is_some_and(|c| c.inverse())
    }

    /// Whether the program switched back from the alternate screen, as it must on exit.
    pub fn left_alternate_screen(&self) -> bool {
        !self.parser.lock().unwrap().screen().alternate_screen()
    }

    pub fn signal(&self, signal: i32) {
        let pid = self.child.process_id().expect("a running child");
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    pub fn bg(&self, row: u16, col: u16) -> vt100::Color {
        let parser = self.parser.lock().unwrap();
        parser
            .screen()
            .cell(row, col)
            .map_or(vt100::Color::Default, |c| c.bgcolor())
    }

    /// Waits until the screen satisfies `pred`, panicking with the screen if it never does.
    pub fn wait(&self, what: &str, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
        let start = Instant::now();
        loop {
            let rows = self.rows();
            if pred(&rows) {
                return rows;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for {what}. Screen:\n{}",
                rows.join("\n")
            );
            thread::sleep(Duration::from_millis(15));
        }
    }

    pub fn wait_for_text(&self, text: &str) -> Vec<String> {
        self.wait(&format!("{text:?}"), |rows| {
            rows.iter().any(|r| r.contains(text))
        })
    }

    /// The row holding the cursor of the focused column, which is always the middle of the tree area.
    pub const CENTER: usize = 1 + (ROWS as usize - 2) / 2;

    pub fn wait_exit(&mut self) -> u32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "process did not exit. Screen:\n{}",
                self.screen()
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// A directory alone inside a fresh parent, so the parent column the tree shows above it is the
/// same on every machine.
pub struct Fixture {
    root: std::path::PathBuf,
    _parent: tempfile::TempDir,
}

impl Fixture {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

pub fn fixture() -> Fixture {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let tmp = Fixture {
        root,
        _parent: parent,
    };
    let r = tmp.path();
    for d in ["apps/web/src", "apps/api", "notes", "zeta"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    fs::write(r.join("README.md"), "# readme\n").unwrap();
    fs::write(r.join("apps/web/index.html"), "<html></html>\n").unwrap();
    tmp
}

pub const ENTER: &str = "\r";
pub const ESC: &str = "\x1b";
pub const CTRL_C: &str = "\x03";
pub const CTRL_U: &str = "\x15";
