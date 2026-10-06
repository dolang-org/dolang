#![deny(warnings)]

pub mod alias;
pub mod hashbrown;
pub mod intern;
pub mod mono;
pub mod pairsort;
pub mod pin;
pub mod ring;
pub mod verified;

/// The debug topics a `DOLANG_DEBUG` value enables.
///
/// The value is a comma-separated list of dotted topics, such as `typeck.flow,gc`.
/// Each enables itself and the topics below it: `typeck` enables `typeck.flow`.
/// An empty value, `1` or `all` enables every topic.
#[doc(hidden)]
#[derive(Debug, PartialEq, Eq)]
pub enum Topics {
    All,
    Some(Vec<String>),
}

impl Topics {
    pub fn parse(value: &str) -> Self {
        let mut topics = Vec::new();
        for topic in value.split(',').map(str::trim) {
            match topic {
                "" => {}
                "1" | "all" => return Self::All,
                _ => topics.push(topic.to_owned()),
            }
        }
        match topics.is_empty() {
            true => Self::All,
            false => Self::Some(topics),
        }
    }

    pub fn enables(&self, topic: &str) -> bool {
        match self {
            Self::All => true,
            Self::Some(topics) => topics.iter().any(|enabled| {
                topic
                    .strip_prefix(enabled.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            }),
        }
    }
}

/// Returns whether debug diagnostics on `topic` are enabled.
///
/// Debug diagnostics require both the crate's `debug` feature and a `DOLANG_DEBUG`
/// environment variable that enables the topic (see [`Topics`]).
#[cfg(feature = "debug")]
#[doc(hidden)]
pub fn debug_enabled(topic: &str) -> bool {
    use std::sync::OnceLock;

    static TOPICS: OnceLock<Option<Topics>> = OnceLock::new();
    TOPICS
        .get_or_init(|| {
            let value = std::env::var_os("DOLANG_DEBUG")?;
            Some(Topics::parse(&value.to_string_lossy()))
        })
        .as_ref()
        .is_some_and(|topics| topics.enables(topic))
}

/// Whether debug diagnostics on a topic are enabled, for skipping work that only
/// they need.
#[cfg(feature = "debug")]
#[macro_export]
macro_rules! debug_enabled {
    ($topic:expr) => {
        $crate::debug_enabled($topic)
    };
}

/// Debug diagnostics are never enabled without the `debug` feature.
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! debug_enabled {
    ($topic:expr) => {
        false
    };
}

/// Writes a message on a topic to standard error when debug diagnostics on it are
/// enabled.
#[cfg(feature = "debug")]
#[macro_export]
macro_rules! debug_eprintln {
    (topic: $topic:expr, $($arg:tt)*) => {
        if $crate::debug_enabled($topic) {
            ::std::eprintln!($($arg)*);
        }
    };
}

/// Discards debug diagnostics when the `debug` feature is disabled.
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! debug_eprintln {
    (topic: $topic:expr, $($arg:tt)*) => {};
}

#[cfg(test)]
mod tests {
    use super::Topics;

    #[test]
    fn topics() {
        for value in ["", "1", "all", " , ", "gc,all"] {
            assert_eq!(Topics::parse(value), Topics::All, "{value:?}");
        }
        let topics = Topics::parse("typeck, gc.sweep");
        assert!(topics.enables("typeck"));
        assert!(topics.enables("typeck.flow"));
        assert!(topics.enables("gc.sweep"));
        assert!(!topics.enables("gc"));
        assert!(!topics.enables("typeckx"));
        assert!(!topics.enables("emit"));
    }
}
