//! # Parser module
//!
//! The parser module will parse the git commit history to build changelog

use std::{collections::HashMap, convert::TryFrom, error::Error, rc::Rc};

use askama::Template;
use chrono::{DateTime, NaiveDateTime, Utc};
use git2 as git;
use regex::Regex;
use slog_scope::{error, info, warn};
use strfmt::strfmt;

use crate::conf::{self, Configuration};

// https://regex101.com/r/X9RoUY/4
const PATTERN: &str =
    r"(?P<kind>[\w \-\./\\]+)(\((?P<scope>[\w \-\./\\]+)\))?: (?P<message>[\w \-\./\\]+)";

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub date: String,
    pub link: Option<String>,
}

impl TryFrom<(&conf::Repository, &git::Commit<'_>)> for Commit {
    type Error = Box<dyn Error + Send + Sync>;

    fn try_from(tuple: (&conf::Repository, &git::Commit<'_>)) -> Result<Self, Self::Error> {
        let (conf, commit) = tuple;
        let author = match commit.author().name() {
            Some(author) => String::from(author),
            None => match commit.committer().name() {
                Some(committer) => String::from(committer),
                None => return Err("No such author or commiter".into()),
            },
        };

        let message = match commit.summary() {
            Some(summary) => String::from(summary),
            None => match commit.message() {
                Some(message) => String::from(message),
                None => return Err("No such message or summary".into()),
            },
        };

        let mut hash = commit.id().to_string();
        let date = DateTime::<Utc>::from_utc(
            NaiveDateTime::from_timestamp(commit.time().seconds(), 0),
            Utc,
        )
        .date()
        .format("%F")
        .to_string();

        let mut link = None;
        if let Some(ref layout) = conf.link {
            let mut vars = HashMap::new();

            vars.insert(String::from("hash"), hash.to_owned());

            link = Some(
                strfmt(layout, &vars)
                    .map_err(|err| format!("could not format commit link, {}", err))?,
            );
        }

        hash.truncate(7);

        Ok(Self {
            hash,
            message,
            author,
            date,
            link,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Tag {
    pub name: String,
    pub commits: HashMap<String, Vec<Commit>>,
}

impl From<(String, HashMap<String, Vec<Commit>>)> for Tag {
    fn from(tuple: (String, HashMap<String, Vec<Commit>>)) -> Self {
        let (name, commits) = tuple;

        Self { name, commits }
    }
}

#[derive(Clone, Debug)]
pub struct Repository {
    pub name: String,
    pub tags: Vec<Tag>,
}

impl From<String> for Repository {
    fn from(name: String) -> Self {
        Repository {
            name,
            tags: Default::default(),
        }
    }
}

impl TryFrom<(&HashMap<String, String>, &conf::Repository)> for Repository {
    type Error = Box<dyn Error + Send + Sync>;

    fn try_from(tuple: (&HashMap<String, String>, &conf::Repository)) -> Result<Self, Self::Error> {
        let (kinds, conf) = tuple;
        let mut repository = Repository::from(conf.name.to_owned());
        let repo = git::Repository::discover(&conf.path).map_err(|err| {
            format!(
                "could not retrieve git repository at '{:?}', {}",
                conf.path, err
            )
        })?;

        // We should build a map(commit-id -> tag) before starting walking over the git commit history.
        //
        // The full explanation is here:
        // https://stackoverflow.com/questions/36528576/get-annotated-tags-from-revwalk-commit/36555358#36555358
        let mut tags = HashMap::new();
        for tag in repo
            .tag_names(None)
            .map_err(|err| format!("could not retrieve git tags, {}", err))?
            .iter()
        {
            let object = repo
                .revparse_single(tag.expect("tag to be written in utf-8 compliant format"))
                .map_err(|err| format!("could not retrieve object for tag, {}", err))?;

            let tag = match object.to_owned().into_tag() {
                Ok(tag) => tag,
                Err(_) => {
                    let mut hash = object.id().to_string();
                    hash.truncate(7);
                    warn!("could not cast object into tag"; "hash" => hash);
                    continue;
                }
            };

            tags.insert(tag.target_id().to_string(), tag);
        }

        let mut revwalk = repo
            .revwalk()
            .map_err(|err| format!("could create a walker on git history, {}", err))?;

        match &conf.range {
            Some(range) => {
                revwalk
                    .push_range(range)
                    .map_err(|err| format!("could not parse commit range, {}", err))?;
            }
            None => {
                revwalk
                    .push_head()
                    .map_err(|err| format!("could not push HEAD commit, {}", err))?;
            }
        }

        revwalk
            .set_sorting(git::Sort::TIME | git::Sort::REVERSE)
            .map_err(|err| format!("failed to sort git commit history, {}", err))?;

        let mut commits = HashMap::new();
        for oid in revwalk {
            let oid =
                oid.map_err(|err| format!("could not retrieve object identifier, {}", err))?;

            let commit = repo
                .find_commit(oid)
                .map_err(|err| format!("could not retrieve commit '{}', {}", oid, err))?;

            let commit = Commit::try_from((conf, &commit))
                .map_err(|err| format!("could not parse commit '{}', {}", oid, err))?;

            let Commit { hash, message, .. } = commit.to_owned();
            if message.starts_with("Merge pull request") || message.starts_with("Merge branch") {
                info!("Skip merge commit"; "hash" => &hash);
                continue;
            }

            let re = Regex::new(PATTERN).expect("pattern to be a valid regular expression");
            if !re.is_match(&message) {
                error!("Could not parse the message"; "hash" => hash, "message" => message);
                continue;
            }

            let captures = re
                .captures(&message)
                .expect("captures to exists in PATTERN regex");
            let kind = String::from(
                captures
                    .name("kind")
                    .expect("To have 'kind' group in the PATTERN regex")
                    .as_str(),
            );

            let scope = captures
                .name("scope")
                .map(|scope| String::from(scope.as_str()));

            if !kinds.contains_key(&kind) {
                warn!("Kind is not contained in provided kinds"; "hash" => &hash, "kind" => kind);
                warn!("Skip commit"; "hash" => &hash);
                continue;
            }

            if let Some(ref scope) = scope {
                let sub_scopes = scope.as_str().split(',');
                if let Some(ref scopes) = conf.scopes {
                    for sub_scope in sub_scopes {
                        if !scopes.contains(&String::from(sub_scope)) {
                            warn!("Scope is not contained in provided scopes";  "hash" => &hash, "scope" => scope);
                            continue;
                        }
                    }
                }
            }

            (&mut commits)
                .entry(String::from(
                    kinds
                        .get(&kind)
                        .expect("To have 'kind' defined in repository's kinds")
                        .as_str(),
                ))
                .or_insert_with(Vec::new)
                .push(commit);

            if let Some(tag) = tags.get(&oid.to_string()) {
                repository.tags.push(Tag::from((
                    String::from(tag.name().expect("tag name to be utf-8 compliant")),
                    commits,
                )));

                commits = HashMap::new();
            }
        }

        if !commits.is_empty() {
            repository
                .tags
                .push(Tag::from((String::from("Technical preview"), commits)));
        }

        repository.tags.reverse();

        Ok(repository)
    }
}

#[derive(Default, Clone, Debug)]
pub struct Changelog {
    pub repositories: Vec<Repository>,
}

impl TryFrom<Rc<Configuration>> for Changelog {
    type Error = Box<dyn Error + Send + Sync>;

    fn try_from(conf: Rc<Configuration>) -> Result<Self, Self::Error> {
        let mut changelog = Changelog::default();

        for repository in &conf.repositories {
            changelog
                .repositories
                .push(
                    Repository::try_from((&conf.kinds, repository)).map_err(|err| {
                        format!(
                            "could not process repository '{}', {}",
                            repository.name, err
                        )
                    })?,
                );
        }

        Ok(changelog)
    }
}

#[derive(Template, Default, Clone, Debug)]
#[template(path = "changelog.html")]
pub struct HTMLChangelog {
    pub repositories: Vec<Repository>,
}

impl From<Changelog> for HTMLChangelog {
    fn from(changelog: Changelog) -> Self {
        Self {
            repositories: changelog.repositories,
        }
    }
}

#[derive(Template, Default, Clone, Debug)]
#[template(path = "changelog.md", escape = "none")]
pub struct MarkdownChangelog {
    pub repositories: Vec<Repository>,
}

impl From<Changelog> for MarkdownChangelog {
    fn from(changelog: Changelog) -> Self {
        Self {
            repositories: changelog.repositories,
        }
    }
}
