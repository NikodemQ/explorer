use std::{
    collections::BTreeMap,
    env,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

const DEFAULT_EDITORS: [&str; 2] = ["nvim", "nano"];

#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    /// Editor commands in order of preference; the first one found on `PATH` is used.
    /// An entry may carry arguments, like `"code --wait"`.
    pub editors: Vec<String>,
    /// `[keys]` entries, `"gg" = "first"`; an empty command unbinds the key.
    pub keys: BTreeMap<String, String>,
    /// Show dotfiles from the start.
    pub show_hidden: bool,
    /// How pictures are drawn: auto, kitty, sixel, iterm2, halfblocks or off.
    pub images: crate::imageview::Mode,
    /// Forced colour depth, or `None` to read it from the environment.
    pub colors: Option<crate::theme::Depth>,
    /// Clicks and the wheel move around. Off leaves the mouse to the terminal, for selecting text.
    pub mouse: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            editors: DEFAULT_EDITORS.map(String::from).to_vec(),
            keys: BTreeMap::new(),
            show_hidden: false,
            images: crate::imageview::Mode::Auto,
            colors: None,
            mouse: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    editor: Option<OneOrMany>,
    keys: Option<BTreeMap<String, String>>,
    show_hidden: Option<bool>,
    images: Option<String>,
    colors: Option<String>,
    mouse: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl Config {
    /// Reads `$XDG_CONFIG_HOME/tx/config.toml` (or `~/.config/tx/config.toml`).
    /// A missing file gives the defaults; a broken one gives the defaults and a message.
    pub fn load() -> (Config, Option<String>) {
        let Some(path) = config_path() else {
            return (Config::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Config::parse(&text) {
                Ok(config) => (config, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("{}: {}", path.display(), e.message())),
                ),
            },
            Err(_) => (Config::default(), None),
        }
    }

    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        let raw: Raw = toml::from_str(text)?;
        let editors = match raw.editor {
            None => Config::default().editors,
            Some(OneOrMany::One(one)) => vec![one],
            Some(OneOrMany::Many(many)) => many,
        };
        Ok(Config {
            editors,
            keys: raw.keys.unwrap_or_default(),
            show_hidden: raw.show_hidden.unwrap_or(false),
            mouse: raw.mouse.unwrap_or(true),
            colors: match raw.colors.as_deref() {
                None => None,
                Some(word) => crate::theme::Depth::parse(word).ok_or_else(|| {
                    serde::de::Error::custom("colors is one of auto, truecolor, 256, 16 or none")
                })?,
            },
            images: match raw.images.as_deref() {
                None => crate::imageview::Mode::Auto,
                Some(word) => crate::imageview::Mode::parse(word).ok_or_else(|| {
                    serde::de::Error::custom(
                        "images is one of auto, kitty, sixel, iterm2, halfblocks or off",
                    )
                })?,
            },
        })
    }

    /// The command line prefix of the first configured editor that `on_path` finds.
    pub fn resolve_editor(&self, on_path: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
        self.editors
            .iter()
            .map(|e| e.split_whitespace().map(String::from).collect::<Vec<_>>())
            .find(|argv| argv.first().is_some_and(|program| on_path(program)))
    }
}

fn config_path() -> Option<PathBuf> {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("tx").join("config.toml"))
}

/// Whether `program` names an executable, either as a path or somewhere on `PATH`.
pub fn on_path(program: &str) -> bool {
    let is_executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| is_executable(&dir.join(program))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(config: &Config, installed: &[&str]) -> Option<Vec<String>> {
        config.resolve_editor(&|p| installed.contains(&p))
    }

    #[test]
    fn default_prefers_neovim_then_nano() {
        let config = Config::default();
        assert_eq!(
            resolve(&config, &["nvim", "nano"]),
            Some(vec!["nvim".into()])
        );
        assert_eq!(resolve(&config, &["nano"]), Some(vec!["nano".into()]));
        assert_eq!(resolve(&config, &["vi"]), None);
    }

    #[test]
    fn editor_can_be_a_string_or_a_list_and_may_carry_arguments() {
        let one = Config::parse(r#"editor = "code --wait""#).unwrap();
        assert_eq!(
            resolve(&one, &["code"]),
            Some(vec!["code".into(), "--wait".into()])
        );
        let many = Config::parse(r#"editor = ["hx", "micro"]"#).unwrap();
        assert_eq!(
            resolve(&many, &["micro", "nvim"]),
            Some(vec!["micro".into()])
        );
    }

    #[test]
    fn keys_table_is_read() {
        let config = Config::parse("[keys]\n\"gg\" = \"first\"\nq = \"none\"").unwrap();
        assert_eq!(config.keys["gg"], "first");
        assert_eq!(config.keys["q"], "none");
        assert_eq!(config.editors, Config::default().editors);
    }

    #[test]
    fn images_names_a_drawing_method() {
        use crate::imageview::Mode;
        assert_eq!(Config::parse("").unwrap().images, Mode::Auto);
        assert_eq!(Config::parse("images = 'off'").unwrap().images, Mode::Off);
        assert_eq!(
            Config::parse("images = 'sixel'").unwrap().images,
            Mode::Sixel
        );
        assert!(Config::parse("images = 'svga'").is_err());
    }

    #[test]
    fn colors_and_mouse_are_read() {
        use crate::theme::Depth;
        let defaults = Config::parse("").unwrap();
        assert_eq!((defaults.colors, defaults.mouse), (None, true));
        let set = Config::parse("colors = '256'\nmouse = false").unwrap();
        assert_eq!((set.colors, set.mouse), (Some(Depth::Ansi256), false));
        assert_eq!(Config::parse("colors = 'auto'").unwrap().colors, None);
        assert!(Config::parse("colors = 'many'").is_err());
    }

    #[test]
    fn show_hidden_is_read() {
        assert!(Config::parse("show_hidden = true").unwrap().show_hidden);
        assert!(!Config::parse("").unwrap().show_hidden);
        assert!(Config::parse("show_hidden = 'yes'").is_err());
    }

    #[test]
    fn an_empty_file_keeps_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_reported() {
        assert!(Config::parse("editr = 'vim'").is_err());
        assert!(Config::parse("editor = 3").is_err());
    }

    #[test]
    fn on_path_finds_real_executables_only() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-program-tx"));
        assert!(on_path("/bin/sh"));
        assert!(!on_path("/etc/passwd"));
    }
}
