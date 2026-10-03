//! `nibble eval`: the same questions about the same small project, put to one
//! model or several, and scored against known answers. The project and the
//! questions are built in, so every model gets the same trial.

use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::chat::{self, message, Events};
use crate::{config, tools};

const USAGE: &str = "usage: nibble eval [options]

Asks the model questions about a small sample project, and reports how many
it got right, how many tool calls it made and how long it took.

  -m, --model NAME     try this model, from the list the window shows;
                       repeat to compare several. Without it, the model in use
  -q, --only N[,N...]  ask only these questions, by number
  -v, --verbose        show every answer and tool call, not only wrong answers";

const QUESTIONS: &str = include_str!("../eval/questions.json");

/// The sample project, written out afresh for each run.
const PROJECT: [(&str, &str); 12] = [
    ("CHANGELOG.md", include_str!("../eval/project/CHANGELOG.md")),
    ("README.md", include_str!("../eval/project/README.md")),
    ("docs/deploy.md", include_str!("../eval/project/docs/deploy.md")),
    ("lighthouse/__init__.py", include_str!("../eval/project/lighthouse/__init__.py")),
    ("lighthouse/checks.py", include_str!("../eval/project/lighthouse/checks.py")),
    ("lighthouse/cli.py", include_str!("../eval/project/lighthouse/cli.py")),
    ("lighthouse/config.py", include_str!("../eval/project/lighthouse/config.py")),
    ("lighthouse/notify.py", include_str!("../eval/project/lighthouse/notify.py")),
    ("lighthouse/store.py", include_str!("../eval/project/lighthouse/store.py")),
    ("pyproject.toml", include_str!("../eval/project/pyproject.toml")),
    ("tests/test_checks.py", include_str!("../eval/project/tests/test_checks.py")),
    ("watch.example.toml", include_str!("../eval/project/watch.example.toml")),
];

struct Question {
    text: String,
    /// Each of these must be in the answer. Each may list alternatives,
    /// separated by "|".
    expect: Vec<String>,
}

fn questions() -> Vec<Question> {
    let all: Value = serde_json::from_str(QUESTIONS).expect("built-in questions are valid JSON");
    all.as_array()
        .expect("built-in questions are a list")
        .iter()
        .map(|q| Question {
            text: q["question"].as_str().unwrap_or_default().to_string(),
            expect: q["expect"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(),
        })
        .collect()
}

/// Whether `term` is in `text` as a whole: "21" is not in "2021", nor "2.1"
/// in "2.1.0", but "45" is in "45s" and "send_ntfy" in "send_ntfy()".
fn has_term(text: &str, term: &str) -> bool {
    let (Some(first), Some(last)) = (term.chars().next(), term.chars().last()) else { return false };
    let joins = |edge: char, next: Option<char>| match next {
        None => false,
        Some(next) if edge.is_ascii_digit() => next.is_ascii_digit(),
        Some(next) if edge.is_alphanumeric() => next.is_alphanumeric() || next == '_',
        Some(_) => false,
    };
    text.match_indices(term).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let rest = &text[at + term.len()..];
        let mut after = rest.chars();
        let next = after.next();
        // A number followed by a dot and a digit goes on: 2.1 is not 2.1.0.
        let longer_number = last.is_ascii_digit() && next == Some('.') && after.next().is_some_and(|c| c.is_ascii_digit());
        !joins(first, before) && !joins(last, next) && !longer_number
    })
}

/// Whether an answer holds every expected term, ignoring case, spacing and
/// any reasoning the model wrote before it.
fn is_right(answer: &str, expect: &[String]) -> bool {
    let answer = match answer.rfind("</think>") {
        Some(end) => &answer[end + "</think>".len()..],
        None => answer,
    };
    let answer = answer.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    !expect.is_empty()
        && expect.iter().all(|term| term.split('|').any(|choice| has_term(&answer, &choice.trim().to_lowercase())))
}

/// Counts tool calls, and shows them when asked to.
struct Counter {
    calls: usize,
    verbose: bool,
}

impl Events for Counter {
    fn text(&mut self, _: &str) -> io::Result<()> {
        Ok(())
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        self.calls += 1;
        if self.verbose {
            println!("          {}", chat::describe(name, arguments));
        }
        Ok(())
    }
}

/// The sample project in a temporary directory, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn write() -> io::Result<Project> {
        let dir = std::env::temp_dir().join(format!("nibble-eval-{}", std::process::id()));
        let project = Project(dir);
        for (path, text) in PROJECT {
            let path = project.0.join(path);
            std::fs::create_dir_all(path.parent().expect("a file has a directory"))?;
            std::fs::write(path, text)?;
        }
        Ok(project)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One model's results.
struct Score {
    model: String,
    right: usize,
    asked: usize,
    calls: usize,
    time: Duration,
}

impl Score {
    fn line(&self) -> String {
        let seconds = self.time.as_secs();
        format!(
            "{} of {} right, {} tool calls, {}",
            self.right,
            self.asked,
            self.calls,
            if seconds >= 60 { format!("{} min {} s", seconds / 60, seconds % 60) } else { format!("{seconds} s") }
        )
    }
}

fn first_line(text: &str, max: usize) -> String {
    let text = text.rsplit("</think>").next().unwrap_or(text);
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    line.chars().take(max).collect::<String>() + "..."
}

/// Ask the questions of the model in use, one fresh conversation each.
fn trial(model: &str, questions: &[(usize, Question)], verbose: bool) -> Result<Score, Box<dyn Error>> {
    // The first request after a switch or a long idle loads the model. Do
    // that before the clock starts, so the first question isn't charged for it.
    eprintln!("nibble: loading {model}...");
    let mut warm = vec![message("system", "Answer in one word."), message("user", "Say ok.")];
    chat::run(&mut warm, &[], 8, &mut chat::Quiet)?;

    let system = format!("{}{}{}", chat::system(), chat::system_tools(), tools::context());
    let schemas = tools::schemas(false);
    let max_tokens = config::get().max_tokens;
    let mut score = Score { model: model.to_string(), right: 0, asked: 0, calls: 0, time: Duration::ZERO };
    println!("{model}");
    for (n, question) in questions {
        let mut messages = vec![message("system", &system), message("user", &question.text)];
        let mut counter = Counter { calls: 0, verbose };
        let started = Instant::now();
        let outcome = chat::run(&mut messages, &schemas, max_tokens, &mut counter)?;
        let took = started.elapsed();
        let right = is_right(&outcome.reply, &question.expect);
        score.asked += 1;
        score.right += usize::from(right);
        score.calls += counter.calls;
        score.time += took;
        println!(
            "  {n:>2}  {:<5}  {:>2} call{}  {:>5.1} s  {}",
            if right { "right" } else { "wrong" },
            counter.calls,
            if counter.calls == 1 { " " } else { "s" },
            took.as_secs_f64(),
            question.text
        );
        if verbose {
            println!("          answer: {}", first_line(&outcome.reply, 400));
        } else if !right {
            let expected = question.expect.join(", ").replace('|', " or ");
            println!("          expected {expected}; answered: {}", first_line(&outcome.reply, 160));
        }
    }
    println!("{model}: {}\n", score.line());
    Ok(score)
}

/// Ask the server to change to another model, and take on that model's presets.
fn switch(model: &str) -> Result<(), Box<dyn Error>> {
    let config = config::get();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut post = agent.post(format!("{}/model", config.url));
    if !config.token.is_empty() {
        post = post.header("Authorization", format!("Bearer {}", config.token));
    }
    let mut response =
        post.send_json(json!({ "model": model })).map_err(|e| format!("can't change to {model}: {e}"))?;
    if response.status() != 200 {
        let said = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("can't change to {model}: {}", said.trim()).into());
    }
    config::switch(model)?;
    Ok(())
}

/// Pick questions by number, as "3,7,12".
fn pick(only: &str, count: usize) -> Result<Vec<usize>, String> {
    only.split(',')
        .map(|n| match n.trim().parse::<usize>() {
            Ok(n) if (1..=count).contains(&n) => Ok(n),
            _ => Err(format!("there is no question {}; they go from 1 to {count}", n.trim())),
        })
        .collect()
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let (mut models, mut only, mut verbose) = (Vec::new(), None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-m" | "--model" => models.push(args.next().ok_or("--model needs a name")?),
            "-q" | "--only" => only = Some(args.next().ok_or("--only needs question numbers")?),
            "-v" | "--verbose" => verbose = true,
            _ => return Err(format!("unknown option {arg}\n\n{USAGE}").into()),
        }
    }
    let all = questions();
    let numbers = match &only {
        Some(only) => pick(only, all.len())?,
        None => (1..=all.len()).collect(),
    };
    let questions: Vec<(usize, Question)> = all.into_iter().enumerate().map(|(i, q)| (i + 1, q)).filter(|(n, _)| numbers.contains(n)).collect();

    let project = Project::write()?;
    std::env::set_current_dir(&project.0)?;
    tools::confine(&[project.0.clone()])?;

    let before = config::get().model_name.clone();
    let mut scores = Vec::new();
    let result = (|| -> Result<(), Box<dyn Error>> {
        if models.is_empty() {
            let name = if before.is_empty() { "the model in use" } else { &before };
            scores.push(trial(name, &questions, verbose)?);
        }
        for model in &models {
            switch(model)?;
            scores.push(trial(model, &questions, verbose)?);
        }
        Ok(())
    })();
    // Leave the server on the model it had, whatever happened.
    if !models.is_empty() && !before.is_empty() && config::get().model_name != before {
        if let Err(e) = switch(&before) {
            eprintln!("nibble: {e}");
        }
    }
    if scores.len() > 1 {
        let width = scores.iter().map(|s| s.model.len()).max().unwrap_or(0);
        for score in &scores {
            println!("{:width$}  {}", score.model, score.line());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn answers_are_matched_as_whole_terms() {
        let expect = |terms: &[&str]| terms.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert!(is_right("The default is **45** seconds.", &expect(&["45"])));
        assert!(is_right("45s", &expect(&["45"])));
        assert!(!is_right("It was released in 2021.", &expect(&["21"])));
        assert!(!is_right("Added in 2.1.0.", &expect(&["2.1"])));
        assert!(is_right("Added in 2.1.0.", &expect(&["2.1.0"])));
        assert!(is_right("See `send_ntfy()`.", &expect(&["send_ntfy"])));
        assert!(!is_right("See send_ntfy_now.", &expect(&["send_ntfy"])));
        assert!(is_right("Email, Matrix and ntfy.", &expect(&["email|smtp", "matrix", "ntfy"])));
        assert!(!is_right("Email and Matrix.", &expect(&["email|smtp", "matrix", "ntfy"])));
        assert!(is_right("On April\n12,  2021.", &expect(&["2021-04-12|April 12, 2021"])));
        assert!(!is_right("<think>maybe 45</think>It is 120.", &expect(&["45"])));
        assert!(!is_right("anything", &[]));
    }

    #[test]
    fn the_project_holds_every_answer_and_every_file() {
        let on_disk = |dir: &Path| {
            fn walk(dir: &Path, base: &Path, found: &mut Vec<String>) {
                for entry in std::fs::read_dir(dir).unwrap().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, base, found);
                    } else {
                        found.push(path.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                    }
                }
            }
            let mut found = Vec::new();
            walk(dir, dir, &mut found);
            found.sort();
            found
        };
        let built_in: Vec<String> = PROJECT.iter().map(|(path, _)| path.to_string()).collect();
        assert_eq!(on_disk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/project")), built_in);

        let questions = questions();
        assert_eq!(questions.len(), 20);
        let everything: String = PROJECT.iter().map(|(_, text)| *text).collect::<Vec<_>>().join("\n");
        for question in &questions {
            assert!(!question.text.is_empty() && !question.expect.is_empty());
            // At least one way of putting each answer is in the files, so
            // every question can be answered by reading.
            for term in &question.expect {
                assert!(
                    term.split('|').any(|choice| everything.to_lowercase().contains(&choice.to_lowercase())),
                    "{}: nothing in the project says {term}",
                    question.text
                );
            }
        }
    }

    #[test]
    fn questions_are_picked_by_number() {
        assert_eq!(pick("3, 1", 20).unwrap(), vec![3, 1]);
        assert!(pick("0", 20).is_err());
        assert!(pick("21", 20).is_err());
        assert!(pick("x", 20).is_err());
    }
}
