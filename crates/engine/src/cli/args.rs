//! The arguments of one command, parsed by hand.
//!
//! The Python command line used `argparse`. What its commands and the people typing them relied
//! on is kept:
//!
//! * options may stand before, between and after the other arguments;
//! * `--name value` and `--name=value` are the same;
//! * an option may be shortened as long as it names one option only (`--lim 5`);
//! * `--` ends the options: everything after it is an argument;
//! * a negative number is a value, never an option (`--preamp -9`), and so is a word with a
//!   space in it that names no option (`search "-x y"`);
//! * `-h` and `--help` print the command's help and end it with status 0;
//! * a usage error prints the usage line and `amui <command>: error: ...` to standard error and
//!   ends the command with status 2. The error texts are `argparse`'s.
//!
//! Differences from `argparse`:
//!
//! * The help text has one layout and is not wrapped to the width of the terminal.
//! * A whole number is an optional sign and decimal digits that fit 64 bits; Python also took
//!   `1_000` and numbers of any size. A number is what Rust reads as one (`-9`, `1.5`, `1e3`,
//!   `inf`); Python also took `1_0`. White space around either is ignored, as in Python.
//! * The arguments of a command may be spread between its options (`search a --limit 5 b`);
//!   `argparse` wanted them in one run.
//! * The help is `-h` or `--help` (or what begins it) as a word of its own. `-hx` and `--help=1`
//!   are unrecognized arguments; `argparse` printed the help for the first and had an error of
//!   its own for the second.

use crate::util::python_repr;

use super::PROGRAM;

/// What an option takes after its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takes {
    /// Nothing: the option is a switch.
    Nothing,
    /// One text; the usage line shows it under this name.
    Text(&'static str),
    /// One whole number.
    Int(&'static str),
    /// One number.
    Float(&'static str),
}

impl Takes {
    fn label(&self) -> Option<&'static str> {
        match self {
            Takes::Nothing => None,
            Takes::Text(label) | Takes::Int(label) | Takes::Float(label) => Some(label),
        }
    }
}

/// One option of a command.
#[derive(Debug, Clone, Copy)]
pub struct Opt {
    /// With its dashes: `--limit`.
    pub name: &'static str,
    pub takes: Takes,
    /// One line for the help text; may be empty.
    pub help: &'static str,
}

impl Opt {
    pub const fn switch(name: &'static str, help: &'static str) -> Opt {
        Opt { name, takes: Takes::Nothing, help }
    }

    pub const fn text(name: &'static str, label: &'static str, help: &'static str) -> Opt {
        Opt { name, takes: Takes::Text(label), help }
    }

    pub const fn int(name: &'static str, label: &'static str, help: &'static str) -> Opt {
        Opt { name, takes: Takes::Int(label), help }
    }

    pub const fn float(name: &'static str, label: &'static str, help: &'static str) -> Opt {
        Opt { name, takes: Takes::Float(label), help }
    }
}

/// How many words an argument takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// Exactly one.
    One,
    /// One or none (`argparse`: `nargs="?"`).
    Optional,
    /// One or more (`argparse`: `nargs="+"`).
    OneOrMore,
}

impl Count {
    fn least(&self) -> usize {
        match self {
            Count::Optional => 0,
            Count::One | Count::OneOrMore => 1,
        }
    }
}

/// One argument of a command that is not an option.
#[derive(Debug, Clone, Copy)]
pub struct Arg {
    pub name: &'static str,
    pub count: Count,
    /// One line for the help text; may be empty.
    pub help: &'static str,
}

impl Arg {
    pub const fn one(name: &'static str, help: &'static str) -> Arg {
        Arg { name, count: Count::One, help }
    }

    pub const fn optional(name: &'static str, help: &'static str) -> Arg {
        Arg { name, count: Count::Optional, help }
    }

    pub const fn one_or_more(name: &'static str, help: &'static str) -> Arg {
        Arg { name, count: Count::OneOrMore, help }
    }
}

/// What a command takes on its command line.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// The command's name; usage and error lines say `amui <name>`.
    pub name: &'static str,
    /// What the command does, for its help text.
    pub description: &'static str,
    pub options: &'static [Opt],
    pub arguments: &'static [Arg],
    /// Options of which only one may be given, group by group.
    pub exclusive: &'static [&'static [&'static str]],
}

/// Why a command line ends before its command has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `-h` or `--help`: the help text, for standard output. Exit status 0.
    Help(String),
    /// A usage error: the usage line and the error line, for standard error. Exit status 2.
    Usage(String),
}

impl Stop {
    pub fn status(&self) -> i32 {
        match self {
            Stop::Help(_) => 0,
            Stop::Usage(_) => 2,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Stop::Help(text) | Stop::Usage(text) => text,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Given {
    Switch,
    Text(String),
    Int(i64),
    Float(f64),
}

/// The options and arguments a command line gave.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    options: Vec<(&'static str, Given)>,
    arguments: Vec<(&'static str, Vec<String>)>,
}

impl Parsed {
    /// What the option was given last, as `argparse` keeps it.
    fn given(&self, name: &str) -> Option<&Given> {
        self.options.iter().rev().find(|(given, _)| *given == name).map(|(_, value)| value)
    }

    /// Whether the switch was given.
    pub fn flag(&self, name: &str) -> bool {
        self.given(name).is_some()
    }

    /// The value of a whole-number option, `None` when it was not given.
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.given(name) {
            Some(Given::Int(value)) => Some(*value),
            _ => None,
        }
    }

    /// The value of a number option, `None` when it was not given.
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.given(name) {
            Some(Given::Float(value)) => Some(*value),
            _ => None,
        }
    }

    /// The value of a text option, `None` when it was not given.
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.given(name) {
            Some(Given::Text(value)) => Some(value),
            _ => None,
        }
    }

    /// The words of an argument; empty when it took none.
    pub fn words(&self, name: &str) -> &[String] {
        self.arguments.iter().find(|(argument, _)| *argument == name).map_or(&[], |(_, words)| words.as_slice())
    }

    /// The first word of an argument, `None` when it took none.
    pub fn word(&self, name: &str) -> Option<&str> {
        self.words(name).first().map(String::as_str)
    }
}

/// `-9`, `-9.5`, `-.5`: what `argparse` takes for a negative number.
fn is_negative_number(word: &str) -> bool {
    let Some(digits) = word.strip_prefix('-') else { return false };
    let all_digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    match digits.split_once('.') {
        None => all_digits(digits),
        Some((whole, fraction)) => (whole.is_empty() || all_digits(whole)) && all_digits(fraction),
    }
}

/// What a name on the command line stands for.
enum Named<'a> {
    Help,
    Opt(&'a Opt),
    Unknown,
}

/// What a word of the command line is read as.
enum Word<'a> {
    /// An argument of the command, or the value of the option before it.
    Plain,
    /// `--`: every word after it is plain.
    End,
    Help,
    /// An option, with the value attached to it by `=`.
    Opt(&'a Opt, Option<&'a str>),
    /// It looks like an option and is none.
    Unknown,
}

impl Spec {
    fn error(&self, message: impl std::fmt::Display) -> Stop {
        Stop::Usage(format!("{}\n{PROGRAM} {}: error: {message}", self.usage(), self.name))
    }

    /// The option a name stands for: the name itself, or the one long option it begins. `word`
    /// is the whole word the name was written in, for the error that names it.
    fn named(&self, name: &str, word: &str) -> Result<Named<'_>, Stop> {
        if name == "-h" || name == "--help" {
            return Ok(Named::Help);
        }
        if let Some(exact) = self.options.iter().find(|option| option.name == name) {
            return Ok(Named::Opt(exact));
        }
        if !name.starts_with("--") || name.len() < 3 {
            return Ok(Named::Unknown);
        }
        let begun: Vec<&Opt> = self.options.iter().filter(|option| option.name.starts_with(name)).collect();
        let help = "--help".starts_with(name);
        match (begun.as_slice(), help) {
            ([], false) => Ok(Named::Unknown),
            ([], true) => Ok(Named::Help),
            ([only], false) => Ok(Named::Opt(only)),
            _ => {
                let mut names: Vec<&str> = begun.iter().map(|option| option.name).collect();
                if help {
                    names.insert(0, "--help");
                }
                Err(self.error(format!("ambiguous option: {word} could match {}", names.join(", "))))
            }
        }
    }

    /// What a word is read as while options are still looked for.
    fn read<'a>(&'a self, word: &'a str) -> Result<Word<'a>, Stop> {
        if !word.starts_with('-') || word == "-" {
            return Ok(Word::Plain);
        }
        if word == "--" {
            return Ok(Word::End);
        }
        let (name, attached) = match word.split_once('=') {
            Some((name, value)) if word.starts_with("--") => (name, Some(value)),
            _ => (word, None),
        };
        match self.named(name, word)? {
            Named::Help if attached.is_none() => Ok(Word::Help),
            Named::Opt(option) => Ok(Word::Opt(option, attached)),
            // what starts with a dash and is an argument all the same
            _ if is_negative_number(word) || word.contains(' ') => Ok(Word::Plain),
            _ => Ok(Word::Unknown),
        }
    }

    /// The value of an option as its kind reads it.
    fn value(&self, option: &Opt, text: &str) -> Result<Given, Stop> {
        let invalid =
            |kind: &str| self.error(format!("argument {}: invalid {kind} value: {}", option.name, python_repr(text)));
        match option.takes {
            Takes::Nothing => Ok(Given::Switch),
            Takes::Text(_) => Ok(Given::Text(text.to_string())),
            Takes::Int(_) => text.trim().parse().map(Given::Int).map_err(|_| invalid("int")),
            Takes::Float(_) => text.trim().parse().map(Given::Float).map_err(|_| invalid("float")),
        }
    }

    /// An error when `option` may not be given together with one given before.
    fn check_exclusive(&self, option: &Opt, parsed: &Parsed) -> Result<(), Stop> {
        for group in self.exclusive.iter().filter(|group| group.contains(&option.name)) {
            let before = group.iter().find(|other| **other != option.name && parsed.given(other).is_some());
            if let Some(before) = before {
                return Err(self.error(format!("argument {}: not allowed with argument {before}", option.name)));
            }
        }
        Ok(())
    }

    /// Read a command line: the words after the command's name.
    pub fn parse(&self, args: &[String]) -> Result<Parsed, Stop> {
        let mut parsed = Parsed::default();
        // (place on the command line, word)
        let mut words: Vec<(usize, &str)> = Vec::new();
        let mut unknown: Vec<(usize, &str)> = Vec::new();
        let mut options_ended = false;
        let mut index = 0;
        while index < args.len() {
            let place = index;
            let word = args[index].as_str();
            index += 1;
            if options_ended {
                words.push((place, word));
                continue;
            }
            let (option, attached) = match self.read(word)? {
                Word::Plain => {
                    words.push((place, word));
                    continue;
                }
                Word::End => {
                    options_ended = true;
                    continue;
                }
                Word::Help => return Err(Stop::Help(self.help())),
                Word::Unknown => {
                    unknown.push((place, word));
                    continue;
                }
                Word::Opt(option, attached) => (option, attached),
            };
            self.check_exclusive(option, &parsed)?;
            let given = match (option.takes, attached) {
                (Takes::Nothing, None) => Given::Switch,
                (Takes::Nothing, Some(value)) => {
                    let value = python_repr(value);
                    return Err(self.error(format!("argument {}: ignored explicit argument {value}", option.name)));
                }
                (_, Some(value)) => self.value(option, value)?,
                // the next word, when it is not an option itself
                (_, None) => match args.get(index).map(String::as_str) {
                    Some(next) if matches!(self.read(next)?, Word::Plain) => {
                        index += 1;
                        self.value(option, next)?
                    }
                    _ => return Err(self.error(format!("argument {}: expected one argument", option.name))),
                },
            };
            parsed.options.push((option.name, given));
        }

        // every argument its share of the words, in order: what it needs first, and of the rest
        // what the ones after it do not need
        let mut left = words.as_slice();
        let mut missing: Vec<&str> = Vec::new();
        for (position, argument) in self.arguments.iter().enumerate() {
            let needed_later: usize = self.arguments[position + 1..].iter().map(|later| later.count.least()).sum();
            let free = left.len().saturating_sub(needed_later);
            let wanted = match argument.count {
                Count::One | Count::Optional => free.min(1),
                Count::OneOrMore => free,
            };
            let take = wanted.max(argument.count.least().min(left.len()));
            if take < argument.count.least() {
                missing.push(argument.name);
            }
            let (taken, rest) = left.split_at(take);
            parsed.arguments.push((argument.name, taken.iter().map(|(_, word)| word.to_string()).collect()));
            left = rest;
        }
        if !missing.is_empty() {
            return Err(self.error(format!("the following arguments are required: {}", missing.join(", "))));
        }
        unknown.extend_from_slice(left);
        if !unknown.is_empty() {
            unknown.sort_by_key(|(place, _)| *place);
            let listed: Vec<&str> = unknown.iter().map(|(_, word)| *word).collect();
            return Err(self.error(format!("unrecognized arguments: {}", listed.join(" "))));
        }
        Ok(parsed)
    }

    /// An option as the usage line and the help text show it: `--limit LIMIT`.
    fn shown(option: &Opt) -> String {
        match option.takes.label() {
            Some(label) => format!("{} {label}", option.name),
            None => option.name.to_string(),
        }
    }

    /// `usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key`
    pub fn usage(&self) -> String {
        let mut parts = vec!["[-h]".to_string()];
        let mut grouped: Vec<&str> = Vec::new();
        for option in self.options {
            if grouped.contains(&option.name) {
                continue;
            }
            match self.exclusive.iter().find(|group| group.contains(&option.name)) {
                Some(group) => {
                    grouped.extend_from_slice(group);
                    let members = self.options.iter().filter(|member| group.contains(&member.name));
                    parts.push(format!("[{}]", members.map(Spec::shown).collect::<Vec<_>>().join(" | ")));
                }
                None => parts.push(format!("[{}]", Spec::shown(option))),
            }
        }
        for argument in self.arguments {
            parts.push(match argument.count {
                Count::One => argument.name.to_string(),
                Count::Optional => format!("[{}]", argument.name),
                Count::OneOrMore => format!("{0} [{0} ...]", argument.name),
            });
        }
        format!("usage: {PROGRAM} {} {}", self.name, parts.join(" "))
    }

    /// The help text of the command: usage, what it does, its arguments and options.
    pub fn help(&self) -> String {
        let arguments: Vec<(String, &str)> =
            self.arguments.iter().map(|argument| (argument.name.to_string(), argument.help)).collect();
        let mut options = vec![("-h, --help".to_string(), "show this help message and exit")];
        options.extend(self.options.iter().map(|option| (Spec::shown(option), option.help)));
        let width = arguments.iter().chain(&options).map(|(shown, _)| shown.chars().count()).max().unwrap_or(0);
        let mut text = format!("{}\n\n{}\n", self.usage(), self.description);
        for (title, entries) in [("positional arguments:", &arguments), ("options:", &options)] {
            if entries.is_empty() {
                continue;
            }
            text.push_str(&format!("\n{title}\n"));
            for (shown, help) in entries {
                let line = format!("  {shown:<width$}  {help}");
                text.push_str(line.trim_end());
                text.push('\n');
            }
        }
        text.pop(); // the caller ends the last line
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACKS: Spec = Spec {
        name: "tracks",
        description: "Tracks of an album or a playlist",
        options: &[Opt::int("--offset", "OFFSET", ""), Opt::int("--limit", "LIMIT", "")],
        arguments: &[Arg::one("key", "")],
        exclusive: &[],
    };
    const SEARCH: Spec = Spec {
        name: "search",
        description: "Search the library and the catalog",
        options: &[Opt::int("--limit", "LIMIT", "")],
        arguments: &[Arg::one_or_more("term", "")],
        exclusive: &[],
    };
    const ACQUIRE: Spec = Spec {
        name: "acquire",
        description: "license, download and decrypt one song",
        options: &[
            Opt::switch("--stereo", "the AAC stereo variant instead of Dolby Atmos"),
            Opt::switch("--lossless", "the ALAC rendition"),
        ],
        arguments: &[Arg::one("song", "Apple Music song URL or numeric catalog id")],
        exclusive: &[&["--stereo", "--lossless"]],
    };
    const DOWNMIX: Spec = Spec {
        name: "downmix",
        description: "show or switch the live stereo downmix",
        options: &[
            Opt::float("--preamp", "DB", "preamp in dB (default: current)"),
            Opt::switch("--list", "list the available profiles"),
            Opt::switch("--live", "also print the sink's live control values"),
            Opt::text("--check", "SONG", ""),
        ],
        arguments: &[Arg::optional("key", "profile key to apply (peqdb, apple, itu)")],
        exclusive: &[],
    };
    const PLAIN: Spec =
        Spec { name: "home", description: "Recently played", options: &[], arguments: &[], exclusive: &[] };

    fn parse(spec: &Spec, args: &[&str]) -> Result<Parsed, Stop> {
        spec.parse(&args.iter().map(|word| word.to_string()).collect::<Vec<_>>())
    }

    /// The error line of a usage error (after the usage line and the program's name).
    fn error(spec: &Spec, args: &[&str]) -> String {
        match parse(spec, args) {
            Err(stop @ Stop::Usage(_)) => {
                assert_eq!(stop.status(), 2);
                let (usage, line) = stop.text().split_once('\n').expect("two lines");
                assert_eq!(usage, spec.usage());
                line.strip_prefix(&format!("amui {}: error: ", spec.name)).expect("the error prefix").to_string()
            }
            other => panic!("no usage error: {other:?}"),
        }
    }

    #[test]
    fn options_and_arguments_in_any_order() {
        for args in [
            &["l.AbCdEf1", "--offset", "5", "--limit", "20"][..],
            &["--offset", "5", "l.AbCdEf1", "--limit", "20"],
            &["--limit=20", "--offset=5", "l.AbCdEf1"],
            &["--off", "5", "--lim", "20", "--", "l.AbCdEf1"],
        ] {
            let parsed = parse(&TRACKS, args).unwrap();
            assert_eq!(
                (parsed.word("key"), parsed.int("--offset"), parsed.int("--limit")),
                (Some("l.AbCdEf1"), Some(5), Some(20)),
                "{args:?}"
            );
        }
        let parsed = parse(&TRACKS, &["1000000001"]).unwrap();
        assert_eq!((parsed.int("--offset"), parsed.int("--limit")), (None, None));
        assert_eq!(parsed.words("key"), ["1000000001"]);
        assert_eq!(parsed.words("nothing"), [] as [&str; 0]);
        assert_eq!((parsed.text("--offset"), parsed.float("--offset"), parsed.flag("--offset")), (None, None, false));
    }

    #[test]
    fn the_last_value_of_an_option_counts() {
        let parsed = parse(&TRACKS, &["--limit", "5", "x", "--limit", "7"]).unwrap();
        assert_eq!(parsed.int("--limit"), Some(7));
    }

    #[test]
    fn numbers() {
        let parsed = parse(&TRACKS, &["k", "--offset", "-5", "--limit", " +12 "]).unwrap();
        assert_eq!((parsed.int("--offset"), parsed.int("--limit")), (Some(-5), Some(12)));
        for (given, read) in [("-9", -9.0), ("-9.5", -9.5), ("-.5", -0.5), ("3", 3.0), ("1e1", 10.0), ("+2.25", 2.25)] {
            assert_eq!(parse(&DOWNMIX, &["--preamp", given]).unwrap().float("--preamp"), Some(read), "{given}");
        }
        assert_eq!(parse(&DOWNMIX, &["--preamp=-9", "apple"]).unwrap().float("--preamp"), Some(-9.0));
        assert_eq!(error(&TRACKS, &["k", "--limit", "x"]), "argument --limit: invalid int value: 'x'");
        assert_eq!(error(&TRACKS, &["k", "--limit", "1.5"]), "argument --limit: invalid int value: '1.5'");
        assert_eq!(error(&TRACKS, &["k", "--limit="]), "argument --limit: invalid int value: ''");
        assert_eq!(
            error(&TRACKS, &["k", "--limit", "99999999999999999999"]),
            "argument --limit: invalid int value: '99999999999999999999'"
        );
        assert_eq!(error(&DOWNMIX, &["--preamp", "loud"]), "argument --preamp: invalid float value: 'loud'");
        assert_eq!(error(&DOWNMIX, &["--preamp=it's"]), "argument --preamp: invalid float value: \"it's\"");
    }

    #[test]
    fn a_negative_number_is_a_word_and_a_dash_too() {
        assert!(is_negative_number("-9") && is_negative_number("-9.5") && is_negative_number("-.5"));
        for word in ["-", "--", "-x", "-9x", "-9.", "-.", "9", "", "-1e3", "--9"] {
            assert!(!is_negative_number(word), "{word}");
        }
        assert_eq!(parse(&SEARCH, &["-5", "-"]).unwrap().words("term"), ["-5", "-"]);
        assert_eq!(parse(&SEARCH, &["--", "--limit", "-x"]).unwrap().words("term"), ["--limit", "-x"]);
        // a word with a space in it names no option, so it is an argument or a value
        assert_eq!(parse(&SEARCH, &["-x y", "--not this"]).unwrap().words("term"), ["-x y", "--not this"]);
        assert_eq!(parse(&DOWNMIX, &["--check", "-x y"]).unwrap().text("--check"), Some("-x y"));
        assert_eq!(error(&SEARCH, &["a", "--limit=5 6"]), "argument --limit: invalid int value: '5 6'");
        assert_eq!(error(&SEARCH, &["a", "--limit", "-x"]), "argument --limit: expected one argument");
        assert_eq!(error(&SEARCH, &["a", "--limit", "--"]), "argument --limit: expected one argument");
    }

    #[test]
    fn missing_values_and_arguments() {
        assert_eq!(error(&TRACKS, &["k", "--limit"]), "argument --limit: expected one argument");
        assert_eq!(error(&TRACKS, &["--limit", "--offset", "5", "k"]), "argument --limit: expected one argument");
        assert_eq!(error(&DOWNMIX, &["--check", "-x"]), "argument --check: expected one argument");
        assert_eq!(parse(&DOWNMIX, &["--check=-x"]).unwrap().text("--check"), Some("-x"));
        assert_eq!(error(&TRACKS, &[]), "the following arguments are required: key");
        assert_eq!(error(&TRACKS, &["--limit", "5"]), "the following arguments are required: key");
        assert_eq!(error(&SEARCH, &["--limit", "5"]), "the following arguments are required: term");
        // a missing argument is reported before what is left over
        assert_eq!(error(&TRACKS, &["--verbose"]), "the following arguments are required: key");
    }

    #[test]
    fn what_is_left_over() {
        assert_eq!(error(&TRACKS, &["a", "b", "c"]), "unrecognized arguments: b c");
        assert_eq!(error(&TRACKS, &["a", "--verbose"]), "unrecognized arguments: --verbose");
        assert_eq!(error(&TRACKS, &["-x", "a", "b", "--y=1"]), "unrecognized arguments: -x b --y=1");
        assert_eq!(error(&PLAIN, &["now"]), "unrecognized arguments: now");
        assert_eq!(error(&PLAIN, &["-hx"]), "unrecognized arguments: -hx");
        assert_eq!(error(&PLAIN, &["--help=1"]), "unrecognized arguments: --help=1");
        assert_eq!(error(&TRACKS, &["a", "-l", "5"]), "unrecognized arguments: -l 5");
        assert_eq!(parse(&PLAIN, &[]).unwrap(), Parsed::default());
    }

    #[test]
    fn switches() {
        let parsed = parse(&DOWNMIX, &["--list"]).unwrap();
        assert!(parsed.flag("--list") && !parsed.flag("--live"));
        assert_eq!(parsed.word("key"), None);
        let parsed = parse(&DOWNMIX, &["apple", "--live", "--preamp", "-9"]).unwrap();
        assert_eq!(
            (parsed.word("key"), parsed.flag("--live"), parsed.float("--preamp")),
            (Some("apple"), true, Some(-9.0))
        );
        assert_eq!(error(&DOWNMIX, &["--list=yes"]), "argument --list: ignored explicit argument 'yes'");
        assert_eq!(error(&DOWNMIX, &["a", "b"]), "unrecognized arguments: b");
    }

    #[test]
    fn shortened_options() {
        assert!(parse(&DOWNMIX, &["--lis"]).unwrap().flag("--list"));
        assert_eq!(parse(&DOWNMIX, &["--p", "3"]).unwrap().float("--preamp"), Some(3.0));
        assert_eq!(error(&DOWNMIX, &["--li"]), "ambiguous option: --li could match --list, --live");
        assert_eq!(error(&DOWNMIX, &["--l=1"]), "ambiguous option: --l=1 could match --list, --live");
        assert!(matches!(parse(&DOWNMIX, &["--he"]), Err(Stop::Help(_))));
        assert_eq!(error(&DOWNMIX, &["--x"]), "unrecognized arguments: --x");
        // an exact name wins over the options it begins
        const NESTED: Spec = Spec {
            name: "x",
            description: "",
            options: &[Opt::switch("--live", ""), Opt::switch("--live-values", "")],
            arguments: &[],
            exclusive: &[],
        };
        let parsed = parse(&NESTED, &["--live"]).unwrap();
        assert!(parsed.flag("--live") && !parsed.flag("--live-values"));
        assert_eq!(error(&NESTED, &["--liv"]), "ambiguous option: --liv could match --live, --live-values");
        assert!(parse(&NESTED, &["--live-v"]).unwrap().flag("--live-values"));
        // the help has a long name like any option
        assert!(matches!(parse(&NESTED, &["--h"]), Err(Stop::Help(_))));
        const HELLO: Spec =
            Spec { name: "x", description: "", options: &[Opt::switch("--hello", "")], arguments: &[], exclusive: &[] };
        assert_eq!(error(&HELLO, &["--he"]), "ambiguous option: --he could match --help, --hello");
    }

    #[test]
    fn options_that_exclude_each_other() {
        assert!(parse(&ACQUIRE, &["1", "--stereo"]).unwrap().flag("--stereo"));
        assert!(parse(&ACQUIRE, &["--lossless", "1", "--lossless"]).unwrap().flag("--lossless"));
        assert_eq!(
            error(&ACQUIRE, &["1", "--stereo", "--lossless"]),
            "argument --lossless: not allowed with argument --stereo"
        );
        assert_eq!(
            error(&ACQUIRE, &["--lossless", "--stereo", "1"]),
            "argument --stereo: not allowed with argument --lossless"
        );
    }

    #[test]
    fn several_arguments_share_the_words() {
        const PAIR: Spec = Spec {
            name: "pair",
            description: "",
            options: &[],
            arguments: &[Arg::optional("first", ""), Arg::one_or_more("many", ""), Arg::one("last", "")],
            exclusive: &[],
        };
        let parsed = parse(&PAIR, &["a", "b"]).unwrap();
        assert_eq!(
            (parsed.words("first").len(), parsed.words("many"), parsed.word("last")),
            (0, &["a".to_string()][..], Some("b"))
        );
        let parsed = parse(&PAIR, &["a", "b", "c", "d"]).unwrap();
        assert_eq!(parsed.word("first"), Some("a"));
        assert_eq!(parsed.words("many"), ["b", "c"]);
        assert_eq!(parsed.word("last"), Some("d"));
        assert_eq!(error(&PAIR, &["a"]), "the following arguments are required: last");
        assert_eq!(error(&PAIR, &[]), "the following arguments are required: many, last");
        assert_eq!(parse(&SEARCH, &["two", "--limit", "3", "words"]).unwrap().words("term"), ["two", "words"]);
    }

    #[test]
    fn usage_lines() {
        assert_eq!(TRACKS.usage(), "usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key");
        assert_eq!(SEARCH.usage(), "usage: amui search [-h] [--limit LIMIT] term [term ...]");
        assert_eq!(ACQUIRE.usage(), "usage: amui acquire [-h] [--stereo | --lossless] song");
        assert_eq!(DOWNMIX.usage(), "usage: amui downmix [-h] [--preamp DB] [--list] [--live] [--check SONG] [key]");
        assert_eq!(PLAIN.usage(), "usage: amui home [-h]");
    }

    #[test]
    fn help_texts() {
        for args in [&["-h"][..], &["--help"], &["1", "--stereo", "-h"], &["--help", "--nonsense"]] {
            let stop = parse(&ACQUIRE, args).unwrap_err();
            assert_eq!(stop.status(), 0, "{args:?}");
            assert_eq!(
                stop,
                Stop::Help(
                    "usage: amui acquire [-h] [--stereo | --lossless] song\n\
                     \n\
                     license, download and decrypt one song\n\
                     \n\
                     positional arguments:\n\
                     \x20 song        Apple Music song URL or numeric catalog id\n\
                     \n\
                     options:\n\
                     \x20 -h, --help  show this help message and exit\n\
                     \x20 --stereo    the AAC stereo variant instead of Dolby Atmos\n\
                     \x20 --lossless  the ALAC rendition"
                        .to_string()
                )
            );
        }
        // an error before the help is the error; lines without a help text end at the name
        assert_eq!(error(&ACQUIRE, &["--stereo=1", "-h"]), "argument --stereo: ignored explicit argument '1'");
        assert_eq!(
            TRACKS.help(),
            "usage: amui tracks [-h] [--offset OFFSET] [--limit LIMIT] key\n\
             \n\
             Tracks of an album or a playlist\n\
             \n\
             positional arguments:\n\
             \x20 key\n\
             \n\
             options:\n\
             \x20 -h, --help       show this help message and exit\n\
             \x20 --offset OFFSET\n\
             \x20 --limit LIMIT"
        );
        assert_eq!(
            PLAIN.help(),
            "usage: amui home [-h]\n\nRecently played\n\noptions:\n  -h, --help  show this help message and exit"
        );
    }
}
