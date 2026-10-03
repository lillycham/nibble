//! Quote-your-evidence mode: the model ends its answer with the lines it rests
//! on, and nibble checks that each one is really in the file it names. A
//! quote that is nowhere to be found is the mark of an invented answer.

use std::borrow::Cow;

use crate::tools;

// Asks for one quote per line, in the shape the search results already have,
// so that a small model has seen the form before it writes it.
pub const SYSTEM_QUOTE: &str = " After your answer, give the evidence for it: quote the lines it \
rests on, each on a line of its own, in this form:\n> path: the exact text of the line\nCopy each \
line exactly as it is in the file, and quote only lines you have read. If nothing you read \
supports the answer, say so and quote nothing.";

/// One quote from a reply.
#[derive(Debug, PartialEq)]
pub struct Quote {
    /// The file it names, if it names one.
    pub path: Option<String>,
    pub text: String,
}

impl Quote {
    /// The quote as the model wrote it, shortened for a report.
    pub fn show(&self) -> String {
        let text = tools::clip(&self.text, 120);
        let more = if text.len() < self.text.len() { "..." } else { "" };
        match &self.path {
            Some(path) => format!("{path}: {text}{more}"),
            None => format!("{text}{more}"),
        }
    }
}

/// Where the quotes may come from: the texts the model was given whole (the
/// attached files, the piped input), and, when it had file tools, any file
/// those tools could have read.
pub struct Sources {
    given: Vec<(String, String)>,
    files: bool,
}

impl Sources {
    pub fn new(files: bool) -> Self {
        Sources { given: Vec::new(), files }
    }

    pub fn give(&mut self, name: &str, text: &str) {
        self.given.push((name.to_string(), text.to_string()));
    }

    /// The text a quote names, or why there is none to check it against.
    fn text(&self, path: Option<&str>) -> Result<Cow<'_, str>, String> {
        let Some(path) = path else {
            // With one text and no other way to read, there is no doubt which it means.
            return match self.given.as_slice() {
                [(_, text)] if !self.files => Ok(Cow::Borrowed(text)),
                _ => Err("names no file".to_string()),
            };
        };
        let bare = |p: &str| p.trim_start_matches("./").to_string();
        let wanted = bare(path);
        let given = self.given.iter().find(|(name, _)| {
            let name = bare(name);
            name == wanted || name.ends_with(&format!("/{wanted}")) || wanted.ends_with(&format!("/{name}"))
        });
        if let Some((_, text)) = given {
            return Ok(Cow::Borrowed(text));
        }
        if !self.files {
            return Err("not a file it was given".to_string());
        }
        // The same rules as the model's own reads, so a quote from a file it
        // could not have read is not found.
        let text = tools::resolve(path).and_then(|real| tools::read_text(&real));
        text.map(Cow::Owned).map_err(|_| "not a file it could read".to_string())
    }
}

/// The quotes in a reply: its lines that start with `>`.
pub fn find(reply: &str) -> Vec<Quote> {
    reply
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix('>'))
        .filter_map(|rest| {
            let rest = rest.trim();
            let (path, text) = match rest.split_once(": ") {
                Some((path, text)) if looks_like_path(path) => (Some(clean_path(path)), text),
                _ => (None, rest),
            };
            let text = unwrap(text.trim()).to_string();
            (!text.is_empty()).then_some(Quote { path, text })
        })
        .collect()
}

/// Whether the part before the colon names a file rather than being the
/// start of the quoted text.
fn looks_like_path(s: &str) -> bool {
    let s = s.trim_matches(|c| "`'\"<>*".contains(c));
    !s.is_empty() && !s.contains(' ') && (s.contains('.') || s.contains('/') || s == "input")
}

/// Take off what models wrap a path in, and a line number after it, which
/// they copy from search results.
fn clean_path(path: &str) -> String {
    let mut path = path.trim_matches(|c| "`'\"<>*".contains(c));
    while let Some((head, tail)) = path.rsplit_once(':') {
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
            break;
        }
        path = head;
    }
    path.to_string()
}

/// Take off a pair of quotation marks or backticks around the text.
fn unwrap(text: &str) -> &str {
    for (open, close) in [("`", "`"), ("\"", "\""), ("“", "”")] {
        if let Some(inner) = text.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
            if !inner.is_empty() {
                return inner;
            }
        }
    }
    text
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the quote is in the text. Runs of spaces count as one, since
/// models re-indent, and an ellipsis stands for text left out.
fn appears(quote: &str, text: &str) -> bool {
    let text = squash(text);
    let mut from = 0;
    for piece in quote.split(['…']).flat_map(|piece| piece.split("...")) {
        let piece = squash(piece);
        if piece.is_empty() {
            continue;
        }
        match text[from..].find(&piece) {
            Some(at) => from += at + piece.len(),
            None => return false,
        }
    }
    from > 0
}

pub struct Check {
    pub found: Vec<Quote>,
    /// Each with the reason: not in the file, or no file to look in.
    pub missing: Vec<(Quote, String)>,
}

impl Check {
    /// One line, then a line per quote that was not found.
    pub fn report(&self) -> String {
        let all = self.found.len() + self.missing.len();
        if all == 0 {
            return "the answer quotes nothing to back it up".to_string();
        }
        if self.missing.is_empty() {
            let s = if all == 1 { "the quote is" } else { "all quotes are" };
            return format!("{s} in the files ({all} checked)");
        }
        let mut out = format!("{} of {all} quotes not found:", self.missing.len());
        for (quote, why) in &self.missing {
            out += &format!("\n  {} ({why})", quote.show());
        }
        out
    }
}

/// Look for each quote in the reply in the file it names.
pub fn check(reply: &str, sources: &Sources) -> Check {
    let mut result = Check { found: Vec::new(), missing: Vec::new() };
    for quote in find(reply) {
        match sources.text(quote.path.as_deref()) {
            Ok(text) if appears(&quote.text, &text) => result.found.push(quote),
            Ok(_) => result.missing.push((quote, "not in the file".to_string())),
            Err(why) => result.missing.push((quote, why)),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_are_read_the_way_models_write_them() {
        let reply = "The limit is 24000.\n\n> src/config.rs: input_chars: 24_000,\n\
                     >`src/config.rs:67-68`: \"max_tokens: 1024,\"\n> just some text\n>\n> input: hello";
        let quotes = find(reply);
        let path = |p: &str| Some(p.to_string());
        assert_eq!(
            quotes,
            vec![
                Quote { path: path("src/config.rs"), text: "input_chars: 24_000,".into() },
                Quote { path: path("src/config.rs"), text: "max_tokens: 1024,".into() },
                Quote { path: None, text: "just some text".into() },
                Quote { path: path("input"), text: "hello".into() },
            ]
        );
    }

    #[test]
    fn a_quote_must_really_be_there() {
        let file = "fn main() {\n    let budget   = 6000;\n    run(budget);\n}\n";
        assert!(appears("let budget = 6000;", file));
        assert!(appears("let budget = 6000; run(budget);", file));
        assert!(appears("fn main() { ... run(budget);", file));
        assert!(!appears("let budget = 8000;", file));
        assert!(!appears("run(budget); ... let budget", file));
        assert!(!appears("...", file));

        let mut sources = Sources::new(false);
        sources.give("./notes/a.txt", file);
        let reply = "It is 6000.\n> notes/a.txt: let budget = 6000;\n> a.txt: let budget = 7000;\n> b.txt: run(budget);";
        let check = check(reply, &sources);
        assert_eq!(check.found.len(), 1);
        let why: Vec<_> = check.missing.iter().map(|(_, why)| why.as_str()).collect();
        assert_eq!(why, ["not in the file", "not a file it was given"]);
        assert!(check.report().starts_with("2 of 3 quotes not found:\n  a.txt: let budget = 7000;"));

        // One text and no tools: a quote without a path can only mean that one.
        assert_eq!(super::check("> run(budget);", &sources).found.len(), 1);
        assert_eq!(super::check("It is 6000.", &sources).report(), "the answer quotes nothing to back it up");
    }
}
