use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Durable,
    Volatile,
}

impl Tier {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "durable" => Ok(Self::Durable),
            "volatile" | "async" => Ok(Self::Volatile),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown policy tier {value:?}"),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    prefix: PathBuf,
    tier: Tier,
}

#[derive(Debug, Clone)]
pub struct Policy {
    default: Tier,
    rules: Vec<Rule>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            default: Tier::Durable,
            rules: Vec::new(),
        }
    }
}

impl Policy {
    pub fn load(path: Option<&Path>) -> io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(input: &str) -> io::Result<Self> {
        let mut policy = Self::default();
        for (index, raw_line) in input.lines().enumerate() {
            let line = raw_line
                .split_once('#')
                .map_or(raw_line, |(head, _)| head)
                .trim();
            if line.is_empty() {
                continue;
            }
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("policy line {}: expected two fields", index + 1),
                ));
            }
            if fields[0] == "default" {
                policy.default = Tier::parse(fields[1])?;
                continue;
            }
            let tier = Tier::parse(fields[0])?;
            let prefix = PathBuf::from(fields[1]);
            validate_prefix(&prefix).map_err(|error| {
                io::Error::new(error.kind(), format!("policy line {}: {error}", index + 1))
            })?;
            policy.rules.push(Rule { prefix, tier });
        }
        Ok(policy)
    }

    #[must_use]
    pub fn tier_for(&self, path: &Path) -> Tier {
        self.rules
            .iter()
            .filter(|rule| path.starts_with(&rule.prefix))
            .max_by_key(|rule| rule.prefix.components().count())
            .map_or(self.default, |rule| rule.tier)
    }
}

fn validate_prefix(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "policy prefix must be a normalized absolute path",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::path::Path;

    use super::{Policy, Tier};

    #[test]
    fn longest_prefix_wins_on_component_boundaries() {
        let policy =
            Policy::parse("default durable\nvolatile /srv/scratch\ndurable /srv/scratch/keep\n")
                .unwrap();
        assert_eq!(policy.tier_for(Path::new("/srv/scratch/a")), Tier::Volatile);
        assert_eq!(
            policy.tier_for(Path::new("/srv/scratch/keep/a")),
            Tier::Durable
        );
        assert_eq!(policy.tier_for(Path::new("/srv/scratchy/a")), Tier::Durable);
    }

    #[test]
    fn async_is_a_volatile_alias() {
        let policy = Policy::parse("async /tmp/cache\n").unwrap();
        assert_eq!(policy.tier_for(Path::new("/tmp/cache/a")), Tier::Volatile);
    }
}
