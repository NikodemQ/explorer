mod common;

use common::*;

/// Everything but the header and footer lines.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn tree_rows(rows: &[String]) -> &[String] {
    &rows[1..rows.len() - 1]
}

fn center(rows: &[String]) -> &str {
    &rows[Session::CENTER]
}

#[test]
fn starts_on_the_first_entry_flush_left_with_the_preview_and_brace() {
    let tmp = fixture();
    let s = Session::spawn(tmp.path());
    let rows = s.wait_for_text("apps/");
    assert!(center(&rows).starts_with(" apps/"), "{:?}", center(&rows));
    assert!(center(&rows).contains("─┬─"), "{:?}", center(&rows));
    assert!(rows.iter().any(|r| r.contains("web/")), "{rows:#?}");
}

#[test]
fn vim_counts_and_jumps_move_the_cursor_row() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("2j");
    let rows = s.wait("cursor on zeta", |r| center(r).contains("zeta/"));
    assert!(center(&rows).starts_with(" zeta/"));
    s.send("gg");
    s.wait("cursor back on apps", |r| center(r).contains("apps/"));
    s.send("G");
    s.wait("cursor on README", |r| center(r).contains("README.md"));
    s.send("fn");
    s.wait("f jumps by letter", |r| center(r).contains("notes/"));
}

#[test]
fn slash_search_previews_as_you_type_and_escape_restores() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("/zet");
    let rows = s.wait("incremental jump", |r| center(r).contains("zeta/"));
    assert!(
        rows[rows.len() - 1].trim() == "/zet",
        "{:?}",
        rows[rows.len() - 1]
    );
    s.send(ESC);
    s.wait("restored", |r| {
        center(r).contains("apps/") && !r[r.len() - 1].contains("/zet")
    });
}

#[test]
fn question_mark_opens_the_key_reference_and_q_closes_it() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("?");
    let rows = s.wait_for_text("move down");
    assert!(rows.iter().any(|r| r.contains("gg")), "{rows:#?}");
    s.send("q");
    s.wait("overlay gone", |r| {
        !r.iter().any(|x| x.contains("move down"))
    });
}

#[test]
fn levels_have_distinct_colors_and_the_cursor_row_is_filled() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("l");
    let rows = s.wait("focus moved into apps", |r| {
        r.iter().any(|x| x.contains("api/"))
    });
    let row = Session::CENTER as u16;
    let text = center(&rows).to_string();
    let first = text.find("apps/").unwrap() as u16;
    let second = text.find("api/").unwrap() as u16;
    assert_ne!(
        s.fg(row, first),
        s.fg(row, second),
        "each level has its own color"
    );
    assert_ne!(
        s.bg(row, second),
        vt100::Color::Default,
        "cursor row is filled"
    );
}

#[test]
fn quit_writes_the_directory_and_ctrl_c_does_not() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("ljl");
    s.wait("focus two levels deep", |r| {
        r.iter().any(|x| x.contains("index.html"))
    });
    s.send("q");
    assert_eq!(s.wait_exit(), 0);
    let written = std::fs::read_to_string(&s.cwd_file).unwrap();
    assert_eq!(
        written,
        tmp.path()
            .join("apps/web")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );

    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("l");
    s.wait_for_text("api/");
    s.send(CTRL_C);
    assert_eq!(s.wait_exit(), 0);
    assert!(!s.cwd_file.exists(), "ctrl-c must not move the shell");
}

#[test]
fn colon_commands_run_from_the_command_line() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send(":cd notes\r");
    s.wait("empty dir", |r| {
        r.iter().any(|x| x.contains("(empty)")) || r.iter().all(|x| !x.contains("apps/"))
    });
    s.send(":q\r");
    assert_eq!(s.wait_exit(), 0);
    assert!(
        std::fs::read_to_string(&s.cwd_file)
            .unwrap()
            .ends_with("notes")
    );
}

#[test]
fn config_can_rebind_keys_and_bad_config_is_reported() {
    let tmp = fixture();
    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some("[keys]\n\"<space>\" = \"down\"\n"),
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    s.send(" ");
    s.wait("space moved down", |r| center(r).contains("notes/"));

    let s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some("[keys]\nx = \"explode\"\n"),
            ..Opts::default()
        },
    );
    let rows = s.wait_for_text("unknown command");
    assert!(rows.last().unwrap().contains("config:"), "{rows:#?}");
}

#[test]
fn new_files_appear_without_a_keypress() {
    let tmp = fixture();
    let s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    std::fs::write(tmp.path().join("aaa-fresh.txt"), "x").unwrap();
    s.wait_for_text("aaa-fresh.txt");
}

#[test]
fn i_on_a_text_file_runs_the_configured_editor_and_returns() {
    let tmp = fixture();
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("log");
    let script = bin.path().join("ed.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nprintf 'IN-EDITOR> '\nread w\necho \"$w\" >> '{}'\n",
            log.display(),
            log.display()
        ),
    )
    .unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&script).unwrap().permissions(),
        0o755,
    );
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let config = format!("editor = \"{}\"\n", script.display());
    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some(&config),
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    s.send("G");
    s.wait("on README", |r| center(r).contains("README.md"));
    s.send("i");
    s.wait_for_text("IN-EDITOR>");
    s.send("hello\r");
    s.wait("tui is back", |r| {
        r.iter().any(|x| x.contains("README.md")) && !r.iter().any(|x| x.contains("IN-EDITOR"))
    });
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains("README.md") && logged.contains("hello"),
        "{logged}"
    );
    s.send("j");
    s.send("q");
    assert_eq!(s.wait_exit(), 0);
}

#[test]
fn zh_shows_dotfiles_live() {
    let tmp = fixture();
    std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    assert!(!s.screen().contains(".env"));
    s.send("zh");
    let rows = s.wait_for_text(".env");
    assert!(rows[0].contains("dotfiles shown"), "{:?}", rows[0]);
    s.send("zh");
    s.wait("hidden again", |r| !r.iter().any(|x| x.contains(".env")));
}

#[test]
fn bookmarks_survive_a_restart_and_jump_across_levels() {
    let tmp = fixture();
    let state = tempfile::tempdir().unwrap();
    let env = || vec![("XDG_STATE_HOME", state.path().to_str().unwrap().to_string())];

    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            env: env(),
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    s.send("ljlmH");
    s.wait_for_text("index.html");
    s.send("q");
    assert_eq!(s.wait_exit(), 0);
    assert!(
        std::fs::read_to_string(state.path().join("tx/marks"))
            .unwrap()
            .starts_with("H\t/")
    );

    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            env: env(),
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    s.send("'H");
    let rows = s.wait("jumped to the bookmark", |r| {
        center(r).contains("index.html") || center(r).contains("src/")
    });
    assert!(rows.iter().any(|r| r.contains("index.html")), "{rows:#?}");
    s.send("<");
    s.send(":marks\r");
    s.wait_for_text("H   ");
}

#[test]
fn ctrl_o_returns_to_where_a_jump_started() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("G");
    s.wait("on README", |r| center(r).contains("README.md"));
    s.send("\x0f");
    s.wait("back on apps", |r| center(r).contains("apps/"));
    s.send("\t");
    s.wait("forward on README", |r| center(r).contains("README.md"));
}

#[cfg(target_os = "linux")]
fn trash_files(s: &Session) -> std::path::PathBuf {
    s.home().join(".local/share/Trash/files")
}

#[cfg(target_os = "linux")]
#[test]
fn dd_moves_a_file_to_the_real_trash_and_u_brings_it_back() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("G");
    s.wait("on README", |r| center(r).contains("README.md"));
    s.send("dd");
    s.wait("gone from the listing", |r| {
        !tree_rows(r).iter().any(|x| x.contains("README.md"))
    });
    assert!(!tmp.path().join("README.md").exists());
    assert!(
        trash_files(&s).join("README.md").exists(),
        "it is in the freedesktop trash"
    );
    let rows = s.wait_for_text("moved README.md to the trash");
    assert!(
        rows.last().unwrap().contains("u undoes it"),
        "{:?}",
        rows.last()
    );
    s.send("u");
    s.wait("back in the listing", |r| {
        tree_rows(r).iter().any(|x| x.contains("README.md"))
    });
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("README.md")).unwrap(),
        "# readme\n"
    );
    assert!(!trash_files(&s).join("README.md").exists());
}

#[test]
fn yank_and_paste_copy_a_file_into_another_directory() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("Gyy");
    s.wait_for_text("yanked README.md");
    s.send("gglp");
    s.wait_for_text("pasted README.md");
    assert!(tmp.path().join("apps/README.md").is_file());
    assert!(tmp.path().join("README.md").is_file());
    let rows = s.wait("cursor on the copy", |r| center(r).contains("README.md"));
    assert!(center(&rows).contains("README.md"));
}

#[test]
fn a_paste_that_clashes_asks_and_keep_both_numbers_the_copy() {
    let tmp = fixture();
    std::fs::write(tmp.path().join("apps/README.md"), "other").unwrap();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("Gyygglp");
    let rows = s.wait_for_text("README.md exists");
    assert!(
        rows.last().unwrap().contains("[k]eep both"),
        "{:?}",
        rows.last()
    );
    s.send("k");
    s.wait_for_text("pasted README.md");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("apps/README.md")).unwrap(),
        "other"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("apps/README (1).md")).unwrap(),
        "# readme\n"
    );
}

#[test]
fn cut_and_paste_move_a_directory() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("2jxx");
    s.wait_for_text("cut zeta");
    s.send("gglp");
    s.wait_for_text("moved zeta");
    assert!(tmp.path().join("apps/zeta").is_dir());
    assert!(!tmp.path().join("zeta").exists());
}

#[test]
fn rename_and_new_prompts_edit_the_file_system_and_the_view() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("G");
    s.wait("on README", |r| center(r).contains("README.md"));
    s.send("r");
    s.wait_for_text("rename: README.md");
    s.send(&format!("{CTRL_U}NOTES.md{ENTER}"));
    s.wait_for_text("renamed to NOTES.md");
    assert!(tmp.path().join("NOTES.md").exists() && !tmp.path().join("README.md").exists());
    s.wait("cursor follows the rename", |r| {
        center(r).contains("NOTES.md")
    });
    s.send("onew-dir/\r");
    s.wait_for_text("created new-dir/");
    assert!(tmp.path().join("new-dir").is_dir());
    s.wait("cursor on the new folder", |r| {
        center(r).contains("new-dir/")
    });
}

#[test]
fn visual_mode_deletes_a_range_and_escape_leaves_it() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("v");
    s.wait_for_text("-- VISUAL --");
    s.send(ESC);
    s.wait("visual gone", |r| {
        !r.iter().any(|x| x.contains("-- VISUAL --"))
    });
    s.send("vjd");
    s.wait("two folders gone", |r| {
        !r.iter().any(|x| x.contains("apps/")) && !r.iter().any(|x| x.contains("notes/"))
    });
    assert!(!tmp.path().join("apps").exists() && !tmp.path().join("notes").exists());
    assert!(tmp.path().join("zeta").exists());
    s.send("u");
    s.wait_for_text("apps/");
    assert!(
        tmp.path().join("apps/web/index.html").exists(),
        "the whole tree came back"
    );
}

#[test]
fn a_selection_made_with_space_survives_moving_around() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send(" ");
    s.wait("marker appears", |r| r.iter().any(|x| x.starts_with('●')));
    s.send("jj");
    s.wait("marker stays", |r| {
        r.iter().any(|x| x.starts_with('●') && x.contains("apps/"))
    });
    s.send("dd");
    s.wait("selected entry deleted", |r| {
        !r.iter().any(|x| x.contains("apps/"))
    });
    assert!(!tmp.path().join("apps").exists());
    assert!(tmp.path().join("zeta").exists(), "only the selection went");
}

#[test]
fn i_edits_a_file_in_place_and_colon_wq_saves_it() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("G");
    s.wait("on README", |r| center(r).contains("README.md"));
    s.send("l");
    let rows = s.wait_for_text(":w save");
    assert!(rows[0].contains("README.md"), "{:?}", rows[0]);
    s.send("A more words");
    s.wait_for_text("-- INSERT --");
    s.send(ESC);
    s.wait("dirty marker", |r| r[0].contains("[+]"));
    s.send("onew line");
    s.send(ESC);
    s.send(":wq\r");
    s.wait("back in the tree", |r| {
        r.last().unwrap().contains("j/k move")
    });
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("README.md")).unwrap(),
        "# readme more words\nnew line\n"
    );
    s.wait("preview shows the saved text", |r| {
        r.iter().any(|x| x.contains("2 new line"))
    });
}

#[test]
fn the_editor_refuses_to_quit_with_unsaved_changes_and_undo_works() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("Gl");
    s.wait_for_text(":w save");
    s.send("dd");
    s.wait("line deleted", |r| {
        !r.iter().any(|x| x.contains("# readme"))
    });
    s.send(":q\r");
    s.wait_for_text("unsaved changes");
    s.send("u");
    s.wait("line back", |r| r.iter().any(|x| x.contains("# readme")));
    s.send(":q!\r");
    s.wait("back in the tree", |r| {
        r.last().unwrap().contains("j/k move")
    });
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("README.md")).unwrap(),
        "# readme\n"
    );
}

#[test]
fn a_file_changed_by_another_program_is_not_overwritten() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("Gl");
    s.wait_for_text(":w save");
    s.send("x");
    s.wait("dirty marker", |r| r[0].contains("[+]"));
    std::fs::write(
        tmp.path().join("README.md"),
        "changed by someone else, longer\n",
    )
    .unwrap();
    s.send(":w\r");
    s.wait_for_text("changed on disk");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("README.md")).unwrap(),
        "changed by someone else, longer\n"
    );
    s.send(":e!\r");
    s.wait("reloaded text", |r| {
        r.iter().any(|x| x.contains("changed by someone else"))
    });
}

#[test]
fn visual_mode_in_the_editor_deletes_a_selection() {
    let tmp = fixture();
    std::fs::write(
        tmp.path().join("README.md"),
        "keep\ndrop one\ndrop two\nkeep too\n",
    )
    .unwrap();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("Gl");
    s.wait_for_text(":w save");
    s.send("jV");
    s.wait_for_text("-- VISUAL LINE --");
    s.send("jd");
    s.wait("lines gone", |r| !r.iter().any(|x| x.contains("drop")));
    s.send(":wq\r");
    s.wait("back in the tree", |r| {
        r.last().unwrap().contains("j/k move")
    });
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("README.md")).unwrap(),
        "keep\nkeep too\n"
    );
}

#[test]
fn a_picture_is_previewed_with_coloured_half_blocks() {
    let tmp = fixture();
    let img = image::RgbImage::from_fn(120, 60, |x, _| {
        if x < 60 {
            image::Rgb([230, 20, 20])
        } else {
            image::Rgb([20, 20, 230])
        }
    });
    img.save(tmp.path().join("a-photo.png")).unwrap();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.send("fa");
    let rows = s.wait_for_text("image/png");
    let brace = rows
        .iter()
        .find_map(|r| r.chars().position(|c| c == '┤'))
        .unwrap() as u16;
    let colours: Vec<_> = (0..common::ROWS)
        .flat_map(|row| (brace + 2..common::COLS).map(move |x| (row, x)))
        .map(|(row, x)| s.bg(row, x))
        .filter(|c| matches!(c, vt100::Color::Rgb(r, g, b) if (*r, *g, *b) != (0x0d, 0x11, 0x17)))
        .collect();
    assert!(colours.len() > 10, "the picture is drawn: {rows:#?}");
}

#[test]
fn no_color_draws_without_colours_and_keeps_the_cursor_visible() {
    let tmp = fixture();
    let s = Session::spawn_with(
        tmp.path(),
        Opts {
            env: vec![("NO_COLOR", "1".to_string())],
            ..Opts::default()
        },
    );
    let rows = s.wait_for_text("apps/");
    let row = Session::CENTER as u16;
    let col = rows[Session::CENTER].find("apps/").unwrap() as u16;
    assert_eq!(s.fg(row, col), vt100::Color::Default);
    assert!(s.inverse(row, col), "the cursor row is reverse video");
}

#[test]
fn sigterm_restores_the_terminal_and_exits() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    s.wait_for_text("apps/");
    s.signal(libc_sigterm());
    assert_eq!(s.wait_exit(), 0);
    assert!(s.left_alternate_screen(), "the shell screen is back");
}

fn libc_sigterm() -> i32 {
    15
}

fn click(s: &mut Session, col: u16, row: u16) {
    s.send(&format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}m",
        col + 1,
        row + 1,
        col + 1,
        row + 1
    ));
}

#[test]
fn clicking_and_scrolling_with_the_mouse() {
    let tmp = fixture();
    let mut s = Session::spawn(tmp.path());
    let rows = s.wait_for_text("apps/");
    let row = rows.iter().position(|r| r.contains("zeta/")).unwrap() as u16;
    let col = rows[row as usize].find("zeta/").unwrap() as u16;
    click(&mut s, col, row);
    s.wait("zeta selected", |r| center(r).contains("zeta/"));
    let row = Session::CENTER as u16;
    s.send(&format!("\x1b[<65;{};{}M", col + 1, row + 1));
    s.wait("wheel moved down", |r| center(r).contains("README.md"));
    click(&mut s, col, row);
    click(&mut s, col, row);
    s.wait_for_text(":w save");
}

#[test]
fn the_terminal_is_asked_what_it_can_draw_and_its_answer_is_used() {
    let tmp = fixture();
    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some("images = \"auto\"\n"),
            terminal_answers: Some(b"\x1bP>|foot(1.16.2)\x1b\\\x1b[6;18;9t\x1b[?62;4;22c"),
            env: vec![("TERM", "xterm-256color".to_string())],
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    s.send(":images\r");
    let rows = s.wait_for_text("pictures:");
    let footer = rows.last().unwrap();
    assert!(
        footer.contains("sixel") && footer.contains("foot(1.16.2)"),
        "{footer:?}"
    );
}

#[test]
fn a_terminal_that_never_answers_costs_only_the_timeout_and_no_keys() {
    let tmp = fixture();
    let started = std::time::Instant::now();
    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some("images = \"auto\"\n"),
            env: vec![("TERM", "xterm-256color".to_string())],
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    s.send("j");
    s.wait("the first key after start works", |r| {
        center(r).contains("notes/")
    });
    s.send(":images\r");
    let rows = s.wait_for_text("pictures:");
    assert!(
        rows.last().unwrap().contains("quadrant blocks"),
        "{:?}",
        rows.last()
    );
}

#[test]
fn over_ssh_a_silent_terminal_is_waited_for_longer_and_keys_still_work() {
    let tmp = fixture();
    let started = std::time::Instant::now();
    let mut s = Session::spawn_with(
        tmp.path(),
        Opts {
            config: Some("images = \"auto\"\n"),
            env: vec![
                ("TERM", "xterm-256color".to_string()),
                ("SSH_CONNECTION", "10.0.0.2 50000 10.0.0.1 22".to_string()),
            ],
            ..Opts::default()
        },
    );
    s.wait_for_text("apps/");
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(1900),
        "{waited:?}"
    );
    assert!(waited < std::time::Duration::from_secs(4), "{waited:?}");
    s.send("j");
    s.wait("the first key after start works", |r| {
        center(r).contains("notes/")
    });
}
