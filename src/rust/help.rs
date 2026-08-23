//! Small, shared help router for qtools binaries. Clap remains responsible for parsing commands;
//! this layer intercepts help requests so the landing page can stay short and topic pages focused.

pub struct Topic {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub text: &'static str,
}

pub enum Request {
    None,
    Show(&'static str),
    Unknown(String),
}

/// Recognize `tool help TOPIC`, ordinary `--help`, and contextual forms such as
/// `tool --net --help`. Context entries are checked in order, so callers put the most specific
/// modes first. The first argv item is the binary name.
pub fn route(
    args: &[String],
    landing: &'static str,
    topics: &'static [Topic],
    contexts: &'static [(&'static [&'static str], &'static str)],
) -> Request {
    let words = args.get(1..).unwrap_or_default();
    if words.first().is_some_and(|word| word == "help") {
        return match words.get(1) {
            None => Request::Show(landing),
            Some(name) => find_topic(name, topics)
                .map(|topic| Request::Show(topic.text))
                .unwrap_or_else(|| Request::Unknown(name.clone())),
        };
    }

    if !words.iter().any(|word| word == "--help" || word == "-h") {
        return Request::None;
    }
    for (flags, topic_name) in contexts {
        if words.iter().any(|word| flags.contains(&word.as_str())) {
            if let Some(topic) = find_topic(topic_name, topics) {
                return Request::Show(topic.text);
            }
        }
    }
    Request::Show(landing)
}

pub fn topic_names(topics: &[Topic]) -> String {
    topics.iter().map(|topic| topic.name).collect::<Vec<_>>().join(", ")
}

fn find_topic<'a>(name: &str, topics: &'a [Topic]) -> Option<&'a Topic> {
    topics.iter().find(|topic| topic.name == name || topic.aliases.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    static TOPICS: &[Topic] = &[
        Topic { name: "net", aliases: &["network"], text: "NET" },
        Topic { name: "output", aliases: &["json"], text: "OUTPUT" },
    ];
    static CONTEXTS: &[(&[&str], &str)] = &[
        (&["--net", "--port"], "net"),
        (&["--json"], "output"),
    ];

    fn args(words: &[&str]) -> Vec<String> { words.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn routes_topics_aliases_and_context_flags() {
        assert!(matches!(route(&args(&["pq", "help"]), "HOME", TOPICS, CONTEXTS), Request::Show("HOME")));
        assert!(matches!(route(&args(&["pq", "help", "network"]), "HOME", TOPICS, CONTEXTS), Request::Show("NET")));
        assert!(matches!(route(&args(&["pq", "--port", "80", "--help"]), "HOME", TOPICS, CONTEXTS), Request::Show("NET")));
        assert!(matches!(route(&args(&["pq", "--help"]), "HOME", TOPICS, CONTEXTS), Request::Show("HOME")));
    }

    #[test]
    fn leaves_normal_commands_alone_and_reports_unknown_topics() {
        assert!(matches!(route(&args(&["pq", "--net"]), "HOME", TOPICS, CONTEXTS), Request::None));
        assert!(matches!(route(&args(&["pq", "help", "wat"]), "HOME", TOPICS, CONTEXTS), Request::Unknown(_)));
    }
}
