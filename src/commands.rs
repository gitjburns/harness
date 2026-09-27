//! Slash commands: one table used both to run them and to list them as completions,
//! so the two can't drift apart.

#[derive(Clone, Copy)]
pub enum Action {
    Exit,
    ToggleToolReasoning,
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// One line, shown in the completion list.
    pub description: &'static str,
    pub action: Action,
}

/// In completion-list order. `/exit` is last, so `/` then Enter can't quit by accident.
const COMMANDS: &[Command] = &[
    Command {
        name: "/tool-reasoning",
        aliases: &[],
        description: "Turn tool reasoning on or off",
        action: Action::ToggleToolReasoning,
    },
    Command {
        name: "/exit",
        aliases: &["/quit"],
        description: "Exit",
        action: Action::Exit,
    },
];

/// The command `input` names exactly, by name or alias.
pub fn find(input: &str) -> Option<Action> {
    COMMANDS
        .iter()
        .find(|command| command.name == input || command.aliases.contains(&input))
        .map(|command| command.action)
}

/// A completion: the spelling shown and inserted, and its command.
pub struct Completion {
    pub spelling: &'static str,
    pub command: &'static Command,
}

/// Commands whose name or an alias starts with `input`, each once: by its name if
/// that matches, otherwise by the first matching alias (so `/q` offers `/quit`).
pub fn completions(input: &str) -> Vec<Completion> {
    COMMANDS
        .iter()
        .filter_map(|command| {
            let spelling = std::iter::once(&command.name)
                .chain(command.aliases)
                .find(|spelling| spelling.starts_with(input))?;
            Some(Completion { spelling, command })
        })
        .collect()
}
