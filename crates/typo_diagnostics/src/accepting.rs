use std::borrow::Cow;
use std::path::Path;

use collections::HashMap;
use serde::Deserialize;
use typos::Status;

/// The files `typos` reads a project's own configuration from, in the order
/// it tries them within one directory. The first that yields a configuration
/// is the one that counts.
pub const CONFIGURATION_FILE_NAMES: [&str; 5] = [
    "typos.toml",
    "_typos.toml",
    ".typos.toml",
    CARGO_MANIFEST,
    PYPROJECT,
];

const CARGO_MANIFEST: &str = "Cargo.toml";
const PYPROJECT: &str = "pyproject.toml";

/// What a project has said about words in its own configuration: which ones
/// are fine as they are, and which ones it corrects differently from the
/// dictionary.
///
/// Words are matched without regard to case, the way `typos` matches them
/// against its own dictionary. Identifiers are matched exactly, because an
/// identifier is a name and its capitals are part of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accepted {
    identifiers: HashMap<String, Judgement>,
    words: HashMap<String, Judgement>,
}

impl Accepted {
    pub(crate) fn said_about_identifier(&self, token: &str) -> Option<Status<'_>> {
        self.identifiers.get(token).map(Judgement::status)
    }

    pub(crate) fn said_about_word(&self, token: &str) -> Option<Status<'_>> {
        self.words.get(&token.to_lowercase()).map(Judgement::status)
    }
}

/// What one entry of `extend-words` or `extend-identifiers` amounts to, read
/// the way `typos` reads it: a word mapped to itself is one the project
/// accepts, a word mapped to nothing is wrong with nothing to put in its
/// place, and anything else is the project's own correction.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Judgement {
    Fine,
    Wrong,
    Instead(String),
}

impl Judgement {
    fn of(written: &str, correction: String) -> Self {
        if written == correction {
            Self::Fine
        } else if correction.is_empty() {
            Self::Wrong
        } else {
            Self::Instead(correction)
        }
    }

    fn status(&self) -> Status<'_> {
        match self {
            Self::Fine => Status::Valid,
            Self::Wrong => Status::Invalid,
            Self::Instead(correction) => Status::Corrections(vec![Cow::Borrowed(correction)]),
        }
    }
}

/// What the project covering this directory has said about words.
///
/// Found the way `typos` finds it: each directory from this one up to the
/// filesystem root is tried in turn, and within a directory the names in
/// [`CONFIGURATION_FILE_NAMES`] in that order. The first configuration found
/// is the whole answer -- the search stops there rather than merging what
/// directories further up say, which is `typos`' own rule.
///
/// A `Cargo.toml` or `pyproject.toml` with no `typos` section in it is not a
/// configuration and does not stop the search; one of the three dedicated
/// names is, even when it holds nothing this reads.
///
/// A file that will not parse is passed over rather than failing the check.
/// The alternative is reporting nothing about a project whose configuration
/// has a comma out of place, which is the moment the reader is least helped
/// by silence.
pub fn accepted_near(directory: &Path, read: impl Fn(&Path) -> Option<String>) -> Accepted {
    for ancestor in directory.ancestors() {
        for name in CONFIGURATION_FILE_NAMES {
            let Some(text) = read(&ancestor.join(name)) else {
                continue;
            };
            let Some(declared) = declared_in(name, &text) else {
                continue;
            };
            return declared.into_accepted();
        }
    }
    Accepted::default()
}

fn declared_in(name: &str, text: &str) -> Option<Configuration> {
    if name == CARGO_MANIFEST {
        let manifest = toml::from_str::<CargoManifest>(text).ok()?;
        manifest
            .workspace
            .and_then(|workspace| workspace.metadata.typos)
            .or_else(|| manifest.package.and_then(|package| package.metadata.typos))
    } else if name == PYPROJECT {
        toml::from_str::<Pyproject>(text).ok()?.tool.typos
    } else {
        toml::from_str::<Configuration>(text).ok()
    }
}

/// The part of a `typos` configuration this reads. Everything else it holds
/// -- which files to walk, per-file-type rules, the patterns to ignore -- is
/// left alone rather than rejected: an unknown key is a key some other
/// version of `typos` understands, and refusing the whole file over one
/// would throw away the words the project did declare.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Configuration {
    default: Engine,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Engine {
    extend_identifiers: HashMap<String, String>,
    extend_words: HashMap<String, String>,
}

impl Configuration {
    fn into_accepted(self) -> Accepted {
        Accepted {
            identifiers: self
                .default
                .extend_identifiers
                .into_iter()
                .map(|(written, correction)| {
                    let judged = Judgement::of(&written, correction);
                    (written, judged)
                })
                .collect(),
            words: self
                .default
                .extend_words
                .into_iter()
                .map(|(written, correction)| {
                    let judged = Judgement::of(&written, correction);
                    (written.to_lowercase(), judged)
                })
                .collect(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CargoManifest {
    workspace: Option<CargoSection>,
    package: Option<CargoSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CargoSection {
    metadata: CargoMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CargoMetadata {
    typos: Option<Configuration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Pyproject {
    tool: PyprojectTool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PyprojectTool {
    typos: Option<Configuration>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn only(files: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> {
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
            .collect();
        move |wanted: &Path| {
            files
                .iter()
                .find(|(path, _)| Path::new(path) == wanted)
                .map(|(_, text)| text.clone())
        }
    }

    /// A configuration above the file being checked is found: a project
    /// declares its words once, at its root, and every file under it is
    /// covered.
    #[test]
    fn a_configuration_further_up_the_tree_is_found() {
        let accepted = accepted_near(
            Path::new("/project/crates/thing/src"),
            only(&[(
                "/project/_typos.toml",
                "[default.extend-words]\nfunctoin = \"functoin\"\n",
            )]),
        );
        assert_eq!(
            accepted.said_about_word("Functoin"),
            Some(Status::Valid),
            "matched without regard to case, the way the dictionary is"
        );
    }

    /// The nearer configuration is the whole answer: `typos` stops at the
    /// first directory that has one rather than merging what the ones above
    /// it say.
    #[test]
    fn the_nearest_configuration_is_the_only_one_read() {
        let accepted = accepted_near(
            Path::new("/project/crates/thing"),
            only(&[
                (
                    "/project/crates/thing/_typos.toml",
                    "[default.extend-words]\nlenght = \"lenght\"\n",
                ),
                (
                    "/project/_typos.toml",
                    "[default.extend-words]\nfunctoin = \"functoin\"\n",
                ),
            ]),
        );
        assert_eq!(accepted.said_about_word("lenght"), Some(Status::Valid));
        assert_eq!(accepted.said_about_word("functoin"), None);
    }

    /// A `Cargo.toml` with no `typos` section in it is not a configuration,
    /// so the search carries on past it. Nearly every Rust project has one,
    /// and stopping there would leave every declared word unread.
    #[test]
    fn a_cargo_manifest_with_no_typos_section_does_not_stop_the_search() {
        let accepted = accepted_near(
            Path::new("/project/crates/thing"),
            only(&[
                (
                    "/project/crates/thing/Cargo.toml",
                    "[package]\nname = \"thing\"\n",
                ),
                (
                    "/project/_typos.toml",
                    "[default.extend-words]\nfunctoin = \"functoin\"\n",
                ),
            ]),
        );
        assert_eq!(accepted.said_about_word("functoin"), Some(Status::Valid));
    }

    /// A project that declares its words in `Cargo.toml` is read, under
    /// either the workspace's metadata or the package's.
    #[test]
    fn a_cargo_manifest_declares_words_under_workspace_or_package_metadata() {
        for section in ["workspace", "package"] {
            let manifest = format!(
                "[{section}.metadata.typos.default.extend-words]\nfunctoin = \"functoin\"\n"
            );
            let accepted = accepted_near(
                Path::new("/project"),
                only(&[("/project/Cargo.toml", manifest.as_str())]),
            );
            assert_eq!(
                accepted.said_about_word("functoin"),
                Some(Status::Valid),
                "under {section}"
            );
        }
    }

    /// A Python project declares its words under `[tool.typos]`, which is
    /// where every tool that reads `pyproject.toml` looks.
    #[test]
    fn a_pyproject_declares_words_under_the_tool_section() {
        let accepted = accepted_near(
            Path::new("/project"),
            only(&[(
                "/project/pyproject.toml",
                "[tool.typos.default.extend-words]\nfunctoin = \"functoin\"\n",
            )]),
        );
        assert_eq!(accepted.said_about_word("functoin"), Some(Status::Valid));
    }

    /// A configuration holding nothing this reads still counts as one, and
    /// the keys it holds that this does not model do not stop it parsing.
    /// They belong to the rest of `typos`, and rejecting the file over them
    /// would throw away the words alongside.
    #[test]
    fn keys_this_does_not_read_are_left_alone_rather_than_rejected() {
        let accepted = accepted_near(
            Path::new("/project"),
            only(&[(
                "/project/_typos.toml",
                "[files]\nextend-exclude = [\"vendor/\"]\n\n[default]\nlocale = \"en-gb\"\ncheck-filename = false\n\n[default.extend-words]\nfunctoin = \"functoin\"\n\n[type.rust]\nextend-glob = [\"*.rs\"]\n",
            )]),
        );
        assert_eq!(accepted.said_about_word("functoin"), Some(Status::Valid));
    }

    /// A project may correct a word its own way rather than accept it, and
    /// that correction is what the reader is shown.
    #[test]
    fn a_project_may_give_its_own_correction_rather_than_accept_the_word() {
        let accepted = accepted_near(
            Path::new("/project"),
            only(&[(
                "/project/_typos.toml",
                "[default.extend-identifiers]\nSerivce = \"Service\"\n",
            )]),
        );
        assert_eq!(
            accepted.said_about_identifier("Serivce"),
            Some(Status::Corrections(vec![Cow::Borrowed("Service")]))
        );
        assert_eq!(
            accepted.said_about_identifier("serivce"),
            None,
            "an identifier is a name, and its capitals are part of it"
        );
    }

    /// A project with no configuration anywhere accepts nothing in
    /// particular, which is the ordinary case and not a fault.
    #[test]
    fn a_project_with_no_configuration_accepts_nothing_in_particular() {
        let accepted = accepted_near(Path::new("/project/src"), |_| None);
        assert_eq!(accepted, Accepted::default());
        assert_eq!(accepted.said_about_word("functoin"), None);
    }

    /// A configuration that will not parse is passed over. Reporting nothing
    /// about a project whose configuration has a comma out of place is the
    /// moment silence helps the reader least.
    #[test]
    fn a_configuration_that_will_not_parse_is_passed_over() {
        let accepted = accepted_near(
            Path::new("/project/crates/thing"),
            only(&[
                (
                    "/project/crates/thing/_typos.toml",
                    "[default.extend-words\n",
                ),
                (
                    "/project/_typos.toml",
                    "[default.extend-words]\nfunctoin = \"functoin\"\n",
                ),
            ]),
        );
        assert_eq!(accepted.said_about_word("functoin"), Some(Status::Valid));
    }
}
