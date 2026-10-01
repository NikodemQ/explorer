use std::{fs, io, os::unix::ffi::OsStrExt, path::PathBuf};

use clap::Parser;
use tx::{
    app::{App, Exit, Settings},
    config::Config,
    imageview::{Mode as ImageMode, Painter},
    keys::Keymap,
    marks::Marks,
    ops::SystemTrash,
    runtime,
    shell::{self, Shell},
    termquery,
    theme::Depth,
};

/// Tree file explorer: every directory you enter opens a new column to the right.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Directory to start in (default: the current directory)
    path: Option<PathBuf>,

    /// On quit, write the directory you ended in to FILE (used by the shell function)
    #[arg(long, value_name = "FILE")]
    cwd_file: Option<PathBuf>,

    /// Print a shell function that makes quitting tx change the shell's directory
    #[arg(long, value_name = "SHELL")]
    init: Option<Shell>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    if let Some(shell) = cli.init {
        print!("{}", shell::init_script(shell));
        return Ok(());
    }
    let root = match cli.path {
        Some(path) => path.canonicalize()?,
        None => std::env::current_dir()?,
    };
    let (config, mut notice) = Config::load();
    let keymap = Keymap::with_overrides(&config.keys).unwrap_or_else(|error| {
        notice = Some(format!("config: {error}"));
        Keymap::default()
    });
    // Asking the terminal about graphics reads its answer from stdin, so it runs before anything else does.
    let depth = config
        .colors
        .unwrap_or_else(|| Depth::detect(|name| std::env::var(name).ok()));
    let images = if depth == Depth::None {
        ImageMode::Off
    } else {
        config.images
    };
    let mut terminal = ratatui::init();
    // Asked in raw mode and before the input thread starts, so the answers are not taken for keys.
    // Graphics modes set in the config are asked too, for the cell size that pictures are sized by.
    let asks = !matches!(
        images,
        ImageMode::Off | ImageMode::Blocks | ImageMode::Halfblocks
    );
    let over_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let answers = if asks {
        let in_tmux = std::env::var_os("TMUX").is_some();
        // Over SSH the answers cross the network. Reading stops at the DA1 answer anyway, so a longer
        // limit only costs time with a terminal that never answers, and an answer arriving after the
        // limit would be read as keystrokes.
        let limit = if over_ssh { 2000 } else { 500 };
        termquery::ask(in_tmux, std::time::Duration::from_millis(limit)).ok()
    } else {
        None
    };
    let painter = Painter::new(images, answers.as_ref()).remote(over_ssh);
    let screen = terminal.size()?;
    // Pictures only ever fill the preview column, which usually gets about half the screen.
    let (width, height) = painter.decode_target(screen.width / 2, screen.height.saturating_sub(2));
    tx::preview::set_image_target(width, height);
    let settings = Settings {
        show_hidden: config.show_hidden,
        painter,
        depth,
        marks: Marks::open(Marks::default_file()),
        trash: std::sync::Arc::new(SystemTrash),
    };
    let mut app = App::with_settings(root, keymap, settings);
    app.message = notice;
    if config.mouse {
        ratatui::crossterm::execute!(io::stdout(), ratatui::crossterm::event::EnableMouseCapture)?;
        // ratatui's own panic hook restores the screen but not mouse reporting, which would keep flooding the shell.
        let restore_screen = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = ratatui::crossterm::execute!(
                io::stdout(),
                ratatui::crossterm::event::DisableMouseCapture
            );
            restore_screen(info);
        }));
    }
    let result = runtime::run(&mut terminal, &mut app, &config);
    let _ =
        ratatui::crossterm::execute!(io::stdout(), ratatui::crossterm::event::DisableMouseCapture);
    ratatui::restore();
    if let (Exit::Quit, Some(file)) = (result?, cli.cwd_file) {
        fs::write(file, app.tree().current_dir().as_os_str().as_bytes())?;
    }
    Ok(())
}
